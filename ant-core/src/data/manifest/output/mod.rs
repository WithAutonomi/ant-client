//! Containment for extraction: every name beneath the output root is
//! resolved from directories held open, and nothing beneath the root is
//! followed through a link.
//!
//! On Unix each directory is a file descriptor and every operation is
//! relative to one (`openat`, `mkdirat`, `renameat`), opened without
//! following links. Once a directory is open, renaming it or replacing any
//! path to it with a symlink cannot redirect a later write.
//!
//! On Windows each directory from the output root down is held open
//! without delete sharing. While those handles are open, Windows refuses to
//! rename or delete any directory on the chain. Directories beneath the
//! root are opened as the reparse point itself, so a junction or symlink is
//! detected, not followed. Operations still go by path, so a concurrent
//! process that can write in the output root can turn a directory this
//! extraction created, while it is still empty, into a junction; containing
//! that would take handle-relative opens through the native API.
//!
//! A file is written under a temporary name in its parent and renamed into
//! place without replacing anything. An existing name is replaced only when
//! overwrite was requested and it is a regular file this extraction did not
//! write. The last rule means two entries that a filesystem treats as one
//! name, such as a long name and its Windows 8.3 alias, never end as one
//! file: the second fails instead of silently replacing the first.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::path::PATH_SEPARATOR;
use super::ManifestError;

#[cfg(test)]
use std::io::Write;
#[cfg(all(test, unix))]
use std::os::unix::fs::symlink;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

use platform::{Dir, FileIdentity};

/// Prefix of the temporary name an entry is written under.
const TEMP_PREFIX: &str = ".ant-extract-";
/// Temporary names tried before giving up on finding a free one.
const MAX_TEMP_ATTEMPTS: u32 = 16;

/// What a name in a directory is, read without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// A name found in a directory, with the file's identity when it is a
/// regular file.
struct Entry {
    kind: EntryKind,
    identity: Option<FileIdentity>,
}

/// The output root of one extraction, held open while it runs.
pub(super) struct OutputRoot {
    dir: Dir,
    /// Files this extraction has written, by filesystem identity.
    written: Mutex<HashSet<FileIdentity>>,
}

impl OutputRoot {
    /// Create `root` if it is missing and open it. The root itself may be a
    /// link, since it is the caller's choice; nothing beneath it is followed.
    pub(super) fn open(root: &Path) -> Result<Self, ManifestError> {
        fs::create_dir_all(root)?;
        Ok(Self {
            dir: Dir::open_root(root)?,
            written: Mutex::default(),
        })
    }

    /// Resolve `name` beneath the root, creating each of its directories or
    /// confirming it is a real one, without following links.
    pub(super) fn target(&self, name: &str) -> Result<Target<'_>, ManifestError> {
        let mut components: Vec<&str> = name.split(PATH_SEPARATOR).collect();
        let file_name = components
            .pop()
            .filter(|component| !component.is_empty())
            .ok_or_else(|| ManifestError::Build("empty effective name".to_string()))?;
        let mut chain: Vec<Dir> = Vec::new();
        for component in components {
            let child = chain.last().unwrap_or(&self.dir).open_child(component)?;
            if !platform::HOLD_ANCESTORS {
                chain.clear();
            }
            chain.push(child);
        }
        Ok(Target {
            root: self,
            chain,
            file_name: file_name.to_string(),
        })
    }
}

/// Where one entry is written: its parent directory, held open, and the
/// file name within it.
pub(super) struct Target<'r> {
    root: &'r OutputRoot,
    /// The directories below the root, or only the parent where holding
    /// the parent alone keeps the chain fixed.
    chain: Vec<Dir>,
    file_name: String,
}

impl Target<'_> {
    fn dir(&self) -> &Dir {
        self.chain.last().unwrap_or(&self.root.dir)
    }

    /// The target's path, for messages.
    pub(super) fn path(&self) -> PathBuf {
        self.dir().display().join(&self.file_name)
    }

    /// The parent directory's path, for messages and free-space checks.
    pub(super) fn parent_path(&self) -> &Path {
        self.dir().display()
    }

    /// Refuse, before any download, a target that could not be written.
    pub(super) fn check(&self, overwrite: bool) -> Result<(), ManifestError> {
        let written = self
            .root
            .written
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.existing(overwrite, &written).map(|_| ())
    }

    /// Create the temporary file the entry is downloaded into.
    pub(super) fn stage(&self) -> Result<Staged<'_>, ManifestError> {
        let mut last_error = None;
        for _ in 0..MAX_TEMP_ATTEMPTS {
            let temp_name = format!("{TEMP_PREFIX}{:016x}", rand::random::<u64>());
            match self.dir().create_new(&temp_name) {
                Ok(file) => {
                    return Ok(Staged {
                        target: self,
                        temp_name: Some(temp_name),
                        file: Some(file),
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists))
            .into())
    }

    /// Whether the target exists and may be replaced. Refuses a link, any
    /// existing name without overwrite, anything but a regular file, and a
    /// file this extraction already wrote under a name the filesystem
    /// treats as the same.
    fn existing(
        &self,
        overwrite: bool,
        written: &HashSet<FileIdentity>,
    ) -> Result<bool, ManifestError> {
        let Some(entry) = self.dir().entry(&self.file_name)? else {
            return Ok(false);
        };
        let path = self.path();
        match entry.kind {
            EntryKind::Symlink => Err(ManifestError::Build(format!(
                "{} is a symlink and will not be replaced",
                path.display()
            ))),
            _ if !overwrite => Err(ManifestError::Build(format!(
                "{} already exists; pass overwrite to replace it",
                path.display()
            ))),
            EntryKind::File if entry.identity.is_some_and(|id| written.contains(&id)) => {
                Err(ManifestError::Build(format!(
                    "{} is a file this extraction already wrote, under a name this \
                     filesystem treats as the same; it will not be replaced",
                    path.display()
                )))
            }
            EntryKind::File => Ok(true),
            EntryKind::Directory | EntryKind::Other => Err(ManifestError::Build(format!(
                "{} exists and is not a regular file",
                path.display()
            ))),
        }
    }
}

/// An entry's temporary file. Dropping it without [`Staged::commit`]
/// removes the file, so a failed or cancelled entry leaves nothing behind.
pub(super) struct Staged<'t> {
    target: &'t Target<'t>,
    /// The temporary name, until the file takes the target's name.
    temp_name: Option<String>,
    /// Open until the commit closes it.
    file: Option<File>,
}

impl Staged<'_> {
    /// The open temporary file to write the entry into.
    pub(super) fn file(&mut self) -> Result<&mut File, ManifestError> {
        self.file
            .as_mut()
            .ok_or_else(|| ManifestError::Build("the temporary file is closed".to_string()))
    }

    /// Make the file durable and give it the target's name. Commits within
    /// one extraction are serialised, so the existence check, the rename
    /// and recording the result happen as one step.
    pub(super) fn commit(mut self, overwrite: bool) -> Result<(), ManifestError> {
        if let Some(file) = self.file.take() {
            match file.sync_all() {
                // A filesystem that cannot flush, as an SMB share on macOS
                // refuses the full flush, keeps the file as it keeps any
                // other write.
                Err(error) if platform::flush_unsupported(&error) => {}
                result => result?,
            }
        }
        let target = self.target;
        let dir = target.dir();
        let temp_name = self
            .temp_name
            .clone()
            .ok_or_else(|| ManifestError::Build("the entry was already committed".to_string()))?;
        let mut written = target
            .root
            .written
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if target.existing(overwrite, &written)? {
            dir.rename_replace(&temp_name, &target.file_name)?;
        } else {
            dir.rename_noreplace(&temp_name, &target.file_name)
                .map_err(|error| match error.kind() {
                    io::ErrorKind::AlreadyExists => ManifestError::Build(format!(
                        "{} appeared while the entry downloaded; it was not replaced",
                        target.path().display()
                    )),
                    _ => error.into(),
                })?;
        }
        // The file is in place, so nothing from here on fails the entry. A
        // rename done as a hard link may have left the temporary name too.
        let _ = dir.remove_file(&temp_name);
        self.temp_name = None;
        // Read the identity back from the final name: on some filesystems,
        // such as FAT, a rename changes it. A file whose identity cannot be
        // read is only left unguarded against a later alias.
        if let Ok(Some(Entry {
            identity: Some(identity),
            ..
        })) = dir.entry(&target.file_name)
        {
            written.insert(identity);
        }
        Ok(())
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        // Close before removing: Windows cannot remove an open file that was
        // opened without delete sharing.
        drop(self.file.take());
        if let Some(temp_name) = &self.temp_name {
            let _ = self.target.dir().remove_file(temp_name);
        }
    }
}

/// The error for a link inside the output root.
fn not_followed(path: &Path) -> ManifestError {
    ManifestError::Build(format!(
        "{} is a symlink; extraction does not follow links inside the output directory",
        path.display()
    ))
}

/// The error for a non-directory where a directory is needed.
fn not_a_directory(path: &Path) -> ManifestError {
    ManifestError::Build(format!("{} exists and is not a directory", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn write_entry(root: &OutputRoot, name: &str, bytes: &[u8], overwrite: bool) {
        let target = root.target(name).unwrap();
        target.check(overwrite).unwrap();
        let mut staged = target.stage().unwrap();
        staged.file().unwrap().write_all(bytes).unwrap();
        staged.commit(overwrite).unwrap();
    }

    fn temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(TEMP_PREFIX))
            .collect()
    }

    #[test]
    fn writes_beneath_the_root_and_refuses_what_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = OutputRoot::open(&dir.path().join("out")).unwrap();
        write_entry(&root, "x/y/z.txt", b"first", false);
        let path = dir.path().join("out/x/y/z.txt");
        assert_eq!(fs::read(&path).unwrap(), b"first");

        let target = root.target("x/y/z.txt").unwrap();
        assert_eq!(target.path(), path);
        assert!(target.check(false).is_err());
        // This extraction wrote it, so overwrite does not replace it either.
        assert!(target.check(true).is_err());
        // A directory where a file should go is never replaced.
        assert!(root.target("x/y").unwrap().check(true).is_err());
        // A file where a directory should go.
        assert!(root.target("x/y/z.txt/deeper").is_err());
        assert!(temp_files(&dir.path().join("out/x/y")).is_empty());
    }

    #[test]
    fn overwrite_replaces_only_a_pre_existing_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("old.txt"), b"old").unwrap();
        let root = OutputRoot::open(dir.path()).unwrap();
        assert!(root.target("old.txt").unwrap().check(false).is_err());
        write_entry(&root, "old.txt", b"new", true);
        assert_eq!(fs::read(dir.path().join("old.txt")).unwrap(), b"new");
    }

    #[test]
    fn a_staged_file_that_is_not_committed_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = OutputRoot::open(dir.path()).unwrap();
        let target = root.target("a/b.bin").unwrap();
        let mut staged = target.stage().unwrap();
        staged.file().unwrap().write_all(b"partial").unwrap();
        assert_eq!(temp_files(&dir.path().join("a")).len(), 1);
        drop(staged);
        assert!(temp_files(&dir.path().join("a")).is_empty());
        assert!(!dir.path().join("a/b.bin").exists());
    }

    #[test]
    fn a_target_that_appears_during_the_download_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = OutputRoot::open(dir.path()).unwrap();
        let target = root.target("late.bin").unwrap();
        target.check(false).unwrap();
        let mut staged = target.stage().unwrap();
        staged.file().unwrap().write_all(b"ours").unwrap();
        fs::write(dir.path().join("late.bin"), b"theirs").unwrap();
        let error = staged.commit(false).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(fs::read(dir.path().join("late.bin")).unwrap(), b"theirs");
        assert!(temp_files(dir.path()).is_empty());

        // The rename itself refuses to replace, for a name that appears
        // after the commit's own check.
        fs::write(dir.path().join("mine.tmp"), b"ours").unwrap();
        let error = root
            .dir
            .rename_noreplace("mine.tmp", "late.bin")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(dir.path().join("late.bin")).unwrap(), b"theirs");
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_the_root_are_never_followed() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = OutputRoot::open(dir.path()).unwrap();

        // A pre-existing directory symlink at the first level.
        symlink(outside.path(), dir.path().join("safe")).unwrap();
        let error = root.target("safe/file.txt").err().unwrap();
        assert!(error.to_string().contains("symlink"), "{error}");

        // Deeper: a real directory containing a symlinked directory.
        fs::create_dir(dir.path().join("real")).unwrap();
        symlink(outside.path(), dir.path().join("real/link")).unwrap();
        assert!(root.target("real/link/f").is_err());

        // A symlink at the target itself, with and without overwrite.
        symlink(outside.path().join("victim"), dir.path().join("t.txt")).unwrap();
        let target = root.target("t.txt").unwrap();
        assert!(target.check(false).is_err());
        assert!(target.check(true).is_err());
        assert!(!outside.path().join("victim").exists());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    /// The window the path-based design left open: a directory checked as
    /// real is swapped for a symlink to somewhere else while the download
    /// runs. The open directory is still where the file lands.
    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_for_a_symlink_mid_download_cannot_redirect_the_file() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = OutputRoot::open(dir.path()).unwrap();
        let target = root.target("late/file.bin").unwrap();
        let mut staged = target.stage().unwrap();
        staged.file().unwrap().write_all(b"payload").unwrap();

        fs::rename(dir.path().join("late"), dir.path().join("moved")).unwrap();
        symlink(outside.path(), dir.path().join("late")).unwrap();
        staged.commit(false).unwrap();

        assert_eq!(
            fs::read(dir.path().join("moved/file.bin")).unwrap(),
            b"payload"
        );
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    /// Two names the filesystem resolves to one file, as a long name and
    /// its 8.3 alias do on Windows, simulated with a hard link.
    #[cfg(unix)]
    #[test]
    fn overwrite_never_replaces_a_file_this_extraction_wrote_under_another_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = OutputRoot::open(dir.path()).unwrap();
        write_entry(&root, "report final.pdf", b"first entry", true);
        fs::hard_link(
            dir.path().join("report final.pdf"),
            dir.path().join("REPORT~1.PDF"),
        )
        .unwrap();

        let target = root.target("REPORT~1.PDF").unwrap();
        let error = target.check(true).unwrap_err();
        assert!(error.to_string().contains("already wrote"), "{error}");
        assert_eq!(
            fs::read(dir.path().join("report final.pdf")).unwrap(),
            b"first entry"
        );
    }
}
