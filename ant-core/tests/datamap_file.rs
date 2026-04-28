//! Integration tests for [`ant_core::datamap_file`].

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use ant_core::datamap_file::{
    datamap_filename_for, original_name_from_datamap, read_datamap, write_datamap, CollisionPolicy,
    DATAMAP_EXTENSION,
};
use self_encryption::{ChunkInfo, DataMap};
use tempfile::TempDir;
use xor_name::XorName;

fn known_data_map() -> DataMap {
    DataMap::new(vec![
        ChunkInfo {
            index: 0,
            dst_hash: XorName([0x11; 32]),
            src_hash: XorName([0x22; 32]),
            src_size: 1024,
        },
        ChunkInfo {
            index: 1,
            dst_hash: XorName([0x33; 32]),
            src_hash: XorName([0x44; 32]),
            src_size: 2048,
        },
    ])
}

// ---------------------------------------------------------------------------
// Filename derivation (pure, no fs)
// ---------------------------------------------------------------------------

#[test]
fn datamap_filename_keeps_extension() {
    assert_eq!(datamap_filename_for("photo.jpg"), "photo.jpg.datamap");
    assert_eq!(datamap_filename_for("Makefile"), "Makefile.datamap");
    assert_eq!(
        datamap_filename_for("archive.tar.gz"),
        "archive.tar.gz.datamap"
    );
}

#[test]
fn datamap_filename_preserves_dotfiles() {
    // Leading dot must survive sanitization so `.bashrc` doesn't become
    // `bashrc.datamap`.
    assert_eq!(datamap_filename_for(".bashrc"), ".bashrc.datamap");
}

#[test]
fn datamap_filename_empty_falls_back() {
    assert_eq!(datamap_filename_for(""), "datamap.datamap");
    assert_eq!(datamap_filename_for("   "), "datamap.datamap");
}

#[test]
fn datamap_filename_replaces_platform_illegal_chars() {
    // Path separators and Windows-illegal chars must be replaced; the
    // shape must remain `<name>.datamap`.
    assert_eq!(datamap_filename_for("a/b.txt"), "a_b.txt.datamap");
    assert_eq!(
        datamap_filename_for("file:name?.txt"),
        "file_name_.txt.datamap"
    );
}

#[test]
fn datamap_filename_trims_surrounding_whitespace() {
    assert_eq!(datamap_filename_for("  photo.jpg  "), "photo.jpg.datamap");
}

// ---------------------------------------------------------------------------
// original_name_from_datamap (pure, no fs)
// ---------------------------------------------------------------------------

#[test]
fn original_name_round_trips_basic_extensions() {
    assert_eq!(
        original_name_from_datamap(Path::new("photo.jpg.datamap")),
        Some(OsString::from("photo.jpg"))
    );
    assert_eq!(
        original_name_from_datamap(Path::new("Makefile.datamap")),
        Some(OsString::from("Makefile"))
    );
    assert_eq!(
        original_name_from_datamap(Path::new("archive.tar.gz.datamap")),
        Some(OsString::from("archive.tar.gz"))
    );
}

#[test]
fn original_name_handles_full_paths() {
    assert_eq!(
        original_name_from_datamap(Path::new("/tmp/sub/photo.jpg.datamap")),
        Some(OsString::from("photo.jpg"))
    );
}

#[test]
fn original_name_does_not_undo_collision_suffix() {
    // The `-2` is a write-side artifact; the inverse function returns
    // the literal stem without trying to peel it off.
    assert_eq!(
        original_name_from_datamap(Path::new("photo.jpg-2.datamap")),
        Some(OsString::from("photo.jpg-2"))
    );
}

#[test]
fn original_name_rejects_non_datamap() {
    assert_eq!(original_name_from_datamap(Path::new("photo.txt")), None);
    assert_eq!(original_name_from_datamap(Path::new("photo")), None);
}

#[test]
fn original_name_rejects_bare_datamap() {
    // Stripping `.datamap` from `.datamap` leaves an empty name; we don't
    // want callers to default-save to "" so return None.
    assert_eq!(original_name_from_datamap(Path::new(".datamap")), None);
}

// ---------------------------------------------------------------------------
// write_datamap (uses tempdir)
// ---------------------------------------------------------------------------

#[test]
fn write_then_read_round_trips_msgpack() {
    let dir = TempDir::new().unwrap();
    let dm = known_data_map();
    let path = write_datamap(dir.path(), "photo.jpg", &dm, CollisionPolicy::NumericSuffix).unwrap();

    assert_eq!(path, dir.path().join("photo.jpg.datamap"));

    // First byte must NOT be `{` — confirms we wrote msgpack, not JSON.
    let bytes = fs::read(&path).unwrap();
    assert_ne!(bytes.first(), Some(&b'{'));

    let read_back = read_datamap(&path).unwrap();
    assert_eq!(read_back, dm);
}

#[test]
fn write_overwrite_replaces_existing_file() {
    let dir = TempDir::new().unwrap();
    let dm_a = known_data_map();
    let dm_b = DataMap::new(vec![ChunkInfo {
        index: 0,
        dst_hash: XorName([0xAA; 32]),
        src_hash: XorName([0xBB; 32]),
        src_size: 4096,
    }]);

    let p1 = write_datamap(dir.path(), "photo.jpg", &dm_a, CollisionPolicy::Overwrite).unwrap();
    let p2 = write_datamap(dir.path(), "photo.jpg", &dm_b, CollisionPolicy::Overwrite).unwrap();

    assert_eq!(p1, p2);
    assert_eq!(read_datamap(&p2).unwrap(), dm_b);

    // Only one file in the dir.
    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1);
}

#[test]
fn write_numeric_suffix_appends_dash_n() {
    let dir = TempDir::new().unwrap();
    let dm = known_data_map();

    let p1 = write_datamap(dir.path(), "photo.jpg", &dm, CollisionPolicy::NumericSuffix).unwrap();
    let p2 = write_datamap(dir.path(), "photo.jpg", &dm, CollisionPolicy::NumericSuffix).unwrap();
    let p3 = write_datamap(dir.path(), "photo.jpg", &dm, CollisionPolicy::NumericSuffix).unwrap();

    assert_eq!(p1.file_name().unwrap(), "photo.jpg.datamap");
    assert_eq!(p2.file_name().unwrap(), "photo.jpg-2.datamap");
    assert_eq!(p3.file_name().unwrap(), "photo.jpg-3.datamap");
}

#[test]
fn write_numeric_suffix_extensionless_input() {
    let dir = TempDir::new().unwrap();
    let dm = known_data_map();

    let p1 = write_datamap(dir.path(), "Makefile", &dm, CollisionPolicy::NumericSuffix).unwrap();
    let p2 = write_datamap(dir.path(), "Makefile", &dm, CollisionPolicy::NumericSuffix).unwrap();

    assert_eq!(p1.file_name().unwrap(), "Makefile.datamap");
    assert_eq!(p2.file_name().unwrap(), "Makefile-2.datamap");
}

#[test]
fn write_numeric_suffix_compound_extension() {
    let dir = TempDir::new().unwrap();
    let dm = known_data_map();

    let p1 = write_datamap(
        dir.path(),
        "archive.tar.gz",
        &dm,
        CollisionPolicy::NumericSuffix,
    )
    .unwrap();
    let p2 = write_datamap(
        dir.path(),
        "archive.tar.gz",
        &dm,
        CollisionPolicy::NumericSuffix,
    )
    .unwrap();

    assert_eq!(p1.file_name().unwrap(), "archive.tar.gz.datamap");
    assert_eq!(p2.file_name().unwrap(), "archive.tar.gz-2.datamap");
}

#[test]
fn write_creates_directory_if_missing() {
    let dir = TempDir::new().unwrap();
    let nested = dir.path().join("does/not/exist/yet");
    let dm = known_data_map();

    let path = write_datamap(&nested, "x.bin", &dm, CollisionPolicy::NumericSuffix).unwrap();
    assert!(path.exists());
    assert_eq!(read_datamap(&path).unwrap(), dm);
}

// ---------------------------------------------------------------------------
// read_datamap format auto-detect
// ---------------------------------------------------------------------------

#[test]
fn read_accepts_legacy_json_format() {
    let dir = TempDir::new().unwrap();
    let dm = known_data_map();
    let json_path = dir.path().join("legacy.datamap");

    // Hand-write a JSON datamap using the same shape ant-gui produced
    // before adopting the shared helper.
    let json = serde_json::to_string(&dm).unwrap();
    assert!(json.starts_with('{'));
    fs::write(&json_path, json).unwrap();

    let read_back = read_datamap(&json_path).unwrap();
    assert_eq!(read_back, dm);
}

#[test]
fn read_accepts_canonical_msgpack_format() {
    let dir = TempDir::new().unwrap();
    let dm = known_data_map();
    let mp_path = dir.path().join("canonical.datamap");

    let bytes = rmp_serde::to_vec(&dm).unwrap();
    assert_ne!(bytes.first(), Some(&b'{'));
    fs::write(&mp_path, bytes).unwrap();

    let read_back = read_datamap(&mp_path).unwrap();
    assert_eq!(read_back, dm);
}

#[test]
fn read_rejects_garbage() {
    let dir = TempDir::new().unwrap();
    let bogus = dir.path().join("bogus.datamap");
    // Bytes that aren't `{` route to the msgpack branch and must fail.
    fs::write(&bogus, b"\xFFnot a real datamap").unwrap();

    assert!(read_datamap(&bogus).is_err());
}

#[test]
fn extension_constant_matches_filename_suffix() {
    // Guard against drift: every test in this file assumes the extension
    // is `datamap`. If anyone changes the constant we should fail loudly.
    assert_eq!(DATAMAP_EXTENSION, "datamap");
}
