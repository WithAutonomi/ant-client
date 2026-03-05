use std::path::PathBuf;

/// Returns the platform-appropriate data directory for ant.
///
/// - Linux: `~/.local/share/ant`
/// - macOS: `~/Library/Application Support/ant`
/// - Windows: `%APPDATA%\ant`
pub fn data_dir() -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        dirs_next().join("Library").join("Application Support")
    } else if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_next().join("AppData").join("Roaming"))
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_next().join(".local").join("share"))
    };
    base.join("ant")
}

/// Returns the platform-appropriate log directory for ant.
///
/// - Linux: `~/.local/share/ant/logs`
/// - macOS: `~/Library/Logs/ant`
/// - Windows: `%APPDATA%\ant\logs`
pub fn log_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs_next().join("Library").join("Logs").join("ant")
    } else {
        data_dir().join("logs")
    }
}

fn dirs_next() -> PathBuf {
    home_dir().expect("Could not determine home directory")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_ends_with_ant() {
        let dir = data_dir();
        assert_eq!(dir.file_name().unwrap(), "ant");
    }

    #[test]
    fn log_dir_contains_ant() {
        let dir = log_dir();
        assert!(
            dir.components().any(|c| c.as_os_str() == "ant"),
            "log_dir should contain 'ant' component: {:?}",
            dir
        );
    }
}
