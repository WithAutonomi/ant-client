//! How the running `ant` binary was installed.
//!
//! `ant update` replaces the running executable in place. That is correct for a binary the user
//! put on disk themselves — via `install.sh`, `install.ps1`, or an unpacked release archive —
//! and wrong for one a package manager owns, because the package manager's metadata would then
//! describe a file that is no longer there. This module tells the two apart.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The npm package that distributes `ant`.
pub const NPM_PACKAGE: &str = "@withautonomi/ant";

/// The directory npm installs dependencies into. An `ant` beneath one is npm-managed.
const NODE_MODULES: &str = "node_modules";

/// How the running binary got onto this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    /// Installed from the npm registry as [`NPM_PACKAGE`]. Updates belong to npm.
    Npm,
    /// Anything else: `install.sh`, `install.ps1`, a manually unpacked release archive,
    /// `cargo install`, or a local build. `ant update` may replace these in place.
    SelfManaged,
}

impl InstallMethod {
    /// Whether `ant update` is allowed to overwrite the running executable.
    #[must_use]
    pub fn can_self_replace(self) -> bool {
        matches!(self, Self::SelfManaged)
    }

    /// The command that updates an installation of this kind, if not `ant update` itself.
    #[must_use]
    pub fn update_command(self) -> Option<String> {
        match self {
            Self::Npm => Some(format!("npm update -g {NPM_PACKAGE}")),
            Self::SelfManaged => None,
        }
    }

    /// Name of the package manager that owns this installation, for user-facing messages.
    #[must_use]
    pub fn package_manager(self) -> Option<&'static str> {
        match self {
            Self::Npm => Some("npm"),
            Self::SelfManaged => None,
        }
    }
}

/// Classify the installation the current process was launched from.
///
/// Falls back to [`InstallMethod::SelfManaged`] when the executable path cannot be determined,
/// which preserves today's behaviour: an undetectable install is treated as one `ant update`
/// owns.
#[must_use]
pub fn detect() -> InstallMethod {
    current_exe()
        .as_deref()
        .map_or(InstallMethod::SelfManaged, classify_path)
}

/// Resolve the running executable, following symlinks where possible.
///
/// npm puts a launcher on `PATH` that is a symlink into its `node_modules` tree, but the process
/// this runs in was spawned from the real file, so the raw path is normally already inside
/// `node_modules`. Canonicalising anyway costs one syscall and covers platforms where
/// `current_exe` hands back the symlink instead.
fn current_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Classify an executable path without touching the filesystem.
///
/// A `node_modules` directory anywhere above the binary means npm (or a compatible client) put
/// it there. Nothing else in a normal install lands under such a directory; the sole way to be
/// wrong is to unpack a release archive into a directory literally named `node_modules`, which
/// costs the user nothing worse than being told to run `npm update` when they meant to
/// self-update.
///
/// Both separators are split on, rather than using [`Path::components`], which only understands
/// the separator of the host it was compiled for. That keeps the classification — and its tests
/// — identical everywhere, including when a Windows path is examined on a Unix host.
#[must_use]
pub fn classify_path(exe: &Path) -> InstallMethod {
    let under_node_modules = exe
        .to_string_lossy()
        .split(['/', '\\'])
        .any(|segment| segment == NODE_MODULES);

    if under_node_modules {
        InstallMethod::Npm
    } else {
        InstallMethod::SelfManaged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_global_install_is_detected() {
        // The layout `npm install -g` produces under a Node version manager.
        let path = Path::new(
            "/home/user/.nvm/versions/node/v22.11.0/lib/node_modules/@withautonomi/ant-linux-x64/bin/ant",
        );
        assert_eq!(classify_path(path), InstallMethod::Npm);
    }

    #[test]
    fn npm_local_install_is_detected() {
        let path = Path::new("/srv/project/node_modules/@withautonomi/ant-linux-x64/bin/ant");
        assert_eq!(classify_path(path), InstallMethod::Npm);
    }

    #[test]
    fn nested_node_modules_is_detected() {
        let path = Path::new(
            "/srv/project/node_modules/some-tool/node_modules/@withautonomi/ant-darwin-arm64/bin/ant",
        );
        assert_eq!(classify_path(path), InstallMethod::Npm);
    }

    #[test]
    fn windows_npm_install_is_detected() {
        let path = Path::new(
            r"C:\Users\user\AppData\Roaming\npm\node_modules\@withautonomi\ant-win32-x64\bin\ant.exe",
        );
        assert_eq!(classify_path(path), InstallMethod::Npm);
    }

    #[test]
    fn install_sh_locations_are_self_managed() {
        for path in [
            "/home/user/.local/bin/ant",
            "/usr/local/bin/ant",
            "/home/user/Library/Application Support/ant/ant",
            "/opt/ant/bin/ant",
        ] {
            assert_eq!(
                classify_path(Path::new(path)),
                InstallMethod::SelfManaged,
                "{path} should be self-managed"
            );
        }
    }

    #[test]
    fn cargo_and_local_builds_are_self_managed() {
        for path in [
            "/home/user/.cargo/bin/ant",
            "/home/user/dev/ant-client/target/release/ant",
        ] {
            assert_eq!(classify_path(Path::new(path)), InstallMethod::SelfManaged);
        }
    }

    #[test]
    fn a_directory_merely_containing_the_substring_is_not_npm() {
        // Only an exact `node_modules` path component counts, not a name that contains it.
        let path = Path::new("/home/user/my_node_modules_backup/bin/ant");
        assert_eq!(classify_path(path), InstallMethod::SelfManaged);
    }

    #[test]
    fn self_managed_installs_may_self_replace() {
        assert!(InstallMethod::SelfManaged.can_self_replace());
        assert!(InstallMethod::SelfManaged.update_command().is_none());
    }

    #[test]
    fn npm_installs_defer_to_npm() {
        assert!(!InstallMethod::Npm.can_self_replace());
        assert_eq!(
            InstallMethod::Npm.update_command().as_deref(),
            Some("npm update -g @withautonomi/ant")
        );
    }

    #[test]
    fn only_package_managed_installs_name_a_manager() {
        assert_eq!(InstallMethod::Npm.package_manager(), Some("npm"));
        assert_eq!(InstallMethod::SelfManaged.package_manager(), None);
    }

    #[test]
    fn install_method_serialises_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&InstallMethod::Npm).unwrap(),
            "\"npm\""
        );
        assert_eq!(
            serde_json::to_string(&InstallMethod::SelfManaged).unwrap(),
            "\"self_managed\""
        );
    }
}
