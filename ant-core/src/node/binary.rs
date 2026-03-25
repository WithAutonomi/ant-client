use std::path::{Path, PathBuf};

use futures_util::StreamExt;

use crate::error::{Error, Result};
use crate::node::types::BinarySource;

const GITHUB_REPO: &str = "WithAutonomi/ant-node";
pub const BINARY_NAME: &str = "ant-node";

/// Trait for reporting progress during long-running operations like binary downloads.
pub trait ProgressReporter: Send + Sync {
    fn report_started(&self, message: &str);
    fn report_progress(&self, bytes: u64, total: u64);
    fn report_complete(&self, message: &str);
}

/// A no-op progress reporter for when callers don't need progress updates.
pub struct NoopProgress;

impl ProgressReporter for NoopProgress {
    fn report_started(&self, _message: &str) {}
    fn report_progress(&self, _bytes: u64, _total: u64) {}
    fn report_complete(&self, _message: &str) {}
}

/// Resolve a node binary from the given source.
///
/// Returns `(path_to_binary, version_string)`.
///
/// For `LocalPath`, validates the binary exists and extracts version.
/// For download variants (`Latest`, `Version`, `Url`), downloads and caches the binary
/// in `install_dir`.
pub async fn resolve_binary(
    source: &BinarySource,
    install_dir: &Path,
    progress: &dyn ProgressReporter,
) -> Result<(PathBuf, String)> {
    match source {
        BinarySource::LocalPath(path) => resolve_local(path).await,
        BinarySource::Latest => resolve_latest(install_dir, progress).await,
        BinarySource::Version(version) => resolve_version(version, install_dir, progress).await,
        BinarySource::Url(url) => resolve_url(url, install_dir, progress).await,
    }
}

/// Resolve a local binary path: validate it exists and extract its version.
async fn resolve_local(path: &Path) -> Result<(PathBuf, String)> {
    if !path.exists() {
        return Err(Error::BinaryNotFound(path.to_path_buf()));
    }

    let version = extract_version(path).await?;
    Ok((path.to_path_buf(), version))
}

/// Download the latest release binary from GitHub.
async fn resolve_latest(
    install_dir: &Path,
    progress: &dyn ProgressReporter,
) -> Result<(PathBuf, String)> {
    let version = fetch_latest_version().await?;
    resolve_version(&version, install_dir, progress).await
}

/// Download a specific version of the binary from GitHub.
async fn resolve_version(
    version: &str,
    install_dir: &Path,
    progress: &dyn ProgressReporter,
) -> Result<(PathBuf, String)> {
    let version = version.strip_prefix('v').unwrap_or(version);

    // Check cache first
    let cached_path = install_dir.join(format!("{BINARY_NAME}-{version}"));
    if cached_path.exists() {
        progress.report_complete(&format!("Using cached {BINARY_NAME} v{version}"));
        return Ok((cached_path, version.to_string()));
    }

    let asset_name = platform_asset_name()?;
    let url = format!("https://github.com/{GITHUB_REPO}/releases/download/v{version}/{asset_name}");

    download_and_extract(&url, install_dir, version, progress).await
}

/// Download a binary from an arbitrary URL.
async fn resolve_url(
    url: &str,
    install_dir: &Path,
    progress: &dyn ProgressReporter,
) -> Result<(PathBuf, String)> {
    // Download to a temp location, extract, then get version from binary
    download_and_extract(url, install_dir, "unknown", progress).await
}

/// Fetch the latest release version tag from the GitHub API.
async fn fetch_latest_version() -> Result<String> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", "ant-cli")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| Error::BinaryResolution(format!("failed to fetch latest release: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::BinaryResolution(format!(
            "GitHub API returned status {} when fetching latest release",
            resp.status()
        )));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::BinaryResolution(format!("failed to parse release JSON: {e}")))?;

    let tag = body["tag_name"]
        .as_str()
        .ok_or_else(|| Error::BinaryResolution("no tag_name in release response".to_string()))?;

    Ok(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

/// Download an archive from a URL, extract the binary, and cache it.
///
/// Streams the download to a temporary file to avoid unbounded memory usage.
async fn download_and_extract(
    url: &str,
    install_dir: &Path,
    version: &str,
    progress: &dyn ProgressReporter,
) -> Result<(PathBuf, String)> {
    progress.report_started(&format!("Downloading {BINARY_NAME} from {url}"));

    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header("User-Agent", "ant-cli")
        .send()
        .await
        .map_err(|e| Error::BinaryResolution(format!("download request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::BinaryResolution(format!(
            "download returned status {}",
            resp.status()
        )));
    }

    let total_size = resp.content_length().unwrap_or(0);
    let mut downloaded: u64 = 0;

    // Stream to a temp file to avoid holding the entire archive in memory
    std::fs::create_dir_all(install_dir)?;
    let tmp_path = install_dir.join(".download.tmp");
    let mut tmp_file = std::fs::File::create(&tmp_path)
        .map_err(|e| Error::BinaryResolution(format!("failed to create temp file: {e}")))?;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| Error::BinaryResolution(format!("download stream error: {e}")))?;
        downloaded += chunk.len() as u64;
        std::io::Write::write_all(&mut tmp_file, &chunk)
            .map_err(|e| Error::BinaryResolution(format!("failed to write temp file: {e}")))?;
        progress.report_progress(downloaded, total_size);
    }
    drop(tmp_file);

    progress.report_started("Extracting archive...");

    // Read the temp file for extraction
    let bytes = std::fs::read(&tmp_path)
        .map_err(|e| Error::BinaryResolution(format!("failed to read temp file: {e}")))?;
    let _ = std::fs::remove_file(&tmp_path);

    // Extract based on file extension
    let binary_path = if url.ends_with(".zip") {
        extract_zip(&bytes, install_dir)?
    } else {
        // Assume .tar.gz
        extract_tar_gz(&bytes, install_dir)?
    };

    // Determine the actual version from the binary
    let actual_version = match extract_version(&binary_path).await {
        Ok(v) => v,
        Err(_) => version.to_string(),
    };

    // Rename to versioned name for caching
    let cached_path = install_dir.join(format!("{BINARY_NAME}-{actual_version}"));
    if binary_path != cached_path {
        // If cached path already exists (race), just use it
        if !cached_path.exists() {
            std::fs::rename(&binary_path, &cached_path)?;
        } else {
            let _ = std::fs::remove_file(&binary_path);
        }
    }

    progress.report_complete(&format!(
        "Downloaded {BINARY_NAME} v{actual_version} to {}",
        cached_path.display()
    ));

    Ok((cached_path, actual_version))
}

/// Extract a .tar.gz archive and return the path to the node binary.
fn extract_tar_gz(data: &[u8], install_dir: &Path) -> Result<PathBuf> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);

    let mut binary_path = None;

    for entry in archive
        .entries()
        .map_err(|e| Error::BinaryResolution(format!("failed to read tar entries: {e}")))?
    {
        let mut entry =
            entry.map_err(|e| Error::BinaryResolution(format!("failed to read tar entry: {e}")))?;

        let path = entry
            .path()
            .map_err(|e| Error::BinaryResolution(format!("invalid path in archive: {e}")))?;

        // Reject paths with traversal components (e.g., "../../../etc/passwd")
        for component in path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                return Err(Error::BinaryResolution(format!(
                    "path traversal detected in archive: {}",
                    path.display()
                )));
            }
        }

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        if file_name == BINARY_NAME {
            let dest = install_dir.join(BINARY_NAME);
            let mut file = std::fs::File::create(&dest)?;
            std::io::copy(&mut entry, &mut file)?;

            // Set executable permission on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
            }

            binary_path = Some(dest);
        }
    }

    binary_path
        .ok_or_else(|| Error::BinaryResolution(format!("'{BINARY_NAME}' not found in archive")))
}

/// Extract a .zip archive and return the path to the node binary.
fn extract_zip(data: &[u8], install_dir: &Path) -> Result<PathBuf> {
    let cursor = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| Error::BinaryResolution(format!("failed to open zip archive: {e}")))?;

    let mut binary_path = None;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| Error::BinaryResolution(format!("failed to read zip entry: {e}")))?;

        let file_name = file
            .enclosed_name()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();

        if file_name == BINARY_NAME || file_name == format!("{BINARY_NAME}.exe") {
            let dest = install_dir.join(&file_name);
            let mut out = std::fs::File::create(&dest)?;
            std::io::copy(&mut file, &mut out)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
            }

            binary_path = Some(dest);
        }
    }

    binary_path
        .ok_or_else(|| Error::BinaryResolution(format!("'{BINARY_NAME}' not found in archive")))
}

/// Extract the version string from a node binary by running `<binary> --version`.
async fn extract_version(binary_path: &Path) -> Result<String> {
    let output = tokio::process::Command::new(binary_path)
        .arg("--version")
        .output()
        .await
        .map_err(|e| {
            Error::BinaryResolution(format!(
                "failed to run {} --version: {e}",
                binary_path.display()
            ))
        })?;

    if !output.status.success() {
        return Err(Error::BinaryResolution(format!(
            "{} --version exited with status {}",
            binary_path.display(),
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Expect output like "ant-node 0.3.4" — extract the version part.
    let version = stdout
        .split_whitespace()
        .last()
        .unwrap_or("unknown")
        .to_string();

    Ok(version)
}

/// Returns the platform-specific archive asset name.
fn platform_asset_name() -> Result<String> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return Err(Error::BinaryResolution(format!(
            "unsupported platform: {}",
            std::env::consts::OS
        )));
    };

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        return Err(Error::BinaryResolution(format!(
            "unsupported architecture: {}",
            std::env::consts::ARCH
        )));
    };

    let ext = if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    };

    Ok(format!("ant-node-cli-{os}-{arch}.{ext}"))
}

/// Returns the directory where downloaded binaries are cached.
pub fn binary_install_dir() -> crate::error::Result<PathBuf> {
    Ok(crate::config::data_dir()?.join("bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_path_not_found() {
        let result = resolve_binary(
            &BinarySource::LocalPath("/nonexistent/binary".into()),
            Path::new("/tmp"),
            &NoopProgress,
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::BinaryNotFound(_)));
    }

    #[test]
    fn platform_asset_name_has_correct_format() {
        let name = platform_asset_name().unwrap();
        assert!(name.starts_with("ant-node-cli-"));
        assert!(
            name.ends_with(".tar.gz") || name.ends_with(".zip"),
            "unexpected extension: {name}"
        );
    }

    #[test]
    fn extract_tar_gz_finds_binary() {
        // Create a tar.gz with a fake binary inside
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());

        let data = b"#!/bin/sh\necho test\n";
        let mut header = tar::Header::new_gnu();
        header.set_path(BINARY_NAME).unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
        let tar_data = builder.into_inner().unwrap();

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        let gz_data = encoder.finish().unwrap();

        let result = extract_tar_gz(&gz_data, tmp.path());
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), BINARY_NAME);
    }

    #[test]
    fn extract_tar_gz_missing_binary_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let builder = tar::Builder::new(Vec::new());
        let tar_data = builder.into_inner().unwrap();

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        let gz_data = encoder.finish().unwrap();

        let result = extract_tar_gz(&gz_data, tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn extract_tar_gz_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();

        // Build a tar archive with a path traversal entry using raw bytes.
        // The tar crate's set_path() rejects ".." so we write the header manually.
        let data = b"malicious content";
        let mut header = tar::Header::new_gnu();
        // Use a safe placeholder first, then overwrite the raw name bytes
        header.set_path("placeholder").unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);

        // Overwrite the name field (first 100 bytes) with a traversal path
        let traversal = b"../../../etc/evil";
        let raw = header.as_mut_bytes();
        raw[..traversal.len()].copy_from_slice(traversal);
        raw[traversal.len()] = 0;
        header.set_cksum();

        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, &data[..]).unwrap();
        let tar_data = builder.into_inner().unwrap();

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        let gz_data = encoder.finish().unwrap();

        let result = extract_tar_gz(&gz_data, tmp.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("path traversal"),
            "expected path traversal error, got: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_version_uses_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cached = tmp.path().join(format!("{BINARY_NAME}-1.2.3"));
        std::fs::write(&cached, "fake binary").unwrap();

        let result = resolve_version("1.2.3", tmp.path(), &NoopProgress).await;
        assert!(result.is_ok());
        let (path, version) = result.unwrap();
        assert_eq!(path, cached);
        assert_eq!(version, "1.2.3");
    }

    #[tokio::test]
    async fn resolve_version_strips_v_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let cached = tmp.path().join(format!("{BINARY_NAME}-0.3.4"));
        std::fs::write(&cached, "fake binary").unwrap();

        let result = resolve_version("v0.3.4", tmp.path(), &NoopProgress).await;
        assert!(result.is_ok());
        let (_, version) = result.unwrap();
        assert_eq!(version, "0.3.4");
    }
}
