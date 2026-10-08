//! Reading and writing `.ant` manifest files.

use std::fs;
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

use super::{Manifest, ManifestError, MANIFEST_EXTENSION, MAX_MANIFEST_BYTES};

#[cfg(test)]
use super::{ContentRef, ManifestEntry};

/// Default basename when a manifest has no name.
const DEFAULT_BASENAME: &str = "manifest";

/// The filename a manifest with `name` is written to: `<name>.ant`.
pub fn manifest_filename_for(name: Option<&str>) -> String {
    format!("{}.{MANIFEST_EXTENSION}", name.unwrap_or(DEFAULT_BASENAME))
}

/// Read and decode a `.ant` file, refusing oversized files before reading.
pub fn read_manifest_file(path: &Path) -> Result<Manifest, ManifestError> {
    let len = fs::metadata(path)?.len();
    if len > MAX_MANIFEST_BYTES as u64 {
        return Err(ManifestError::TooLarge {
            len: len as usize,
            max: MAX_MANIFEST_BYTES,
        });
    }
    Manifest::decode(&fs::read(path)?)
}

/// Encode and write a manifest atomically. Refuses to replace an existing
/// file unless `overwrite` is set.
pub fn write_manifest_file(
    path: &Path,
    manifest: &Manifest,
    overwrite: bool,
) -> Result<(), ManifestError> {
    let bytes = manifest.encode()?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(&bytes)?;
    tmp.as_file().sync_all()?;
    let result = if overwrite {
        tmp.persist(path)
    } else {
        tmp.persist_noclobber(path)
    };
    result.map_err(|e| ManifestError::Io(e.error))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            name: Some("pack".into()),
            torrent: None,
            entries: vec![ManifestEntry {
                path: Some("a".into()),
                size: None,
                source: ContentRef::Public { address: [1; 32] },
            }],
        }
    }

    #[test]
    fn write_then_read_round_trips_and_respects_noclobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(manifest_filename_for(Some("pack")));
        assert_eq!(path.file_name().unwrap(), "pack.ant");
        write_manifest_file(&path, &sample(), false).unwrap();
        assert_eq!(read_manifest_file(&path).unwrap(), sample());
        assert!(matches!(
            write_manifest_file(&path, &sample(), false),
            Err(ManifestError::Io(_))
        ));
        write_manifest_file(&path, &sample(), true).unwrap();
        assert_eq!(manifest_filename_for(None), "manifest.ant");
    }

    #[test]
    fn read_rejects_non_manifest_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ant");
        fs::write(&path, b"hello").unwrap();
        assert!(matches!(
            read_manifest_file(&path),
            Err(ManifestError::BadHeader)
        ));
    }
}
