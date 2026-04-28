//! Persisted DataMap file format.
//!
//! A `.datamap` file is the on-disk form of a `self_encryption::DataMap` that
//! private uploads need in order to be downloaded later. Every callsite that
//! writes one (today: `ant-cli` and `ant-gui`) routes through this module so
//! the wire format and naming convention stay consistent.
//!
//! # Wire format
//!
//! Canonical: msgpack (`rmp_serde`). The file contains the bare serialized
//! `DataMap` — no header, no envelope. This matches the format `ant-cli` has
//! always written; `ant-gui` adopts it here.
//!
//! For backwards compatibility, [`read_datamap`] also accepts the legacy JSON
//! format that older `ant-gui` versions wrote. Format detection is by sniffing
//! the first byte of the file:
//!   - `0x7B` (`{`) → JSON (legacy ant-gui)
//!   - else        → msgpack (canonical)
//!
//! A future envelope format wrapping the DataMap with metadata (e.g. original
//! filename, version) would be signalled by a magic byte that is neither `{`
//! nor a valid msgpack initial byte. The reserved byte for that purpose is
//! `0xC1`, which is unused in the msgpack spec.
//!
//! # Naming convention
//!
//! The original filename is preserved verbatim with `.datamap` appended:
//! `holiday.jpg` → `holiday.jpg.datamap`, `Makefile` → `Makefile.datamap`,
//! `archive.tar.gz` → `archive.tar.gz.datamap`. Files without an extension
//! are handled naturally because we append rather than replace.
//!
//! On collision under [`CollisionPolicy::NumericSuffix`], a `-N` (starting at
//! 2) is inserted before the `.datamap` extension: `holiday.jpg-2.datamap`,
//! `holiday.jpg-3.datamap`, …

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use self_encryption::DataMap;
use tempfile::NamedTempFile;

use crate::data::error::{Error, Result};

/// Extension appended to every persisted datamap file.
pub const DATAMAP_EXTENSION: &str = "datamap";

/// Cap on collision-suffix attempts before [`write_datamap`] gives up.
///
/// In normal use a directory will see at most a handful of repeated
/// uploads; this bound just protects against pathological state (e.g.
/// thousands of stale entries) so we fail fast instead of looping.
/// Set generously (1000) so legitimate users with many backups of the
/// same file don't hit a confusing error before pathological state does.
const MAX_COLLISION_ATTEMPTS: u32 = 1000;

/// Behaviour when a target datamap filename already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionPolicy {
    /// Replace the existing file. Used by `ant-cli` when invoked with
    /// `--overwrite` to preserve its pre-`feat/datamap-fs-helper` behaviour.
    Overwrite,
    /// Insert `-N` (starting at 2) between the filename and `.datamap`.
    /// `holiday.jpg.datamap` → `holiday.jpg-2.datamap` → `-3` → … capped at
    /// [`MAX_COLLISION_ATTEMPTS`].
    NumericSuffix,
}

/// Construct the canonical datamap filename for an arbitrary input filename.
///
/// Appends `.datamap` without replacing any existing extension, then runs
/// the result through [`sanitize_filename`] so platform-illegal characters
/// don't reach the filesystem. Falls back to `datamap.datamap` when the
/// input sanitizes to an empty string.
///
/// Pure function: takes only the basename, never a path with separators.
pub fn datamap_filename_for(original_name: &str) -> String {
    let sanitized = sanitize_filename(original_name);
    if sanitized.is_empty() {
        format!("datamap.{DATAMAP_EXTENSION}")
    } else {
        format!("{sanitized}.{DATAMAP_EXTENSION}")
    }
}

/// Inverse of [`datamap_filename_for`] for download UX. Strips a single
/// trailing `.datamap` from the basename of `path`.
///
/// Returns `None` when `path` has no UTF-8 basename, doesn't end in
/// `.datamap`, or would produce an empty result. Does *not* attempt to undo
/// `-N` collision suffixes — `holiday.jpg-2.datamap` returns `holiday.jpg-2`
/// rather than `holiday.jpg`. The collision suffix is a write-side artifact;
/// callers can offer the literal stem as a default and let users edit.
pub fn original_name_from_datamap(path: &Path) -> Option<OsString> {
    let basename = path.file_name()?.to_str()?;
    let stripped = basename.strip_suffix(&format!(".{DATAMAP_EXTENSION}"))?;
    if stripped.is_empty() {
        None
    } else {
        Some(OsString::from(stripped))
    }
}

/// Write `dm` to `dir` using the canonical naming and the given collision
/// policy. Returns the absolute path to the written file.
///
/// Atomicity: writes to a tempfile in `dir`, fsyncs the file, renames into
/// place, then (on Unix) best-effort-fsyncs the parent directory. A crash
/// at any point cannot leave a half-serialized datamap at the target
/// path, and on Unix the rename itself is durable across power loss
/// because the directory entry is flushed. The tempfile is created on
/// the same filesystem as `dir` to keep the rename atomic. Windows
/// relies on NTFS metadata journaling for rename durability — we do not
/// fsync the directory there because Windows does not expose that
/// operation through `std::fs`.
pub fn write_datamap(
    dir: &Path,
    original_name: &str,
    dm: &DataMap,
    policy: CollisionPolicy,
) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let bytes = rmp_serde::to_vec(dm)
        .map_err(|e| Error::Serialization(format!("DataMap msgpack encode failed: {e}")))?;
    let base_filename = datamap_filename_for(original_name);
    let target = reserve_target_path(dir, &base_filename, policy)?;
    write_atomic(dir, &target, &bytes)?;
    Ok(target)
}

/// Read a persisted datamap, auto-detecting msgpack vs legacy JSON.
pub fn read_datamap(path: &Path) -> Result<DataMap> {
    let bytes = fs::read(path)?;
    if bytes.first() == Some(&b'{') {
        // Legacy JSON written by ant-gui versions prior to the shared helper.
        serde_json::from_slice::<DataMap>(&bytes)
            .map_err(|e| Error::Serialization(format!("DataMap JSON decode failed: {e}")))
    } else {
        rmp_serde::from_slice::<DataMap>(&bytes)
            .map_err(|e| Error::Serialization(format!("DataMap msgpack decode failed: {e}")))
    }
}

/// Reduce an arbitrary filename to one safe to place on disk: keep
/// alphanumerics and a small set of common punctuation, replace every other
/// character with `_`, then trim surrounding whitespace. Leading/trailing
/// dots are intentionally preserved so dotfiles like `.bashrc` survive.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn reserve_target_path(
    dir: &Path,
    base_filename: &str,
    policy: CollisionPolicy,
) -> Result<PathBuf> {
    match policy {
        CollisionPolicy::Overwrite => Ok(dir.join(base_filename)),
        CollisionPolicy::NumericSuffix => {
            // base_filename is e.g. `photo.jpg.datamap`. The collision suffix
            // sits between the trailing `.datamap` and the rest of the name:
            // `photo.jpg-2.datamap`, never `photo-2.jpg.datamap`.
            let stem = base_filename
                .strip_suffix(&format!(".{DATAMAP_EXTENSION}"))
                .unwrap_or(base_filename);
            for attempt in 0..MAX_COLLISION_ATTEMPTS {
                let candidate = if attempt == 0 {
                    base_filename.to_string()
                } else {
                    format!("{stem}-{}.{DATAMAP_EXTENSION}", attempt + 1)
                };
                let path = dir.join(&candidate);
                if !path.exists() {
                    return Ok(path);
                }
            }
            Err(Error::Storage(format!(
                "Unable to reserve a free datamap filename after {MAX_COLLISION_ATTEMPTS} attempts in {}",
                dir.display()
            )))
        }
    }
}

fn write_atomic(dir: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(target).map_err(|e| Error::Io(e.error))?;
    // On Unix, fsync the parent directory so the rename itself survives
    // a crash. On ext4 (default mount opts) and btrfs the rename can
    // otherwise be lost if the directory entry hasn't reached disk. This
    // is best-effort — a failure here means we wrote a valid datamap but
    // can't prove the rename is durable, which is no worse than where we
    // were before the call. Windows has no portable directory-fsync, so
    // we skip it there and lean on NTFS metadata journaling.
    #[cfg(unix)]
    {
        if let Ok(dir_handle) = fs::File::open(dir) {
            let _ = dir_handle.sync_all();
        }
    }
    Ok(())
}
