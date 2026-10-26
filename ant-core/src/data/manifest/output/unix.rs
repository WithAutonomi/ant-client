//! Unix containment: every operation is relative to a directory file
//! descriptor, and nothing beneath the root is opened through a link.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{
    linkat, mkdirat, open, openat, renameat, statat, unlinkat, AtFlags, FileType, Mode, OFlags,
    Stat,
};
use rustix::io::Errno;

use super::{not_a_directory, not_followed, Entry, EntryKind};
use crate::data::manifest::ManifestError;

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
use rustix::fs::{renameat_with, RenameFlags};

/// A descriptor pins its directory however paths to it change, so only the
/// parent needs holding.
pub(super) const HOLD_ANCESTORS: bool = false;

/// Errors from a no-replace rename on a filesystem that cannot do one.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
const NO_ATOMIC_NOREPLACE: &[Errno] =
    &[Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP];
/// Errors from a flush on a filesystem that cannot do one.
const NO_FLUSH: &[Errno] = &[Errno::NOTSUP, Errno::OPNOTSUPP];

/// Identity of a file: its device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct FileIdentity {
    device: u64,
    inode: u64,
}

/// A directory held open.
pub(super) struct Dir {
    fd: OwnedFd,
    /// Where it was found, for messages.
    display: PathBuf,
}

impl Dir {
    /// Open the output root, following a link if the root is one.
    pub(super) fn open_root(path: &Path) -> io::Result<Self> {
        let fd = open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            fd,
            display: path.to_path_buf(),
        })
    }

    /// Open the directory `name` in this one, creating it when missing,
    /// without following a link.
    pub(super) fn open_child(&self, name: &str) -> Result<Self, ManifestError> {
        let display = self.display.join(name);
        match mkdirat(&self.fd, name, new_directory_mode()) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => return Err(io::Error::from(error).into()),
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match openat(&self.fd, name, flags, Mode::empty()) {
            Ok(fd) => Ok(Self { fd, display }),
            Err(error) => Err(match self.entry(name) {
                Ok(Some(Entry {
                    kind: EntryKind::Symlink,
                    ..
                })) => not_followed(&display),
                Ok(Some(Entry {
                    kind: EntryKind::File | EntryKind::Other,
                    ..
                })) => not_a_directory(&display),
                // A directory that could not be opened, as one without read
                // permission: the error says why.
                _ => io::Error::from(error).into(),
            }),
        }
    }

    /// What `name` is in this directory, without following a link, or
    /// `None` when nothing has that name.
    pub(super) fn entry(&self, name: &str) -> io::Result<Option<Entry>> {
        let stat = match statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let kind = match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => EntryKind::File,
            FileType::Directory => EntryKind::Directory,
            FileType::Symlink => EntryKind::Symlink,
            _ => EntryKind::Other,
        };
        Ok(Some(Entry {
            kind,
            identity: (kind == EntryKind::File).then(|| identity(&stat)),
        }))
    }

    /// Create the file `name`, failing if anything has that name, even a
    /// dangling link.
    pub(super) fn create_new(&self, name: &str) -> io::Result<File> {
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(File::from(openat(&self.fd, name, flags, new_file_mode())?))
    }

    /// Rename `from` to `to` in this directory, failing if `to` exists.
    pub(super) fn rename_noreplace(&self, from: &str, to: &str) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        match renameat_with(&self.fd, from, &self.fd, to, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(()),
            Err(error) if NO_ATOMIC_NOREPLACE.contains(&error) => {}
            Err(error) => return Err(error.into()),
        }
        // A hard link fails when the target exists. Once it succeeds the file
        // is in place, so failing to drop the temporary name only leaves a
        // second name for it.
        match linkat(&self.fd, from, &self.fd, to, AtFlags::empty()) {
            Ok(()) => {
                let _ = unlinkat(&self.fd, from, AtFlags::empty());
                return Ok(());
            }
            Err(Errno::EXIST) => return Err(Errno::EXIST.into()),
            // No hard links either, as on FAT.
            Err(_) => {}
        }
        // Only a writer outside this extraction can create `to` between this
        // check and the rename.
        if self.entry(to)?.is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        renameat(&self.fd, from, &self.fd, to).map_err(Into::into)
    }

    /// Rename `from` over `to` in this directory. A link at `to` is
    /// replaced, never followed.
    pub(super) fn rename_replace(&self, from: &str, to: &str) -> io::Result<()> {
        renameat(&self.fd, from, &self.fd, to).map_err(Into::into)
    }

    /// Remove the file `name` from this directory.
    pub(super) fn remove_file(&self, name: &str) -> io::Result<()> {
        unlinkat(&self.fd, name, AtFlags::empty()).map_err(Into::into)
    }

    /// Where the directory was found, for messages.
    pub(super) fn display(&self) -> &Path {
        &self.display
    }
}

/// Whether a flush failed only because the filesystem cannot flush.
pub(super) fn flush_unsupported(error: &io::Error) -> bool {
    Errno::from_io_error(error).is_some_and(|errno| NO_FLUSH.contains(&errno))
}

/// `rwx` for everyone, narrowed by the umask, as `std::fs::create_dir` does.
fn new_directory_mode() -> Mode {
    Mode::RWXU | Mode::RWXG | Mode::RWXO
}

/// `rw` for everyone, narrowed by the umask, as `std::fs::File::create` does.
fn new_file_mode() -> Mode {
    Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH
}

// `st_dev` and `st_ino` are `u64` on Linux but other widths elsewhere, such
// as an `i32` device on Apple platforms.
#[allow(clippy::unnecessary_cast)]
fn identity(stat: &Stat) -> FileIdentity {
    FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    }
}
