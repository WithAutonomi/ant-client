use clap::Subcommand;

use ant_core::node::daemon::client;
use ant_core::node::types::DaemonConfig;

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Launch the daemon as a detached background process
    Start,
    /// Shut down the running daemon
    Stop,
    /// Show whether the daemon is running and summary stats
    Status,
    /// Output connection details for programmatic use (always JSON)
    Info,
    /// Run the daemon in the foreground (used internally)
    #[command(hide = true)]
    Run,
}

impl DaemonCommand {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let config = DaemonConfig::default();

        match self {
            DaemonCommand::Start => {
                let result = client::start(&config).await?;
                if json_output {
                    println!("{}", serde_json::to_string(&result)?);
                } else if result.already_running {
                    println!("Daemon is already running (pid: {})", result.pid);
                } else {
                    match result.port {
                        Some(p) => println!("Daemon started (pid: {}, port: {p})", result.pid),
                        None => println!(
                            "Daemon started (pid: {}), port not yet available",
                            result.pid
                        ),
                    }
                }
            }
            DaemonCommand::Stop => {
                let result = client::stop(&config).await?;
                if json_output {
                    println!("{}", serde_json::to_string(&result)?);
                } else {
                    println!("Daemon stopped (pid: {})", result.pid);
                }
            }
            DaemonCommand::Status => {
                let status = client::status(&config).await?;
                if json_output {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else if !status.running {
                    println!("Daemon is not running");
                } else {
                    println!("Daemon is running");
                    if let Some(pid) = status.pid {
                        println!("  PID:           {pid}");
                    }
                    if let Some(port) = status.port {
                        println!("  Port:          {port}");
                    }
                    if let Some(uptime) = status.uptime_secs {
                        println!("  Uptime:        {uptime}s");
                    }
                    println!("  Nodes total:   {}", status.nodes_total);
                    println!("  Nodes running: {}", status.nodes_running);
                    println!("  Nodes stopped: {}", status.nodes_stopped);
                    println!("  Nodes errored: {}", status.nodes_errored);
                }
            }
            DaemonCommand::Info => {
                let info = client::info(&config);
                // Always JSON output regardless of --json flag
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
            DaemonCommand::Run => {
                client::run(config).await?;
            }
        }

        Ok(())
    }
}
