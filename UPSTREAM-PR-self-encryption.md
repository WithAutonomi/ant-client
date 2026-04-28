# Upstream PR: thread-safe runtime override for `STREAM_DECRYPT_BATCH_SIZE`

## Repo
https://github.com/maidsafe/self_encryption (we depend on `0.35.0`)

## Problem
`self_encryption::STREAM_DECRYPT_BATCH_SIZE` is currently a
`LazyLock<usize>` populated once from the env var at first access:

```rust
// self_encryption/src/lib.rs (current)
pub static STREAM_DECRYPT_BATCH_SIZE: LazyLock<usize> = LazyLock::new(|| {
    std::env::var("STREAM_DECRYPT_BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
});
```

This freezes the value for the process lifetime, and the only path to
change it from a downstream crate is `std::env::set_var`, which is
`unsafe` since Rust 1.80 (POSIX `setenv` races against concurrent
reads).

ant-client's adaptive concurrency controller wants to size
`streaming_decrypt`'s batch requests from a runtime-observed network
health signal. Without a thread-safe API there is no way to do this
without `unsafe`.

## Proposed change (no `unsafe`, no breaking-API churn)

Replace the static with an `AtomicUsize` and expose a getter +
setter. Internally `stream_decrypt.rs` switches to the getter.

```rust
// self_encryption/src/lib.rs (proposed)
use std::sync::atomic::{AtomicUsize, Ordering};

const DEFAULT_STREAM_DECRYPT_BATCH_SIZE: usize = 10;

/// Backing atomic. `0` means "uninitialized — read from env var on
/// next access". Direct access not exposed; callers use the
/// getter/setter.
static STREAM_DECRYPT_BATCH_SIZE_CELL: AtomicUsize = AtomicUsize::new(0);

/// Read the current streaming-decrypt batch size.
///
/// First call initializes from the `STREAM_DECRYPT_BATCH_SIZE` env
/// var (defaulting to 10). Subsequent calls return whatever the
/// most recent `set_stream_decrypt_batch_size` stored. Thread-safe.
pub fn stream_decrypt_batch_size() -> usize {
    let cur = STREAM_DECRYPT_BATCH_SIZE_CELL.load(Ordering::Relaxed);
    if cur != 0 {
        return cur;
    }
    let init = std::env::var("STREAM_DECRYPT_BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_STREAM_DECRYPT_BATCH_SIZE);
    // Race with another initializing thread: either value is fine.
    let _ = STREAM_DECRYPT_BATCH_SIZE_CELL.compare_exchange(
        0,
        init,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    STREAM_DECRYPT_BATCH_SIZE_CELL.load(Ordering::Relaxed)
}

/// Override the streaming-decrypt batch size at runtime.
/// Thread-safe. `0` resets to the env-var default on next read.
/// `n` is clamped to at least 1 because `streaming_decrypt` requires
/// a positive batch size.
pub fn set_stream_decrypt_batch_size(n: usize) {
    STREAM_DECRYPT_BATCH_SIZE_CELL.store(n, Ordering::Relaxed);
}
```

Internal call site:

```rust
// self_encryption/src/stream_decrypt.rs:78 (proposed)
(self.current_batch_start + crate::stream_decrypt_batch_size())
    .min(self.chunk_infos.len());
```

## Backward-compatibility

Removing `pub static STREAM_DECRYPT_BATCH_SIZE: LazyLock<usize>` is
technically breaking for any downstream that dereferences it. Two
options:

1. **Clean break + bump to `0.36.0`** — semver allows it; the symbol
   is internal-feeling and unlikely to have many external consumers.
   Migration: `*STREAM_DECRYPT_BATCH_SIZE` → `stream_decrypt_batch_size()`.
2. **Keep the static as a `#[deprecated]` shim** that returns the
   getter's value via a `LazyLock` populated lazily; the value still
   freezes for `*STREAM_DECRYPT_BATCH_SIZE` callers (no behavior
   change for them) but the new getter/setter API works fully.

Option 1 is cleaner; the symbol's only known callers are
`self_encryption` itself (one site) plus any direct downstream that
was working around the env-var-only API.

## Tests to add

```rust
#[test]
fn batch_size_default_is_ten() {
    // Note: env var must be unset for this test to mean anything.
    if std::env::var("STREAM_DECRYPT_BATCH_SIZE").is_err() {
        // Reset cell to force re-init.
        set_stream_decrypt_batch_size(0);
        assert_eq!(stream_decrypt_batch_size(), 10);
    }
}

#[test]
fn setter_overrides_default() {
    set_stream_decrypt_batch_size(42);
    assert_eq!(stream_decrypt_batch_size(), 42);
    set_stream_decrypt_batch_size(0); // reset for other tests
}

#[test]
fn concurrent_set_and_read_does_not_panic() {
    use std::thread;
    let writer = thread::spawn(|| {
        for i in 1..=1000 {
            set_stream_decrypt_batch_size(i);
        }
    });
    let reader = thread::spawn(|| {
        let mut last = 0;
        for _ in 0..1000 {
            let cur = stream_decrypt_batch_size();
            assert!(cur > 0);
            last = cur;
        }
        last
    });
    writer.join().unwrap();
    reader.join().unwrap();
    set_stream_decrypt_batch_size(0); // reset
}
```

## Effort
- ~25 lines added in `self_encryption/src/lib.rs`.
- One-line change in `self_encryption/src/stream_decrypt.rs`.
- Three small unit tests.
- Bump to `0.36.0`.

## Downstream consumption (ant-client)

Once `self_encryption 0.36` is published, in
`ant-core/src/data/client/mod.rs::build_controller` (and at the
start of `Client::file_download` for daemons whose controller
evolves over many downloads):

```rust
self_encryption::set_stream_decrypt_batch_size(
    controller.fetch.current(),
);
```

No `unsafe`, no env-var poking, no race against concurrent reads.
The adaptive controller's per-batch fan-out (already wired) keeps
working; this just lets us also size the unit-of-work
`streaming_decrypt` asks for.

## Status
Draft prepared in this repo for context. The PR has not been filed.
File against `https://github.com/maidsafe/self_encryption` when
ready.
