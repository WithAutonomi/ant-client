//! Crash-safe progress of a manifest build, kept in the upload history.
//!
//! A build pays for its uploads one file at a time, so it must not lose a
//! paid upload when the process stops before the build ends: a crash, a
//! kill, a power cut or a second Ctrl-C. Before the first upload the builder
//! creates `<id>.ant.partial` in the history directory. As each upload
//! completes it appends the entry and flushes it to disk before the next
//! upload is paid for. When the build ends the journal becomes the ordinary
//! `<id>.ant` record and is removed. A journal left behind is an interrupted
//! upload: the history lists it, and every entry that reached the disk can
//! be shown, linked, exported and downloaded like any other record.
//!
//! The file is magic bytes, a version byte, and frames: each a little-endian
//! `u32` length, the CRC-32 of the payload, and the payload. The first
//! payload carries the manifest's name and torrent reference; every other
//! payload is one entry, msgpack-encoded as in a manifest. Reading stops at
//! the first frame that is incomplete or fails its check, which is where a
//! write was cut short. A later entry with the same effective name replaces
//! an earlier one, so an entry is recorded as soon as its upload completes
//! and again once its reference is final.
//!
//! The builder holds an exclusive lock on the journal while it runs, so a
//! listing can tell an upload in progress from an interrupted one.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::file::write_manifest_file;
use super::history::{new_ids, no_free_id, record_path, record_upload, RecordState, UploadRecord};
use super::{Manifest, ManifestEntry, ManifestError, TorrentReference, MAX_MANIFEST_BYTES};

#[cfg(test)]
use super::history::{list_uploads, load_upload};
#[cfg(test)]
use super::ContentRef;

/// Suffix of a journal's file name after its record id.
pub(super) const JOURNAL_SUFFIX: &str = ".ant.partial";
/// Magic bytes that open a journal.
const JOURNAL_MAGIC: [u8; 4] = [0xC1, b'A', b'N', b'J'];
/// The journal layout this client writes and reads.
const JOURNAL_FORMAT_VERSION: u8 = 1;
/// Bytes before the first frame: the magic and the version.
const JOURNAL_HEADER_LEN: usize = JOURNAL_MAGIC.len() + 1;
/// Bytes of a frame's length field, and of its checksum field.
const FRAME_FIELD_LEN: usize = std::mem::size_of::<u32>();
/// Largest journal read. Every entry is recorded at most twice, and each
/// frame's header is small next to an entry, so a journal of any manifest
/// within the limit stays under three times that limit.
const MAX_JOURNAL_BYTES: u64 = 3 * MAX_MANIFEST_BYTES as u64;

/// The first frame: what a manifest carries besides its entries.
#[derive(Serialize, Deserialize)]
struct JournalHeader {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    torrent: Option<TorrentReference>,
}

/// A running build's journal, held open and locked until the build ends.
pub(super) struct UploadJournal {
    dir: PathBuf,
    id: String,
    path: PathBuf,
    recorded_at: u64,
    label: String,
    file: File,
}

impl UploadJournal {
    /// Start a journal in the history directory `dir` for a manifest with
    /// this name and torrent reference.
    pub(super) fn create(
        dir: &Path,
        name: Option<&str>,
        torrent: Option<&TorrentReference>,
    ) -> Result<Self, ManifestError> {
        fs::create_dir_all(dir)?;
        let mut ids = new_ids(name);
        while let Some(id) = ids.next() {
            if record_path(dir, &id).exists() {
                continue;
            }
            let path = journal_path(dir, &id);
            let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            };
            let mut journal = Self {
                dir: dir.to_path_buf(),
                id,
                path,
                recorded_at: ids.recorded_at,
                label: ids.label.clone(),
                file,
            };
            let header = JournalHeader {
                name: name.map(str::to_owned),
                torrent: torrent.cloned(),
            };
            return match journal.start(&header) {
                Ok(()) => Ok(journal),
                Err(error) => {
                    let _ = journal.discard();
                    Err(error)
                }
            };
        }
        Err(no_free_id(dir))
    }

    /// The record id the build will be recorded under.
    pub(super) fn id(&self) -> &str {
        &self.id
    }

    /// Record `entry` and flush it to disk before returning.
    pub(super) fn append(&mut self, entry: &ManifestEntry) -> Result<(), ManifestError> {
        self.append_frame(entry)
    }

    /// Write `manifest` as the build's record and remove the journal.
    pub(super) fn finish(self, manifest: &Manifest) -> Result<UploadRecord, ManifestError> {
        let path = record_path(&self.dir, &self.id);
        let record = match write_manifest_file(&path, manifest, false) {
            Ok(()) => UploadRecord {
                id: self.id.clone(),
                path,
                recorded_at: self.recorded_at,
                label: self.label.clone(),
                manifest: manifest.clone(),
                state: RecordState::Complete,
            },
            // Another writer took the id after this journal did.
            Err(ManifestError::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                record_upload(&self.dir, manifest, Some(&self.label))?
            }
            Err(e) => return Err(e),
        };
        // The record is written; a journal left beside a record of the same
        // id is ignored by listings.
        let _ = self.discard();
        Ok(record)
    }

    /// Remove the journal of a build that recorded nothing worth keeping.
    pub(super) fn discard(self) -> Result<(), ManifestError> {
        let Self { file, path, .. } = self;
        // Close, and so unlock, before removing.
        drop(file);
        fs::remove_file(path).map_err(Into::into)
    }

    /// Lock the new file, write the opening and the header frame, and make
    /// the journal's name durable in its directory.
    fn start(&mut self, header: &JournalHeader) -> Result<(), ManifestError> {
        self.file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => {
                ManifestError::Build(format!("{} is locked", self.path.display()))
            }
            TryLockError::Error(error) => error.into(),
        })?;
        let mut opening = Vec::with_capacity(JOURNAL_HEADER_LEN);
        opening.extend_from_slice(&JOURNAL_MAGIC);
        opening.push(JOURNAL_FORMAT_VERSION);
        self.file.write_all(&opening)?;
        self.append_frame(header)?;
        sync_directory(&self.dir)
    }

    fn append_frame<T: Serialize>(&mut self, value: &T) -> Result<(), ManifestError> {
        let payload =
            rmp_serde::to_vec_named(value).map_err(|e| ManifestError::Encode(e.to_string()))?;
        let len = u32::try_from(payload.len())
            .map_err(|_| ManifestError::Encode("journal entry is too large".to_string()))?;
        let mut frame = Vec::with_capacity(2 * FRAME_FIELD_LEN + payload.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
        frame.extend_from_slice(&payload);
        self.file.write_all(&frame)?;
        self.file.sync_data()?;
        Ok(())
    }
}

/// Where the journal of the record `id` lives in `dir`.
pub(super) fn journal_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}{JOURNAL_SUFFIX}"))
}

/// The manifest a journal holds, and whether its build is still running.
/// A running build on Windows keeps its journal from being read, so only
/// that it is running is known then.
pub(super) fn read_journal(path: &Path) -> Result<(Manifest, bool), ManifestError> {
    let file = File::open(path)?;
    let in_progress = match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(error)) => return Err(error.into()),
    };
    let len = file.metadata()?.len();
    if len > MAX_JOURNAL_BYTES {
        return Err(ManifestError::TooLarge {
            len: usize::try_from(len).unwrap_or(usize::MAX),
            max: usize::try_from(MAX_JOURNAL_BYTES).unwrap_or(usize::MAX),
        });
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) if in_progress => return Ok((Manifest::default(), true)),
        Err(error) => return Err(error.into()),
    };
    Ok((parse_journal(&bytes)?, in_progress))
}

/// The manifest in a journal's bytes, up to the first frame that is cut
/// short or damaged.
fn parse_journal(bytes: &[u8]) -> Result<Manifest, ManifestError> {
    let (opening, frames) = bytes
        .split_at_checked(JOURNAL_HEADER_LEN)
        .ok_or_else(|| ManifestError::Decode("not an upload journal".to_string()))?;
    if opening[..JOURNAL_MAGIC.len()] != JOURNAL_MAGIC {
        return Err(ManifestError::Decode("not an upload journal".to_string()));
    }
    let version = opening[JOURNAL_MAGIC.len()];
    if version != JOURNAL_FORMAT_VERSION {
        return Err(ManifestError::UnsupportedVersion(version));
    }
    let mut frames = Frames { rest: frames };
    // A journal cut short before its header holds no entries either.
    let header = match frames.next() {
        Some(payload) => rmp_serde::from_slice(payload)
            .map_err(|e| ManifestError::Decode(format!("journal header: {e}")))?,
        None => JournalHeader {
            name: None,
            torrent: None,
        },
    };
    let mut entries: Vec<ManifestEntry> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    for payload in frames {
        // An entry that does not decode was damaged, so nothing after it is
        // trusted either.
        let Ok(entry) = rmp_serde::from_slice::<ManifestEntry>(payload) else {
            break;
        };
        let name = entry.effective_name()?;
        match positions.get(&name) {
            Some(&position) => entries[position] = entry,
            None => {
                positions.insert(name, entries.len());
                entries.push(entry);
            }
        }
    }
    let manifest = Manifest {
        name: header.name,
        torrent: header.torrent,
        entries,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// The payloads of whole, checked frames, ending at the first that is not.
struct Frames<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Frames<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let frame = self.split_frame();
        if frame.is_none() {
            self.rest = &[];
        }
        frame
    }
}

impl<'a> Frames<'a> {
    fn split_frame(&mut self) -> Option<&'a [u8]> {
        let (len, rest) = self.rest.split_first_chunk::<FRAME_FIELD_LEN>()?;
        let (checksum, rest) = rest.split_first_chunk::<FRAME_FIELD_LEN>()?;
        let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
        let (payload, rest) = rest.split_at_checked(len)?;
        if crc32fast::hash(payload) != u32::from_le_bytes(*checksum) {
            return None;
        }
        self.rest = rest;
        Some(payload)
    }
}

/// Make a newly created name in `dir` survive a power cut. Windows keeps
/// directory entries durable without being asked.
fn sync_directory(dir: &Path) -> Result<(), ManifestError> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn entry(path: &str, seed: u8) -> ManifestEntry {
        ManifestEntry {
            path: Some(path.into()),
            size: Some(u64::from(seed)),
            sha256: None,
            source: ContentRef::Public {
                address: [seed; 32],
            },
        }
    }

    #[test]
    fn a_journal_left_behind_is_an_interrupted_record_of_what_reached_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = UploadJournal::create(dir.path(), Some("photos"), None).unwrap();
        let id = journal.id().to_string();
        journal.append(&entry("a.bin", 1)).unwrap();
        journal.append(&entry("b.bin", 2)).unwrap();
        // The same name again: the final reference replaces the first.
        journal.append(&entry("a.bin", 3)).unwrap();

        // While the build runs, the record is in progress.
        let running = load_upload(dir.path(), &id).unwrap().unwrap();
        assert_eq!(running.state, RecordState::InProgress);

        // The process stops without finishing: the lock is released.
        drop(journal);
        let listing = list_uploads(dir.path()).unwrap();
        assert_eq!(listing.records.len(), 1, "{:?}", listing.unreadable);
        let record = &listing.records[0];
        assert_eq!(record.id, id);
        assert_eq!(record.state, RecordState::Interrupted);
        assert_eq!(record.manifest.name.as_deref(), Some("photos"));
        assert_eq!(
            record.manifest.entries,
            vec![entry("a.bin", 3), entry("b.bin", 2)]
        );
    }

    #[test]
    fn a_write_cut_short_keeps_every_entry_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = UploadJournal::create(dir.path(), None, None).unwrap();
        let id = journal.id().to_string();
        journal.append(&entry("a.bin", 1)).unwrap();
        journal.append(&entry("b.bin", 2)).unwrap();
        drop(journal);
        let path = journal_path(dir.path(), &id);
        let full = fs::read(&path).unwrap();

        // Torn in the middle of the last frame.
        fs::write(&path, &full[..full.len() - 3]).unwrap();
        let (manifest, in_progress) = read_journal(&path).unwrap();
        assert!(!in_progress);
        assert_eq!(manifest.entries, vec![entry("a.bin", 1)]);

        // Garbage where the last frame was: its checksum fails.
        let mut damaged = full.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xff;
        fs::write(&path, &damaged).unwrap();
        assert_eq!(
            read_journal(&path).unwrap().0.entries,
            vec![entry("a.bin", 1)]
        );

        // Cut before the header: an interrupted record with nothing in it.
        fs::write(&path, &full[..JOURNAL_HEADER_LEN + 2]).unwrap();
        assert!(read_journal(&path).unwrap().0.entries.is_empty());

        fs::write(&path, b"something else").unwrap();
        assert!(read_journal(&path).is_err());
    }

    #[test]
    fn finishing_writes_the_record_and_removes_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = UploadJournal::create(dir.path(), Some("set"), None).unwrap();
        let id = journal.id().to_string();
        journal.append(&entry("a.bin", 1)).unwrap();
        let manifest = Manifest {
            name: Some("set".into()),
            torrent: None,
            entries: vec![entry("a.bin", 1)],
        };
        let record = journal.finish(&manifest).unwrap();
        assert_eq!(record.id, id);
        assert_eq!(record.state, RecordState::Complete);
        assert!(!journal_path(dir.path(), &id).exists());
        let loaded = load_upload(dir.path(), &id).unwrap().unwrap();
        assert_eq!(loaded.manifest, manifest);
        assert_eq!(loaded.state, RecordState::Complete);
    }

    #[test]
    fn journals_and_records_never_share_an_id() {
        let dir = tempfile::tempdir().unwrap();
        let journal = UploadJournal::create(dir.path(), Some("same"), None).unwrap();
        let manifest = Manifest {
            name: Some("same".into()),
            torrent: None,
            entries: vec![entry("a.bin", 1)],
        };
        // A record made in the same second with the same label skips the
        // id the running build holds.
        let record = record_upload(dir.path(), &manifest, None).unwrap();
        assert_ne!(record.id, journal.id());
        let other = UploadJournal::create(dir.path(), Some("same"), None).unwrap();
        assert_ne!(other.id(), journal.id());
        assert_ne!(other.id(), record.id);
    }

    #[test]
    fn a_leftover_journal_beside_its_record_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = UploadJournal::create(dir.path(), None, None).unwrap();
        let id = journal.id().to_string();
        journal.append(&entry("a.bin", 1)).unwrap();
        drop(journal);
        let manifest = Manifest {
            name: None,
            torrent: None,
            entries: vec![entry("a.bin", 1)],
        };
        write_manifest_file(&record_path(dir.path(), &id), &manifest, false).unwrap();
        let listing = list_uploads(dir.path()).unwrap();
        assert_eq!(listing.records.len(), 1);
        assert_eq!(listing.records[0].state, RecordState::Complete);
    }
}
