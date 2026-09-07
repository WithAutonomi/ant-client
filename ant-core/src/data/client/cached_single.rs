//! On-disk cache for single-node (non-merkle) chunk payment proofs.
//!
//! Why this exists
//! ---------------
//! Single-node uploads break the file into payment waves. Each wave is
//! one EVM transaction that produces N per-chunk payment proofs (one
//! per chunk in the wave). The proof bytes are what the storer needs
//! to accept a PUT — without them, the on-chain payment is "stranded":
//! the chain saw the tokens move but the client can no longer prove to
//! a storer that any specific chunk was paid for.
//!
//! Before this module, those proofs lived only in process memory. If
//! the upload died mid-file (network flake, residual close-K stress,
//! a Ctrl-C), every wave already paid for was unrecoverable and the
//! user had to re-quote and re-pay on the next attempt.
//!
//! This module persists the `(chunk_address, proof_bytes)` pair to
//! disk **immediately after each wave's `batch_pay` confirms**, before
//! the wave's PUT phase begins. On the next upload attempt for the same
//! source file, the cache is loaded and any chunk whose address matches
//! the current encryption skips quote+pay and goes straight to PUT.
//!
//! Lifecycle
//! ---------
//! * **append_wave** — called once per successfully paid wave, before
//!   the PUT phase. Adds the wave's `(addr, proof_bytes)` entries to
//!   the on-disk receipt and updates the cumulative cost figures.
//! * **load_for_file** — called once at the top of the upload. If a
//!   non-expired cached receipt exists for the file, the proofs are
//!   merged into the upload plan and the matching chunks skip quoting
//!   and payment.
//! * **delete_for_file** — called after a fully successful upload to
//!   remove the receipt so a future re-upload of the same path pays
//!   anew.
//! * **cleanup_outdated** — called opportunistically inside
//!   `load_for_file` to garbage-collect receipts past the expiry
//!   window.
//!
//! Filename format
//! ---------------
//! Same as `cached_merkle`: `<timestamp>_<file_hash>` under
//! `<data_dir>/payments/single/`. The subdirectory keeps single-node
//! and merkle caches from colliding (they have different on-disk
//! schemas) and makes it easy for a user to wipe one without touching
//! the other.
//!
//! Expiry
//! ------
//! On-chain quote receipts have a finite validity window
//! (`QUOTE_MAX_AGE_SECS` in `ant-node`, currently 24 h). After that,
//! storers reject the proof even if the file is otherwise resumable.
//! The cache uses a conservative 24 h expiry to match.
//!
//! Failure-mode tolerance
//! ----------------------
//! All public-facing API (`try_*` variants) swallows IO and
//! serialization errors with a `warn!` log. A busted cache never
//! prevents a real upload — at worst the user re-pays.
//!
//! Filesystem requirements
//! -----------------------
//! The atomic-write and exclusive-lock guarantees assume the data
//! directory lives on a local filesystem with working `flock(2)` (or
//! `LockFileEx` on Windows). On Linux NFS, `flock` is emulated via
//! `fcntl` POSIX locks and may degrade to per-host advisory-only;
//! SMB shares mounted on Linux are similarly fragile. Two
//! concurrent CLI processes on different hosts both pointing at the
//! same shared `payments/single/` directory could therefore lose a
//! wave's proofs to a last-writer-wins race. The platform-default
//! data dir (`~/.local/share/autonomi`, `~/Library/Application
//! Support`, `%LOCALAPPDATA%`) is local, so this is a concern only
//! for users who explicitly redirect the data dir to network
//! storage. No code-level mitigation is planned; if this becomes a
//! reported problem the right fix is a per-host instance lock on
//! `payments/single/.exclusive` at the daemon level.

use crate::config;
use crate::error::Result;
use ant_protocol::evm::Amount;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, DirEntry, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Cached single-node receipts older than this are removed from disk.
///
/// Conservative match for `QUOTE_MAX_AGE_SECS` in `ant-node` (24 h).
/// After that window, storers will reject the cached proof even if
/// the file is otherwise resumable, so keeping the cache wouldn't help.
const PAYMENT_EXPIRATION_SECS: u64 = 24 * 60 * 60;

/// Subdirectory under the platform-appropriate data dir.
///
/// `payments/single` rather than `payments/` directly so the merkle
/// cache (in `payments/`) and this cache cannot collide on filename.
const PAYMENTS_SUBDIR: &str = "payments/single";

/// On-disk schema for a single-node (non-merkle) upload receipt.
///
/// Designed to be appended to: each successful wave adds its chunk
/// proofs to `proofs` and bumps the cumulative cost fields. The whole
/// file is rewritten on each append (the size is bounded by the chunk
/// count, so this is fine in practice — a 1 GB upload at 1 MB/chunk
/// gives ~1000 entries × ~1 KB proof ≈ 1 MB receipt file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleNodeReceipt {
    /// On-disk schema version.
    ///
    /// Bumped when fields change incompatibly. A version this client
    /// doesn't recognize is treated as unreadable in `read_receipt`
    /// (returns an error → `find_existing` logs + unlinks → next
    /// attempt pays anew). `#[serde(default)]` so receipts written
    /// before this field existed deserialize as `version: 0`, which
    /// is still treated as known-current (the field's only purpose
    /// is rejecting *future* schemas the running binary doesn't
    /// understand, not migrating in-flight v0 receipts).
    #[serde(default)]
    pub version: u8,
    /// Per-chunk serialized `PaymentProof` bytes, keyed by content address.
    pub proofs: HashMap<[u8; 32], Vec<u8>>,
    /// Unix timestamp (seconds) the first wave was paid. Used for the
    /// 24 h expiry check.
    pub first_pay_timestamp: u64,
    /// Cumulative storage cost in atto, summed across all paid waves.
    pub storage_cost_atto: String,
    /// Cumulative gas cost in wei, summed across all paid waves.
    pub gas_cost_wei: u128,
}

/// Highest schema version this binary knows how to read. Receipts
/// with a higher version are rejected (the user must have upgraded
/// and downgraded between attempts).
const SCHEMA_VERSION: u8 = 1;

impl SingleNodeReceipt {
    fn new(now_secs: u64) -> Self {
        Self {
            version: SCHEMA_VERSION,
            proofs: HashMap::new(),
            first_pay_timestamp: now_secs,
            storage_cost_atto: "0".to_string(),
            gas_cost_wei: 0,
        }
    }
}

fn payments_dir() -> Result<PathBuf> {
    let dir = config::data_dir()?.join(PAYMENTS_SUBDIR);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Stable digest of the canonical path string, used as the on-disk
/// cache key.
///
/// **Must be stable across binary versions** — the user can pay a
/// wave on binary A, upgrade or downgrade between attempts, and
/// expect resume to find the receipt on binary B. The standard-
/// library `DefaultHasher` (`std::collections::hash_map::DefaultHasher`)
/// is explicitly documented as NOT stable across rustc releases, so
/// using it here would silently lose resumability on any toolchain
/// upgrade. BLAKE3 gives a permanent, fixed-output digest. The first
/// 16 bytes are plenty: with the lock-protected `find_existing` we
/// content-validate cache hits against the current encrypted chunk
/// addresses, so a 128-bit collision space is well beyond practical
/// concern.
fn file_hash_key(file_path: &str) -> String {
    let digest = blake3::hash(file_path.as_bytes());
    let bytes = digest.as_bytes();
    let mut out = String::with_capacity(32);
    for byte in &bytes[..16] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn receipt_path(dir: &Path, ts: u64, key: &str) -> PathBuf {
    dir.join(format!("{ts}_{key}"))
}

/// Append a wave's worth of paid-chunk proofs to the on-disk receipt.
///
/// If no receipt exists yet for this file, one is created. Otherwise
/// the existing file is loaded, extended with the new proofs, and
/// rewritten under a fresh `<now>_<key>` filename (with the old
/// canonical unlinked atomically).
///
/// Why the filename rotates on every append
/// ----------------------------------------
/// The 24-hour TTL is enforced by parsing the timestamp prefix from
/// the canonical filename (`cleanup_outdated` + `is_expired_filename`).
/// If we kept the original filename across waves, a receipt holding a
/// wave paid 23 h ago plus a wave paid 1 minute ago would be deleted
/// wholesale at the 24-hour mark — stranding the fresh wave's
/// payment. Rotating the filename to `<now>_<key>` on every successful
/// append makes the on-disk TTL track "time since most recent paid
/// wave" instead of "time since first wave", matching the semantic
/// users expect: the cache survives as long as it keeps being used.
/// Individual stale proofs inside the receipt are pruned by
/// `prune_locally_expired_proofs` in `batch.rs`, which checks each
/// `quote.timestamp` against the storer's per-quote budget.
///
/// Atomicity & concurrency
/// -----------------------
/// The whole read-modify-write is guarded by an exclusive advisory
/// lock on a `.lock` sidecar so two concurrent invocations of the
/// CLI on the same file path serialize at this boundary rather than
/// last-writer-wins on the receipt content. The write itself is
/// `tmp + fsync + rename` so an interrupted write never leaves a
/// truncated or partial receipt on disk.
pub fn append_wave(
    file_path: &str,
    new_proofs: HashMap<[u8; 32], Vec<u8>>,
    wave_storage_cost_atto: &str,
    wave_gas_cost_wei: u128,
) -> Result<PathBuf> {
    let dir = payments_dir()?;
    let key = file_hash_key(file_path);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let _guard = ReceiptLock::acquire(&dir, &key)?;

    // Crash-recovery: if a previous `write_receipt_atomic` was killed
    // between `sync_all(tmp)` and `rename(tmp -> canonical)`, the
    // fully-flushed `.tmp` sibling holds the only copy of the newest
    // wave's proofs. Rename it into place (or unlink it if it's
    // corrupt) under our exclusive lock so the upcoming
    // `find_existing` sees the recovered receipt.
    recover_orphaned_tmps(&dir, &key);

    // Find an existing receipt for this file (non-expired) and load
    // it, or create a fresh one stamped with now().
    let (old_path, mut receipt) = match find_existing(&dir, &key)? {
        Some((p, r)) => (Some(p), r),
        None => (None, SingleNodeReceipt::new(now)),
    };
    let new_path = receipt_path(&dir, now, &key);

    receipt.proofs.extend(new_proofs);
    // Sum costs as U256 (Amount). A wave's storage cost is wei-scale
    // atto-token and a multi-TB upload's cumulative can plausibly
    // overflow u128 (2^128 ≈ 3.4e38; a few thousand chunks at high
    // gas pricing already reach 1e36 atto). Parsing failure on
    // either side drops that wave's contribution rather than
    // silently zeroing the running total.
    if let (Ok(prev), Ok(add)) = (
        receipt.storage_cost_atto.parse::<Amount>(),
        wave_storage_cost_atto.parse::<Amount>(),
    ) {
        receipt.storage_cost_atto = prev.saturating_add(add).to_string();
    }
    receipt.gas_cost_wei = receipt.gas_cost_wei.saturating_add(wave_gas_cost_wei);

    write_receipt_atomic(&new_path, &receipt)?;
    // Unlink the old canonical (different filename), if any. Order:
    // write-new then unlink-old means a crash between them leaves
    // both files on disk briefly; `find_existing` returns the newer
    // by directory iteration order and a subsequent
    // `dedupe_canonical_receipts` cleans the older up. No proofs
    // are ever lost in the gap because both files hold the same
    // load-extend-write content; new_path is a strict superset.
    if let Some(old) = old_path {
        if old != new_path {
            let _ = fs::remove_file(&old);
        }
    }
    debug!(
        "Appended {} proofs to single-node receipt for {file_path:?} ({})",
        receipt.proofs.len(),
        new_path.display()
    );
    Ok(new_path)
}

/// Remove specific chunk proofs from the cached receipt for a file,
/// but only if the on-disk proof bytes still match the bytes the
/// caller observed at load time (compare-and-swap semantics).
///
/// Why the "if unchanged" check matters
/// ------------------------------------
/// `load_for_file` releases its exclusive lock before returning the
/// snapshot. Between the load and a subsequent drop, another process
/// (or the same process from a different code path) can lock, observe
/// that a chunk needs re-payment, pay it, and append a FRESH proof
/// for the same address. Without a content check, this drop would
/// clobber the fresh proof and strand the just-completed on-chain
/// payment — see test `toctou_load_then_drop_evicts_concurrently_refreshed_proof`.
///
/// Caller passes `(address, expected_bytes)` pairs. Under the lock,
/// we drop the address only if its current on-disk bytes still equal
/// `expected_bytes`. If they differ, a concurrent re-pay won the race
/// and we leave the new entry intact.
///
/// If the receipt becomes empty after the drop, the file is removed
/// from disk so a fresh upload starts cleanly.
pub fn drop_proofs_for_file(file_path: &str, expected: &[([u8; 32], Vec<u8>)]) -> Result<()> {
    if expected.is_empty() {
        return Ok(());
    }
    let dir = payments_dir()?;
    let key = file_hash_key(file_path);
    let _guard = ReceiptLock::acquire(&dir, &key)?;
    recover_orphaned_tmps(&dir, &key);
    let Some((path, mut receipt)) = find_existing(&dir, &key)? else {
        return Ok(());
    };
    let before = receipt.proofs.len();
    let mut refreshed = 0usize;
    for (addr, expected_bytes) in expected {
        match receipt.proofs.get(addr) {
            Some(current) if current == expected_bytes => {
                receipt.proofs.remove(addr);
            }
            Some(_) => {
                refreshed += 1;
            }
            None => {}
        }
    }
    if refreshed > 0 {
        info!(
            "Skipped dropping {refreshed} stale proofs whose bytes changed since load \
             (concurrent re-pay refreshed them — keeping the fresh proof)"
        );
    }
    let dropped = before.saturating_sub(receipt.proofs.len());
    if dropped == 0 {
        return Ok(());
    }
    if receipt.proofs.is_empty() {
        if let Err(e) = fs::remove_file(&path) {
            // remove_file failed (eg. EACCES). Fall back to writing
            // the empty receipt atomically so the on-disk content is
            // not stale — an empty proofs map still forces the next
            // attempt to re-quote+re-pay every chunk, which is the
            // intended outcome of "every cached proof is stale".
            warn!(
                "Could not remove emptied single-node receipt {} ({e}); \
                 writing empty receipt instead",
                path.display()
            );
            write_receipt_atomic(&path, &receipt)?;
        } else {
            debug!(
                "Dropped final {dropped} proofs from single-node receipt for {file_path:?}; \
                 receipt removed"
            );
        }
        return Ok(());
    }
    write_receipt_atomic(&path, &receipt)?;
    debug!(
        "Dropped {dropped} stale proofs from single-node receipt for {file_path:?} ({})",
        path.display()
    );
    Ok(())
}

/// Best-effort `drop_proofs_for_file`. Logs on failure.
pub fn try_drop_proofs_for_file(file_path: &str, expected: &[([u8; 32], Vec<u8>)]) {
    if let Err(e) = drop_proofs_for_file(file_path, expected) {
        warn!(
            "Failed to drop stale proofs from cached single-node receipt for \
             {file_path:?}: {e}. Stale entries may be retried next attempt."
        );
    }
}

/// Best-effort `append_wave`. Logs on failure, returns nothing.
///
/// Intended for the hot path: if we can't persist the receipt the
/// upload still proceeds, the user just loses resume capability for
/// that wave.
pub fn try_append_wave(
    file_path: &str,
    new_proofs: HashMap<[u8; 32], Vec<u8>>,
    wave_storage_cost_atto: &str,
    wave_gas_cost_wei: u128,
) {
    if let Err(e) = append_wave(
        file_path,
        new_proofs,
        wave_storage_cost_atto,
        wave_gas_cost_wei,
    ) {
        warn!(
            "Failed to cache single-node payment receipt for {file_path:?}: {e}. \
             Upload will proceed without resume support for this wave."
        );
    }
}

/// Load the cached single-node receipt for a source file path, if any.
///
/// Side-effect: opportunistically removes expired receipts and recovers
/// orphaned `.tmp` files from a crashed previous write.
pub fn load_for_file(file_path: &str) -> Result<Option<(PathBuf, SingleNodeReceipt)>> {
    cleanup_outdated();
    let dir = payments_dir()?;
    let key = file_hash_key(file_path);
    // Recover under the same lock that append_wave/drop hold so we
    // can't race them mid-rename.
    let _guard = ReceiptLock::acquire(&dir, &key)?;
    recover_orphaned_tmps(&dir, &key);
    find_existing(&dir, &key)
}

/// Best-effort load. Logs and returns `None` on error.
pub fn try_load_for_file(file_path: &str) -> Option<(PathBuf, SingleNodeReceipt)> {
    match load_for_file(file_path) {
        Ok(opt) => opt,
        Err(e) => {
            warn!(
                "Failed to look up cached single-node receipt for {file_path:?}: {e}. \
                 Starting a fresh upload."
            );
            None
        }
    }
}

pub fn delete_for_file(file_path: &str) -> Result<()> {
    let dir = payments_dir()?;
    let key = file_hash_key(file_path);
    // Lock so we don't race with an in-flight append_wave from another
    // process. The lock sidecar itself is excluded from removal.
    let _guard = ReceiptLock::acquire(&dir, &key)?;
    if let Ok(read_dir) = fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Skip the lock sidecar (still held by `_guard`).
            if name.ends_with(".lock") {
                continue;
            }
            if !name.contains(&key) {
                continue;
            }
            // Also unlink matching `.tmp` siblings — otherwise an
            // interrupted write left behind from this key's last
            // crash would be promoted to canonical by
            // `recover_orphaned_tmps` on the next upload of the same
            // path, resurrecting a receipt the user explicitly deleted.
            let _ = fs::remove_file(&path);
            debug!("Deleted cached single-node receipt {}", path.display());
        }
    }
    Ok(())
}

pub fn try_delete_for_file(file_path: &str) {
    if let Err(e) = delete_for_file(file_path) {
        warn!(
            "Failed to delete cached single-node receipt for {file_path:?}: {e}. \
             Will be cleaned up after expiry."
        );
    }
}

/// Garbage-collect cached receipts past the expiry window.
pub fn cleanup_outdated() {
    let Ok(dir) = payments_dir() else {
        return;
    };
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        if is_expired_entry(&entry) {
            let path = entry.path();
            info!(
                "Removing expired cached single-node payment file: {}",
                path.display()
            );
            let _ = fs::remove_file(path);
        }
    }
}

/// Recover or unlink any `<canonical>.tmp` sidecar for this key.
///
/// A crash between `sync_all(tmp)` and `rename(tmp -> canonical)` in
/// `write_receipt_atomic` leaves a fully-flushed `.tmp` on disk. It's
/// the ONLY copy of the newest wave's proofs (the canonical file still
/// holds the pre-append state, or doesn't exist for a fresh upload).
/// Without recovery, `find_existing` skips it via the `.tmp` filter and
/// the wave's payment is silently lost on the next attempt.
///
/// Recovery is safe to run only while the receipt lock is held —
/// otherwise we could race an in-flight `write_receipt_atomic` that
/// has just opened its own `.tmp`.
///
/// For each `<...>_<key>.tmp` we find: deserialize it. If valid,
/// rename to its canonical name (strip the `.tmp` suffix). If invalid,
/// unlink — it's a torn or zero-byte file from a kill mid-write.
fn recover_orphaned_tmps(dir: &Path, key: &str) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };

    // Collect candidates first so we can pick the newest one
    // deterministically. Two-pass design covers the case where two
    // separate crashes left two distinct `<ts1>_<key>.tmp` and
    // `<ts2>_<key>.tmp` siblings: a naïve loop would rename BOTH
    // into their own canonical names (different filenames, same
    // key), and `find_existing` would non-deterministically pick
    // one to load. That's a bounded but real payment-loss bug —
    // proofs in the unloaded receipt are silently discarded.
    let mut candidates: Vec<(u64, PathBuf, bool)> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".tmp") || !name.contains(key) {
            continue;
        }
        let ts = name
            .split_once('_')
            .and_then(|(ts, _)| ts.parse::<u64>().ok())
            .unwrap_or(0);
        let readable = read_receipt(&path).is_ok();
        candidates.push((ts, path, readable));
    }

    // Sort descending by timestamp so the newest readable .tmp wins.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));

    let mut recovered = false;
    for (_, path, readable) in candidates {
        if recovered || !readable {
            // Either we already promoted a newer .tmp to canonical
            // (this older one is superseded and would clobber it on
            // rename), OR this .tmp is corrupt — either way, unlink.
            let _ = fs::remove_file(&path);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let canonical_name = &name[..name.len() - ".tmp".len()];
        let canonical = path.with_file_name(canonical_name);
        match fs::rename(&path, &canonical) {
            Ok(()) => {
                info!(
                    "Recovered orphaned receipt {} -> {}",
                    path.display(),
                    canonical.display()
                );
                recovered = true;
            }
            Err(e) => warn!(
                "Could not recover orphaned receipt {} ({e})",
                path.display()
            ),
        }
    }

    dedupe_canonical_receipts(dir, key);
}

/// Keep at most one canonical receipt per key, **merging** the proof
/// content of every readable sibling into the winning one before
/// unlinking the rest.
///
/// Multiple canonical receipts for the same key can arise if a
/// previous `append_wave` raced an aborted recovery, if a buggier
/// older binary wrote without rotating, or if manual file recovery
/// dropped a backup alongside the live file. Without merging, an
/// older sibling can hold proofs the newer one never saw — eg. the
/// older was written before a partial `delete_for_file` was
/// interrupted, leaving the older as the only carrier of some
/// waves' on-chain payments. Blind newest-wins would strand those.
///
/// Strategy: read every readable canonical sibling for the key, union
/// their `proofs` maps and sum costs into the newest-timestamp
/// canonical (overwriting it atomically), then unlink the rest.
/// Unreadable siblings are unlinked without contributing — they
/// can't strand a payment that's already corrupt-on-disk.
fn dedupe_canonical_receipts(dir: &Path, key: &str) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    let mut canonicals: Vec<(u64, PathBuf)> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".tmp") || name.ends_with(".lock") {
            continue;
        }
        if !name.contains(key) {
            continue;
        }
        let ts = name
            .split_once('_')
            .and_then(|(ts, _)| ts.parse::<u64>().ok())
            .unwrap_or(0);
        canonicals.push((ts, path));
    }
    if canonicals.len() <= 1 {
        return;
    }
    canonicals.sort_by_key(|c| std::cmp::Reverse(c.0));

    // Identify the winner (newest), then fold every other readable
    // sibling into it.
    let (winner_ts, winner_path) = canonicals[0].clone();
    let mut winner = match read_receipt(&winner_path) {
        Ok(r) => r,
        Err(_) => {
            // Newest is corrupt: unlink it and let the next-newest
            // become the winner on the recursive retry.
            warn!(
                "Newest canonical {} unreadable; unlinking and retrying dedupe",
                winner_path.display()
            );
            let _ = fs::remove_file(&winner_path);
            return dedupe_canonical_receipts(dir, key);
        }
    };

    let mut merged_from = 0usize;
    for (_, stale) in canonicals.iter().skip(1) {
        match read_receipt(stale) {
            Ok(other) => {
                // Union proofs: an entry only present in the older
                // sibling represents a paid wave the newer never saw.
                // Keep the WINNER's bytes when both have the same
                // address (newer paid wave's proof — by load-extend-
                // write semantics newer should hold the same proof
                // unless a buggier binary wrote independently).
                let mut added = 0usize;
                for (addr, bytes) in other.proofs {
                    winner.proofs.entry(addr).or_insert_with(|| {
                        added += 1;
                        bytes
                    });
                }
                if let (Ok(w), Ok(o)) = (
                    winner.storage_cost_atto.parse::<Amount>(),
                    other.storage_cost_atto.parse::<Amount>(),
                ) {
                    winner.storage_cost_atto = w.saturating_add(o).to_string();
                }
                winner.gas_cost_wei = winner.gas_cost_wei.saturating_add(other.gas_cost_wei);
                winner.first_pay_timestamp =
                    winner.first_pay_timestamp.min(other.first_pay_timestamp);
                merged_from += 1;
                info!(
                    "Merged {added} proofs from older canonical {} into winner {}",
                    stale.display(),
                    winner_path.display()
                );
            }
            Err(_) => {
                warn!(
                    "Dropping unreadable duplicate canonical {} (no recoverable proofs)",
                    stale.display()
                );
            }
        }
        let _ = fs::remove_file(stale);
    }

    if merged_from > 0 {
        // Rewrite the winner under its own filename with the merged
        // content. Same path, write-tmp-and-rename keeps the on-disk
        // state coherent across the rewrite.
        if let Err(e) = write_receipt_atomic(&winner_path, &winner) {
            warn!(
                "Could not rewrite merged canonical receipt {} ({e}); \
                 winner retains pre-merge content and the older proofs \
                 are lost. Best-effort: leaving on-disk state as-is.",
                winner_path.display()
            );
        }
    }
    let _ = winner_ts;
}

fn find_existing(dir: &Path, key: &str) -> Result<Option<(PathBuf, SingleNodeReceipt)>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            debug!("Could not read payments dir {}: {e}", dir.display());
            return Ok(None);
        }
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Skip lock sidecars and in-flight atomic-write temp files.
        if name.ends_with(".lock") || name.ends_with(".tmp") {
            continue;
        }
        if !name.contains(key) {
            continue;
        }
        if is_expired_filename(name) {
            continue;
        }
        match read_receipt(&path) {
            Ok(receipt) => {
                info!(
                    "Found previous single-node upload attempt, resuming with \
                     {} cached proofs from {}",
                    receipt.proofs.len(),
                    path.display()
                );
                return Ok(Some((path, receipt)));
            }
            Err(e) => {
                // Unlink so corrupt receipts can't accumulate on
                // disk for up to 24 h (the filename-timestamp
                // expiry doesn't reap them — only the canonical
                // timestamp is checked, and a corrupt-but-recent
                // receipt would be silently kept). Callers always
                // hold the receipt lock when this runs, so unlinking
                // here cannot race a concurrent rename.
                warn!(
                    "Cached single-node receipt at {} is unreadable ({e}). \
                     Unlinking and starting a fresh upload.",
                    path.display()
                );
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(None)
}

fn is_expired_entry(entry: &DirEntry) -> bool {
    let path = entry.path();
    if !path.is_file() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // Don't reap lock sidecars or in-flight tmp files via filename
    // timestamp parsing — they aren't receipts.
    if name.ends_with(".lock") || name.ends_with(".tmp") {
        return false;
    }
    is_expired_filename(name)
}

fn is_expired_filename(name: &str) -> bool {
    let ts_str = match name.split_once('_') {
        Some((ts, _)) => ts,
        None => return false,
    };
    let Ok(ts) = ts_str.parse::<u64>() else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now > ts.saturating_add(PAYMENT_EXPIRATION_SECS)
}

fn read_receipt(path: &Path) -> Result<SingleNodeReceipt> {
    let handle = File::open(path)?;
    let receipt: SingleNodeReceipt = rmp_serde::decode::from_read(BufReader::new(handle))
        .map_err(|e| crate::error::Error::Io(std::io::Error::other(e.to_string())))?;
    if receipt.version > SCHEMA_VERSION {
        // Future schema written by a newer binary the user downgraded
        // from. Treat as unreadable so the caller unlinks it; the
        // alternative (silently re-paying) is no worse, and unlink
        // keeps the cache directory from accumulating poison.
        return Err(crate::error::Error::Io(std::io::Error::other(format!(
            "cached receipt has unknown schema version {} (this binary supports up to {SCHEMA_VERSION})",
            receipt.version
        ))));
    }
    Ok(receipt)
}

/// Atomic write via `<path>.tmp` + `fsync(tmp)` + `rename` + `fsync(dir)`.
///
/// `File::create` (the prior implementation) truncated the destination
/// before writing, so a crash or concurrent reader mid-write saw a
/// zero-byte or partial receipt — payment proofs gone, on-chain payment
/// stranded. `rename(2)` is atomic on POSIX: either the new contents
/// replace the old or nothing changes. We then fsync the parent
/// directory so the rename itself is durable: without that, a power
/// cut after rename could leave the directory entry unflushed and the
/// next boot would see the old (now-stale) name.
///
/// The BufWriter is held in a named local and explicitly
/// `into_inner()`-checked. The prior version constructed it inline as
/// the argument to `rmp_serde::encode::write`, which meant any flush
/// error during BufWriter drop was silently swallowed and a truncated
/// msgpack file could be renamed into place.
fn write_receipt_atomic(path: &Path, receipt: &SingleNodeReceipt) -> Result<()> {
    let tmp_path = tmp_path_for(path);
    {
        let handle = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        let mut writer = BufWriter::new(handle);
        if let Err(e) = rmp_serde::encode::write(&mut writer, receipt) {
            let _ = fs::remove_file(&tmp_path);
            return Err(crate::error::Error::Io(std::io::Error::other(
                e.to_string(),
            )));
        }
        let mut handle = writer.into_inner().map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            crate::error::Error::Io(std::io::Error::other(format!(
                "BufWriter flush failed: {e}"
            )))
        })?;
        if let Err(e) = handle.flush() {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        if let Err(e) = handle.sync_all() {
            let _ = fs::remove_file(&tmp_path);
            return Err(e.into());
        }
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    // fsync the parent dir so the rename itself is durable on power
    // loss. On macOS this requires opening the dir read-only; on Linux
    // O_RDONLY is the only option that works for directories anyway.
    // Best-effort: if the parent can't be fsync'd we still consider
    // the rename committed, since most modern filesystems (ext4,
    // APFS) journal directory metadata.
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("receipt");
    tmp.set_file_name(format!("{name}.tmp"));
    tmp
}

/// Advisory exclusive file lock on a per-file sidecar.
///
/// Two concurrent `ant file upload` invocations on the same source path
/// would otherwise race: both read the existing receipt, both extend
/// it with their own wave's proofs, both write — and the later write
/// silently loses the earlier wave's proofs. That stranded the on-chain
/// payment for the first wave. The lock makes `append_wave` and
/// `drop_proofs_for_file` mutually exclusive across processes.
///
/// `fs2::FileExt::lock_exclusive` translates to `flock(2)` on Unix and
/// `LockFileEx` on Windows. The lock releases when the underlying
/// `File` is dropped.
struct ReceiptLock {
    file: File,
}

impl ReceiptLock {
    fn acquire(dir: &Path, key: &str) -> Result<Self> {
        let path = dir.join(format!("{key}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for ReceiptLock {
    fn drop(&mut self) {
        // The sidecar file is left on disk by design: deleting it
        // would race with another waiter that has already `open`-ed
        // it but not yet `lock_exclusive`-ed it — they'd silently
        // hold a lock on an unlinked inode and not actually exclude
        // us. A stale empty `.lock` file is harmless.
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_hash_key_is_stable() {
        assert_eq!(file_hash_key("/tmp/a"), file_hash_key("/tmp/a"));
        assert_ne!(file_hash_key("/tmp/a"), file_hash_key("/tmp/b"));
    }

    #[test]
    fn file_hash_key_uses_stable_digest_across_invocations() {
        // BLAKE3 is a fixed-output cryptographic hash, so the key for
        // a given path string must be identical not just within a
        // process run but across binary versions / rustc upgrades.
        // Pin the expected hex digest so a future change away from
        // BLAKE3 (or back to DefaultHasher) trips this test loudly.
        // First 16 bytes of BLAKE3("/tmp/anselme-cache-stable-test"):
        let expected = "491a1a569cd6c544074a70504b2b5183";
        assert_eq!(file_hash_key("/tmp/anselme-cache-stable-test"), expected);
    }

    /// Reproduces codex finding #1: receipt filename used to embed
    /// the FIRST wave's timestamp. A wave paid 23h after that first
    /// wave would get dropped by filename-TTL at the 24h mark even
    /// though it's only an hour old.
    ///
    /// Post-fix: `append_wave` rotates the canonical filename to
    /// `<now>_<key>` on every successful append, so the filename
    /// timestamp tracks the LAST paid wave. The receipt survives as
    /// long as it keeps being used.
    #[test]
    fn append_wave_rotates_filename_so_late_waves_dont_age_out() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-ttl-rotation-test-{nanos}");

        let mut wave1: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        wave1.insert([1u8; 32], vec![1]);
        let path_after_wave1 = append_wave(&file_path, wave1, "10", 20)?;
        let ts_after_wave1 = path_after_wave1
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split_once('_'))
            .and_then(|(ts, _)| ts.parse::<u64>().ok())
            .expect("wave1 receipt name parses");

        // Sleep just long enough that `now` advances by at least 1
        // second. Without this, both waves can land on the same
        // timestamp and the rotation is a no-op for this test
        // (still correct semantics, just not observable here).
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let mut wave2: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        wave2.insert([2u8; 32], vec![2]);
        let path_after_wave2 = append_wave(&file_path, wave2, "5", 10)?;
        let ts_after_wave2 = path_after_wave2
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split_once('_'))
            .and_then(|(ts, _)| ts.parse::<u64>().ok())
            .expect("wave2 receipt name parses");

        assert_ne!(
            path_after_wave1, path_after_wave2,
            "filename must rotate so TTL tracks LAST wave, not first"
        );
        assert!(
            ts_after_wave2 > ts_after_wave1,
            "rotated filename's timestamp must be strictly newer"
        );
        assert!(
            !path_after_wave1.exists(),
            "old canonical must be unlinked after the rewrite"
        );
        assert!(path_after_wave2.exists());

        // The merged receipt contains BOTH waves' proofs at the new
        // path — the older entries are NOT lost in the rotation.
        let (_, loaded) = load_for_file(&file_path)?.expect("receipt should load");
        assert!(loaded.proofs.contains_key(&[1u8; 32]));
        assert!(loaded.proofs.contains_key(&[2u8; 32]));

        delete_for_file(&file_path)?;
        Ok(())
    }

    #[test]
    fn expired_filename_detected() {
        let stale = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(PAYMENT_EXPIRATION_SECS + 60);
        assert!(is_expired_filename(&format!("{stale}_abc")));

        let fresh = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(60);
        assert!(!is_expired_filename(&format!("{fresh}_abc")));
    }

    #[test]
    fn malformed_filename_is_not_expired() {
        assert!(!is_expired_filename("nonsense"));
        assert!(!is_expired_filename("not_a_number_abc"));
    }

    #[test]
    fn drop_proofs_removes_only_specified_addresses() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-drop-proofs-test-{nanos}");
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([1u8; 32], vec![1]);
        proofs.insert([2u8; 32], vec![2]);
        proofs.insert([3u8; 32], vec![3]);
        append_wave(&file_path, proofs, "30", 60)?;

        drop_proofs_for_file(&file_path, &[([2u8; 32], vec![2])])?;

        let (_, loaded) = load_for_file(&file_path)?.expect("receipt still present");
        assert_eq!(loaded.proofs.len(), 2);
        assert!(loaded.proofs.contains_key(&[1u8; 32]));
        assert!(!loaded.proofs.contains_key(&[2u8; 32]));
        assert!(loaded.proofs.contains_key(&[3u8; 32]));

        delete_for_file(&file_path)?;
        Ok(())
    }

    #[test]
    fn drop_proofs_skips_drop_if_bytes_have_changed() -> Result<()> {
        // CAS semantics: caller passes the bytes they observed; the
        // drop is a no-op if a concurrent writer refreshed those
        // bytes. This is the TOCTOU fix — without it, a stale-list
        // computed at load time can clobber a fresh proof appended
        // mid-prune.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-drop-cas-test-{nanos}");
        let mut old: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        old.insert([5u8; 32], vec![0xAA]);
        append_wave(&file_path, old, "10", 20)?;

        // Simulate a concurrent re-pay that refreshed the proof
        // bytes for [5; 32] between load and drop.
        let mut fresh: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        fresh.insert([5u8; 32], vec![0xBB]);
        append_wave(&file_path, fresh, "0", 0)?;

        // Caller's stale view was vec![0xAA]; CAS must reject the drop.
        drop_proofs_for_file(&file_path, &[([5u8; 32], vec![0xAA])])?;

        let (_, loaded) = load_for_file(&file_path)?.expect("receipt still present");
        assert_eq!(
            loaded.proofs.get(&[5u8; 32]),
            Some(&vec![0xBB]),
            "fresh proof must NOT be clobbered by a CAS drop with stale bytes"
        );

        delete_for_file(&file_path)?;
        Ok(())
    }

    #[test]
    fn drop_proofs_removes_receipt_file_when_emptied() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-drop-empty-test-{nanos}");
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([7u8; 32], vec![7]);
        append_wave(&file_path, proofs, "10", 20)?;

        drop_proofs_for_file(&file_path, &[([7u8; 32], vec![7])])?;

        assert!(
            load_for_file(&file_path)?.is_none(),
            "empty receipt should be removed"
        );
        Ok(())
    }

    #[test]
    fn drop_proofs_unknown_address_is_noop() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-drop-noop-test-{nanos}");
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([9u8; 32], vec![9]);
        append_wave(&file_path, proofs, "10", 20)?;

        drop_proofs_for_file(&file_path, &[([42u8; 32], vec![42])])?;

        let (_, loaded) = load_for_file(&file_path)?.expect("receipt still present");
        assert_eq!(loaded.proofs.len(), 1);
        assert!(loaded.proofs.contains_key(&[9u8; 32]));

        delete_for_file(&file_path)?;
        Ok(())
    }

    #[test]
    fn drop_proofs_on_missing_receipt_is_noop() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-drop-missing-test-{nanos}");
        drop_proofs_for_file(&file_path, &[([0u8; 32], vec![0])])?;
        assert!(load_for_file(&file_path)?.is_none());
        Ok(())
    }

    #[test]
    fn write_receipt_atomic_leaves_no_tmp_file() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-atomic-tmp-test-{nanos}");
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([5u8; 32], vec![5]);
        let receipt_path = append_wave(&file_path, proofs, "1", 2)?;
        let tmp = tmp_path_for(&receipt_path);
        assert!(!tmp.exists(), "tmp sibling must be cleaned up after rename");
        assert!(receipt_path.exists());
        delete_for_file(&file_path)?;
        Ok(())
    }

    #[test]
    fn find_existing_ignores_lock_and_tmp_sidecars() -> Result<()> {
        // Two real receipts plus stray .lock and .tmp files in the
        // same directory should not confuse find_existing or get
        // GC'd by cleanup_outdated. Crash-during-write leaves .tmp
        // siblings behind; concurrent locks leave .lock siblings.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-sidecar-test-{nanos}");
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([6u8; 32], vec![6]);
        let receipt_path = append_wave(&file_path, proofs, "1", 2)?;

        // Drop a stray .tmp file alongside (simulates crash during
        // a previous atomic write).
        let dir = receipt_path.parent().expect("receipt has parent dir");
        let stray_tmp = dir.join("123456_deadbeef.tmp");
        fs::write(&stray_tmp, b"garbage")?;

        let (loaded_path, loaded) = load_for_file(&file_path)?.expect("receipt still loaded");
        assert_eq!(loaded_path, receipt_path);
        assert_eq!(loaded.proofs.len(), 1);
        assert!(stray_tmp.exists(), "stray tmp not auto-deleted by load");

        // cleanup_outdated must not touch .tmp / .lock by mistake
        // even if their filename prefix would parse as a long-ago
        // timestamp.
        cleanup_outdated();
        assert!(stray_tmp.exists());

        let _ = fs::remove_file(&stray_tmp);
        delete_for_file(&file_path)?;
        Ok(())
    }

    #[test]
    fn concurrent_append_waves_do_not_lose_proofs() -> Result<()> {
        // Two threads appending to the SAME file_path. Without the
        // exclusive lock + atomic rename, both threads read the old
        // receipt, both extend with their own proofs, both write —
        // last writer wins and the other thread's proofs are lost
        // (== on-chain payment for those chunks is stranded). With
        // the lock, both waves' proofs must survive.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-concurrent-test-{nanos}");
        let fp1 = file_path.clone();
        let fp2 = file_path.clone();

        let t1 = std::thread::spawn(move || {
            let mut wave: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
            for i in 0u8..32 {
                wave.insert([i; 32], vec![i]);
            }
            append_wave(&fp1, wave, "10", 20)
        });
        let t2 = std::thread::spawn(move || {
            let mut wave: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
            for i in 32u8..64 {
                wave.insert([i; 32], vec![i]);
            }
            append_wave(&fp2, wave, "10", 20)
        });
        t1.join().expect("thread1 panicked")?;
        t2.join().expect("thread2 panicked")?;

        let (_, loaded) = load_for_file(&file_path)?.expect("receipt should load");
        assert_eq!(
            loaded.proofs.len(),
            64,
            "all 64 proofs must survive concurrent appends"
        );
        for i in 0u8..64 {
            assert!(
                loaded.proofs.contains_key(&[i; 32]),
                "proof {i} lost in concurrent append"
            );
        }

        delete_for_file(&file_path)?;
        Ok(())
    }

    #[test]
    fn roundtrip_save_load_delete() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-resumable-single-test-{nanos}");
        let mut wave1: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        wave1.insert([2u8; 32], vec![10, 20]);
        let path1 = append_wave(&file_path, wave1, "50", 100)?;
        assert!(path1.exists());

        let mut wave2: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        wave2.insert([3u8; 32], vec![30, 40]);
        let path2 = append_wave(&file_path, wave2, "70", 50)?;
        // Appending refreshes the receipt filename's timestamp. If the writes
        // straddle a second boundary, the old path is replaced by the new one.
        assert!(path2.exists());
        if path1 != path2 {
            assert!(!path1.exists());
        }

        let (loaded_path, loaded) = load_for_file(&file_path)?.expect("receipt should load");
        assert_eq!(loaded_path, path2);
        assert_eq!(loaded.proofs.len(), 2);
        assert!(loaded.proofs.contains_key(&[2u8; 32]));
        assert!(loaded.proofs.contains_key(&[3u8; 32]));
        // Cumulative cost summed across waves.
        assert_eq!(loaded.storage_cost_atto, "120");
        assert_eq!(loaded.gas_cost_wei, 150);

        delete_for_file(&file_path)?;
        assert!(load_for_file(&file_path)?.is_none());
        Ok(())
    }

    /// Stronger version of `concurrent_append_waves_do_not_lose_proofs`.
    ///
    /// The 2-thread test fails when the lock is removed but the failure
    /// mode is an `Os { code: 2, NotFound }` from `rename(2)` colliding
    /// on a fresh canonical path — not the silent proof loss the lock
    /// is supposed to prevent. This pre-seeds a receipt so both
    /// concurrent appenders run the read-modify-write path against the
    /// same existing canonical file. Without the lock the last writer
    /// overwrites the others and proofs are silently dropped while
    /// every `append_wave` call returns `Ok`.
    #[test]
    fn concurrent_append_after_existing_receipt_keeps_all_proofs() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-concurrent-silent-test-{nanos}");

        let mut seed: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        seed.insert([200u8; 32], vec![200]);
        seed.insert([201u8; 32], vec![201]);
        seed.insert([202u8; 32], vec![202]);
        seed.insert([203u8; 32], vec![203]);
        append_wave(&file_path, seed, "1", 1)?;

        const THREADS: u8 = 16;
        const PER_THREAD: u8 = 8;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let fp = file_path.clone();
                std::thread::spawn(move || {
                    let mut wave: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
                    let base = t.wrapping_mul(PER_THREAD);
                    for i in 0..PER_THREAD {
                        let addr = base.wrapping_add(i);
                        wave.insert([addr; 32], vec![addr]);
                    }
                    append_wave(&fp, wave, "1", 1)
                })
            })
            .collect();
        for h in handles {
            h.join().expect("appender thread panicked")?;
        }

        let (_, loaded) = load_for_file(&file_path)?.expect("receipt should load");
        for k in [200u8, 201, 202, 203] {
            assert!(
                loaded.proofs.contains_key(&[k; 32]),
                "seed proof {k} disappeared (silent loss)"
            );
        }
        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                let addr = t.wrapping_mul(PER_THREAD).wrapping_add(i);
                assert!(
                    loaded.proofs.contains_key(&[addr; 32]),
                    "appended proof {addr} disappeared (silent loss)"
                );
            }
        }

        delete_for_file(&file_path)?;
        Ok(())
    }

    /// `delete_for_file` must also unlink matching `.tmp` siblings
    /// for the deleted key — otherwise a crashed-write residue from
    /// this same key would be promoted back to canonical by
    /// `recover_orphaned_tmps` on the next upload of the same path,
    /// resurrecting a receipt the user explicitly deleted. The
    /// `.lock` sidecar must be preserved (we hold it).
    #[test]
    fn delete_for_file_unlinks_tmp_residue_and_keeps_lock() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-delete-skip-{nanos}");
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([0x42; 32], vec![0x42]);
        let receipt_path = append_wave(&file_path, proofs, "1", 1)?;
        let dir = receipt_path.parent().expect("receipt has parent dir");
        let key = file_hash_key(&file_path);

        let stray_tmp = dir.join(format!("9999_{key}.tmp"));
        fs::write(&stray_tmp, b"in-flight")?;
        let lock_sidecar = dir.join(format!("{key}.lock"));
        assert!(lock_sidecar.exists(), "append_wave should leave a .lock");

        delete_for_file(&file_path)?;

        assert!(!receipt_path.exists(), "canonical receipt deleted");
        assert!(
            !stray_tmp.exists(),
            "delete_for_file must unlink .tmp residue (prevents zombie resurrection)"
        );
        assert!(
            lock_sidecar.exists(),
            "delete_for_file must not delete the .lock sidecar"
        );

        let _ = fs::remove_file(&lock_sidecar);
        Ok(())
    }

    /// A `.tmp` sibling holding a fully-valid serialized receipt is the
    /// crash-mid-rename case: payment proofs are in the .tmp file but
    /// the canonical name does not yet exist. `recover_orphaned_tmps`
    /// (called from `load_for_file` and `append_wave`) must rename it
    /// into place. Without recovery the wave's payment is silently lost.
    #[test]
    fn orphaned_tmp_with_valid_receipt_is_recovered() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-orphan-recover-{nanos}");
        let key = file_hash_key(&file_path);
        let dir = payments_dir()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let canonical = receipt_path(&dir, now, &key);
        let tmp = tmp_path_for(&canonical);

        let mut r = SingleNodeReceipt::new(now);
        r.proofs.insert([0xEE; 32], vec![0xEE, 0xEF]);
        r.storage_cost_atto = "13".into();
        r.gas_cost_wei = 7;
        let handle = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        rmp_serde::encode::write(&mut BufWriter::new(handle), &r)
            .map_err(|e| crate::error::Error::Io(std::io::Error::other(e.to_string())))?;
        assert!(tmp.exists());
        assert!(!canonical.exists());

        let (loaded_path, loaded) = load_for_file(&file_path)?.expect("orphan recovered");
        assert!(
            loaded.proofs.contains_key(&[0xEE; 32]),
            "recovered proof bytes lost"
        );
        assert!(
            !loaded_path.to_string_lossy().ends_with(".tmp"),
            "loaded path should be canonical, not .tmp"
        );
        assert!(loaded_path.exists());
        assert!(!tmp.exists(), "orphan .tmp should have been renamed away");

        delete_for_file(&file_path)?;
        let _ = fs::remove_file(dir.join(format!("{key}.lock")));
        Ok(())
    }

    /// A `.tmp` sibling holding garbage is the crash-mid-write case:
    /// the write was interrupted before `sync_all`. `recover_orphaned_tmps`
    /// must unlink it rather than rename the corrupt bytes onto the
    /// canonical path.
    #[test]
    fn orphaned_tmp_with_garbage_is_unlinked() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-orphan-unlink-{nanos}");
        let key = file_hash_key(&file_path);
        let dir = payments_dir()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let canonical = receipt_path(&dir, now, &key);
        let tmp = tmp_path_for(&canonical);
        fs::write(&tmp, b"not valid msgpack")?;
        assert!(tmp.exists());

        let result = load_for_file(&file_path)?;
        assert!(result.is_none(), "no usable receipt should be present");
        assert!(
            !tmp.exists(),
            "garbage orphan .tmp should have been unlinked"
        );
        assert!(
            !canonical.exists(),
            "garbage must not be renamed to canonical"
        );

        let _ = fs::remove_file(dir.join(format!("{key}.lock")));
        Ok(())
    }

    /// Atomic-write proof: a torn `<canonical>.tmp` lying around must
    /// never replace the live canonical receipt. The original
    /// `write_receipt_atomic_leaves_no_tmp_file` test is theatre
    /// (asserts cleanup not atomicity); this one fails if the write
    /// path ever regresses to truncate-in-place.
    #[test]
    fn write_receipt_atomic_preserves_existing_on_torn_tmp() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-atomic-preserve-{nanos}");
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([0xAA; 32], vec![0xAA]);
        let canonical = append_wave(&file_path, proofs, "10", 20)?;
        let canonical_bytes_before = fs::read(&canonical)?;

        // Plant a torn .tmp (zero-byte): simulates kill between open
        // and write. recover_orphaned_tmps must unlink it and NOT
        // rename it over canonical.
        let tmp = tmp_path_for(&canonical);
        fs::write(&tmp, b"")?;

        // Force recovery by reloading.
        let (_, loaded) = load_for_file(&file_path)?.expect("canonical preserved");
        assert_eq!(loaded.proofs.len(), 1);
        assert!(loaded.proofs.contains_key(&[0xAA; 32]));
        assert!(!tmp.exists(), "torn .tmp unlinked");
        assert_eq!(
            fs::read(&canonical)?,
            canonical_bytes_before,
            "canonical bytes unchanged by torn .tmp recovery"
        );

        delete_for_file(&file_path)?;
        Ok(())
    }

    /// Mixed drop + append concurrency: 8 threads alternating drops
    /// of one address and appends of another. The CAS-on-bytes drop
    /// + exclusive lock must keep every appended proof reachable.
    #[test]
    fn concurrent_drop_and_append_keep_consistent_state() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-drop-append-concurrent-{nanos}");

        // Seed with one proof for address [99; 32] so the dropper
        // has something to remove.
        let mut seed: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        seed.insert([99u8; 32], vec![99]);
        append_wave(&file_path, seed, "1", 1)?;

        let mut handles = Vec::new();
        for i in 0u8..8 {
            let fp = file_path.clone();
            handles.push(std::thread::spawn(move || -> Result<()> {
                if i % 2 == 0 {
                    // Even thread: append a fresh proof at address [i; 32].
                    let mut wave: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
                    wave.insert([i; 32], vec![i, i, i]);
                    append_wave(&fp, wave, "1", 1)?;
                } else {
                    // Odd thread: try to drop [99; 32]. CAS expected
                    // bytes match seed, so the first to win removes
                    // it; later attempts no-op.
                    drop_proofs_for_file(&fp, &[([99u8; 32], vec![99])])?;
                }
                Ok(())
            }));
        }
        for h in handles {
            h.join().expect("thread panicked")?;
        }

        // Every appended even-index proof must be present.
        if let Some((_, loaded)) = load_for_file(&file_path)? {
            for i in (0u8..8).step_by(2) {
                assert!(
                    loaded.proofs.contains_key(&[i; 32]),
                    "appended proof {i} must survive concurrent drop+append"
                );
                assert_eq!(loaded.proofs.get(&[i; 32]), Some(&vec![i, i, i]));
            }
        } else {
            // Edge case: all drops ran before any append AND the seed
            // [99; 32] dropper emptied the receipt before the
            // appenders re-created it. With our atomic ordering this
            // shouldn't happen — assert it doesn't.
            panic!("receipt should still exist with all appended proofs");
        }

        delete_for_file(&file_path)?;
        Ok(())
    }

    /// Cost overflow safety: two waves each contributing nearly
    /// u128::MAX/1 atto must sum without silently dropping the
    /// overflow contribution. Pre-fix this saturated; with U256 sums
    /// the result is exact.
    #[test]
    fn wave_cost_above_u128_max_does_not_silently_drop_cumulative() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-cost-overflow-{nanos}");

        // 2^127 — fits in u128. Sum of two = 2^128, which overflows
        // u128 but is exact in U256.
        let near_half_max = "170141183460469231731687303715884105728"; // 2^127
        let mut w1: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        w1.insert([1u8; 32], vec![1]);
        append_wave(&file_path, w1, near_half_max, 0)?;

        let mut w2: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        w2.insert([2u8; 32], vec![2]);
        append_wave(&file_path, w2, near_half_max, 0)?;

        let (_, loaded) = load_for_file(&file_path)?.expect("receipt should load");
        // Expected: 2 * 2^127 = 2^128.
        let expected = "340282366920938463463374607431768211456";
        assert_eq!(
            loaded.storage_cost_atto, expected,
            "cumulative cost must NOT silently saturate at u128::MAX"
        );

        delete_for_file(&file_path)?;
        Ok(())
    }

    /// cleanup_outdated must skip .tmp siblings even when their
    /// filename timestamp prefix would parse as ancient. Otherwise
    /// an in-flight write's .tmp would get reaped mid-flight.
    #[test]
    fn cleanup_outdated_skips_tmp_even_with_ancient_prefix() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-cleanup-tmp-skip-{nanos}");
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([0xAA; 32], vec![0xAA]);
        append_wave(&file_path, proofs, "1", 1)?;

        let dir = payments_dir()?;
        let key = file_hash_key(&file_path);
        // Year 1970 + 1 second.
        let ancient_tmp = dir.join(format!("1_{key}.tmp"));
        fs::write(&ancient_tmp, b"in-flight")?;

        cleanup_outdated();

        assert!(
            ancient_tmp.exists(),
            "cleanup_outdated must not reap .tmp by ancient timestamp prefix"
        );

        let _ = fs::remove_file(&ancient_tmp);
        delete_for_file(&file_path)?;
        Ok(())
    }

    /// Duplicate canonical receipts must be merged before the older
    /// is unlinked — never blindly newest-wins. The older may hold
    /// proofs the newer never saw (residue from a buggier binary,
    /// manual file recovery, interrupted operation). Blind unlink
    /// would strand any on-chain payment whose proof lives only in
    /// the older sibling.
    #[test]
    fn duplicate_canonical_receipts_are_merged_then_older_unlinked() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-dedupe-canonical-{nanos}");
        let dir = payments_dir()?;
        let key = file_hash_key(&file_path);

        // Use recent timestamps so a concurrent test's
        // `cleanup_outdated` (which walks the shared payments dir
        // unfiltered by key) doesn't reap our hand-written receipts
        // before the dedup runs.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let old_ts = now.saturating_sub(120);
        let new_ts = now.saturating_sub(60);
        let old_path = dir.join(format!("{old_ts}_{key}"));
        let new_path = dir.join(format!("{new_ts}_{key}"));
        let mut old = SingleNodeReceipt::new(old_ts);
        old.proofs.insert([1u8; 32], vec![0xAA]);
        old.storage_cost_atto = "10".to_string();
        old.gas_cost_wei = 20;
        let mut new = SingleNodeReceipt::new(new_ts);
        new.proofs.insert([2u8; 32], vec![0xBB]);
        new.storage_cost_atto = "30".to_string();
        new.gas_cost_wei = 40;
        write_receipt_atomic(&old_path, &old)?;
        write_receipt_atomic(&new_path, &new)?;
        assert!(old_path.exists() && new_path.exists());

        let _guard = ReceiptLock::acquire(&dir, &key)?;
        dedupe_canonical_receipts(&dir, &key);
        drop(_guard);

        assert!(
            !old_path.exists(),
            "older canonical receipt must be unlinked after merge"
        );
        assert!(new_path.exists(), "newer canonical receipt must survive");

        // The winner now holds BOTH proofs and the SUMMED costs —
        // the older's proof was NOT stranded.
        let merged = read_receipt(&new_path)?;
        assert!(
            merged.proofs.contains_key(&[1u8; 32]),
            "older sibling's proof must be merged into the winner"
        );
        assert!(merged.proofs.contains_key(&[2u8; 32]));
        assert_eq!(merged.proofs.len(), 2);
        assert_eq!(merged.storage_cost_atto, "40", "costs must be summed");
        assert_eq!(merged.gas_cost_wei, 60);
        assert_eq!(
            merged.first_pay_timestamp, old_ts,
            "first_pay_timestamp must be the MIN across merged siblings"
        );

        delete_for_file(&file_path)?;
        Ok(())
    }

    /// An unreadable canonical receipt (corrupt msgpack) must be
    /// unlinked, not left to occupy the directory for up to 24 h.
    /// Pre-fix the file just got logged as "unreadable" and skipped.
    #[test]
    fn unreadable_canonical_receipt_is_unlinked_by_find_existing() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-unreadable-canonical-{nanos}");
        let dir = payments_dir()?;
        let key = file_hash_key(&file_path);

        // Recent timestamp so is_expired_filename returns false.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let canonical = dir.join(format!("{now}_{key}"));
        fs::write(&canonical, b"this is not msgpack")?;
        assert!(canonical.exists());

        // load_for_file -> find_existing must unlink the corrupt file.
        let result = load_for_file(&file_path)?;
        assert!(result.is_none(), "no usable receipt");
        assert!(
            !canonical.exists(),
            "corrupt canonical receipt should be unlinked, not left for 24 h"
        );

        Ok(())
    }

    /// A receipt written by a future schema version (eg. user
    /// downgraded the binary between attempts) must be treated as
    /// unreadable so the corruption-unlink path kicks in.
    #[test]
    fn future_schema_version_is_treated_as_unreadable() -> Result<()> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_path = format!("/tmp/anselme-future-schema-{nanos}");
        let dir = payments_dir()?;
        let key = file_hash_key(&file_path);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let canonical = dir.join(format!("{now}_{key}"));
        let receipt = SingleNodeReceipt {
            version: SCHEMA_VERSION.saturating_add(1),
            proofs: {
                let mut m: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
                m.insert([1u8; 32], vec![1]);
                m
            },
            first_pay_timestamp: now,
            storage_cost_atto: "10".to_string(),
            gas_cost_wei: 20,
        };
        write_receipt_atomic(&canonical, &receipt)?;
        assert!(canonical.exists());

        let result = load_for_file(&file_path)?;
        assert!(result.is_none(), "future schema must be rejected");
        assert!(!canonical.exists(), "rejected receipt must be unlinked");

        Ok(())
    }
}
