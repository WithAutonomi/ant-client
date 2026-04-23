use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::node::binary::extract_version;
use crate::node::events::NodeEvent;
use crate::node::process::spawn::spawn_node;
use crate::node::registry::NodeRegistry;
use crate::node::types::{
    NodeConfig, NodeStarted, NodeStatus, NodeStopFailed, NodeStopped, StopNodeResult,
};

/// How often the upgrade-detection task polls each running node's binary for a version change.
pub const UPGRADE_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How often the liveness poll verifies that each Running node's OS process still exists.
///
/// Nodes the current daemon spawned are watched via their owned `Child` handle in
/// `monitor_node`, so this poll exists purely to catch exits of nodes adopted across
/// a daemon restart (whose `Child` handle died with the previous daemon). Five seconds
/// is a rough trade-off: long enough that the syscall cost is negligible, short enough
/// that a crashed adopted node still looks broken to the user within a few heartbeats.
pub const LIVENESS_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Path of the pid file a running node writes to so a future daemon instance can
/// adopt it across restarts. Lives alongside the node's other on-disk state.
fn node_pid_file(data_dir: &Path) -> PathBuf {
    data_dir.join("node.pid")
}

/// Persist the running node's PID to `<data_dir>/node.pid`. Best-effort: a failure
/// here only costs us the ability to adopt the node after a daemon restart, so we
/// warn and continue rather than aborting the start.
fn write_node_pid(data_dir: &Path, pid: u32) {
    let path = node_pid_file(data_dir);
    if let Err(e) = std::fs::write(&path, pid.to_string()) {
        tracing::warn!(
            "Failed to write node pid file at {}: {e}. Node will still run, but a future \
             daemon restart will not be able to adopt it.",
            path.display()
        );
    }
}

/// Remove the pid file. Called on every terminal-exit path in `monitor_node` so the
/// next daemon doesn't try to adopt a PID belonging to a process that's gone.
fn remove_node_pid(data_dir: &Path) {
    let _ = std::fs::remove_file(node_pid_file(data_dir));
}

/// Read the pid file without validating liveness. Returns `None` if the file is
/// missing or its contents can't be parsed as a u32.
fn read_node_pid(data_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(node_pid_file(data_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Scan the OS process table for a running node that matches `config`, as a
/// fallback for when `<data_dir>/node.pid` is missing or stale.
///
/// Nodes spawned by a pre-adoption daemon never had a pid file written, so
/// without this scan the first restart after installing the adoption fix
/// would leave every previously-running node classified as Stopped. The scan
/// matches on:
///
/// - executable path identical to `config.binary_path`, AND
/// - command line containing `--root-dir` (as a standalone arg or
///   `--root-dir=<path>`) whose value resolves to `config.data_dir`.
///
/// The double match keeps us safe when multiple nodes share the same binary
/// on disk (common on installs where one copy services several data dirs).
///
/// Returns `None` if no running process matches.
fn find_running_node_process(sys: &sysinfo::System, config: &NodeConfig) -> Option<u32> {
    let target_data_dir = config.data_dir.as_path();
    for (pid, process) in sys.processes() {
        let Some(exe) = process.exe() else {
            continue;
        };
        if exe != config.binary_path.as_path() {
            continue;
        }

        let cmd = process.cmd();
        let matches_root_dir = cmd.iter().enumerate().any(|(i, arg)| {
            let arg = arg.to_string_lossy();
            if let Some(value) = arg.strip_prefix("--root-dir=") {
                Path::new(value) == target_data_dir
            } else if arg == "--root-dir" {
                cmd.get(i + 1)
                    .map(|v| Path::new(&*v.to_string_lossy()) == target_data_dir)
                    .unwrap_or(false)
            } else {
                false
            }
        });

        if matches_root_dir {
            return Some(pid.as_u32());
        }
    }
    None
}

/// Determine the PID to adopt for a node, trying the pid file first and
/// falling back to a process-table scan. `process_table` is lazily populated
/// — we avoid paying the `System::new_all` cost when every node's pid file
/// is present and valid, and amortise it across nodes when any scan is
/// required. On successful scan, writes the pid file so the next adoption
/// takes the fast path.
///
/// Returns `None` if no live process can be attributed to this node.
fn resolve_adopted_pid(
    config: &NodeConfig,
    process_table: &mut Option<sysinfo::System>,
) -> Option<u32> {
    if let Some(pid) = read_node_pid(&config.data_dir) {
        if is_process_alive(pid) {
            return Some(pid);
        }
        // Pid file points at a dead process. Don't leave it around to mislead
        // the next adoption pass.
        remove_node_pid(&config.data_dir);
    }

    let sys = process_table.get_or_insert_with(|| {
        // `refresh_processes` with the default `ProcessRefreshKind` skips the
        // command line on Windows (and on some Linux configurations), which
        // would make the `--root-dir` match below silently return no-matches
        // even for a live process whose exe path is correct. Force the full
        // refresh so `process.cmd()` is actually populated.
        let mut s = sysinfo::System::new();
        s.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::everything(),
        );
        s
    });
    let pid = find_running_node_process(sys, config)?;
    write_node_pid(&config.data_dir, pid);
    Some(pid)
}

/// Maximum restart attempts before marking a node as errored.
const MAX_CRASHES_BEFORE_ERRORED: u32 = 5;

/// Window in which crashes are counted. If this many crashes happen within
/// this duration, the node is marked errored.
const CRASH_WINDOW: Duration = Duration::from_secs(300); // 5 minutes

/// If a node runs for this long without crashing, reset the crash counter.
const STABLE_DURATION: Duration = Duration::from_secs(300); // 5 minutes

/// Maximum backoff delay between restarts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Manages running node processes. Holds child process handles and runtime state.
pub struct Supervisor {
    event_tx: broadcast::Sender<NodeEvent>,
    /// Runtime status of each node, keyed by node ID.
    node_states: HashMap<u32, NodeRuntime>,
}

struct NodeRuntime {
    status: NodeStatus,
    pid: Option<u32>,
    started_at: Option<Instant>,
    restart_count: u32,
    first_crash_at: Option<Instant>,
    /// When `status == UpgradeScheduled`, the target version the on-disk binary now reports.
    pending_version: Option<String>,
}

impl Supervisor {
    pub fn new(event_tx: broadcast::Sender<NodeEvent>) -> Self {
        Self {
            event_tx,
            node_states: HashMap::new(),
        }
    }

    /// Start a node by spawning the actual process.
    ///
    /// Returns `NodeStarted` on success. Spawns a background monitoring task
    /// that watches the child process and handles restart logic.
    pub async fn start_node(
        &mut self,
        config: &NodeConfig,
        supervisor_ref: Arc<RwLock<Supervisor>>,
        registry_ref: Arc<RwLock<NodeRegistry>>,
    ) -> Result<NodeStarted> {
        let node_id = config.id;

        if let Some(state) = self.node_states.get(&node_id) {
            if state.status == NodeStatus::Running {
                return Err(Error::NodeAlreadyRunning(node_id));
            }
        }

        let _ = self.event_tx.send(NodeEvent::NodeStarting { node_id });

        let mut child = spawn_node_from_config(config).await?;
        let pid = child
            .id()
            .ok_or_else(|| Error::ProcessSpawn("Failed to get PID from spawned process".into()))?;

        // Brief health check: give the process a moment to start, then check if it
        // exited immediately. This catches errors like invalid CLI arguments or missing
        // shared libraries. We use timeout + wait() rather than try_wait() because
        // tokio's child reaper requires the wait future to be polled.
        match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
            Ok(Ok(exit_status)) => {
                // Process already exited — read stderr for details.
                // spawn_node always redirects stderr to a file in the log dir
                // (falling back to data_dir when no log dir is configured).
                let spawn_log_dir = config.log_dir.as_deref().unwrap_or(&config.data_dir);
                let stderr_path = spawn_log_dir.join("stderr.log");
                let stderr_msg = std::fs::read_to_string(&stderr_path).unwrap_or_default();
                let detail = if stderr_msg.trim().is_empty() {
                    format!("exit code: {exit_status}")
                } else {
                    stderr_msg.trim().to_string()
                };
                self.node_states.insert(
                    node_id,
                    NodeRuntime {
                        status: NodeStatus::Errored,
                        pid: None,
                        started_at: None,
                        restart_count: 0,
                        first_crash_at: None,
                        pending_version: None,
                    },
                );
                return Err(Error::ProcessSpawn(format!(
                    "Node {node_id} exited immediately: {detail}"
                )));
            }
            Ok(Err(e)) => {
                return Err(Error::ProcessSpawn(format!(
                    "Failed to check node process status: {e}"
                )));
            }
            Err(_) => {} // Timeout — process is still running after 1s, good
        }

        self.node_states.insert(
            node_id,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: Some(pid),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );

        let _ = self.event_tx.send(NodeEvent::NodeStarted { node_id, pid });

        let result = NodeStarted {
            node_id,
            service_name: config.service_name.clone(),
            pid,
        };

        // Spawn monitoring task
        let event_tx = self.event_tx.clone();
        let config = config.clone();
        tokio::spawn(async move {
            monitor_node(child, config, supervisor_ref, registry_ref, event_tx).await;
        });

        Ok(result)
    }

    /// Stop a node by gracefully terminating its process.
    ///
    /// Sends SIGTERM (Unix) or kills (Windows), waits up to 10 seconds for exit,
    /// then sends SIGKILL if needed. The monitor task detects the Stopping status
    /// and exits cleanly without attempting a restart.
    pub async fn stop_node(&mut self, node_id: u32) -> Result<()> {
        let state = self
            .node_states
            .get_mut(&node_id)
            .ok_or(Error::NodeNotFound(node_id))?;

        if state.status != NodeStatus::Running {
            return Err(Error::NodeNotRunning(node_id));
        }

        let pid = state.pid;

        let _ = self.event_tx.send(NodeEvent::NodeStopping { node_id });
        state.status = NodeStatus::Stopping;

        if let Some(pid) = pid {
            graceful_kill(pid).await;
        }

        // Update state after kill
        let state = self.node_states.get_mut(&node_id).unwrap();
        state.status = NodeStatus::Stopped;
        state.pid = None;
        state.started_at = None;

        let _ = self.event_tx.send(NodeEvent::NodeStopped { node_id });

        Ok(())
    }

    /// Stop all running nodes, returning an aggregate result.
    pub async fn stop_all_nodes(&mut self, configs: &[(u32, String)]) -> StopNodeResult {
        let mut stopped = Vec::new();
        let mut failed = Vec::new();
        let mut already_stopped = Vec::new();

        for (node_id, service_name) in configs {
            let node_id = *node_id;
            match self.node_status(node_id) {
                Ok(NodeStatus::Running) => {}
                Ok(_) => {
                    already_stopped.push(node_id);
                    continue;
                }
                Err(_) => {
                    already_stopped.push(node_id);
                    continue;
                }
            }

            match self.stop_node(node_id).await {
                Ok(()) => {
                    stopped.push(NodeStopped {
                        node_id,
                        service_name: service_name.clone(),
                    });
                }
                Err(Error::NodeNotRunning(_)) => {
                    already_stopped.push(node_id);
                }
                Err(e) => {
                    failed.push(NodeStopFailed {
                        node_id,
                        service_name: service_name.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }

        StopNodeResult {
            stopped,
            failed,
            already_stopped,
        }
    }

    /// Get the status of a node.
    pub fn node_status(&self, node_id: u32) -> Result<NodeStatus> {
        self.node_states
            .get(&node_id)
            .map(|s| s.status)
            .ok_or(Error::NodeNotFound(node_id))
    }

    /// Get the PID of a running node.
    pub fn node_pid(&self, node_id: u32) -> Option<u32> {
        self.node_states.get(&node_id).and_then(|s| s.pid)
    }

    /// Get the uptime of a running node in seconds.
    pub fn node_uptime_secs(&self, node_id: u32) -> Option<u64> {
        self.node_states
            .get(&node_id)
            .and_then(|s| s.started_at.map(|t| t.elapsed().as_secs()))
    }

    /// The target version when the node is in `UpgradeScheduled` state, otherwise `None`.
    pub fn node_pending_version(&self, node_id: u32) -> Option<String> {
        self.node_states
            .get(&node_id)
            .and_then(|s| s.pending_version.clone())
    }

    /// Transition a Running node into `UpgradeScheduled` with the target version.
    ///
    /// Only affects nodes currently in `Running`: any other state is left alone (a stopped
    /// node legitimately has an out-of-date binary; a node already in UpgradeScheduled has
    /// already been marked). Returns `true` if the transition happened.
    fn mark_upgrade_scheduled(&mut self, node_id: u32, pending_version: String) -> bool {
        let Some(state) = self.node_states.get_mut(&node_id) else {
            return false;
        };
        if state.status != NodeStatus::Running {
            return false;
        }
        state.status = NodeStatus::UpgradeScheduled;
        state.pending_version = Some(pending_version.clone());
        let _ = self.event_tx.send(NodeEvent::UpgradeScheduled {
            node_id,
            pending_version,
        });
        true
    }

    /// Check whether a node is running.
    pub fn is_running(&self, node_id: u32) -> bool {
        self.node_states
            .get(&node_id)
            .is_some_and(|s| s.status == NodeStatus::Running)
    }

    /// Get counts of nodes in each state: (running, stopped, errored).
    pub fn node_counts(&self) -> (u32, u32, u32) {
        let mut running = 0u32;
        let mut stopped = 0u32;
        let mut errored = 0u32;
        for state in self.node_states.values() {
            match state.status {
                // UpgradeScheduled means the process is still running; count it with running.
                NodeStatus::Running | NodeStatus::Starting | NodeStatus::UpgradeScheduled => {
                    running += 1
                }
                NodeStatus::Stopped | NodeStatus::Stopping => stopped += 1,
                NodeStatus::Errored => errored += 1,
            }
        }
        (running, stopped, errored)
    }

    /// Update the runtime state for a node (used by the monitor task).
    fn update_state(&mut self, node_id: u32, status: NodeStatus, pid: Option<u32>) {
        if let Some(state) = self.node_states.get_mut(&node_id) {
            state.status = status;
            state.pid = pid;
            if status == NodeStatus::Running {
                state.started_at = Some(Instant::now());
            }
        }
    }

    /// Restore running-node state from a previous daemon instance.
    ///
    /// For each registered node, determines the PID to adopt via
    /// `resolve_adopted_pid`: try `<data_dir>/node.pid` first, and if it's
    /// missing or stale, fall back to a process-table scan matching the
    /// node's binary path and `--root-dir` argument. Live matches are
    /// inserted into `node_states` as `Running`.
    ///
    /// The scan is what covers the upgrade path: nodes spawned by a
    /// pre-adoption daemon never had a pid file written, so without the
    /// fallback the first restart after installing this fix would still
    /// leave every previously-running node classified as Stopped.
    ///
    /// Must be called before the HTTP server starts accepting requests —
    /// the window between `Supervisor::new` and adoption is where the API
    /// would otherwise report live nodes as Stopped. Adopted nodes have no
    /// associated `monitor_node` task (the `tokio::process::Child` handle
    /// belonged to the previous daemon, and `tokio::process::Child::wait`
    /// only works for the process's actual parent). Their exits are
    /// detected instead by the `spawn_liveness_monitor` polling task.
    ///
    /// Returns the list of node IDs that were adopted.
    pub fn adopt_from_registry(&mut self, registry: &NodeRegistry) -> Vec<u32> {
        let mut adopted = Vec::new();
        // Populated lazily on the first scan so deployments where every
        // node's pid file is present pay zero for process-table enumeration.
        let mut process_table: Option<sysinfo::System> = None;
        for config in registry.list() {
            let Some(pid) = resolve_adopted_pid(config, &mut process_table) else {
                continue;
            };
            self.node_states.insert(
                config.id,
                NodeRuntime {
                    status: NodeStatus::Running,
                    pid: Some(pid),
                    // Best-effort: the real start time lives in the OS process table
                    // and can't be read portably without pulling in more of sysinfo's
                    // surface. Users observing uptime across daemon restarts see it
                    // reset from zero, which is the right trade vs. the previous
                    // behaviour (every node reporting Stopped despite a live process).
                    started_at: Some(Instant::now()),
                    restart_count: 0,
                    first_crash_at: None,
                    pending_version: None,
                },
            );
            let _ = self.event_tx.send(NodeEvent::NodeStarted {
                node_id: config.id,
                pid,
            });
            adopted.push(config.id);
        }
        adopted
    }

    /// Record a crash and determine if the node should be restarted or marked errored.
    /// Returns (should_restart, attempt_number, backoff_duration).
    fn record_crash(&mut self, node_id: u32) -> (bool, u32, Duration) {
        let state = match self.node_states.get_mut(&node_id) {
            Some(s) => s,
            None => return (false, 0, Duration::ZERO),
        };

        let now = Instant::now();

        // Check if we were stable long enough to reset crash counter
        if let Some(started_at) = state.started_at {
            if started_at.elapsed() >= STABLE_DURATION {
                state.restart_count = 0;
                state.first_crash_at = None;
            }
        }

        state.restart_count += 1;
        let attempt = state.restart_count;

        if state.first_crash_at.is_none() {
            state.first_crash_at = Some(now);
        }

        // Check if too many crashes in the window
        if let Some(first_crash) = state.first_crash_at {
            if attempt >= MAX_CRASHES_BEFORE_ERRORED
                && now.duration_since(first_crash) < CRASH_WINDOW
            {
                state.status = NodeStatus::Errored;
                state.pid = None;
                state.started_at = None;
                return (false, attempt, Duration::ZERO);
            }
        }

        // Exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s, 60s cap
        let backoff_secs = 1u64 << (attempt - 1).min(5);
        let backoff = Duration::from_secs(backoff_secs).min(MAX_BACKOFF);

        (true, attempt, backoff)
    }
}

/// Periodically probe each Running node's on-disk binary for a version change.
///
/// When a node's binary-on-disk reports a different version than was recorded in the registry
/// at `ant node add` time, ant-node has replaced the binary in place as part of its auto-upgrade
/// flow and will restart the process shortly. We flip the node to `UpgradeScheduled` with the
/// target version, which lets `ant node status` render the in-between state and lets
/// `monitor_node` reclassify the upcoming clean exit as an expected restart rather than a crash.
///
/// The task exits when `shutdown` is cancelled.
pub fn spawn_upgrade_monitor(
    registry: Arc<RwLock<NodeRegistry>>,
    supervisor: Arc<RwLock<Supervisor>>,
    interval: Duration,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip the immediate first tick — we don't want to probe while nodes are still in the
        // Starting -> Running transition.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => {},
            }

            // Collect a snapshot of (node_id, binary_path, recorded_version, current_pending)
            // to release the locks before running --version subprocesses (which take time).
            let candidates: Vec<(u32, std::path::PathBuf, String, Option<String>)> = {
                let reg = registry.read().await;
                let sup = supervisor.read().await;
                reg.list()
                    .into_iter()
                    .filter_map(|config| match sup.node_status(config.id) {
                        Ok(NodeStatus::Running) => Some((
                            config.id,
                            config.binary_path.clone(),
                            config.version.clone(),
                            sup.node_pending_version(config.id),
                        )),
                        _ => None,
                    })
                    .collect()
            };

            for (node_id, binary_path, recorded_version, current_pending) in candidates {
                let observed = match extract_version(&binary_path).await {
                    Ok(v) => v,
                    // Transient failures (e.g. binary mid-replacement) — skip this round.
                    Err(_) => continue,
                };
                if observed == recorded_version {
                    continue;
                }
                if current_pending.as_deref() == Some(observed.as_str()) {
                    continue;
                }
                supervisor
                    .write()
                    .await
                    .mark_upgrade_scheduled(node_id, observed);
            }
        }
    });
}

/// Build CLI arguments for the node binary from a NodeConfig.
pub fn build_node_args(config: &NodeConfig) -> Vec<String> {
    let mut args = vec![
        "--rewards-address".to_string(),
        config.rewards_address.clone(),
        "--root-dir".to_string(),
        config.data_dir.display().to_string(),
    ];

    if let Some(ref log_dir) = config.log_dir {
        args.push("--enable-logging".to_string());
        args.push("--log-dir".to_string());
        args.push(log_dir.display().to_string());
    }

    if let Some(port) = config.node_port {
        args.push("--port".to_string());
        args.push(port.to_string());
    }

    if let Some(port) = config.metrics_port {
        args.push("--metrics-port".to_string());
        args.push(port.to_string());
    }

    for peer in &config.bootstrap_peers {
        args.push("--bootstrap".to_string());
        args.push(peer.clone());
    }

    // The daemon's supervisor is the service manager. Tell ant-node not to spawn its own
    // replacement on auto-upgrade; instead, exit cleanly and let us respawn. Without this,
    // ant-node's default spawn-grandchild-then-exit flow races for the node's port during
    // the parent's graceful shutdown and the grandchild fails to bind.
    args.push("--stop-on-upgrade".to_string());

    args
}

/// Spawn a node process from a NodeConfig.
///
/// Writes `<data_dir>/node.pid` on successful spawn so that a future daemon instance
/// can adopt the running process via `Supervisor::adopt_from_registry`. The file is
/// cleaned up by `monitor_node` on the node's terminal exit.
async fn spawn_node_from_config(config: &NodeConfig) -> Result<tokio::process::Child> {
    let args = build_node_args(config);
    let env_vars: Vec<(String, String)> = config.env_variables.clone().into_iter().collect();

    let log_dir = config
        .log_dir
        .as_deref()
        .unwrap_or(config.data_dir.as_path());

    let child = spawn_node(&config.binary_path, &args, &env_vars, log_dir).await?;
    if let Some(pid) = child.id() {
        write_node_pid(&config.data_dir, pid);
    }
    Ok(child)
}

/// Monitor a node process. On exit, handle restart logic. On permanent exit
/// (user stop, crash limit, errored), cleans up the pid file so a subsequent
/// daemon restart doesn't try to adopt a dead process.
async fn monitor_node(
    child: tokio::process::Child,
    mut config: NodeConfig,
    supervisor: Arc<RwLock<Supervisor>>,
    registry: Arc<RwLock<NodeRegistry>>,
    event_tx: broadcast::Sender<NodeEvent>,
) {
    monitor_node_inner(child, &mut config, supervisor, registry, event_tx).await;
    remove_node_pid(&config.data_dir);
}

async fn monitor_node_inner(
    mut child: tokio::process::Child,
    config: &mut NodeConfig,
    supervisor: Arc<RwLock<Supervisor>>,
    registry: Arc<RwLock<NodeRegistry>>,
    event_tx: broadcast::Sender<NodeEvent>,
) {
    let node_id = config.id;

    loop {
        // Wait for the process to exit
        let exit_status = child.wait().await;

        // Check whether this is a scheduled upgrade restart or an intentional stop.
        let status_at_exit = {
            let sup = supervisor.read().await;
            sup.node_status(node_id).ok()
        };

        match status_at_exit {
            Some(NodeStatus::Stopped) | Some(NodeStatus::Stopping) => return,
            Some(NodeStatus::UpgradeScheduled) => {
                // ant-node cleanly exited after replacing its binary in place. Respawn
                // directly (no backoff, no crash counter) and refresh the recorded version.
                match respawn_upgraded_node(config, &supervisor, &registry, &event_tx).await {
                    Ok(new_child) => {
                        child = new_child;
                        continue;
                    }
                    Err(e) => {
                        let _ = event_tx.send(NodeEvent::NodeErrored {
                            node_id,
                            message: format!("Failed to respawn after upgrade: {e}"),
                        });
                        let mut sup = supervisor.write().await;
                        sup.update_state(node_id, NodeStatus::Errored, None);
                        return;
                    }
                }
            }
            _ => {}
        }

        let exit_code = exit_status.ok().and_then(|s| s.code());

        // A process-reported exit that wasn't user-initiated (Stopping was filtered above) is
        // either an auto-upgrade (exit 0 after ant-node replaced its binary) or a crash. In
        // neither case should the node be parked in `Stopped` — that state is reserved for
        // intentional user stops.
        //
        // Distinguish upgrade from crash by checking whether the on-disk binary's version
        // drifted from the registry. Between replacing its binary and actually exiting,
        // ant-node can hold the process open for anywhere from seconds to minutes, depending
        // on in-flight work and its own config. The periodic version poll will usually have
        // flipped the node to `UpgradeScheduled` well before the exit, but when the window is
        // short we cannot rely on that — hence this synchronous re-check here.
        if exit_code == Some(0) {
            if let Ok(disk_version) = extract_version(&config.binary_path).await {
                if disk_version != config.version {
                    {
                        let mut sup = supervisor.write().await;
                        sup.mark_upgrade_scheduled(node_id, disk_version.clone());
                    }
                    match respawn_upgraded_node(config, &supervisor, &registry, &event_tx)
                        .await
                    {
                        Ok(new_child) => {
                            child = new_child;
                            continue;
                        }
                        Err(e) => {
                            let _ = event_tx.send(NodeEvent::NodeErrored {
                                node_id,
                                message: format!("Failed to respawn after upgrade: {e}"),
                            });
                            let mut sup = supervisor.write().await;
                            sup.update_state(node_id, NodeStatus::Errored, None);
                            return;
                        }
                    }
                }
            }
            // Exit 0 but the binary didn't change — fall through to the crash / restart path.
            // We report the crash with the exit code preserved; the crash counter guards
            // against infinite restart loops if the process keeps exiting immediately.
        }

        // Crash (or clean exit that wasn't an upgrade)
        let _ = event_tx.send(NodeEvent::NodeCrashed { node_id, exit_code });

        let (should_restart, attempt, backoff) = {
            let mut sup = supervisor.write().await;
            sup.record_crash(node_id)
        };

        if !should_restart {
            let _ = event_tx.send(NodeEvent::NodeErrored {
                node_id,
                message: format!(
                    "Node crashed {} times within {} seconds, giving up",
                    MAX_CRASHES_BEFORE_ERRORED,
                    CRASH_WINDOW.as_secs()
                ),
            });
            return;
        }

        let _ = event_tx.send(NodeEvent::NodeRestarting { node_id, attempt });

        tokio::time::sleep(backoff).await;

        // Try to restart
        match spawn_node_from_config(&*config).await {
            Ok(new_child) => {
                let pid = match new_child.id() {
                    Some(pid) => pid,
                    None => {
                        // Process exited before we could read its PID
                        let _ = event_tx.send(NodeEvent::NodeErrored {
                            node_id,
                            message: "Restarted process exited before PID could be read"
                                .to_string(),
                        });
                        let mut sup = supervisor.write().await;
                        sup.update_state(node_id, NodeStatus::Errored, None);
                        return;
                    }
                };
                {
                    let mut sup = supervisor.write().await;
                    sup.update_state(node_id, NodeStatus::Running, Some(pid));
                }
                let _ = event_tx.send(NodeEvent::NodeStarted { node_id, pid });
                child = new_child;
            }
            Err(e) => {
                let _ = event_tx.send(NodeEvent::NodeErrored {
                    node_id,
                    message: format!("Failed to restart node: {e}"),
                });
                let mut sup = supervisor.write().await;
                sup.update_state(node_id, NodeStatus::Errored, None);
                return;
            }
        }
    }
}

/// Respawn a node whose `UpgradeScheduled` status tells us the exit was expected.
///
/// On success: persists the new version to the registry, updates the in-memory config clone,
/// clears pending_version, sets status back to Running, and fires `NodeUpgraded`.
async fn respawn_upgraded_node(
    config: &mut NodeConfig,
    supervisor: &Arc<RwLock<Supervisor>>,
    registry: &Arc<RwLock<NodeRegistry>>,
    event_tx: &broadcast::Sender<NodeEvent>,
) -> Result<tokio::process::Child> {
    let node_id = config.id;
    let old_version = config.version.clone();

    let new_child = spawn_node_from_config(config).await?;
    let pid = new_child
        .id()
        .ok_or_else(|| Error::ProcessSpawn("Failed to get PID after upgrade respawn".into()))?;

    // Read the new version from the replaced binary. If this fails we still consider the respawn
    // successful; we just don't refresh the recorded version this round.
    let new_version = extract_version(&config.binary_path).await.ok();

    if let Some(ref version) = new_version {
        config.version = version.clone();
        let mut reg = registry.write().await;
        if let Ok(stored) = reg.get_mut(node_id) {
            stored.version = version.clone();
            let _ = reg.save();
        }
    }

    {
        let mut sup = supervisor.write().await;
        if let Some(state) = sup.node_states.get_mut(&node_id) {
            state.status = NodeStatus::Running;
            state.pid = Some(pid);
            state.started_at = Some(Instant::now());
            state.pending_version = None;
            state.restart_count = 0;
            state.first_crash_at = None;
        }
    }

    let _ = event_tx.send(NodeEvent::NodeStarted { node_id, pid });
    if let Some(version) = new_version {
        let _ = event_tx.send(NodeEvent::NodeUpgraded {
            node_id,
            old_version,
            new_version: version,
        });
    }

    Ok(new_child)
}

/// Timeout for graceful shutdown before force-killing.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Send SIGTERM to a process, wait for it to exit, and SIGKILL if it doesn't.
async fn graceful_kill(pid: u32) {
    send_signal_term(pid);

    // Poll for process exit
    let start = Instant::now();
    loop {
        if !is_process_alive(pid) {
            return;
        }
        if start.elapsed() >= GRACEFUL_SHUTDOWN_TIMEOUT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Force kill if still alive
    send_signal_kill(pid);

    // Brief wait for force kill to take effect
    for _ in 0..10 {
        if !is_process_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll each Running node's PID for OS liveness every `LIVENESS_POLL_INTERVAL`,
/// flipping dead ones to `Stopped` and emitting `NodeStopped`.
///
/// Exists to detect exits of nodes adopted across a daemon restart
/// (`Supervisor::adopt_from_registry`). Daemon-spawned nodes have a
/// `monitor_node` task awaiting on the owned `Child` handle, which detects
/// exit immediately — the poll is redundant-but-harmless for them. Adopted
/// nodes don't have a `Child` (it died with the previous daemon), so the poll
/// is the only way the supervisor learns that one has exited.
///
/// The task terminates when `shutdown` is cancelled.
pub fn spawn_liveness_monitor(
    registry: Arc<RwLock<NodeRegistry>>,
    supervisor: Arc<RwLock<Supervisor>>,
    event_tx: broadcast::Sender<NodeEvent>,
    interval: Duration,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => {}
            }

            // Snapshot candidates to release locks before the per-process syscalls.
            let candidates: Vec<(u32, u32, PathBuf)> = {
                let sup = supervisor.read().await;
                let reg = registry.read().await;
                reg.list()
                    .into_iter()
                    .filter_map(|config| {
                        let pid = sup.node_pid(config.id)?;
                        matches!(sup.node_status(config.id), Ok(NodeStatus::Running))
                            .then_some((config.id, pid, config.data_dir.clone()))
                    })
                    .collect()
            };

            for (node_id, pid, data_dir) in candidates {
                if is_process_alive(pid) {
                    continue;
                }
                let mut sup = supervisor.write().await;
                // Re-check under the write lock to avoid racing with a concurrent
                // start/stop that flipped the state between the snapshot and now.
                if !matches!(sup.node_status(node_id), Ok(NodeStatus::Running)) {
                    continue;
                }
                sup.update_state(node_id, NodeStatus::Stopped, None);
                let _ = event_tx.send(NodeEvent::NodeStopped { node_id });
                remove_node_pid(&data_dir);
            }
        }
    });
}

#[cfg(unix)]
fn pid_to_i32(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|&p| p > 0)
}

#[cfg(unix)]
fn send_signal_term(pid: u32) {
    if let Some(pid) = pid_to_i32(pid) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

#[cfg(unix)]
fn send_signal_kill(pid: u32) {
    if let Some(pid) = pid_to_i32(pid) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
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
fn send_signal_term(pid: u32) {
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_C_EVENT,
    };

    unsafe {
        // Detach from our own console (no-op if daemon has none, which is
        // typical since it's spawned with DETACHED_PROCESS).
        FreeConsole();

        // Attach to the target process's console and send Ctrl+C
        if AttachConsole(pid) != 0 {
            // Disable Ctrl+C handling so GenerateConsoleCtrlEvent doesn't
            // terminate us while we're attached to the node's console.
            SetConsoleCtrlHandler(None, 1);
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
            // Detach from the node's console first — once detached, the
            // async Ctrl+C event can only reach the node, not us.
            FreeConsole();
            // Brief delay to let the event drain before re-enabling our
            // handler. Without this, the handler thread can process the
            // event between FreeConsole and SetConsoleCtrlHandler.
            std::thread::sleep(std::time::Duration::from_millis(50));
            // Restore Ctrl+C handling so `daemon run` (foreground mode)
            // can still be stopped via Ctrl+C / tokio::signal::ctrl_c().
            SetConsoleCtrlHandler(None, 0);
        }
    }
}

#[cfg(windows)]
fn send_signal_kill(pid: u32) {
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

    #[test]
    fn build_node_args_basic() {
        let config = NodeConfig {
            id: 1,
            service_name: "node1".to_string(),
            rewards_address: "0xabc123".to_string(),
            data_dir: "/data/node-1".into(),
            log_dir: Some("/logs/node-1".into()),
            node_port: Some(12000),
            metrics_port: Some(13000),
            network_id: Some(1),
            binary_path: "/bin/node".into(),
            version: "0.1.0".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: vec!["peer1".to_string(), "peer2".to_string()],
        };

        let args = build_node_args(&config);

        assert!(args.contains(&"--rewards-address".to_string()));
        assert!(args.contains(&"0xabc123".to_string()));
        assert!(args.contains(&"--root-dir".to_string()));
        assert!(args.contains(&"/data/node-1".to_string()));
        assert!(args.contains(&"--enable-logging".to_string()));
        assert!(args.contains(&"--log-dir".to_string()));
        assert!(args.contains(&"/logs/node-1".to_string()));
        assert!(args.contains(&"--port".to_string()));
        assert!(args.contains(&"12000".to_string()));
        assert!(args.contains(&"--metrics-port".to_string()));
        assert!(args.contains(&"13000".to_string()));
        assert!(args.contains(&"--bootstrap".to_string()));
        assert!(args.contains(&"peer1".to_string()));
        assert!(args.contains(&"peer2".to_string()));
        assert!(args.contains(&"--stop-on-upgrade".to_string()));
    }

    #[test]
    fn build_node_args_minimal() {
        let config = NodeConfig {
            id: 1,
            service_name: "node1".to_string(),
            rewards_address: "0xabc".to_string(),
            data_dir: "/data/node-1".into(),
            log_dir: None,
            node_port: None,
            metrics_port: None,
            network_id: None,
            binary_path: "/bin/node".into(),
            version: "0.1.0".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: vec![],
        };

        let args = build_node_args(&config);

        assert!(args.contains(&"--rewards-address".to_string()));
        assert!(args.contains(&"--root-dir".to_string()));
        assert!(!args.contains(&"--enable-logging".to_string()));
        assert!(!args.contains(&"--log-dir".to_string()));
        assert!(!args.contains(&"--port".to_string()));
        assert!(!args.contains(&"--metrics-port".to_string()));
        assert!(!args.contains(&"--bootstrap".to_string()));
        assert!(args.contains(&"--stop-on-upgrade".to_string()));
    }

    #[test]
    fn record_crash_backoff_increases() {
        let (tx, _rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        // Insert a running node
        sup.node_states.insert(
            1,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: Some(100),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );

        let (should_restart, attempt, backoff) = sup.record_crash(1);
        assert!(should_restart);
        assert_eq!(attempt, 1);
        assert_eq!(backoff, Duration::from_secs(1));

        let (should_restart, attempt, backoff) = sup.record_crash(1);
        assert!(should_restart);
        assert_eq!(attempt, 2);
        assert_eq!(backoff, Duration::from_secs(2));

        let (should_restart, attempt, backoff) = sup.record_crash(1);
        assert!(should_restart);
        assert_eq!(attempt, 3);
        assert_eq!(backoff, Duration::from_secs(4));

        let (should_restart, attempt, backoff) = sup.record_crash(1);
        assert!(should_restart);
        assert_eq!(attempt, 4);
        assert_eq!(backoff, Duration::from_secs(8));

        // 5th crash within window → errored
        let (should_restart, attempt, _) = sup.record_crash(1);
        assert!(!should_restart);
        assert_eq!(attempt, 5);
        assert_eq!(sup.node_states[&1].status, NodeStatus::Errored);
    }

    #[test]
    fn node_counts_tracks_states() {
        let (tx, _rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        sup.node_states.insert(
            1,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: Some(100),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );
        sup.node_states.insert(
            2,
            NodeRuntime {
                status: NodeStatus::Stopped,
                pid: None,
                started_at: None,
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );
        sup.node_states.insert(
            3,
            NodeRuntime {
                status: NodeStatus::Errored,
                pid: None,
                started_at: None,
                restart_count: 5,
                first_crash_at: None,
                pending_version: None,
            },
        );

        let (running, stopped, errored) = sup.node_counts();
        assert_eq!(running, 1);
        assert_eq!(stopped, 1);
        assert_eq!(errored, 1);
    }

    #[test]
    fn mark_upgrade_scheduled_only_affects_running_nodes() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        sup.node_states.insert(
            1,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: Some(111),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );
        sup.node_states.insert(
            2,
            NodeRuntime {
                status: NodeStatus::Stopped,
                pid: None,
                started_at: None,
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );

        // Running node: transitions to UpgradeScheduled with pending_version set and event fires.
        let affected = sup.mark_upgrade_scheduled(1, "0.10.11-rc.1".to_string());
        assert!(affected);
        assert_eq!(sup.node_status(1).unwrap(), NodeStatus::UpgradeScheduled);
        assert_eq!(sup.node_pending_version(1).as_deref(), Some("0.10.11-rc.1"));
        match rx.try_recv() {
            Ok(NodeEvent::UpgradeScheduled {
                node_id,
                pending_version,
            }) => {
                assert_eq!(node_id, 1);
                assert_eq!(pending_version, "0.10.11-rc.1");
            }
            other => panic!("expected UpgradeScheduled event, got {other:?}"),
        }

        // Stopped node: untouched, no event fired.
        let affected = sup.mark_upgrade_scheduled(2, "0.10.11-rc.1".to_string());
        assert!(!affected);
        assert_eq!(sup.node_status(2).unwrap(), NodeStatus::Stopped);
        assert!(sup.node_pending_version(2).is_none());

        // Already-UpgradeScheduled node: calling again is a no-op.
        let affected = sup.mark_upgrade_scheduled(1, "0.10.12".to_string());
        assert!(!affected);
        // Pending version is the original one set.
        assert_eq!(sup.node_pending_version(1).as_deref(), Some("0.10.11-rc.1"));
    }

    #[test]
    fn node_counts_counts_upgrade_scheduled_as_running() {
        let (tx, _rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        sup.node_states.insert(
            1,
            NodeRuntime {
                status: NodeStatus::UpgradeScheduled,
                pid: Some(111),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
                pending_version: Some("0.10.11-rc.1".to_string()),
            },
        );

        let (running, stopped, errored) = sup.node_counts();
        assert_eq!(running, 1);
        assert_eq!(stopped, 0);
        assert_eq!(errored, 0);
    }

    #[tokio::test]
    async fn stop_node_not_found() {
        let (tx, _rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        let result = sup.stop_node(999).await;
        assert!(matches!(result, Err(Error::NodeNotFound(999))));
    }

    #[tokio::test]
    async fn stop_node_not_running() {
        let (tx, _rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        sup.node_states.insert(
            1,
            NodeRuntime {
                status: NodeStatus::Stopped,
                pid: None,
                started_at: None,
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );

        let result = sup.stop_node(1).await;
        assert!(matches!(result, Err(Error::NodeNotRunning(1))));
    }

    #[tokio::test]
    async fn stop_all_nodes_mixed_states() {
        let (tx, _rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        // Node 1: running (but with a fake PID that won't exist)
        sup.node_states.insert(
            1,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: Some(999999),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );
        // Node 2: already stopped
        sup.node_states.insert(
            2,
            NodeRuntime {
                status: NodeStatus::Stopped,
                pid: None,
                started_at: None,
                restart_count: 0,
                first_crash_at: None,
                pending_version: None,
            },
        );

        let configs = vec![(1, "node1".to_string()), (2, "node2".to_string())];

        let result = sup.stop_all_nodes(&configs).await;

        assert_eq!(result.stopped.len(), 1);
        assert_eq!(result.stopped[0].node_id, 1);
        assert_eq!(result.stopped[0].service_name, "node1");
        assert_eq!(result.already_stopped, vec![2]);
        assert!(result.failed.is_empty());
    }
}
