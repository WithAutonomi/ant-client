//! Windows containment: each directory from the output root down is held
//! open without delete sharing, so none of them can be renamed or deleted
//! while an entry is written. Directories beneath the root are opened as the
//! reparse point itself, so a junction or symlink is seen rather than
//! followed. Operations go by path, so a concurrent writer in the output
//! root that turns an empty directory into a junction is not contained.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use tempfile::TempPath;
use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, WIN32_ERROR,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use super::{not_a_directory, not_followed, EntryKind};
use crate::data::manifest::ManifestError;

/// Only open handles keep a directory from being renamed, so every
/// directory on the chain is held.
pub(super) const HOLD_ANCESTORS: bool = true;

/// Sharing for a held directory: others may read and write in it, but not
/// open it for deletion, which renaming it needs as well.
const HOLD_SHARE_MODE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
/// Errors from a flush on a volume that cannot do one: drivers without a
/// flush handler report the first two.
const NO_FLUSH: &[WIN32_ERROR] = &[
    ERROR_INVALID_FUNCTION,
    ERROR_NOT_SUPPORTED,
    ERROR_CALL_NOT_IMPLEMENTED,
];

/// A directory held open.
pub(super) struct Dir {
    /// Keeps the directory from being renamed, deleted or replaced.
    _held: File,
    /// The directory's full path, resolved when the root was opened.
    path: PathBuf,
    /// Where it was found, for messages.
    display: PathBuf,
}

impl Dir {
    /// Open the output root. A link in the root's own path is resolved once;
    /// holding the result keeps that resolution valid. A volume that cannot
    /// report final paths, such as a RAM disk, is used by its absolute path.
    pub(super) fn open_root(path: &Path) -> io::Result<Self> {
        let resolved = fs::canonicalize(path).or_else(|_| std::path::absolute(path))?;
        let held = hold(&resolved, FILE_FLAG_BACKUP_SEMANTICS)?;
        if !held.metadata()?.is_dir() {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(Self {
            _held: held,
            path: resolved,
            display: path.to_path_buf(),
        })
    }

    /// Open the directory `name` in this one, creating it when missing,
    /// without following a link.
    pub(super) fn open_child(&self, name: &str) -> Result<Self, ManifestError> {
        let path = self.path.join(name);
        let display = self.display.join(name);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let held = hold(
            &path,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        let metadata = held.metadata()?;
        if metadata.file_type().is_symlink() {
            return Err(not_followed(&display));
        }
        if !metadata.is_dir() {
            return Err(not_a_directory(&display));
        }
        Ok(Self {
            _held: held,
            path,
            display,
        })
    }

    /// What `name` is in this directory, without following a link, or
    /// `None` when nothing has that name.
    pub(super) fn entry(&self, name: &str) -> io::Result<Option<EntryKind>> {
        let path = self.path.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let file_type = metadata.file_type();
        Ok(Some(if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        }))
    }

    /// Create the file `name`, failing if anything has that name.
    pub(super) fn create_new(&self, name: &str) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(name))
    }

    /// Open the file `name` for reading, as the link itself if it is one.
    pub(super) fn open_file(&self, name: &str) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(name))
    }

    /// Rename `from` to `to` in this directory, failing if `to` exists.
    pub(super) fn rename_noreplace(&self, from: &str, to: &str) -> io::Result<()> {
        TempPath::try_from_path(self.path.join(from))?
            .persist_noclobber(self.path.join(to))
            .map_err(|error| {
                // The staged file removes its own temporary name.
                let _ = error.path.keep();
                error.error
            })
    }

    /// Rename `from` over `to` in this directory. A link at `to` is
    /// replaced, never followed.
    pub(super) fn rename_replace(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.path.join(from), self.path.join(to))
    }

    /// Remove the file `name` from this directory.
    pub(super) fn remove_file(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.path.join(name))
    }

    /// Where the directory was found, for messages.
    pub(super) fn display(&self) -> &Path {
        &self.display
    }
}

/// Open the directory at `path` and hold it against renaming and deletion.
fn hold(path: &Path, flags: u32) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(HOLD_SHARE_MODE)
        .custom_flags(flags)
        .open(path)
}

/// Whether a flush failed only because the volume cannot flush.
pub(super) fn flush_unsupported(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .and_then(|code| WIN32_ERROR::try_from(code).ok())
        .is_some_and(|code| NO_FLUSH.contains(&code))
}
