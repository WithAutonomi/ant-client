use std::net::IpAddr;

use clap::{Args, Subcommand};
use colored::Colorize;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use ant_core::node::daemon::client;
use ant_core::node::types::DaemonConfig;

/// Overrides shared by `daemon start` and `daemon run`.
#[derive(Args, Clone, Debug, Default)]
pub struct BindArgs {
    /// Write the daemon's own log to this path.
    ///
    /// Unset by default, in which case the daemon logs nothing. The daemon is a long-running
    /// background process, so this is the only way to see what its supervisor did — which node
    /// exited, whether an exit was treated as an auto-upgrade or a crash, and whether it was
    /// restarted.
    ///
    /// The path names the log family rather than a single file: it rotates daily and keeps a
    /// month, so `--log-path ~/ant-daemon.log` writes `~/ant-daemon.2026-08-27.log`. A relative
    /// path is resolved against the current directory before being handed to the detached daemon,
    /// which does not share it.
    #[arg(long, value_name = "PATH")]
    pub log_path: Option<std::path::PathBuf>,

    /// Pin the daemon's HTTP port. Unset (default) lets the OS assign one;
    /// `0` is also accepted as an explicit OS-assigned request.
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Address the daemon binds to. Defaults to `127.0.0.1`.
    ///
    /// Binding to a non-loopback address (e.g. `0.0.0.0`) exposes node
    /// management to anyone who can reach the port. The daemon has no
    /// authentication — only do this when the network path is controlled
    /// (e.g. inside a container with an explicit port mapping).
    #[arg(long, value_name = "IP")]
    pub listen_addr: Option<IpAddr>,
}

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Launch the daemon as a detached background process
    Start(BindArgs),
    /// Shut down the running daemon
    Stop,
    /// Show whether the daemon is running and summary stats
    Status,
    /// Output connection details for programmatic use (always JSON)
    Info,
    /// Run the daemon in the foreground (used internally)
    #[command(hide = true)]
    Run(BindArgs),
}

/// Overlay user-provided bind overrides onto `DaemonConfig::default()`.
fn apply_bind_args(args: &BindArgs) -> DaemonConfig {
    let mut config = DaemonConfig::default();
    if let Some(port) = args.port {
        config.port = Some(port);
    }
    if let Some(addr) = args.listen_addr {
        config.listen_addr = addr;
    }
    // Absolute before it crosses the detach boundary: `daemon start` resolves a relative path
    // against the shell's directory, but the daemon it spawns does not inherit that context in any
    // way the user can reason about.
    config.log_path = args.log_path.as_ref().map(|path| absolute_path(path));
    config
}

/// Resolve `path` against the current directory if it is relative.
///
/// Falls back to the path as given if the current directory cannot be read — a wrong path the user
/// can see beats refusing to start the daemon over it.
fn absolute_path(path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// How many daily log files the daemon keeps.
///
/// A month, not a week. The behaviour this log exists to capture — an upgrade restart being
/// mistaken for a crash — can take well over a week to occur, and a retention window shorter than
/// the reproduction window deletes the evidence before anyone reads it.
///
/// Rotation is still daily rather than off, because the daemon's volume is not guaranteed small:
/// several of its warnings sit inside the forwarder's poll loop (`could not persist tail offsets`
/// fires once every `DEFAULT_POLL_INTERVAL`), so a persistent failure there produces thousands of
/// lines a day. Daily files bound what any one bad day can cost while still keeping a month.
const DAEMON_LOG_FILES_KEPT: usize = 30;

/// Start writing the daemon's log to a rotating file, returning the guard that must outlive it.
///
/// The returned `WorkerGuard` flushes the non-blocking writer when dropped; dropping it early
/// silently truncates the log, so the caller holds it for the lifetime of the daemon.
///
/// `RUST_LOG` still wins if set, so a participant can be asked for debug output without a new
/// build. Otherwise INFO, which is the level the supervisor's restart decisions are logged at.
fn init_daemon_file_logging(log_path: &std::path::Path) -> anyhow::Result<WorkerGuard> {
    let directory = log_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let prefix = log_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("daemon");
    std::fs::create_dir_all(directory)?;

    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log")
        .max_log_files(DAEMON_LOG_FILES_KEPT)
        .build(directory)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer().with_ansi(false).with_writer(writer))
        .with(filter)
        .init();

    Ok(guard)
}

fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let remaining = secs % 60;

    if days > 0 {
        format!("{days}d {hours}h {minutes}m {remaining}s")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {remaining}s")
    } else if minutes > 0 {
        format!("{minutes}m {remaining}s")
    } else {
        format!("{remaining}s")
    }
}

/// Get the actual port the daemon is listening on.
///
/// The `/api/v1/status` endpoint may report port 0 when the daemon was started
/// with an OS-assigned port (the default). Fall back to reading the port file
/// via `client::info()` which always has the real bound port.
fn resolve_port(config: &DaemonConfig, status_port: Option<u16>) -> Option<u16> {
    match status_port {
        Some(p) if p != 0 => Some(p),
        _ => client::info(config).port,
    }
}

impl DaemonCommand {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let config = match &self {
            DaemonCommand::Start(args) | DaemonCommand::Run(args) => apply_bind_args(args),
            _ => DaemonConfig::default(),
        };

        match self {
            DaemonCommand::Start(args) => {
                let result = client::start(&config).await?;
                if json_output {
                    println!("{}", serde_json::to_string(&result)?);
                } else if result.already_running {
                    let port = resolve_port(&config, result.port);
                    println!(
                        "{} Node management daemon already running (PID {})",
                        "●".yellow(),
                        result.pid.to_string().bold()
                    );
                    if let Some(p) = port {
                        println!("  {} http://127.0.0.1:{p}/console", "Console".dimmed());
                    }
                    if args.port.is_some() || args.listen_addr.is_some() {
                        println!(
                            "  {} the running daemon was started with different settings; \
                             stop it first to apply --port / --listen-addr",
                            "Note:".yellow()
                        );
                    }
                } else {
                    let pid = result.pid.to_string().bold();
                    let port = resolve_port(&config, result.port);
                    if let Some(ref log_path) = config.log_path {
                        // The directory, not the path as given: daily rotation means the live file
                        // is `<stem>.<date>.log`, so echoing the exact path back would point the
                        // user at a filename that never exists.
                        let dir = log_path.parent().unwrap_or(log_path).display().to_string();
                        println!("{} Daemon logs: {}", "●".green(), dir.cyan());
                    }
                    match port {
                        Some(p) => {
                            println!(
                                "{} Node management daemon started — PID {} on port {}",
                                "✓".green().bold(),
                                pid,
                                p.to_string().cyan()
                            );
                            println!("  {} http://127.0.0.1:{p}/console", "Console".dimmed());
                        }
                        None => println!(
                            "{} Node management daemon started — PID {} (port pending)",
                            "✓".green().bold(),
                            pid
                        ),
                    }
                }
            }
            DaemonCommand::Stop => {
                let result = client::stop(&config).await?;
                if json_output {
                    println!("{}", serde_json::to_string(&result)?);
                } else {
                    println!(
                        "{} Node management daemon stopped (was PID {})",
                        "✓".green().bold(),
                        result.pid.to_string().dimmed()
                    );
                }
            }
            DaemonCommand::Status => {
                let status = client::status(&config).await?;
                if json_output {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else if !status.running {
                    println!(
                        "{} Node management daemon is {}",
                        "●".red(),
                        "not running".red().bold()
                    );
                    println!("  Start it with: {}", "ant node daemon start".cyan());
                } else {
                    let port = resolve_port(&config, status.port);

                    println!(
                        "{} Node management daemon is {}",
                        "●".green(),
                        "running".green().bold()
                    );
                    println!();
                    if let Some(pid) = status.pid {
                        println!("  {}      {}", "PID".dimmed(), pid.to_string().bold());
                    }
                    if let Some(p) = port {
                        println!("  {}     {}", "Port".dimmed(), p.to_string().cyan());
                        println!("  {}  http://127.0.0.1:{p}/console", "Console".dimmed());
                    }
                    if let Some(uptime) = status.uptime_secs {
                        println!(
                            "  {}   {}",
                            "Uptime".dimmed(),
                            format_uptime(uptime).white()
                        );
                    }
                    println!();
                    println!(
                        "  {} {} total, {} running, {} stopped, {} errored",
                        "Nodes".dimmed(),
                        status.nodes_total.to_string().bold(),
                        status.nodes_running.to_string().green(),
                        status.nodes_stopped.to_string().yellow(),
                        if status.nodes_errored > 0 {
                            status.nodes_errored.to_string().red()
                        } else {
                            status.nodes_errored.to_string().dimmed()
                        }
                    );
                }
            }
            DaemonCommand::Info => {
                let info = client::info(&config);
                if json_output {
                    println!("{}", serde_json::to_string_pretty(&info)?);
                } else if !info.running {
                    println!(
                        "{} Node management daemon is {}",
                        "●".red(),
                        "not running".red().bold()
                    );
                    println!("  Start it with: {}", "ant node daemon start".cyan());
                } else {
                    println!(
                        "{} Node management daemon is {}",
                        "●".green(),
                        "running".green().bold()
                    );
                    println!();
                    if let Some(pid) = info.pid {
                        println!("  {}      {}", "PID".dimmed(), pid.to_string().bold());
                    }
                    if let Some(port) = info.port {
                        println!("  {}     {}", "Port".dimmed(), port.to_string().cyan());
                        println!("  {}  http://127.0.0.1:{port}/console", "Console".dimmed());
                    }
                    if let Some(ref api_base) = info.api_base {
                        println!("  {} {}", "API".dimmed(), api_base.cyan());
                    }
                }
            }
            DaemonCommand::Run(_) => {
                // Held for the whole run: dropping the guard stops the writer flushing.
                let _log_guard = if let Some(ref log_path) = config.log_path {
                    let guard = init_daemon_file_logging(log_path)?;
                    // A first line on every start, so the file is never silently empty and so the
                    // supervisor's later entries can be tied to a particular daemon instance.
                    tracing::info!(
                        "ant node daemon starting — version {}, pid {}",
                        env!("CARGO_PKG_VERSION"),
                        std::process::id()
                    );
                    Some(guard)
                } else {
                    None
                };
                client::run(config).await?;
            }
        }

        Ok(())
    }
}
