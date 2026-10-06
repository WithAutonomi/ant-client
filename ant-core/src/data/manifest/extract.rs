//! Extracting a manifest into an output directory.
//!
//! Containment is a filesystem contract, not a string check. Inside the
//! output root the extractor never follows a symlink, junction or reparse
//! point: every intermediate directory is created, or confirmed to be a
//! real directory, without following links, before the download, and the
//! whole parent chain and the target are checked again, without following
//! links, immediately before the final rename. The file is written through
//! a reserved temporary name in its parent. An existing target is a
//! per-entry failure unless overwrite was requested, and even then only a
//! regular file is replaced.

use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use futures::stream::{self, StreamExt};
use self_encryption::DataMap;
use serde::{Deserialize, Serialize};
use tempfile::Builder as TempBuilder;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::data::client::file::DownloadEvent;
use crate::data::Client;

use super::path::PATH_SEPARATOR;
use super::{ContentRef, Manifest, ManifestEntry, ManifestError};

#[cfg(all(test, unix))]
use std::os::unix::fs::symlink;

/// Prefix of the temporary file an entry is downloaded into.
const TEMP_PREFIX: &str = ".ant-extract-";
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
    fs::create_dir_all(&options.output_root)?;
    let total = selected.len();

    let outcomes: Vec<(usize, EntryOutcome)> = stream::iter(selected.into_iter().enumerate())
        .map(|(index, (name, entry))| {
            let progress = progress.clone();
            async move {
                let status = extract_entry(
                    client,
                    entry,
                    &name,
                    index,
                    total,
                    options,
                    progress.as_ref(),
                )
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

async fn extract_entry(
    client: &Client,
    entry: &ManifestEntry,
    name: &str,
    index: usize,
    total: usize,
    options: &ExtractOptions,
    progress: Option<&mpsc::Sender<ExtractEvent>>,
) -> EntryStatus {
    if options.cancel.is_cancelled() {
        return EntryStatus::Cancelled;
    }
    if let Some(tx) = progress {
        let _ = tx
            .send(ExtractEvent::EntryStarted {
                name: name.to_string(),
                index,
                total,
            })
            .await;
    }
    let work = download_entry(client, entry, name, options, progress);
    tokio::select! {
        _ = options.cancel.cancelled() => EntryStatus::Cancelled,
        result = work => match result {
            Ok(bytes) => EntryStatus::Written { bytes },
            Err(e) => EntryStatus::Failed { error: e.to_string() },
        },
    }
}

async fn download_entry(
    client: &Client,
    entry: &ManifestEntry,
    name: &str,
    options: &ExtractOptions,
    progress: Option<&mpsc::Sender<ExtractEvent>>,
) -> Result<u64, ManifestError> {
    let target = prepare_target(&options.output_root, name, options.overwrite)?;
    let parent = target
        .parent()
        .ok_or_else(|| ManifestError::Build(format!("{} has no parent", target.display())))?;

    let data_map = resolve_data_map(client, &entry.source).await?;

    let temp = TempBuilder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(parent)?
        .into_temp_path();

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

    let bytes = client
        .file_download_with_progress(&data_map, &temp, download_progress)
        .await?;

    // Re-check the parent chain and the target immediately before the
    // rename; either may have changed while the download ran.
    verify_parent_chain(&options.output_root, name)?;
    check_target(&target, options.overwrite)?;
    let persisted = if options.overwrite {
        temp.persist(&target)
    } else {
        temp.persist_noclobber(&target)
    };
    persisted.map_err(|e| ManifestError::Io(e.error))?;
    Ok(bytes)
}

async fn resolve_data_map(client: &Client, source: &ContentRef) -> Result<DataMap, ManifestError> {
    match source {
        ContentRef::Embedded { data_map } => Ok(data_map.clone()),
        ContentRef::Public { address } => Ok(client.data_map_fetch(address).await?),
    }
}

/// Resolve `name` beneath `root`, creating intermediate directories, with
/// the containment rules above. Returns the target path, which does not
/// exist unless `overwrite` is set and it is a regular file.
pub fn prepare_target(root: &Path, name: &str, overwrite: bool) -> Result<PathBuf, ManifestError> {
    let mut components = name.split(PATH_SEPARATOR).peekable();
    let mut current = root.to_path_buf();
    while let Some(component) = components.next() {
        current.push(component);
        if components.peek().is_none() {
            check_target(&current, overwrite)?;
            return Ok(current);
        }
        ensure_real_directory(&current)?;
    }
    Err(ManifestError::Build("empty effective name".to_string()))
}

/// Confirm, without creating anything or following links, that every
/// directory component of `name` beneath `root` is still a real directory.
fn verify_parent_chain(root: &Path, name: &str) -> Result<(), ManifestError> {
    let mut components = name.split(PATH_SEPARATOR).peekable();
    let mut current = root.to_path_buf();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok(());
        }
        current.push(component);
        let meta = fs::symlink_metadata(&current)?;
        verify_directory(&current, &meta)?;
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> Result<(), ManifestError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => verify_directory(path, &meta),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            let meta = fs::symlink_metadata(path)?;
            verify_directory(path, &meta)
        }
        Err(e) => Err(e.into()),
    }
}

fn verify_directory(path: &Path, meta: &fs::Metadata) -> Result<(), ManifestError> {
    if meta.file_type().is_symlink() {
        return Err(ManifestError::Build(format!(
            "{} is a symlink; extraction does not follow links inside the output directory",
            path.display()
        )));
    }
    if !meta.is_dir() {
        return Err(ManifestError::Build(format!(
            "{} exists and is not a directory",
            path.display()
        )));
    }
    Ok(())
}

fn check_target(target: &Path, overwrite: bool) -> Result<(), ManifestError> {
    match fs::symlink_metadata(target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(ManifestError::Build(format!(
                    "{} is a symlink and will not be replaced",
                    target.display()
                )));
            }
            if !overwrite {
                return Err(ManifestError::Build(format!(
                    "{} already exists; pass overwrite to replace it",
                    target.display()
                )));
            }
            if !meta.is_file() {
                return Err(ManifestError::Build(format!(
                    "{} exists and is not a regular file",
                    target.display()
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn entry(path: &str) -> ManifestEntry {
        ManifestEntry {
            path: Some(path.into()),
            size: None,
            source: ContentRef::Public { address: [1; 32] },
        }
    }

    #[test]
    fn selection_matches_exact_and_prefix() {
        let manifest = Manifest {
            name: None,
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

    #[test]
    fn prepare_target_creates_directories_and_refuses_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let target = prepare_target(root.path(), "x/y/z.txt", false).unwrap();
        assert_eq!(target, root.path().join("x").join("y").join("z.txt"));
        assert!(root.path().join("x/y").is_dir());
        fs::write(&target, b"old").unwrap();
        assert!(prepare_target(root.path(), "x/y/z.txt", false).is_err());
        assert!(prepare_target(root.path(), "x/y/z.txt", true).is_ok());
        // A directory where a file should go is never replaced.
        assert!(prepare_target(root.path(), "x/y", true).is_err());
        // A file where a directory should go.
        assert!(prepare_target(root.path(), "x/y/z.txt/deeper", false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_target_never_follows_symlinks_inside_root() {
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();

        // Pre-existing directory symlink at the first level.
        symlink(outside.path(), root.path().join("safe")).unwrap();
        let err = prepare_target(root.path(), "safe/file.txt", false).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!outside.path().join("file.txt").exists());

        // Deeper: a real directory containing a symlinked directory.
        fs::create_dir(root.path().join("real")).unwrap();
        symlink(outside.path(), root.path().join("real/link")).unwrap();
        assert!(prepare_target(root.path(), "real/link/f", false).is_err());

        // A symlink at the target itself, with and without overwrite.
        symlink(outside.path().join("victim"), root.path().join("t.txt")).unwrap();
        assert!(prepare_target(root.path(), "t.txt", false).is_err());
        assert!(prepare_target(root.path(), "t.txt", true).is_err());
        assert!(!outside.path().join("victim").exists());

        // A directory swapped for a symlink after preparation is caught by
        // the pre-rename check.
        prepare_target(root.path(), "late/file", false).unwrap();
        fs::remove_dir(root.path().join("late")).unwrap();
        symlink(outside.path(), root.path().join("late")).unwrap();
        assert!(verify_parent_chain(root.path(), "late/file").is_err());
        assert!(verify_parent_chain(root.path(), "real/f").is_ok());
    }
}
