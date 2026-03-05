use std::process::Command;

use crate::error::{Error, Result};

/// Spawn a command detached from the current session.
/// Returns the PID of the detached process.
///
/// On Unix: uses setsid to create a new session, redirects stdio to /dev/null.
/// On Windows: uses CREATE_NO_WINDOW | DETACHED_PROCESS creation flags.
pub fn spawn_detached(cmd: &str, args: &[&str]) -> Result<u32> {
    spawn_detached_inner(cmd, args)
}

#[cfg(unix)]
fn spawn_detached_inner(cmd: &str, args: &[&str]) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    // Safety: pre_exec runs between fork and exec in the child process.
    // setsid() creates a new session, detaching from the controlling terminal.
    let child = unsafe {
        Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .pre_exec(|| {
                libc::setsid();
                Ok(())
            })
            .spawn()
    }
    .map_err(|e| Error::ProcessSpawn(format!("Failed to spawn detached process: {e}")))?;

    Ok(child.id())
}

#[cfg(windows)]
fn spawn_detached_inner(cmd: &str, args: &[&str]) -> Result<u32> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const DETACHED_PROCESS: u32 = 0x00000008;

    let child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .map_err(|e| Error::ProcessSpawn(format!("Failed to spawn detached process: {e}")))?;

    Ok(child.id())
}
