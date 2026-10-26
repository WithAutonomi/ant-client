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

    if !is_confirmed_daemon(config, pid) {
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
    if let Some(pid) = running_daemon_or_clear_stale(config)? {
        let port = read_port_file(&config.port_file_path);
        return Ok(DaemonStartResult {
            already_running: true,
            pid,
            port,
        });
    }

    // In ant-core's own unit tests the current executable is the test harness. It would read
    // `node daemon run` as test-name filters and re-run the tests that got here, each spawning
    // again. A unit-test build never spawns it.
    if cfg!(test) {
        return Err(Error::ProcessSpawn(
            "a unit-test build of ant-core does not spawn the daemon".to_string(),
        ));
    }

    // Get the path to the current executable
    let exe = std::env::current_exe()
        .map_err(|e| Error::ProcessSpawn(format!("Failed to get current executable: {e}")))?;
    // The daemon is recognised as an `ant` executable running `node daemon run`
    // (`process_identity`), so only the `ant` binary can start one that `info` and `start` will
    // recognise. This also covers test harnesses outside ant-core's unit tests, such as
    // integration tests, which would read `node daemon run` as test-name filters.
    if !is_ant_executable_name(&exe) {
        return Err(Error::ProcessSpawn(format!(
            "only the `ant` binary can start the daemon, not {}",
            exe.display()
        )));
    }
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
/// confirmed as the daemon, since a PID left behind by a daemon that died may have been reused.
/// The process confirms itself by running `ant node daemon run`; failing that, the daemon's API on
/// the recorded port must report the PID. `pid` and `port` are reported as recorded either way.
pub fn info(config: &DaemonConfig) -> DaemonInfo {
    let recorded = recorded_daemon(config);
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

/// What a live process's own details show about whether it is the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessIdentity {
    /// An `ant` executable running exactly the detached daemon's command line.
    Daemon,
    /// Not an `ant` executable, so not the daemon.
    Other,
    /// An `ant` executable whose arguments don't show that it is the daemon. They are another
    /// command line, such as another `ant` command or a foreground daemon started behind global
    /// options, or they can't be read: a process owned by another user, an elevated one on
    /// Windows, or no `/proc`. [`is_confirmed_daemon`] asks the daemon's API instead.
    Unconfirmed,
}

/// Classify a process from whether its executable is `ant` and from its arguments (`argv[0]`
/// first), `None` when they can't be read. Unreadable arguments are never taken as proof.
fn identity_from<S: AsRef<std::ffi::OsStr>>(
    executable_is_ant: bool,
    args: Option<&[S]>,
) -> ProcessIdentity {
    if !executable_is_ant {
        ProcessIdentity::Other
    } else if args.is_some_and(is_daemon_command) {
        ProcessIdentity::Daemon
    } else {
        ProcessIdentity::Unconfirmed
    }
}

/// Whether `args` (`argv[0]` first) are the detached daemon's command line: `node daemon run`
/// straight after the executable, as [`start`] spawns it with [`daemon_run_args`]. `daemon run`
/// anywhere else, as in `ant file upload daemon run`, is not proof.
fn is_daemon_command<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> bool {
    matches!(
        args.get(1..4),
        Some([node, daemon, run])
            if node.as_ref() == "node" && daemon.as_ref() == "daemon" && run.as_ref() == "run"
    )
}

/// Identify a process from `/proc/PID/cmdline`, which holds its NUL-separated arguments.
#[cfg(all(unix, not(target_os = "macos")))]
fn process_identity(pid: u32) -> ProcessIdentity {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        // No /proc (other Unixes) or a hidepid mount: nothing to go on.
        return ProcessIdentity::Unconfirmed;
    };
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let executable_is_ant = args
        .first()
        .is_some_and(|exe| is_ant_executable_name(Path::new(exe)));
    identity_from(executable_is_ant, Some(args.as_slice()))
}

/// Whether an executable path names the `ant` binary. The basename must be exactly `ant` or
/// `ant.exe`, which rules out processes like `rant`.
fn is_ant_executable_name(exe: &Path) -> bool {
    exe.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "ant" || name == "ant.exe")
}

/// Identify a process from its executable name and its arguments.
#[cfg(any(target_os = "macos", windows))]
fn process_identity(pid: u32) -> ProcessIdentity {
    identity_from(executable_is_ant(pid), process_args(pid).as_deref())
}

/// The process's arguments, read through `sysinfo` (`KERN_PROCARGS2` on macOS, the process
/// environment block on Windows), or `None` if they can't be read. `sysinfo` reports none for a
/// process owned by another user, or for an elevated one on Windows; every real process has at
/// least `argv[0]`.
#[cfg(any(target_os = "macos", windows))]
fn process_args(pid: u32) -> Option<Vec<std::ffi::OsString>> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::new().with_cmd(UpdateKind::Always),
    );
    let args = system.process(pid)?.cmd();
    (!args.is_empty()).then(|| args.to_vec())
}

/// Whether the live process `pid` is the daemon that `config`'s files record.
///
/// The process's own details settle it when they can (see [`ProcessIdentity`]). An
/// [`Unconfirmed`](ProcessIdentity::Unconfirmed) `ant` process counts only if the daemon's API, on
/// the recorded port, reports that PID.
fn is_confirmed_daemon(config: &DaemonConfig, pid: u32) -> bool {
    match process_identity(pid) {
        ProcessIdentity::Daemon => true,
        ProcessIdentity::Other => false,
        ProcessIdentity::Unconfirmed => daemon_api_reports_pid(&config.port_file_path, pid),
    }
}

/// Whether the daemon API on the port recorded in `port_file` reports `pid` as its PID
/// (`GET /api/v1/status`).
///
/// A dead daemon's port has nothing listening on it, or something that isn't the daemon. This is a
/// blocking local request with one-second timeouts, made only for an unconfirmed process.
fn daemon_api_reports_pid(port_file: &Path, pid: u32) -> bool {
    use std::io::{Read, Write};

    let Some(port) = read_port_file(port_file) else {
        return false;
    };
    let timeout = Duration::from_secs(1);
    let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let request = format!(
        "GET /api/v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    let mut response = Vec::new();
    stream.set_read_timeout(Some(timeout)).is_ok()
        && stream.set_write_timeout(Some(timeout)).is_ok()
        && stream.write_all(request.as_bytes()).is_ok()
        && stream.take(64 * 1024).read_to_end(&mut response).is_ok()
        && status_response_reports_pid(&response, pid)
}

/// Whether a raw HTTP response is a `200` whose [`DaemonStatus`] body reports `pid`.
fn status_response_reports_pid(response: &[u8], pid: u32) -> bool {
    let Some(end_of_head) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let status_code = std::str::from_utf8(&response[..end_of_head])
        .ok()
        .and_then(|head| head.lines().next())
        .and_then(|status_line| status_line.split_whitespace().nth(1));
    status_code == Some("200")
        && serde_json::from_slice::<DaemonStatus>(&response[end_of_head + 4..])
            .is_ok_and(|status| status.pid == Some(pid))
}

/// Whether the process's executable is named `ant`. `proc_pidpath` works for any user's process,
/// even where the arguments can't be read.
#[cfg(target_os = "macos")]
fn executable_is_ant(pid: u32) -> bool {
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

/// Whether the process's executable is named `ant`. `PROCESS_QUERY_LIMITED_INFORMATION` is granted
/// even for an elevated process, whose arguments can't be read.
#[cfg(windows)]
fn executable_is_ant(pid: u32) -> bool {
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

/// Classify the daemon recorded in `config`'s PID file.
///
/// A live PID is not enough on its own: the files outlive an unclean exit, and after a reboot the
/// PID is often reused by an unrelated process. The process must also be confirmed as the daemon
/// ([`is_confirmed_daemon`]), the same test `stop` applies before signalling it.
fn recorded_daemon(config: &DaemonConfig) -> RecordedDaemon {
    let Ok(pid) = read_pid_file(&config.pid_file_path) else {
        return RecordedDaemon::Absent;
    };
    if is_process_alive(pid) && is_confirmed_daemon(config, pid) {
        RecordedDaemon::Live(pid)
    } else {
        RecordedDaemon::Stale(pid)
    }
}

/// The PID of the running daemon, if there is one.
///
/// Otherwise removes any PID and port files a previous daemon left behind, so the next daemon
/// starts clean: the port poll in [`start`] would otherwise read the dead daemon's port straight
/// back. That is also why a file that can't be removed is an error rather than ignored.
fn running_daemon_or_clear_stale(config: &DaemonConfig) -> Result<Option<u32>> {
    match recorded_daemon(config) {
        RecordedDaemon::Live(pid) => Ok(Some(pid)),
        RecordedDaemon::Absent | RecordedDaemon::Stale(_) => {
            remove_stale_file(&config.pid_file_path)?;
            remove_stale_file(&config.port_file_path)?;
            Ok(None)
        }
    }
}

/// Remove a file a previous daemon left behind. A file that is already gone is fine.
fn remove_stale_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::ProcessSpawn(format!(
            "could not remove the stale daemon file {}: {e}",
            path.display()
        ))),
        _ => Ok(()),
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

    // Other Unixes have no /proc to read, so there every process is unconfirmed.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn a_process_that_is_not_ant_is_not_the_daemon() {
        assert_eq!(process_identity(std::process::id()), ProcessIdentity::Other);
    }

    #[test]
    fn only_node_daemon_run_after_the_executable_is_the_daemon_command() {
        assert!(is_daemon_command(&[
            "/usr/bin/ant",
            "node",
            "daemon",
            "run"
        ]));
        assert!(is_daemon_command(&[
            "ant.exe", "node", "daemon", "run", "--port", "8765"
        ]));
        for args in [
            // The review's example: `daemon run` as arguments to another command.
            &["ant", "file", "upload", "daemon", "run"][..],
            // A foreground daemon behind a global option: left to the daemon's API.
            &["ant", "--json", "node", "daemon", "run"],
            &["ant", "node", "daemon", "start"],
            &["ant", "node", "daemon", "status"],
            &["ant", "node", "daemon"],
            &["ant"],
            &[],
        ] {
            assert!(!is_daemon_command(args), "{args:?} is not proof");
        }
    }

    /// `start` refuses to spawn from any other executable, which is what keeps a test harness
    /// from spawning itself as `node daemon run`.
    #[test]
    fn only_the_ant_binary_is_an_ant_executable() {
        // Forward slashes, which every platform's `Path` splits on.
        for exe in ["/usr/local/bin/ant", "ant", "ant.exe", "Autonomi/ant.exe"] {
            assert!(is_ant_executable_name(Path::new(exe)), "{exe}");
        }
        for exe in [
            "target/debug/deps/ant_core-ea7d791ec7076b25",
            "target/debug/deps/ant_core-ea7d791ec7076b25.exe",
            "/usr/bin/rant",
            "/usr/bin/ant-node",
            "",
        ] {
            assert!(!is_ant_executable_name(Path::new(exe)), "{exe}");
        }
        assert!(
            !is_ant_executable_name(&std::env::current_exe().unwrap()),
            "this test binary must never pass for `ant`"
        );
    }

    #[test]
    fn unreadable_arguments_never_confirm_the_daemon() {
        let daemon = Some(&["ant", "node", "daemon", "run"][..]);
        let unreadable: Option<&[&str]> = None;
        assert_eq!(identity_from(true, daemon), ProcessIdentity::Daemon);
        assert_eq!(
            identity_from(true, unreadable),
            ProcessIdentity::Unconfirmed
        );
        assert_eq!(
            identity_from(true, Some(&["ant", "file", "upload", "daemon", "run"][..])),
            ProcessIdentity::Unconfirmed
        );
        assert_eq!(identity_from(false, daemon), ProcessIdentity::Other);
        assert_eq!(identity_from(false, unreadable), ProcessIdentity::Other);
    }

    /// A one-shot stand-in for the daemon's API: answers a single request with a `DaemonStatus`
    /// reporting `pid`.
    fn serve_status_once(pid: u32) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let body = serde_json::to_string(&DaemonStatus {
                running: true,
                pid: Some(pid),
                port: Some(port),
                uptime_secs: Some(1),
                nodes_total: 0,
                nodes_running: 0,
                nodes_stopped: 0,
                nodes_errored: 0,
            })
            .unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        (port, server)
    }

    /// A port with nothing listening on it, as a dead daemon's port usually is.
    fn unused_port() -> u16 {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn the_daemon_api_must_report_the_same_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let port_file = tmp.path().join("daemon.port");
        assert!(!daemon_api_reports_pid(&port_file, 42), "no port file");

        std::fs::write(&port_file, unused_port().to_string()).unwrap();
        assert!(!daemon_api_reports_pid(&port_file, 42), "nothing listening");

        let (port, server) = serve_status_once(42);
        std::fs::write(&port_file, port.to_string()).unwrap();
        assert!(daemon_api_reports_pid(&port_file, 42));
        server.join().unwrap();

        let (port, server) = serve_status_once(43);
        std::fs::write(&port_file, port.to_string()).unwrap();
        assert!(
            !daemon_api_reports_pid(&port_file, 42),
            "another daemon's PID"
        );
        server.join().unwrap();
    }

    #[test]
    fn only_a_200_with_the_pid_confirms_the_daemon() {
        let ok = |body: &str| format!("HTTP/1.1 200 OK\r\n\r\n{body}");
        let status = r#"{"running":true,"pid":42,"port":1,"uptime_secs":1,"nodes_total":0,"nodes_running":0,"nodes_stopped":0,"nodes_errored":0}"#;
        assert!(status_response_reports_pid(ok(status).as_bytes(), 42));
        assert!(!status_response_reports_pid(ok(status).as_bytes(), 7));
        assert!(!status_response_reports_pid(
            format!("HTTP/1.1 404 Not Found\r\n\r\n{status}").as_bytes(),
            42
        ));
        assert!(!status_response_reports_pid(ok("not json").as_bytes(), 42));
        assert!(!status_response_reports_pid(b"garbage", 42));
    }

    /// Test-name filter that selects [`sleep_as_an_ant_named_process`] in a copy of this binary.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    const SLEEPER: &str = "node::daemon::client::tests::sleep_as_an_ant_named_process";
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    const SLEEP_ENV: &str = "ANT_CORE_TEST_SLEEP_AS_ANT";

    /// The body of [`AntNamedProcess`], not a test: ignored, and a no-op unless its environment
    /// variable is set, so an `--ignored` run returns at once.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    #[ignore = "helper process for AntNamedProcess"]
    fn sleep_as_an_ant_named_process() {
        if std::env::var_os(SLEEP_ENV).is_some() {
            std::thread::sleep(Duration::from_secs(120));
        }
    }

    /// A live process whose executable is named `ant`, running `args`: a copy of this test binary
    /// that sleeps until dropped. libtest reads `args` as more test-name filters, which with
    /// `--exact` select no other test.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    struct AntNamedProcess {
        child: std::process::Child,
        _dir: tempfile::TempDir,
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    impl AntNamedProcess {
        fn spawn(args: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let exe = dir
                .path()
                .join(if cfg!(windows) { "ant.exe" } else { "ant" });
            std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
            let mut command = std::process::Command::new(&exe);
            command
                .args(args)
                .args(["--exact", "--ignored", SLEEPER])
                .env(SLEEP_ENV, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            // On Linux, executing a file that was just written fails with ETXTBSY while a process
            // forked by another test thread still holds the copy's write descriptor. That clears
            // once the other child execs, so retry briefly.
            let mut attempts = 0;
            let child = loop {
                match command.spawn() {
                    Err(e)
                        if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 50 =>
                    {
                        attempts += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    result => break result.unwrap(),
                }
            };
            let process = Self { child, _dir: dir };
            // `spawn` returns once the exec has begun, but on Linux /proc only shows the new
            // arguments a moment later: until then the process reads as `Other`. Hand tests a
            // process that already shows as `ant`.
            let shows_as_ant = (0..250).any(|_| {
                process_identity(process.pid()) != ProcessIdentity::Other || {
                    std::thread::sleep(Duration::from_millis(20));
                    false
                }
            });
            assert!(shows_as_ant, "the ant-named helper never showed as `ant`");
            process
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        fn assert_still_running(&mut self) {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "the ant-named helper process exited early"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    impl Drop for AntNamedProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn an_ant_process_running_node_daemon_run_is_the_daemon() {
        let mut process = AntNamedProcess::spawn(&["node", "daemon", "run"]);
        let pid = process.pid();
        // The arguments can still be unreadable for a moment after the exec, which reads as
        // `Unconfirmed`; the retry only absorbs that.
        let identified = (0..250).any(|_| {
            process_identity(pid) == ProcessIdentity::Daemon || {
                std::thread::sleep(Duration::from_millis(20));
                false
            }
        });
        process.assert_still_running();
        assert!(identified, "an `ant node daemon run` process is the daemon");
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn another_ant_command_holding_the_pid_is_not_the_daemon() {
        // `daemon run` as another command's arguments, the review's example.
        let mut process = AntNamedProcess::spawn(&["file", "upload", "daemon", "run"]);
        let pid = process.pid();
        let tmp = tempfile::tempdir().unwrap();
        let config = config_in(tmp.path());
        std::fs::write(&config.port_file_path, unused_port().to_string()).unwrap();

        assert_eq!(process_identity(pid), ProcessIdentity::Unconfirmed);
        assert!(!is_confirmed_daemon(&config, pid));
        process.assert_still_running();
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn an_unconfirmed_ant_process_is_the_daemon_only_if_its_api_says_so() {
        let mut process = AntNamedProcess::spawn(&["file", "upload", "daemon", "run"]);
        let pid = process.pid();
        let tmp = tempfile::tempdir().unwrap();
        let config = config_in(tmp.path());
        std::fs::write(&config.pid_file_path, pid.to_string()).unwrap();

        std::fs::write(&config.port_file_path, unused_port().to_string()).unwrap();
        assert!(
            !info(&config).running,
            "nothing answers on the recorded port"
        );

        let (port, server) = serve_status_once(pid);
        std::fs::write(&config.port_file_path, port.to_string()).unwrap();
        assert!(
            info(&config).running,
            "the daemon's own API reports this PID"
        );
        server.join().unwrap();
        process.assert_still_running();
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn info_with_a_pid_reused_by_another_ant_command_is_not_running() {
        let mut process = AntNamedProcess::spawn(&["node", "daemon", "status"]);
        let tmp = tempfile::tempdir().unwrap();
        let config = config_in(tmp.path());
        std::fs::write(&config.pid_file_path, process.pid().to_string()).unwrap();

        let info = info(&config);
        process.assert_still_running();
        assert!(
            !info.running,
            "another `ant` command holding a reused PID must not be reported as the daemon"
        );
        assert_eq!(info.pid, Some(process.pid()));
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

        assert_eq!(running_daemon_or_clear_stale(&config).unwrap(), None);
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

        assert_eq!(running_daemon_or_clear_stale(&config).unwrap(), None);
        assert!(!config.port_file_path.exists());
    }

    /// A stale port file that can't be removed: a directory in its place makes `remove_file` fail
    /// on every platform.
    fn config_with_an_unremovable_port_file(dir: &Path) -> DaemonConfig {
        let config = config_in(dir);
        std::fs::create_dir(&config.port_file_path).unwrap();
        config
    }

    #[test]
    fn a_stale_file_that_cannot_be_removed_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_an_unremovable_port_file(tmp.path());

        let err = running_daemon_or_clear_stale(&config).unwrap_err();
        assert!(
            err.to_string()
                .contains("could not remove the stale daemon file"),
            "{err}"
        );
    }
}
