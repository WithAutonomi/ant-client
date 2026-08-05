// folder.rs — folder upload orchestrator for ant-core.
// Walks a directory, self-encrypts all files, builds a manifest with
// embedded DataMaps, uploads everything via antd, and optionally backs
// up the manifest DataMap on-chain via PaymentMode::Recovery.

use self_encryption::{encrypt, DataMap};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A manifest entry: file path → content address + embedded DataMap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Content address of the uploaded chunk.
    pub addr: String,
    /// Serialized DataMap bytes (bincode), base64-encoded.
    pub datamap: String,
    /// Original file size in bytes.
    pub size: u64,
}

/// Folder manifest: maps relative paths to their content entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderManifest {
    pub folder_name: String,
    pub created_at: String,
    pub files: HashMap<String, ManifestEntry>,
    /// Total number of files in the folder.
    pub file_count: usize,
    /// Total size of all files in bytes.
    pub total_size: u64,
}

impl FolderManifest {
    /// Build a manifest by walking a directory and encrypting all files.
    pub fn build(folder_path: &Path) -> Result<Self, String> {
        let folder_name = folder_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let mut files = HashMap::new();
        let mut file_count = 0;
        let mut total_size = 0u64;

        for entry in walkdir::WalkDir::new(folder_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let relative = path
                .strip_prefix(folder_path)
                .map_err(|e| format!("strip prefix: {e}"))?;

            let content = std::fs::read(path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;

            let size = content.len() as u64;

            // Self-encrypt the file
            let (data_map, _encrypted_chunks) = encrypt(Bytes::from(content))
                .map_err(|e| format!("encrypt {}: {e}", path.display()))?;

            // Serialize DataMap to bytes
            let datamap_bytes = data_map.to_bytes()
                .map_err(|e| format!("serialize datamap: {e}"))?;

            let entry = ManifestEntry {
                addr: String::new(), // filled after upload
                datamap: base64_encode(&datamap_bytes),
                size,
            };

            files.insert(relative.to_string_lossy().to_string(), entry);
            file_count += 1;
            total_size += size;
        }

        Ok(FolderManifest {
            folder_name,
            created_at: chrono_now(),
            files,
            file_count,
            total_size,
        })
    }

    /// Serialize manifest to JSON bytes.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|e| format!("serialize manifest: {e}"))
    }

    /// Deserialize manifest from JSON bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("deserialize manifest: {e}"))
    }
}

/// Result of a folder upload operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderUploadResult {
    /// Name of the uploaded folder.
    pub folder_name: String,
    /// Content address of the manifest on the network.
    pub manifest_addr: String,
    /// Number of files uploaded.
    pub file_count: usize,
    /// Total size uploaded in bytes.
    pub total_size: u64,
    /// Transaction hash for recovery (if PaymentMode::Recovery was used).
    pub recovery_tx_hash: Option<String>,
}

/// Staging area state tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadState {
    pub version: u32,
    pub pending: Vec<PendingFolder>,
    pub uploading: Option<ActiveUpload>,
    pub completed: Vec<CompletedFolder>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingFolder {
    pub path: String,
    pub detected_at: String,
    pub file_count: usize,
    pub total_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveUpload {
    pub path: String,
    pub started_at: String,
    pub progress_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedFolder {
    pub folder_name: String,
    pub completed_at: String,
    pub file_count: usize,
    pub manifest_addr: String,
    pub recovery_tx_hash: Option<String>,
}

impl UploadState {
    pub fn load(path: &Path) -> Result<Self, String> {
        if path.exists() {
            let bytes = std::fs::read(path).map_err(|e| format!("read state: {e}"))?;
            serde_json::from_slice(&bytes).map_err(|e| format!("parse state: {e}"))
        } else {
            Ok(UploadState {
                version: 1,
                pending: vec![],
                uploading: None,
                completed: vec![],
            })
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| format!("serialize state: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("write state: {e}"))
    }
}

fn chrono_now() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let time = secs % 86400;
    let hours = time / 3600;
    let mins = (time % 3600) / 60;
    let secs = time % 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        1970 + (days / 365) as i64,
        ((days % 365) / 30 + 1).min(12),
        (days % 30 + 1).min(31),
        hours,
        mins,
        secs
    )
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_manifest_build_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(b"hello folder upload test content for self-encryption").unwrap();
        // Need >3KB for self-encryption to work
        let big_path = dir.path().join("big.txt");
        let mut bf = std::fs::File::create(&big_path).unwrap();
        for _ in 0..100 {
            bf.write_all(b"padding data to reach minimum self-encryption size requirement ").unwrap();
        }

        let manifest = FolderManifest::build(dir.path()).unwrap();
        assert_eq!(manifest.folder_name, dir.path().file_name().unwrap().to_string_lossy());
        assert!(manifest.file_count >= 1);
        assert!(manifest.total_size > 3072); // minimum for self-encryption

        let json = manifest.to_json_bytes().unwrap();
        let roundtripped = FolderManifest::from_json_bytes(&json).unwrap();
        assert_eq!(roundtripped.file_count, manifest.file_count);
    }

    #[test]
    fn test_upload_state_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join(".upload-state.json");

        let mut state = UploadState::load(&state_path).unwrap();
        state.pending.push(PendingFolder {
            path: "/tmp/test-folder".into(),
            detected_at: "2026-01-01T00:00:00Z".into(),
            file_count: 3,
            total_size: 5000,
        });
        state.save(&state_path).unwrap();

        let loaded = UploadState::load(&state_path).unwrap();
        assert_eq!(loaded.pending.len(), 1);
        assert_eq!(loaded.pending[0].file_count, 3);
    }
}
