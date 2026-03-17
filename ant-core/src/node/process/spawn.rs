use std::path::Path;
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::error::{Error, Result};

/// Spawn a node process as a child of the current process (the daemon).
///
/// stdout and stderr are redirected to the given log directory.
pub async fn spawn_node(
    binary_path: &Path,
    args: &[String],
    env_vars: &[(String, String)],
    log_dir: &Path,
) -> Result<Child> {
    tokio::fs::create_dir_all(log_dir)
        .await
        .map_err(|e| Error::ProcessSpawn(format!("Failed to create log directory: {e}")))?;

    let stdout_path = log_dir.join("stdout.log");
    let stderr_path = log_dir.join("stderr.log");

    let stdout_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_path)
        .map_err(|e| Error::ProcessSpawn(format!("Failed to open stdout log: {e}")))?;
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .map_err(|e| Error::ProcessSpawn(format!("Failed to open stderr log: {e}")))?;

    let mut cmd = Command::new(binary_path);
    cmd.args(args)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file)
        // Nodes intentionally survive daemon restarts. The daemon re-discovers
        // running nodes on startup via the registry and PID checks.
        .kill_on_drop(false);

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd
        .spawn()
        .map_err(|e| Error::ProcessSpawn(format!("Failed to spawn node process: {e}")))?;

    Ok(child)
}
