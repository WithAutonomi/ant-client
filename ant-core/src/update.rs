use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::node::binary::{extract_tar_gz, extract_zip, ProgressReporter};

const GITHUB_REPO: &str = "WithAutonomi/ant-client";
const CLI_BINARY_NAME: &str = "ant";

/// Result of checking whether an update is available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheck {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub download_url: Option<String>,
}

/// Result of a completed update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResult {
    pub previous_version: String,
    pub new_version: String,
}

/// Options for the update operation.
pub struct UpdateOpts {
    /// Force re-download even if already on the latest version.
    pub force: bool,
}

/// Check whether a newer version is available on GitHub Releases.
///
/// Compares `current_version` against the latest release tag using semantic versioning.
pub async fn check_for_update(current_version: &str) -> Result<UpdateCheck> {
    let latest = fetch_latest_cli_version().await?;
    let current = parse_version(current_version)?;
    let latest_parsed = parse_version(&latest)?;
    let update_available = latest_parsed > current;

    let download_url = if update_available {
        Some(build_download_url(&latest)?)
    } else {
        None
    };

    Ok(UpdateCheck {
        current_version: current_version.to_string(),
        latest_version: latest,
        update_available,
        download_url,
    })
}

/// Download and install the update, replacing the current binary.
///
/// The `current_exe` path is the location of the running binary. The new binary is
/// downloaded to a temporary file beside it, then atomically swapped into place.
pub async fn perform_update(
    check: &UpdateCheck,
    progress: &dyn ProgressReporter,
) -> Result<UpdateResult> {
    let download_url = check.download_url.as_deref().ok_or_else(|| {
        Error::UpdateFailed("no download URL — are you already on the latest version?".to_string())
    })?;

    let current_exe = std::env::current_exe().map_err(|e| {
        Error::UpdateFailed(format!("could not determine current executable path: {e}"))
    })?;

    let new_binary = download_and_extract_cli(download_url, progress).await?;
    replace_binary(&current_exe, &new_binary)?;
    let _ = std::fs::remove_file(&new_binary);

    Ok(UpdateResult {
        previous_version: check.current_version.clone(),
        new_version: check.latest_version.clone(),
    })
}

const TAG_PREFIX: &str = "ant-cli-v";

/// Fetch the latest stable CLI release version from GitHub.
///
/// Lists all releases and finds the newest non-draft, non-prerelease one whose
/// tag starts with `ant-cli-v`.
async fn fetch_latest_cli_version() -> Result<String> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases?per_page=50");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", "ant-cli")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| Error::UpdateFailed(format!("failed to fetch releases: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::UpdateFailed(format!(
            "GitHub API returned status {} when fetching releases",
            resp.status()
        )));
    }

    let releases: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| Error::UpdateFailed(format!("failed to parse releases JSON: {e}")))?;

    // Find the newest stable ant-cli release by semver (skip drafts and pre-releases).
    let mut best: Option<semver::Version> = None;
    for release in &releases {
        if release["draft"].as_bool().unwrap_or(false)
            || release["prerelease"].as_bool().unwrap_or(false)
        {
            continue;
        }
        let tag = release["tag_name"].as_str().unwrap_or_default();
        if let Some(version_str) = tag.strip_prefix(TAG_PREFIX) {
            if let Ok(v) = semver::Version::parse(version_str) {
                if best.as_ref().is_none_or(|b| v > *b) {
                    best = Some(v);
                }
            }
        }
    }

    best.map(|v| v.to_string())
        .ok_or_else(|| Error::UpdateFailed("no ant-cli release found on GitHub".to_string()))
}

/// Download the CLI archive and extract the binary to a temp directory.
///
/// Returns the path to the extracted binary.
async fn download_and_extract_cli(url: &str, progress: &dyn ProgressReporter) -> Result<PathBuf> {
    progress.report_started(&format!("Downloading {CLI_BINARY_NAME} from {url}"));

    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header("User-Agent", "ant-cli")
        .send()
        .await
        .map_err(|e| Error::UpdateFailed(format!("download request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::UpdateFailed(format!(
            "download returned status {}",
            resp.status()
        )));
    }

    let total_size = resp.content_length().unwrap_or(0);
    let mut downloaded: u64 = 0;

    let tmp_dir = std::env::temp_dir().join("ant-update");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_archive = tmp_dir.join(".download.tmp");
    let mut tmp_file = std::fs::File::create(&tmp_archive)
        .map_err(|e| Error::UpdateFailed(format!("failed to create temp file: {e}")))?;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| Error::UpdateFailed(format!("download stream error: {e}")))?;
        downloaded += chunk.len() as u64;
        std::io::Write::write_all(&mut tmp_file, &chunk)
            .map_err(|e| Error::UpdateFailed(format!("failed to write temp file: {e}")))?;
        progress.report_progress(downloaded, total_size);
    }
    drop(tmp_file);

    progress.report_started("Extracting archive...");

    let bytes = std::fs::read(&tmp_archive)
        .map_err(|e| Error::UpdateFailed(format!("failed to read temp file: {e}")))?;
    let _ = std::fs::remove_file(&tmp_archive);

    let binary_path = if url.ends_with(".zip") {
        extract_zip(&bytes, &tmp_dir, CLI_BINARY_NAME)?
    } else {
        extract_tar_gz(&bytes, &tmp_dir, CLI_BINARY_NAME)?
    };

    progress.report_complete("Download complete");

    Ok(binary_path)
}

/// Replace the current executable with the new binary.
///
/// Uses the `self_replace` crate which handles platform-specific nuances,
/// particularly on Windows where the running executable is locked.
fn replace_binary(_current_exe: &Path, new_binary: &Path) -> Result<()> {
    self_replace::self_replace(new_binary)
        .map_err(|e| Error::UpdateFailed(format!("failed to replace binary: {e}")))?;
    Ok(())
}

/// Build the download URL for a given version.
///
/// Tag format: `ant-cli-v{version}`
/// Asset format: `ant-{version}-{target_triple}.{ext}`
pub fn build_download_url(version: &str) -> Result<String> {
    let asset_name = cli_platform_asset_name(version)?;
    Ok(format!(
        "https://github.com/{GITHUB_REPO}/releases/download/{TAG_PREFIX}{version}/{asset_name}"
    ))
}

/// Returns the platform-specific archive asset name for the CLI binary.
///
/// Matches the naming convention from CI: `ant-{version}-{target_triple}.{ext}`
fn cli_platform_asset_name(version: &str) -> Result<String> {
    let target_triple = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else {
        return Err(Error::UpdateFailed(format!(
            "unsupported platform: {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )));
    };

    let ext = if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    };

    Ok(format!("ant-{version}-{target_triple}.{ext}"))
}

fn parse_version(version: &str) -> Result<semver::Version> {
    let cleaned = version.strip_prefix('v').unwrap_or(version);
    semver::Version::parse(cleaned)
        .map_err(|e| Error::UpdateFailed(format!("invalid version '{version}': {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_valid() {
        assert!(parse_version("1.2.3").is_ok());
        assert!(parse_version("v1.2.3").is_ok());
        assert!(parse_version("0.1.0").is_ok());
    }

    #[test]
    fn parse_version_invalid() {
        assert!(parse_version("not-a-version").is_err());
        assert!(parse_version("").is_err());
    }

    #[test]
    fn version_comparison() {
        let v1 = parse_version("0.1.0").unwrap();
        let v2 = parse_version("0.2.0").unwrap();
        assert!(v2 > v1);

        let v3 = parse_version("1.0.0").unwrap();
        assert!(v3 > v2);

        let same = parse_version("0.1.0").unwrap();
        assert_eq!(v1, same);
    }

    #[test]
    fn check_result_no_update() {
        let check = UpdateCheck {
            current_version: "1.0.0".to_string(),
            latest_version: "1.0.0".to_string(),
            update_available: false,
            download_url: None,
        };
        assert!(!check.update_available);
        assert!(check.download_url.is_none());
    }

    #[test]
    fn check_result_with_update() {
        let check = UpdateCheck {
            current_version: "0.1.0".to_string(),
            latest_version: "0.2.0".to_string(),
            update_available: true,
            download_url: Some("https://example.com/ant.tar.gz".to_string()),
        };
        assert!(check.update_available);
        assert!(check.download_url.is_some());
    }

    #[test]
    fn platform_asset_name_format() {
        let name = cli_platform_asset_name("1.2.3").unwrap();
        assert!(name.starts_with("ant-1.2.3-"));
        assert!(
            name.ends_with(".tar.gz") || name.ends_with(".zip"),
            "unexpected extension: {name}"
        );
    }

    #[test]
    fn build_download_url_format() {
        let url = build_download_url("1.2.3").unwrap();
        assert!(url.starts_with(
            "https://github.com/WithAutonomi/ant-client/releases/download/ant-cli-v1.2.3/ant-1.2.3-"
        ));
        assert!(url.ends_with(".tar.gz") || url.ends_with(".zip"));
    }

    #[test]
    fn update_check_serializes() {
        let check = UpdateCheck {
            current_version: "0.1.0".to_string(),
            latest_version: "0.2.0".to_string(),
            update_available: true,
            download_url: Some("https://example.com/ant.tar.gz".to_string()),
        };
        let json = serde_json::to_string(&check).unwrap();
        let deserialized: UpdateCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.current_version, "0.1.0");
        assert!(deserialized.update_available);
    }
}
