//! Building a manifest from local files and public addresses.
//!
//! Local files are uploaded through the ordinary file upload with the
//! visibility the caller chose. Already-public files are added by address
//! and nothing is uploaded. The builder never stores a DataMap chunk on its
//! own: a file becomes public only through the caller's upload choice.

use std::fs;
use std::path::{Path, PathBuf};

use self_encryption::MIN_ENCRYPTABLE_BYTES;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::data::client::file::{UploadEvent, Visibility};
use crate::data::client::merkle::PaymentMode;
use crate::data::Client;

use super::embed::embeddable_data_map;
use super::path::{check_collisions, validate_component, validate_path, PATH_SEPARATOR};
use super::{
    data_map_address, ContentRef, Manifest, ManifestEntry, ManifestError, TorrentReference,
    ADDRESS_LEN, MAX_MANIFEST_ENTRIES,
};

/// Capacity of the per-file upload progress channel.
const UPLOAD_PROGRESS_CAPACITY: usize = 64;

/// How a locally uploaded file is referenced from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReferenceMode {
    /// Always embed the DataMap, even for public uploads. The recipient
    /// skips one fetch per file.
    #[default]
    Embedded,
    /// Prefer a public address wherever the DataMap chunk is on the
    /// network, because the upload was public or the chunk already exists;
    /// embed otherwise. Never stores a DataMap chunk itself.
    Compact,
}

/// Options for [`ManifestBuilder`].
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// Suggested root directory name.
    pub name: Option<String>,
    /// BitTorrent identity of the same files, when known.
    pub torrent: Option<TorrentReference>,
    /// How uploaded files are referenced.
    pub reference_mode: ReferenceMode,
    /// Visibility of the file uploads themselves. `Public` stores each
    /// file's DataMap chunk and makes the file public; the manifest still
    /// embeds the DataMap unless `reference_mode` is `Compact`.
    pub visibility: Visibility,
    /// Payment mode for the uploads.
    pub payment_mode: PaymentMode,
    /// Follow symlinks to regular files instead of skipping them.
    pub follow_symlinks: bool,
    /// Cancels the build between files, or during an upload. Files already
    /// uploaded are kept in the partial result.
    pub cancel: CancellationToken,
}

/// Progress during [`ManifestBuilder::finish`].
#[derive(Debug, Clone)]
pub enum BuildEvent {
    /// An upload is starting.
    FileStarted {
        /// Manifest path of the file.
        path: String,
        /// Zero-based index among the files to upload.
        index: usize,
        /// Number of files to upload.
        total: usize,
    },
    /// Progress from the underlying file upload.
    Upload {
        /// Manifest path of the file.
        path: String,
        /// The upload event.
        event: UploadEvent,
    },
    /// An upload finished.
    FileFinished {
        /// Manifest path of the file.
        path: String,
        /// Chunks newly stored by this upload.
        chunks_stored: usize,
        /// `embedded` or `public`.
        reference: &'static str,
    },
}

/// What [`ManifestBuilder::finish`] produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    /// The canonical manifest. Partial after cancellation or a file
    /// failure: completed uploads plus entries added without uploading.
    pub manifest: Manifest,
    /// Whether the build was cancelled before every file was uploaded.
    pub cancelled: bool,
    /// Symlinks skipped during directory walks.
    pub skipped_symlinks: Vec<PathBuf>,
    /// Files uploaded.
    pub files_uploaded: usize,
    /// Local files originally queued for upload, excluding entries added
    /// by address or by an existing DataMap.
    #[serde(default)]
    pub total_to_upload: usize,
    /// Chunks newly stored across all uploads.
    pub chunks_stored: usize,
    /// Storage paid across all uploads, in atto tokens.
    pub storage_cost_atto: u128,
    /// Gas paid across all uploads, in wei.
    pub gas_cost_wei: u128,
}

struct PendingFile {
    local: PathBuf,
    path: String,
}

/// Collects files and addresses, then uploads and assembles a manifest.
pub struct ManifestBuilder<'a> {
    client: &'a Client,
    options: BuildOptions,
    pending: Vec<PendingFile>,
    entries: Vec<ManifestEntry>,
    skipped_symlinks: Vec<PathBuf>,
}

impl<'a> ManifestBuilder<'a> {
    /// A builder that uploads through `client`.
    pub fn new(client: &'a Client, options: BuildOptions) -> Self {
        Self {
            client,
            options,
            pending: Vec::new(),
            entries: Vec::new(),
            skipped_symlinks: Vec::new(),
        }
    }

    /// Number of local files queued for upload.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Add a local file or directory. A directory's entries are prefixed
    /// with its own name; a file is added under its file name.
    pub fn add_path(&mut self, local: &Path) -> Result<(), ManifestError> {
        let meta = fs::symlink_metadata(local)?;
        let name = file_name_utf8(local)?;
        if meta.is_dir() {
            self.add_directory(local, Some(&name))
        } else if meta.file_type().is_symlink() {
            if self.options.follow_symlinks && fs::metadata(local)?.is_file() {
                self.add_file(local, name)
            } else {
                self.skipped_symlinks.push(local.to_path_buf());
                Ok(())
            }
        } else {
            self.add_file(local, name)
        }
    }

    /// Queue one local file under `path`.
    pub fn add_file(&mut self, local: &Path, path: String) -> Result<(), ManifestError> {
        validate_path(&path).map_err(|reason| ManifestError::InvalidPath {
            path: path.clone(),
            reason,
        })?;
        self.pending.push(PendingFile {
            local: local.to_path_buf(),
            path,
        });
        Ok(())
    }

    /// Queue every regular file under `root`, with paths relative to it,
    /// optionally under `prefix`. Entries are sorted by path. Symlinks are
    /// skipped and reported unless `follow_symlinks` is set, in which case
    /// symlinks to regular files are included.
    pub fn add_directory(
        &mut self,
        root: &Path,
        prefix: Option<&str>,
    ) -> Result<(), ManifestError> {
        let mut files = Vec::new();
        self.walk(root, prefix.unwrap_or(""), &mut files)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        for file in files {
            self.add_file(&file.local, file.path)?;
        }
        Ok(())
    }

    /// Add a file by a DataMap already in hand, embedded. Nothing is
    /// uploaded. This is how a public file is brought into a manifest with
    /// its DataMap embedded, so recipients skip the DataMap fetch. The
    /// entry's content address is derived from `data_map`.
    pub fn add_embedded(
        &mut self,
        data_map: self_encryption::DataMap,
        path: Option<String>,
        size: Option<u64>,
    ) -> Result<(), ManifestError> {
        if let Some(p) = &path {
            validate_path(p).map_err(|reason| ManifestError::InvalidPath {
                path: p.clone(),
                reason,
            })?;
        }
        self.entries.push(ManifestEntry {
            path,
            size,
            source: ContentRef::Embedded { data_map },
        });
        Ok(())
    }

    /// Add an already-public file by address. Nothing is uploaded.
    pub fn add_public(
        &mut self,
        address: [u8; ADDRESS_LEN],
        path: Option<String>,
        size: Option<u64>,
    ) -> Result<(), ManifestError> {
        if let Some(p) = &path {
            validate_path(p).map_err(|reason| ManifestError::InvalidPath {
                path: p.clone(),
                reason,
            })?;
        }
        self.entries.push(ManifestEntry {
            path,
            size,
            source: ContentRef::Public { address },
        });
        Ok(())
    }

    /// Check everything that could make the finished manifest invalid
    /// before the first paid upload: file sizes, the name, and collisions
    /// among all queued paths and address entries.
    fn preflight(&self) -> Result<(), ManifestError> {
        let count = self.pending.len().saturating_add(self.entries.len());
        if count > MAX_MANIFEST_ENTRIES {
            return Err(ManifestError::TooManyEntries {
                count,
                max: MAX_MANIFEST_ENTRIES,
            });
        }
        if let Some(name) = &self.options.name {
            validate_component(name).map_err(ManifestError::InvalidName)?;
        }
        if self
            .options
            .torrent
            .as_ref()
            .is_some_and(TorrentReference::is_empty)
        {
            return Err(ManifestError::InvalidTorrentHash(
                "a torrent reference needs a v1 or v2 info hash".into(),
            ));
        }
        let mut names = Vec::with_capacity(self.pending.len() + self.entries.len());
        for file in &self.pending {
            let size = fs::metadata(&file.local)?.len();
            if size < MIN_ENCRYPTABLE_BYTES as u64 {
                return Err(ManifestError::Build(format!(
                    "{} is {size} bytes; files need at least {MIN_ENCRYPTABLE_BYTES} bytes to upload",
                    file.local.display()
                )));
            }
            names.push(file.path.clone());
        }
        for entry in &self.entries {
            names.push(entry.effective_name()?);
        }
        check_collisions(names.iter().map(String::as_str)).map_err(ManifestError::Conflict)
    }

    /// Upload every queued file and assemble the manifest.
    ///
    /// Validation runs first so no upload is paid for a manifest that
    /// could not be finished. Cancellation stops before the next file, or
    /// abandons the upload in flight, and returns the partial result with
    /// `cancelled` set so already-paid uploads are not lost. A file failure
    /// returns [`ManifestError::BuildFailed`] with the same partial result;
    /// callers should persist it before reporting the error.
    pub async fn finish(
        mut self,
        progress: Option<mpsc::Sender<BuildEvent>>,
    ) -> Result<BuildResult, ManifestError> {
        self.preflight()?;
        let total = self.pending.len();
        let mut files_uploaded = 0;
        let mut chunks_stored = 0;
        let mut storage_cost_atto: u128 = 0;
        let mut gas_cost_wei: u128 = 0;
        let mut cancelled = false;
        let mut failure = None;

        let pending = std::mem::take(&mut self.pending);
        for (index, file) in pending.into_iter().enumerate() {
            if self.options.cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            let size = match fs::metadata(&file.local) {
                Ok(metadata) => metadata.len(),
                Err(error) => {
                    failure = Some((file.path, ManifestError::Io(error)));
                    break;
                }
            };
            if let Some(tx) = &progress {
                let _ = tx
                    .send(BuildEvent::FileStarted {
                        path: file.path.clone(),
                        index,
                        total,
                    })
                    .await;
            }

            let upload_progress = progress.as_ref().map(|tx| {
                let (upload_tx, mut upload_rx) = mpsc::channel(UPLOAD_PROGRESS_CAPACITY);
                let tx = tx.clone();
                let path = file.path.clone();
                tokio::spawn(async move {
                    while let Some(event) = upload_rx.recv().await {
                        let _ = tx
                            .send(BuildEvent::Upload {
                                path: path.clone(),
                                event,
                            })
                            .await;
                    }
                });
                upload_tx
            });

            let upload = self.client.file_upload_with_visibility_and_progress(
                &file.local,
                self.options.payment_mode,
                self.options.visibility,
                upload_progress,
            );
            let result = tokio::select! {
                _ = self.options.cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                result = upload => match result {
                    Ok(result) => result,
                    Err(error) => {
                        failure = Some((file.path, ManifestError::Data(error)));
                        break;
                    }
                },
            };

            // Keep the upload's usable DataMap before any fallible cost
            // parsing or reference optimisation. A later error must not
            // discard the file that has just been paid for and uploaded.
            self.entries.push(ManifestEntry {
                path: Some(file.path.clone()),
                size: Some(size),
                source: ContentRef::Embedded {
                    data_map: result.data_map.clone(),
                },
            });
            files_uploaded += 1;
            chunks_stored += result.chunks_stored;
            gas_cost_wei = gas_cost_wei.saturating_add(result.gas_cost_wei);
            match parse_atto(&result.storage_cost_atto) {
                Ok(cost) => storage_cost_atto = storage_cost_atto.saturating_add(cost),
                Err(error) => {
                    failure = Some((file.path, error));
                    break;
                }
            }

            let source = match self
                .reference_for(&result.data_map, result.data_map_address)
                .await
            {
                Ok(source) => source,
                Err(error) => {
                    failure = Some((file.path, error));
                    break;
                }
            };
            let reference = source.kind();
            // The entry just appended is always present at this point.
            if let Some(entry) = self.entries.last_mut() {
                entry.source = source;
            }

            if let Some(tx) = &progress {
                let _ = tx
                    .send(BuildEvent::FileFinished {
                        path: file.path,
                        chunks_stored: result.chunks_stored,
                        reference,
                    })
                    .await;
            }
        }

        let mut manifest = Manifest {
            name: self.options.name.clone(),
            torrent: self.options.torrent.clone(),
            entries: std::mem::take(&mut self.entries),
        };
        manifest.canonicalize()?;

        let result = BuildResult {
            manifest,
            cancelled,
            skipped_symlinks: self.skipped_symlinks,
            files_uploaded,
            total_to_upload: total,
            chunks_stored,
            storage_cost_atto,
            gas_cost_wei,
        };
        match failure {
            Some((path, cause)) => Err(ManifestError::BuildFailed {
                path,
                cause: Box::new(cause),
                partial: Box::new(result),
            }),
            None => Ok(result),
        }
    }

    /// Embedded mode always embeds a DataMap, even when the upload was
    /// public, and embeds the root map when it is small enough so readers
    /// skip the wrapper-record fetches. Compact mode records the public
    /// address when the upload stored the DataMap chunk, or when that chunk
    /// already exists on the network, and embeds otherwise.
    async fn reference_for(
        &self,
        data_map: &self_encryption::DataMap,
        public_address: Option<[u8; ADDRESS_LEN]>,
    ) -> Result<ContentRef, ManifestError> {
        if self.options.reference_mode != ReferenceMode::Compact {
            // The root and the shrunk map share one content address, so
            // embedding the root does not change the entry's identity.
            return Ok(ContentRef::Embedded {
                data_map: embeddable_data_map(self.client, data_map).await?,
            });
        }
        if let Some(address) = public_address {
            return Ok(ContentRef::Public { address });
        }
        let address = data_map_address(data_map)?;
        if self.client.chunk_exists(&address).await? {
            return Ok(ContentRef::Public { address });
        }
        Ok(ContentRef::Embedded {
            data_map: embeddable_data_map(self.client, data_map).await?,
        })
    }

    fn walk(
        &mut self,
        dir: &Path,
        prefix: &str,
        out: &mut Vec<PendingFile>,
    ) -> Result<(), ManifestError> {
        let mut children: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let local = child.path();
            let name = file_name_utf8(&local)?;
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}{PATH_SEPARATOR}{name}")
            };
            let meta = fs::symlink_metadata(&local)?;
            if meta.file_type().is_symlink() {
                if self.options.follow_symlinks && fs::metadata(&local)?.is_file() {
                    out.push(PendingFile { local, path });
                } else {
                    self.skipped_symlinks.push(local);
                }
            } else if meta.is_dir() {
                self.walk(&local, &path, out)?;
            } else if meta.is_file() {
                out.push(PendingFile { local, path });
            }
        }
        Ok(())
    }
}

fn file_name_utf8(path: &Path) -> Result<String, ManifestError> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| ManifestError::Build(format!("{} has no UTF-8 file name", path.display())))
}

fn parse_atto(value: &str) -> Result<u128, ManifestError> {
    value
        .parse::<u128>()
        .map_err(|e| ManifestError::Build(format!("storage cost {value:?} is not a number: {e}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn file_name_requires_utf8_and_a_name() {
        assert_eq!(file_name_utf8(Path::new("a/b.txt")).unwrap(), "b.txt");
        assert!(file_name_utf8(Path::new("/")).is_err());
    }

    #[test]
    fn atto_parsing() {
        assert_eq!(parse_atto("0").unwrap(), 0);
        assert_eq!(
            parse_atto("123456789012345678901").unwrap(),
            123456789012345678901
        );
        assert!(parse_atto("x").is_err());
    }
}
