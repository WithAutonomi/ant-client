//! Extracting a manifest into an output directory.
//!
//! Containment is a filesystem contract, not a string check. Inside the
//! output root the extractor never follows a symlink, junction or reparse
//! point, and every name is resolved from directories held open, so
//! nothing that changes while an entry downloads can redirect where it is
//! written (see the `output` module). The file is written through a
//! temporary name in its parent and renamed into place without replacing
//! anything. A regular file already at a target with the entry's recorded
//! SHA-256 is kept as that entry, so an extraction that stopped part way can
//! be run again. Any other existing target is a per-entry failure unless
//! overwrite was requested, and even then only a regular file is replaced.

use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use futures::stream::{self, StreamExt};
use self_encryption::DataMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::data::client::file::DownloadEvent;
use crate::data::error::Error;
use crate::data::Client;

use super::output::OutputRoot;
use super::path::PATH_SEPARATOR;
use super::{ContentRef, Manifest, ManifestEntry, ManifestError, SHA256_LEN};

/// Capacity of the per-entry download progress channel.
const DOWNLOAD_PROGRESS_CAPACITY: usize = 64;

/// Options for [`extract_manifest`].
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    /// Directory everything is written under. Created if missing.
    pub output_root: PathBuf,
    /// Effective names or directory prefixes to extract. Empty means all.
    pub selection: Vec<String>,
    /// Replace an existing regular file at a target.
    pub overwrite: bool,
    /// Entries downloaded at once.
    pub concurrency: NonZeroUsize,
    /// Cancels the operation. Entries not yet started report `Cancelled`.
    pub cancel: CancellationToken,
}

/// Progress during extraction.
#[derive(Debug, Clone)]
pub enum ExtractEvent {
    /// An entry is starting.
    EntryStarted {
        /// Effective name.
        name: String,
        /// Zero-based index among selected entries.
        index: usize,
        /// Number of selected entries.
        total: usize,
    },
    /// Progress from the underlying file download.
    Download {
        /// Effective name.
        name: String,
        /// The download event.
        event: DownloadEvent,
    },
    /// An entry finished.
    EntryFinished {
        /// Effective name.
        name: String,
        /// How it ended.
        status: EntryStatus,
    },
}

/// How one entry ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EntryStatus {
    /// Written to its target.
    Written {
        /// Bytes written.
        bytes: u64,
    },
    /// Already at its target with the entry's recorded SHA-256, as an
    /// earlier run left it. Nothing was downloaded or written.
    Present {
        /// Bytes of the file.
        bytes: u64,
    },
    /// Failed; nothing was left at the target.
    Failed {
        /// The error.
        error: String,
    },
    /// Not started, or abandoned, because the operation was cancelled.
    Cancelled,
}

/// Outcome of one entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryOutcome {
    /// Effective name.
    pub name: String,
    /// How it ended.
    pub status: EntryStatus,
}

/// Outcome of an extraction, one row per selected entry in manifest order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractReport {
    /// Per-entry outcomes.
    pub entries: Vec<EntryOutcome>,
}

impl ExtractReport {
    /// Entries written.
    pub fn written(&self) -> usize {
        self.count(|s| matches!(s, EntryStatus::Written { .. }))
    }

    /// Entries already present.
    pub fn present(&self) -> usize {
        self.count(|s| matches!(s, EntryStatus::Present { .. }))
    }

    /// Entries that failed.
    pub fn failed(&self) -> usize {
        self.count(|s| matches!(s, EntryStatus::Failed { .. }))
    }

    /// Entries cancelled.
    pub fn cancelled(&self) -> usize {
        self.count(|s| matches!(s, EntryStatus::Cancelled))
    }

    fn count(&self, pred: impl Fn(&EntryStatus) -> bool) -> usize {
        self.entries.iter().filter(|e| pred(&e.status)).count()
    }
}

/// The selected entries with their effective names, in manifest order.
///
/// A selector matches an entry whose effective name equals it or lies
/// beneath it as a directory. A selector matching nothing is an error.
pub fn select_entries<'a>(
    manifest: &'a Manifest,
    selection: &[String],
) -> Result<Vec<(String, &'a ManifestEntry)>, ManifestError> {
    let named = manifest
        .entries
        .iter()
        .map(|entry| entry.effective_name().map(|name| (name, entry)))
        .collect::<Result<Vec<_>, _>>()?;
    if selection.is_empty() {
        return Ok(named);
    }
    for selector in selection {
        if !named
            .iter()
            .any(|(name, _)| selector_matches(selector, name))
        {
            return Err(ManifestError::Build(format!(
                "selection {selector:?} matches no entry"
            )));
        }
    }
    Ok(named
        .into_iter()
        .filter(|(name, _)| selection.iter().any(|s| selector_matches(s, name)))
        .collect())
}

fn selector_matches(selector: &str, name: &str) -> bool {
    let selector = selector.trim_end_matches(PATH_SEPARATOR);
    name == selector
        || name
            .strip_prefix(selector)
            .is_some_and(|rest| rest.starts_with(PATH_SEPARATOR))
}

/// Download the selected entries of `manifest` under the output root.
pub async fn extract_manifest(
    client: &Client,
    manifest: &Manifest,
    options: &ExtractOptions,
    progress: Option<mpsc::Sender<ExtractEvent>>,
) -> Result<ExtractReport, ManifestError> {
    manifest.validate()?;
    let selected = select_entries(manifest, &options.selection)?;
    let root = OutputRoot::open(&options.output_root)?;
    let extraction = Extraction {
        client,
        root: &root,
        options,
        total: selected.len(),
    };
    let extraction = &extraction;

    let outcomes: Vec<(usize, EntryOutcome)> = stream::iter(selected.into_iter().enumerate())
        .map(|(index, (name, entry))| {
            let progress = progress.clone();
            async move {
                let status = extraction
                    .extract_entry(entry, &name, index, progress.as_ref())
                    .await;
                if let Some(tx) = &progress {
                    let _ = tx
                        .send(ExtractEvent::EntryFinished {
                            name: name.clone(),
                            status: status.clone(),
                        })
                        .await;
                }
                (index, EntryOutcome { name, status })
            }
        })
        .buffer_unordered(options.concurrency.get())
        .collect()
        .await;

    let mut ordered = outcomes;
    ordered.sort_by_key(|(index, _)| *index);
    Ok(ExtractReport {
        entries: ordered.into_iter().map(|(_, outcome)| outcome).collect(),
    })
}

/// What every entry of one extraction shares.
struct Extraction<'a> {
    client: &'a Client,
    root: &'a OutputRoot,
    options: &'a ExtractOptions,
    /// Number of selected entries.
    total: usize,
}

impl Extraction<'_> {
    async fn extract_entry(
        &self,
        entry: &ManifestEntry,
        name: &str,
        index: usize,
        progress: Option<&mpsc::Sender<ExtractEvent>>,
    ) -> EntryStatus {
        if self.options.cancel.is_cancelled() {
            return EntryStatus::Cancelled;
        }
        if let Some(tx) = progress {
            let _ = tx
                .send(ExtractEvent::EntryStarted {
                    name: name.to_string(),
                    index,
                    total: self.total,
                })
                .await;
        }
        let work = self.download_entry(entry, name, progress);
        tokio::select! {
            _ = self.options.cancel.cancelled() => EntryStatus::Cancelled,
            result = work => match result {
                Ok(status) => status,
                Err(e) => EntryStatus::Failed { error: e.to_string() },
            },
        }
    }

    async fn download_entry(
        &self,
        entry: &ManifestEntry,
        name: &str,
        progress: Option<&mpsc::Sender<ExtractEvent>>,
    ) -> Result<EntryStatus, ManifestError> {
        let overwrite = self.options.overwrite;
        let target = self.root.target(name)?;
        // Hashing a large file blocks, so keep it off the async workers.
        // The size, exact or as the creator recorded it, spares reading a
        // file that cannot be the entry.
        if let Some(expected) = &entry.sha256 {
            let size = entry.known_size().or(entry.size);
            if let Some(bytes) = tokio::task::block_in_place(|| target.identical(expected, size)) {
                return Ok(EntryStatus::Present { bytes });
            }
        }
        target.check(overwrite)?;

        // The root map lists every chunk, so its size is the file's real
        // size, unlike the creator's unverified `size` hint. The download
        // would resolve the root anyway.
        let client = self.client;
        let data_map = client
            .data_map_resolve_root(&resolve_data_map(client, &entry.source).await?)
            .await?;
        ensure_space(target.parent_path(), data_map.original_file_size())?;

        let download_progress = progress.map(|tx| {
            let (dl_tx, mut dl_rx) = mpsc::channel(DOWNLOAD_PROGRESS_CAPACITY);
            let tx = tx.clone();
            let name = name.to_string();
            tokio::spawn(async move {
                while let Some(event) = dl_rx.recv().await {
                    let _ = tx
                        .send(ExtractEvent::Download {
                            name: name.clone(),
                            event,
                        })
                        .await;
                }
            });
            dl_tx
        });

        // Dropping the staged file on any early return, including a
        // cancellation, removes it.
        let mut staged = target.stage()?;
        let mut writer = HashingWriter::new(staged.file()?, entry.sha256.is_some());
        let bytes = client
            .file_download_to_writer(&data_map, &mut writer, download_progress)
            .await?;

        // Check the creator's recorded SHA-256 before the file takes its
        // name, so nothing unverified is left at the target.
        if let (Some(expected), Some(actual)) = (entry.sha256, writer.finish()) {
            if actual != expected {
                return Err(ManifestError::Sha256Mismatch {
                    expected: hex::encode(expected),
                    actual: hex::encode(actual),
                });
            }
        }
        // The commit syncs the file to disk; keep that off the async workers.
        tokio::task::block_in_place(|| staged.commit(overwrite))?;
        Ok(EntryStatus::Written { bytes })
    }
}

/// Writes through to a file, hashing what it writes when asked to, so a
/// download is checked in the same pass that stores it.
struct HashingWriter<'a, W> {
    inner: &'a mut W,
    hasher: Option<Sha256>,
}

impl<'a, W: Write> HashingWriter<'a, W> {
    fn new(inner: &'a mut W, hash: bool) -> Self {
        Self {
            inner,
            hasher: hash.then(Sha256::new),
        }
    }

    /// The SHA-256 of everything written, when hashing was asked for.
    fn finish(self) -> Option<[u8; SHA256_LEN]> {
        self.hasher.map(|hasher| hasher.finalize().into())
    }
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        if let Some(hasher) = &mut self.hasher {
            hasher.update(&buf[..written]);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Fail the entry before downloading when `dir`'s filesystem cannot hold
/// `needed` bytes. When free space cannot be determined the download goes
/// ahead and any shortage surfaces as a write error.
fn ensure_space(dir: &Path, needed: usize) -> Result<(), ManifestError> {
    let Ok(available) = fs2::available_space(dir) else {
        return Ok(());
    };
    let needed = u64::try_from(needed).unwrap_or(u64::MAX);
    if available < needed {
        return Err(ManifestError::Data(Error::InsufficientDiskSpace(format!(
            "the file needs {needed} bytes but only {available} are free in {}",
            dir.display()
        ))));
    }
    Ok(())
}

async fn resolve_data_map(client: &Client, source: &ContentRef) -> Result<DataMap, ManifestError> {
    match source {
        ContentRef::Embedded { data_map } => Ok(data_map.clone()),
        ContentRef::Public { address } => Ok(client.data_map_fetch(address).await?),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn hashing_writer_matches_a_one_shot_digest_and_writes_everything() {
        let data: Vec<u8> = (0..100_003u32).map(|i| (i % 251) as u8).collect();
        let mut out = Vec::new();
        let mut writer = HashingWriter::new(&mut out, true);
        for piece in data.chunks(4096) {
            writer.write_all(piece).unwrap();
        }
        let expected: [u8; SHA256_LEN] = Sha256::digest(&data).into();
        assert_eq!(writer.finish(), Some(expected));
        assert_eq!(out, data);

        let mut unhashed = Vec::new();
        let mut writer = HashingWriter::new(&mut unhashed, false);
        writer.write_all(&data).unwrap();
        assert_eq!(writer.finish(), None);
    }

    fn entry(path: &str) -> ManifestEntry {
        ManifestEntry {
            path: Some(path.into()),
            size: None,
            sha256: None,
            source: ContentRef::Public { address: [1; 32] },
        }
    }

    #[test]
    fn ensure_space_refuses_what_cannot_fit() {
        let dir = tempfile::tempdir().unwrap();
        ensure_space(dir.path(), 0).unwrap();
        assert!(matches!(
            ensure_space(dir.path(), usize::MAX),
            Err(ManifestError::Data(Error::InsufficientDiskSpace(_)))
        ));
    }

    #[test]
    fn selection_matches_exact_and_prefix() {
        let manifest = Manifest {
            name: None,
            torrent: None,
            entries: vec![entry("a/b"), entry("a/c"), entry("ab"), entry("d")],
        };
        let all = select_entries(&manifest, &[]).unwrap();
        assert_eq!(all.len(), 4);
        let picked = select_entries(&manifest, &["a".into(), "d".into()]).unwrap();
        let names: Vec<_> = picked.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a/b", "a/c", "d"]);
        let trailing = select_entries(&manifest, &["a/".into()]).unwrap();
        assert_eq!(trailing.len(), 2);
        assert!(select_entries(&manifest, &["zzz".into()]).is_err());
    }
}
