use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::node::process::detach;
use crate::node::types::{
    DaemonConfig, DaemonInfo, DaemonStartResult, DaemonStatus, DaemonStopResult, NodeStarted,
    NodeStatusResult, NodeStopped, StartNodeResult, StopNodeResult,
};

/// Get the daemon's current status by querying its REST API.
///
/// If the daemon is not running, returns a `DaemonStatus` with `running: false`.
pub async fn status(config: &DaemonConfig) -> Result<DaemonStatus> {
    let port = match read_port_file(&config.port_file_path) {
        Some(port) => port,
        None => {
            return Ok(DaemonStatus {
                running: false,
                pid: None,
                port: None,
                uptime_secs: None,
                nodes_total: 0,
                nodes_running: 0,
                nodes_stopped: 0,
                nodes_errored: 0,
            });
        }
    };

    let url = format!("http://127.0.0.1:{port}/api/v1/status");
    match reqwest::get(&url).await {
        Ok(resp) => resp
            .json::<DaemonStatus>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string())),
        Err(_) => Ok(DaemonStatus {
            running: false,
            pid: None,
            port: Some(port),
            uptime_secs: None,
            nodes_total: 0,
            nodes_running: 0,
            nodes_stopped: 0,
            nodes_errored: 0,
        }),
    }
}

/// Stop the running daemon.
///
/// Reads the PID from the PID file, validates the process is actually a daemon
/// instance, sends SIGTERM (Unix) or Ctrl+C (Windows), and waits for the process
/// to exit.
pub async fn stop(config: &DaemonConfig) -> Result<DaemonStopResult> {
    let pid = read_pid_file(&config.pid_file_path)?;

    // Validate the process is actually our daemon before killing it.
    // After a crash, the PID may have been reused by an unrelated process.
    if !is_process_alive(pid) {
        // Process is already dead — just clean up stale files
        let _ = std::fs::remove_file(&config.pid_file_path);
        let _ = std::fs::remove_file(&config.port_file_path);
        return Ok(DaemonStopResult { pid });
    }

    if !validate_daemon_process(pid) {
        // PID is alive but isn't our daemon — clean up stale files without killing
        let _ = std::fs::remove_file(&config.pid_file_path);
        let _ = std::fs::remove_file(&config.port_file_path);
        return Err(Error::DaemonStopFailed(format!(
            "PID {pid} is alive but does not appear to be the ant daemon (possible PID reuse). \
             Stale PID file removed."
        )));
    }

    send_terminate(pid);

    // Wait for process to exit
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !is_process_alive(pid) {
            break;
        }
    }

    // Verify the process actually died
    if is_process_alive(pid) {
        return Err(Error::DaemonStopFailed(format!(
            "Daemon (PID {pid}) is still alive after 5 seconds"
        )));
    }

    // Clean up files if they still exist
    let _ = std::fs::remove_file(&config.pid_file_path);
    let _ = std::fs::remove_file(&config.port_file_path);

    Ok(DaemonStopResult { pid })
}

/// Start the daemon as a detached background process.
///
/// If the daemon is already running, returns a result with `already_running: true`.
/// Otherwise, spawns the daemon and polls for the port file to confirm startup.
pub async fn start(config: &DaemonConfig) -> Result<DaemonStartResult> {
    // Check if daemon is already running
    if let Some(pid) = check_running(&config.pid_file_path) {
        let port = read_port_file(&config.port_file_path);
        return Ok(DaemonStartResult {
            already_running: true,
            pid,
            port,
        });
    }

    // Get the path to the current executable
    let exe = std::env::current_exe()
        .map_err(|e| Error::ProcessSpawn(format!("Failed to get current executable: {e}")))?;
    let exe_str = exe
        .to_str()
        .ok_or_else(|| Error::ProcessSpawn("Executable path is not valid UTF-8".to_string()))?;

    let pid = detach::spawn_detached(exe_str, &["node", "daemon", "run"])?;

    // Wait briefly for the daemon to write its port file
    let mut port = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(p) = read_port_file(&config.port_file_path) {
            port = Some(p);
            break;
        }
    }

    Ok(DaemonStartResult {
        already_running: false,
        pid,
        port,
    })
}

/// Start a specific node by ID via the daemon REST API.
pub async fn start_node(config: &DaemonConfig, node_id: u32) -> Result<NodeStarted> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/nodes/{node_id}/start");
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<NodeStarted>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::HttpRequest(body))
    }
}

/// Start all registered nodes via the daemon REST API.
pub async fn start_all_nodes(config: &DaemonConfig) -> Result<StartNodeResult> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/nodes/start-all");
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<StartNodeResult>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::HttpRequest(body))
    }
}

/// Stop a specific node by ID via the daemon REST API.
pub async fn stop_node(config: &DaemonConfig, node_id: u32) -> Result<NodeStopped> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/nodes/{node_id}/stop");
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<NodeStopped>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::HttpRequest(body))
    }
}

/// Get the status of all registered nodes via the daemon REST API.
pub async fn node_status(config: &DaemonConfig) -> Result<NodeStatusResult> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/nodes/status");
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<NodeStatusResult>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::HttpRequest(body))
    }
}

/// Stop all running nodes via the daemon REST API.
pub async fn stop_all_nodes(config: &DaemonConfig) -> Result<StopNodeResult> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/nodes/stop-all");
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<StopNodeResult>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::HttpRequest(body))
    }
}

/// Get daemon connection info for programmatic use.
///
/// Reads PID and port files and checks if the process is alive.
pub fn info(config: &DaemonConfig) -> DaemonInfo {
    let pid = std::fs::read_to_string(&config.pid_file_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());

    let port = read_port_file(&config.port_file_path);

    let running = pid.is_some_and(is_process_alive);

    DaemonInfo {
        running,
        pid,
        port,
        api_base: port.map(|p| format!("http://127.0.0.1:{p}/api/v1")),
    }
}

/// Run the daemon in the foreground (the actual daemon process entry point).
///
/// Starts the HTTP server, sets up signal handling, and blocks until shutdown.
pub async fn run(config: DaemonConfig) -> Result<()> {
    use crate::node::daemon::server;
    use crate::node::registry::NodeRegistry;

    let registry = NodeRegistry::load(&config.registry_path)?;
    let shutdown = tokio_util::sync::CancellationToken::new();

    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        shutdown_clone.cancel();
    });

    let _addr = server::start(config, registry, shutdown.clone()).await?;

    shutdown.cancelled().await;
    // Give the server a moment to clean up
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(())
}

/// Validate that a PID belongs to an ant daemon process by checking its
/// command line. This guards against PID reuse after a daemon crash.
#[cfg(unix)]
fn validate_daemon_process(pid: u32) -> bool {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    match std::fs::read(&cmdline_path) {
        Ok(raw) => {
            // /proc/PID/cmdline uses null bytes as separators.
            // Check that the executable basename ends with "ant" and one
            // of the arguments is "daemon". This avoids false positives
            // from processes like "rant" or "phantom-daemon".
            let args: Vec<String> = raw
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).to_string())
                .collect();
            let exe_matches = args
                .first()
                .and_then(|exe| std::path::Path::new(exe).file_name())
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "ant" || name == "ant.exe");
            let has_daemon_arg = args.iter().any(|a| a == "daemon");
            exe_matches && has_daemon_arg
        }
        Err(_) => {
            // On non-Linux Unix (macOS), /proc doesn't exist. Fall back to
            // trusting the PID file since there's no cheap way to inspect
            // the command line without shelling out.
            true
        }
    }
}

#[cfg(windows)]
fn validate_daemon_process(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let success = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);

        if success == 0 {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        // Check the executable basename, not just substring
        std::path::Path::new(&path)
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name == "ant")
    }
}

fn read_port_file(path: &Path) -> Option<u16> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

fn read_pid_file(path: &Path) -> Result<u32> {
    let contents = std::fs::read_to_string(path).map_err(|_| Error::DaemonNotRunning)?;
    contents
        .trim()
        .parse::<u32>()
        .map_err(|_| Error::DaemonNotRunning)
}

/// Check if a daemon is running. Returns the PID if so.
fn check_running(pid_file: &Path) -> Option<u32> {
    let pid = read_pid_file(pid_file).ok()?;
    if is_process_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

#[cfg(unix)]
fn pid_to_i32(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|&p| p > 0)
}

#[cfg(unix)]
fn send_terminate(pid: u32) {
    if let Some(pid) = pid_to_i32(pid) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

#[cfg(windows)]
fn send_terminate(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    let Some(pid) = pid_to_i32(pid) else {
        return false;
    };
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we lack permission to signal it
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let success = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        success != 0 && exit_code == STILL_ACTIVE as u32
    }
}
