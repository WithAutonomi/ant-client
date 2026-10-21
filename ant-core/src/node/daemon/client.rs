use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::node::daemon::forward::{LogForwardEnableRequest, LogForwardResult, LogForwardStatus};
use crate::node::daemon::health::FleetHealth;
use crate::node::process::detach;
use crate::node::types::{
    AddNodeOpts, AddNodeResult, DaemonConfig, DaemonInfo, DaemonStartResult, DaemonStatus,
    DaemonStopResult, NodeStarted, NodeStatusResult, NodeStopped, RemoveNodeResult, ResetResult,
    StartNodeResult, StopNodeResult,
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
    if let Some(pid) = running_daemon_or_clear_stale(config) {
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

    let args = daemon_run_args(config);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let pid = detach::spawn_detached(exe_str, &arg_refs)?;

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

/// Read the daemon's log-forwarding status.
pub async fn log_forward_status(config: &DaemonConfig) -> Result<LogForwardStatus> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/logs/forward");
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<LogForwardStatus>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        Err(Error::HttpRequest(resp.text().await.unwrap_or_default()))
    }
}

/// Enable log forwarding via the daemon, so it starts shipping immediately.
pub async fn log_forward_enable(
    config: &DaemonConfig,
    request: &LogForwardEnableRequest,
) -> Result<LogForwardResult> {
    post_log_forward(config, "enable", Some(request)).await
}

/// Disable log forwarding via the daemon.
pub async fn log_forward_disable(config: &DaemonConfig) -> Result<LogForwardResult> {
    post_log_forward(config, "disable", None).await
}

async fn post_log_forward(
    config: &DaemonConfig,
    action: &str,
    body: Option<&LogForwardEnableRequest>,
) -> Result<LogForwardResult> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/logs/forward/{action}");
    let mut request = reqwest::Client::new().post(&url);
    if let Some(body) = body {
        request = request.json(body);
    }

    let resp = request
        .send()
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<LogForwardResult>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        // The daemon returns `{"error": "..."}` for a rejected enable; surface just that text
        // rather than the raw JSON envelope.
        let body = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(body);
        Err(Error::HttpRequest(message))
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

/// Add node(s) to the registry via the daemon REST API.
pub async fn add_node(config: &DaemonConfig, opts: &AddNodeOpts) -> Result<AddNodeResult> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/nodes");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(opts)
        .send()
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<AddNodeResult>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::HttpRequest(body))
    }
}

/// Reset all node state — clear the registry and remove node data/log
/// directories — via the daemon REST API.
pub async fn reset(config: &DaemonConfig) -> Result<ResetResult> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/reset");
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<ResetResult>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::HttpRequest(body))
    }
}

/// Dismiss a node — remove it from the registry — via the daemon REST API.
///
/// Intended for evicted nodes (whose data directory has already been deleted), but the daemon will
/// remove any non-running node. Running nodes are rejected with a conflict error.
pub async fn dismiss_node(config: &DaemonConfig, node_id: u32) -> Result<RemoveNodeResult> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/nodes/{node_id}");
    let resp = reqwest::Client::new()
        .delete(&url)
        .send()
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<RemoveNodeResult>()
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(Error::HttpRequest(body))
    }
}

/// Get the current fleet health snapshot via the daemon REST API.
pub async fn fleet_health(config: &DaemonConfig) -> Result<FleetHealth> {
    let port = read_port_file(&config.port_file_path).ok_or(Error::DaemonNotRunning)?;

    let url = format!("http://127.0.0.1:{port}/api/v1/health");
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| Error::HttpRequest(e.to_string()))?;

    if resp.status().is_success() {
        resp.json::<FleetHealth>()
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

/// Resolve a node's ID from its service name via the daemon REST API.
///
/// Goes through the daemon — the registry's owner — rather than reading
/// node_registry.json directly, so the lookup cannot observe a
/// half-written registry or race a concurrent mutation by the daemon.
pub async fn resolve_node_id_by_name(config: &DaemonConfig, service_name: &str) -> Result<u32> {
    let status = node_status(config).await?;
    status
        .nodes
        .iter()
        .find(|n| n.name == service_name)
        .map(|n| n.node_id)
        .ok_or_else(|| Error::NodeNotFoundByName(service_name.to_string()))
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
/// Reads the PID and port files. `running` is true only if the recorded PID is a live process
/// identifiable as the daemon, since a PID left behind by a daemon that died may have been
/// reused; `pid` and `port` are reported as recorded either way.
pub fn info(config: &DaemonConfig) -> DaemonInfo {
    let recorded = recorded_daemon(&config.pid_file_path);
    let port = read_port_file(&config.port_file_path);

    DaemonInfo {
        running: matches!(recorded, RecordedDaemon::Live(_)),
        pid: recorded.pid(),
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
#[cfg(all(unix, not(target_os = "macos")))]
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
            // Without /proc there's no cheap way to inspect the command line
            // without shelling out, so fall back to trusting the PID file.
            true
        }
    }
}

/// Validate that a PID belongs to an ant daemon process by checking its
/// executable name. macOS has no /proc, and reading another process's
/// arguments needs `sysctl(KERN_PROCARGS2)` parsing, so this checks the
/// basename only, as the Windows version does.
#[cfg(target_os = "macos")]
fn validate_daemon_process(pid: u32) -> bool {
    let Some(pid) = pid_to_i32(pid) else {
        return false;
    };
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let Ok(len) = usize::try_from(len) else {
        return false;
    };
    if len == 0 {
        return false;
    }
    let path = String::from_utf8_lossy(&buf[..len]);
    std::path::Path::new(path.as_ref())
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "ant")
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

/// Build the arg list passed to the detached `ant node daemon run` child.
///
/// Overrides are forwarded explicitly so the child binds to the same address
/// and port the caller asked for. Unset fields fall through to the child's
/// own defaults (loopback + OS-assigned port).
fn daemon_run_args(config: &DaemonConfig) -> Vec<String> {
    let defaults = DaemonConfig::default();
    let mut args = vec!["node".to_string(), "daemon".to_string(), "run".to_string()];
    if let Some(port) = config.port {
        args.push("--port".to_string());
        args.push(port.to_string());
    }
    if config.listen_addr != defaults.listen_addr {
        args.push("--listen-addr".to_string());
        args.push(config.listen_addr.to_string());
    }
    // Without this the path would be lost at the detach boundary: `daemon start` parses it, then
    // spawns a fresh `daemon run` process that would otherwise log nothing.
    if let Some(ref log_path) = config.log_path {
        args.push("--log-path".to_string());
        args.push(log_path.display().to_string());
    }
    args
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

/// What the PID file says about the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordedDaemon {
    /// No readable PID file.
    Absent,
    /// The recorded PID is a live daemon process.
    Live(u32),
    /// The daemon exited without removing its files (a reboot, an OOM kill, SIGKILL). The
    /// recorded PID is now dead, or belongs to an unrelated process.
    Stale(u32),
}

impl RecordedDaemon {
    fn pid(self) -> Option<u32> {
        match self {
            RecordedDaemon::Absent => None,
            RecordedDaemon::Live(pid) | RecordedDaemon::Stale(pid) => Some(pid),
        }
    }
}

/// Classify the daemon recorded in `pid_file`.
///
/// A live PID is not enough on its own: the files outlive an unclean exit, and after a reboot the
/// PID is often reused by an unrelated process. The process must also be identifiable as the
/// daemon, the same test `stop` applies before signalling it.
fn recorded_daemon(pid_file: &Path) -> RecordedDaemon {
    let Ok(pid) = read_pid_file(pid_file) else {
        return RecordedDaemon::Absent;
    };
    if is_process_alive(pid) && validate_daemon_process(pid) {
        RecordedDaemon::Live(pid)
    } else {
        RecordedDaemon::Stale(pid)
    }
}

/// The PID of the running daemon, if there is one.
///
/// Otherwise removes any PID and port files a previous daemon left behind, so the next daemon
/// starts clean: the port poll in [`start`] would otherwise read the dead daemon's port straight
/// back.
fn running_daemon_or_clear_stale(config: &DaemonConfig) -> Option<u32> {
    match recorded_daemon(&config.pid_file_path) {
        RecordedDaemon::Live(pid) => Some(pid),
        RecordedDaemon::Absent | RecordedDaemon::Stale(_) => {
            let _ = std::fs::remove_file(&config.pid_file_path);
            let _ = std::fs::remove_file(&config.port_file_path);
            None
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn run_args_default_config_has_no_overrides() {
        let config = DaemonConfig::default();
        let args = daemon_run_args(&config);
        assert_eq!(args, vec!["node", "daemon", "run"]);
    }

    #[test]
    fn run_args_forward_explicit_port() {
        let config = DaemonConfig {
            port: Some(8765),
            ..DaemonConfig::default()
        };
        let args = daemon_run_args(&config);
        assert_eq!(args, vec!["node", "daemon", "run", "--port", "8765"]);
    }

    /// The flag has to cross the detach boundary: `daemon start` parses it, but the process that
    /// does the logging is the `daemon run` it spawns.
    #[test]
    fn run_args_forward_the_log_path() {
        let config = DaemonConfig {
            log_path: Some(std::path::PathBuf::from("/var/log/ant/daemon.log")),
            ..DaemonConfig::default()
        };
        let args = daemon_run_args(&config);
        assert_eq!(
            args,
            vec![
                "node",
                "daemon",
                "run",
                "--log-path",
                "/var/log/ant/daemon.log"
            ]
        );
    }

    #[test]
    fn run_args_omit_the_log_path_by_default() {
        let args = daemon_run_args(&DaemonConfig::default());
        assert!(
            !args.contains(&"--log-path".to_string()),
            "daemon logging must stay opt-in"
        );
    }

    #[test]
    fn run_args_forward_explicit_listen_addr() {
        let config = DaemonConfig {
            listen_addr: std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ..DaemonConfig::default()
        };
        let args = daemon_run_args(&config);
        assert_eq!(
            args,
            vec!["node", "daemon", "run", "--listen-addr", "0.0.0.0"]
        );
    }

    #[test]
    fn run_args_forward_both_overrides() {
        let config = DaemonConfig {
            port: Some(8765),
            listen_addr: std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ..DaemonConfig::default()
        };
        let args = daemon_run_args(&config);
        assert_eq!(
            args,
            vec![
                "node",
                "daemon",
                "run",
                "--port",
                "8765",
                "--listen-addr",
                "0.0.0.0",
            ]
        );
    }

    #[test]
    fn run_args_forward_explicit_zero_port() {
        // Explicit `--port 0` is preserved so the user's intent (OS-assigned)
        // round-trips through the spawn, even though the child's default would
        // produce the same bind behavior.
        let config = DaemonConfig {
            port: Some(0),
            ..DaemonConfig::default()
        };
        let args = daemon_run_args(&config);
        assert_eq!(args, vec!["node", "daemon", "run", "--port", "0"]);
    }

    fn config_in(dir: &Path) -> DaemonConfig {
        DaemonConfig {
            pid_file_path: dir.join("daemon.pid"),
            port_file_path: dir.join("daemon.port"),
            ..DaemonConfig::default()
        }
    }

    /// The files a daemon leaves behind when it dies without cleaning up, after its PID has been
    /// reused: the test process stands in for the unrelated process now holding it.
    fn write_files_with_reused_pid(config: &DaemonConfig) {
        std::fs::write(&config.pid_file_path, std::process::id().to_string()).unwrap();
        std::fs::write(&config.port_file_path, "39007").unwrap();
    }

    #[test]
    fn info_without_a_pid_file_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let info = info(&config_in(tmp.path()));
        assert!(!info.running);
        assert_eq!(info.pid, None);
    }

    // The fallback for other Unixes trusts any live PID, so these hold only where the process can
    // be identified.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn validate_rejects_a_live_process_that_is_not_the_daemon() {
        assert!(!validate_daemon_process(std::process::id()));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn info_with_a_reused_pid_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_in(tmp.path());
        write_files_with_reused_pid(&config);

        let info = info(&config);
        assert!(
            !info.running,
            "a reused PID must not be reported as the daemon"
        );
        assert_eq!(info.pid, Some(std::process::id()));
        assert_eq!(info.port, Some(39007));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn start_clears_files_left_by_a_dead_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_in(tmp.path());
        write_files_with_reused_pid(&config);

        assert_eq!(running_daemon_or_clear_stale(&config), None);
        assert!(!config.pid_file_path.exists());
        assert!(
            !config.port_file_path.exists(),
            "a stale port file would be read back as the new daemon's port"
        );
    }

    #[test]
    fn start_clears_a_port_file_without_a_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_in(tmp.path());
        std::fs::write(&config.port_file_path, "39007").unwrap();

        assert_eq!(running_daemon_or_clear_stale(&config), None);
        assert!(!config.port_file_path.exists());
    }
}
