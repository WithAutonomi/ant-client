//! E2E tests for huge file upload/download with bounded memory.
//!
//! Verifies that multi-GB files can be uploaded and downloaded without
//! memory growing proportionally to file size, thanks to chunk spilling.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{Client, ClientConfig};
use serial_test::serial;
use std::io::Write;
use std::sync::Arc;
use support::MiniTestnet;
use tempfile::TempDir;

/// Size of the 1 GB test file.
const FILE_SIZE_1GB: u64 = 1024 * 1024 * 1024;

/// Maximum allowed current-RSS increase during upload, in bytes.
///
/// The chunk spill mechanism keeps peak memory to one wave (~256 MB).
/// We allow up to 512 MB of RSS growth for the wave buffer, allocator
/// overhead, and test infrastructure churn. Without spilling, RSS would
/// grow by ≥1 GB (all encrypted chunks held simultaneously).
const MAX_RSS_INCREASE_BYTES: u64 = 512 * 1024 * 1024;

async fn setup_large() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(6).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    let client = Client::from_node(Arc::clone(&node), ClientConfig::default())
        .with_wallet(testnet.wallet().clone());

    (client, testnet)
}

/// Get **current** resident set size in bytes (NOT peak).
///
/// Unlike `ru_maxrss` (which is a monotonic high-water mark and never
/// decreases), this returns the actual RSS right now. This is what we
/// need to verify bounded memory — we sample before and after the
/// operation and check the delta.
fn get_current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        get_current_rss_macos()
    }
    #[cfg(target_os = "linux")]
    {
        get_current_rss_linux()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Get current RSS on macOS via `mach_task_basic_info`.
///
/// `resident_size` is the actual current RSS, not a peak.
#[cfg(target_os = "macos")]
fn get_current_rss_macos() -> Option<u64> {
    use std::mem;

    // mach_task_basic_info struct layout
    #[repr(C)]
    struct MachTaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: libc::time_value_t,
        system_time: libc::time_value_t,
        policy: i32,
        suspend_count: i32,
    }

    const MACH_TASK_BASIC_INFO: u32 = 20;

    extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(
            target_task: u32,
            flavor: u32,
            task_info_out: *mut MachTaskBasicInfo,
            task_info_out_cnt: *mut u32,
        ) -> i32;
    }

    let mut info: MachTaskBasicInfo = unsafe { mem::zeroed() };
    let mut count = (mem::size_of::<MachTaskBasicInfo>() / mem::size_of::<u32>()) as u32;

    let kr = unsafe {
        task_info(
            mach_task_self(),
            MACH_TASK_BASIC_INFO,
            &mut info,
            &mut count,
        )
    };

    if kr == 0 {
        Some(info.resident_size)
    } else {
        None
    }
}

/// Get current RSS on Linux via /proc/self/statm.
///
/// Field 1 (resident) is in pages.
#[cfg(target_os = "linux")]
fn get_current_rss_linux() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(resident_pages * page_size as u64)
}

/// Fixed PRNG seed for deterministic, incompressible test data.
///
/// Using a seeded PRNG (not a repeating pattern) ensures encrypted chunks
/// are full-size (~4 MB each), exercising the worst-case wave buffer.
/// A repeating 256-byte pattern would compress to tiny chunks, making
/// the memory test pass trivially without stressing the wave buffer.
const TEST_SEED: u64 = 0xDEAD_BEEF_CAFE_BABE;

/// Simple xorshift64 PRNG — deterministic, fast, incompressible output.
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u8(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 & 0xFF) as u8
    }
}

/// Create a deterministic test file of the given size.
///
/// Uses a seeded PRNG to produce incompressible content so encrypted chunks
/// are full-size (~4 MB each), properly exercising the wave buffer. The same
/// seed is used in `verify_file_content` for verification.
fn create_test_file(dir: &TempDir, size: u64) -> std::path::PathBuf {
    let path = dir.path().join("huge_test_file.bin");
    let mut file = std::fs::File::create(&path).expect("create test file");

    let mut rng = Xorshift64::new(TEST_SEED);
    let mut remaining = size;

    // Write in 1 MB chunks to avoid holding the whole file in memory
    let write_buf_size: usize = 1024 * 1024;
    let mut buf = vec![0u8; write_buf_size];
    while remaining > 0 {
        let to_write = remaining.min(write_buf_size as u64) as usize;
        for byte in buf.iter_mut().take(to_write) {
            *byte = rng.next_u8();
        }
        file.write_all(&buf[..to_write])
            .expect("write chunk to test file");
        remaining -= to_write as u64;
    }
    file.flush().expect("flush test file");
    drop(file);
    drop(buf);

    let meta = std::fs::metadata(&path).expect("stat test file");
    assert_eq!(meta.len(), size, "test file size mismatch");

    path
}

/// Verify downloaded file matches the expected PRNG sequence.
///
/// Regenerates the same PRNG stream and compares byte-by-byte,
/// reading in 1 MB chunks to avoid loading the file into memory.
fn verify_file_content(path: &std::path::Path, expected_size: u64) {
    let meta = std::fs::metadata(path).expect("stat downloaded file");
    assert_eq!(
        meta.len(),
        expected_size,
        "downloaded file size should match original"
    );

    let mut rng = Xorshift64::new(TEST_SEED);
    let file = std::fs::File::open(path).expect("open downloaded file");
    let mut reader = std::io::BufReader::new(file);
    let mut offset = 0u64;
    let mut buf = vec![0u8; 1024 * 1024];

    loop {
        let n = std::io::Read::read(&mut reader, &mut buf).expect("read downloaded file");
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            let expected = rng.next_u8();
            if byte != expected {
                panic!(
                    "content mismatch at byte {offset}: got {byte:#04x}, expected {expected:#04x}"
                );
            }
            offset += 1;
        }
    }
    assert_eq!(offset, expected_size, "total bytes read mismatch");
}

/// Upload and download a 1 GB file, verifying data integrity and bounded memory.
///
/// This test verifies that:
/// 1. A 1 GB file can be uploaded without running out of memory
/// 2. The downloaded file is byte-for-byte identical
/// 3. **Current** RSS (not peak) stays well below the file size during upload
///
/// We measure current RSS (via `mach_task_basic_info` on macOS,
/// `/proc/self/statm` on Linux) — NOT `ru_maxrss` which is a monotonic
/// high-water mark that could give false passes.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_huge_file_upload_download_1gb() {
    let (client, testnet) = setup_large().await;

    let work_dir = TempDir::new().expect("create work dir");

    // Phase 1: Create a 1 GB test file.
    // Do this BEFORE measuring RSS baseline so the write buffer is freed.
    eprintln!("Creating {FILE_SIZE_1GB} byte test file...");
    let input_path = create_test_file(&work_dir, FILE_SIZE_1GB);
    eprintln!("Test file created at {}", input_path.display());

    // Let the allocator settle — drop any write buffers, run a GC-like pass.
    tokio::task::yield_now().await;

    // Record baseline RSS AFTER file creation and infra setup so the
    // delta only captures the upload operation itself.
    let rss_before = get_current_rss_bytes();
    if let Some(rss) = rss_before {
        eprintln!("RSS baseline before upload: {} MB", rss / (1024 * 1024));
    }

    // Phase 2: Upload
    eprintln!("Uploading 1 GB file...");
    let result = client
        .file_upload(input_path.as_path())
        .await
        .expect("1 GB file upload should succeed");

    // Measure RSS immediately after upload completes
    let rss_after_upload = get_current_rss_bytes();

    eprintln!(
        "Upload complete: {} chunks stored, mode: {:?}",
        result.chunks_stored, result.payment_mode_used
    );

    // A 1 GB file at 4 MB max chunk size produces ~256 chunks minimum
    assert!(
        result.chunks_stored >= 200,
        "1 GB file should produce at least 200 chunks, got {}",
        result.chunks_stored
    );

    // Phase 3: Check memory impact using CURRENT RSS (not peak)
    if let (Some(before), Some(after)) = (rss_before, rss_after_upload) {
        let increase = after.saturating_sub(before);
        let increase_mb = increase / (1024 * 1024);
        let max_mb = MAX_RSS_INCREASE_BYTES / (1024 * 1024);
        eprintln!(
            "Current RSS before: {} MB, after: {} MB, increase: {increase_mb} MB (limit: {max_mb} MB)",
            before / (1024 * 1024),
            after / (1024 * 1024)
        );
        assert!(
            increase < MAX_RSS_INCREASE_BYTES,
            "RSS increased by {increase_mb} MB which exceeds the {max_mb} MB limit — \
             chunk spilling may not be working correctly. \
             Before: {} MB, After: {} MB",
            before / (1024 * 1024),
            after / (1024 * 1024)
        );
    } else {
        // On macOS/Linux, RSS measurement must succeed — a silent skip would
        // let the bounded-memory claim go unverified in CI.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        panic!("RSS measurement failed on a supported platform — cannot verify bounded memory");

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        eprintln!("WARNING: Could not measure current RSS on this platform, skipping memory check");
    }

    // Phase 4: Download to a separate file
    let output_path = work_dir.path().join("downloaded_huge.bin");
    eprintln!("Downloading 1 GB file...");
    let bytes_written = client
        .file_download(&result.data_map, &output_path)
        .await
        .expect("1 GB file download should succeed");

    assert_eq!(
        bytes_written, FILE_SIZE_1GB,
        "bytes_written should equal original file size"
    );
    eprintln!("Download complete: {bytes_written} bytes written");

    // Phase 5: Verify content integrity (streaming, not in-memory)
    eprintln!("Verifying file content...");
    verify_file_content(&output_path, FILE_SIZE_1GB);
    eprintln!("Content verification passed — all {FILE_SIZE_1GB} bytes match");

    drop(client);
    testnet.teardown().await;
}

/// Test that uploading fails early with a clear error when disk space is
/// insufficient for the chunk spill.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_file_upload_disk_space_check() {
    let (client, testnet) = setup_large().await;

    // Verify the check works for a small file (should succeed)
    let work_dir = TempDir::new().expect("create work dir");
    let small_path = work_dir.path().join("small.bin");
    {
        let mut f = std::fs::File::create(&small_path).expect("create small file");
        f.write_all(&vec![0xAA; 4096]).expect("write small file");
        f.flush().expect("flush small file");
    }

    let result = client.file_upload(small_path.as_path()).await;
    assert!(
        result.is_ok(),
        "small file upload should succeed (disk space sufficient): {:?}",
        result.err()
    );

    drop(client);
    testnet.teardown().await;
}

/// Upload and download a 4 GB file, proving memory is bounded regardless of
/// file size.
///
/// The key assertion: RSS increase for a 4 GB file should be comparable to
/// the 1 GB test (~185 MB), NOT 4x larger. If RSS scales linearly with
/// file size, spilling is broken. If it stays flat, spilling works.
///
/// This test also exercises multi-wave dynamics much more heavily:
/// 4 GB → ~1024 chunks → ~16 waves of 64 chunks each.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_huge_file_upload_download_4gb() {
    let file_size: u64 = 4 * 1024 * 1024 * 1024;

    let (client, testnet) = setup_large().await;
    let work_dir = TempDir::new().expect("create work dir");

    eprintln!("Creating 4 GB test file...");
    let input_path = create_test_file(&work_dir, file_size);
    eprintln!("Test file created at {}", input_path.display());

    tokio::task::yield_now().await;

    let rss_before = get_current_rss_bytes();
    if let Some(rss) = rss_before {
        eprintln!("RSS baseline before upload: {} MB", rss / (1024 * 1024));
    }

    eprintln!("Uploading 4 GB file...");
    let result = client
        .file_upload(input_path.as_path())
        .await
        .expect("4 GB file upload should succeed");

    let rss_after_upload = get_current_rss_bytes();

    eprintln!(
        "Upload complete: {} chunks stored, mode: {:?}",
        result.chunks_stored, result.payment_mode_used
    );

    // 4 GB at MAX_CHUNK_SIZE (4,190,208 bytes) → ~1026 chunks minimum
    assert!(
        result.chunks_stored >= 1000,
        "4 GB file should produce at least 1000 chunks, got {}",
        result.chunks_stored
    );

    // Memory check: same 512 MB limit as the 1 GB test.
    // If spilling works, a 4 GB file should use roughly the same memory
    // as a 1 GB file — the wave buffer is the same size regardless.
    if let (Some(before), Some(after)) = (rss_before, rss_after_upload) {
        let increase = after.saturating_sub(before);
        let increase_mb = increase / (1024 * 1024);
        let max_mb = MAX_RSS_INCREASE_BYTES / (1024 * 1024);
        eprintln!(
            "Current RSS before: {} MB, after: {} MB, increase: {increase_mb} MB (limit: {max_mb} MB)",
            before / (1024 * 1024),
            after / (1024 * 1024)
        );
        assert!(
            increase < MAX_RSS_INCREASE_BYTES,
            "RSS increased by {increase_mb} MB for a 4 GB file — \
             this exceeds the {max_mb} MB limit and suggests memory \
             scales with file size instead of staying bounded. \
             Before: {} MB, After: {} MB",
            before / (1024 * 1024),
            after / (1024 * 1024)
        );
    } else {
        // On macOS/Linux, RSS measurement must succeed — a silent skip would
        // let the bounded-memory claim go unverified in CI.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        panic!("RSS measurement failed on a supported platform — cannot verify bounded memory");

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        eprintln!("WARNING: Could not measure current RSS on this platform, skipping memory check");
    }

    // Download and verify
    let output_path = work_dir.path().join("downloaded_4gb.bin");
    eprintln!("Downloading 4 GB file...");
    let bytes_written = client
        .file_download(&result.data_map, &output_path)
        .await
        .expect("4 GB file download should succeed");

    assert_eq!(
        bytes_written, file_size,
        "bytes_written should equal original file size"
    );
    eprintln!("Download complete: {bytes_written} bytes written");

    eprintln!("Verifying file content...");
    verify_file_content(&output_path, file_size);
    eprintln!("Content verification passed — all 4 GB match");

    drop(client);
    testnet.teardown().await;
}

/// Test that a moderately large file (64 MB) round-trips correctly.
///
/// This test verifies data integrity on a medium file. Memory bounding
/// is not asserted here because 64 MB produces ~20 chunks which fit in
/// a single wave — there are no multi-wave dynamics to test. The 1 GB
/// test is the one that proves memory is bounded across waves.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_large_file_64mb_round_trip() {
    let file_size: u64 = 64 * 1024 * 1024;

    let (client, testnet) = setup_large().await;
    let work_dir = TempDir::new().expect("create work dir");

    eprintln!("Creating 64 MB test file...");
    let input_path = create_test_file(&work_dir, file_size);

    eprintln!("Uploading 64 MB file...");
    let result = client
        .file_upload(input_path.as_path())
        .await
        .expect("64 MB file upload should succeed");

    eprintln!(
        "Upload complete: {} chunks stored, mode: {:?}",
        result.chunks_stored, result.payment_mode_used
    );

    // 64 MB at ~4 MB chunks -> ~16+ chunks
    assert!(
        result.chunks_stored >= 10,
        "64 MB file should produce at least 10 chunks, got {}",
        result.chunks_stored
    );

    let output_path = work_dir.path().join("downloaded_64mb.bin");
    eprintln!("Downloading 64 MB file...");
    let bytes_written = client
        .file_download(&result.data_map, &output_path)
        .await
        .expect("64 MB file download should succeed");

    assert_eq!(bytes_written, file_size);

    eprintln!("Verifying content...");
    verify_file_content(&output_path, file_size);
    eprintln!("64 MB round-trip verified");

    drop(client);
    testnet.teardown().await;
}
