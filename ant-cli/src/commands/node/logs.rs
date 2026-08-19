use clap::{Args, Subcommand};
use colored::Colorize;

use ant_core::node::daemon::client;
use ant_core::node::daemon::forward::{
    apply_enable, classify_nodes, LogForwardConfig, LogForwardEnableRequest, LogForwardResult,
    LogForwardStatus, LogLevel,
};
use ant_core::node::registry::NodeRegistry;
use ant_core::node::types::DaemonConfig;

#[derive(Subcommand)]
pub enum LogsCommand {
    /// Forward node logs to the Autonomi beta log endpoint
    Forward {
        #[command(subcommand)]
        command: ForwardCommand,
    },
}

#[derive(Subcommand)]
pub enum ForwardCommand {
    /// Start forwarding this machine's node logs. Running this is your consent.
    Enable(EnableArgs),
    /// Stop forwarding. Nothing else about your nodes changes.
    Disable,
    /// Show whether forwarding is on and what it has shipped
    Status,
}

#[derive(Args)]
pub struct EnableArgs {
    /// Write-only token issued for the beta programme. Only needed the first time — re-enabling
    /// after a disable reuses the stored one.
    #[arg(long)]
    pub token: Option<String>,

    /// Override the endpoint logs are shipped to. Intended for testing against a local sink.
    #[arg(long)]
    pub endpoint: Option<String>,

    /// Lowest level to forward: trace, debug, info, warn or error. Defaults to info.
    #[arg(long)]
    pub level: Option<LogLevel>,
}

impl LogsCommand {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        match self {
            Self::Forward { command } => command.execute(json_output).await,
        }
    }
}

impl ForwardCommand {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let config = DaemonConfig::default();

        match self {
            Self::Enable(args) => enable(&config, args, json_output).await,
            Self::Disable => disable(&config, json_output).await,
            Self::Status => status(&config, json_output).await,
        }
    }
}

/// Enable forwarding.
///
/// Dual-path, as elsewhere in the CLI: with the daemon up, this goes through its API so shipping
/// starts at once. With the daemon down the consent is still recorded — the config file is the
/// source of truth and the daemon picks it up when it next starts — and the output says so rather
/// than implying logs are already flowing.
async fn enable(daemon: &DaemonConfig, args: EnableArgs, json_output: bool) -> anyhow::Result<()> {
    let request = LogForwardEnableRequest {
        token: args.token,
        endpoint: args.endpoint,
        min_level: args.level,
    };

    let result = if client::status(daemon).await?.running {
        client::log_forward_enable(daemon, &request).await?
    } else {
        enable_without_daemon(daemon, &request)?
    };

    print_result(&result, json_output)
}

/// Record the opt-in directly, for when the daemon is not running.
fn enable_without_daemon(
    daemon: &DaemonConfig,
    request: &LogForwardEnableRequest,
) -> anyhow::Result<LogForwardResult> {
    let path = LogForwardConfig::default_path()?;
    let stored = LogForwardConfig::load(&path)?;
    let was_enabled = stored.enabled;

    let config = apply_enable(&stored, request)?;
    config.save(&path)?;

    let registry = NodeRegistry::load(&daemon.registry_path)?;
    let (nodes_forwarding, nodes_skipped) = classify_nodes(&registry);

    Ok(LogForwardResult {
        enabled: true,
        already_in_state: was_enabled,
        endpoint: config.endpoint.clone(),
        min_level: config.min_level,
        nodes_forwarding,
        nodes_skipped,
        pending_daemon_start: true,
    })
}

async fn disable(daemon: &DaemonConfig, json_output: bool) -> anyhow::Result<()> {
    let result = if client::status(daemon).await?.running {
        client::log_forward_disable(daemon).await?
    } else {
        let path = LogForwardConfig::default_path()?;
        let mut config = LogForwardConfig::load(&path)?;
        let was_enabled = config.enabled;
        config.enabled = false;
        config.save(&path)?;

        LogForwardResult {
            enabled: false,
            already_in_state: !was_enabled,
            endpoint: config.endpoint.clone(),
            min_level: config.min_level,
            nodes_forwarding: Vec::new(),
            nodes_skipped: Vec::new(),
            pending_daemon_start: false,
        }
    };

    print_result(&result, json_output)
}

async fn status(daemon: &DaemonConfig, json_output: bool) -> anyhow::Result<()> {
    let status = if client::status(daemon).await?.running {
        client::log_forward_status(daemon).await?
    } else {
        let path = LogForwardConfig::default_path()?;
        let config = LogForwardConfig::load(&path)?;
        let mut status = LogForwardStatus::inactive(&config);

        let registry = NodeRegistry::load(&daemon.registry_path)?;
        let (forwarding, skipped) = classify_nodes(&registry);
        status.nodes_forwarding = forwarding;
        status.nodes_skipped = skipped;
        status
    };

    print_status(&status, json_output)
}

fn print_result(result: &LogForwardResult, json_output: bool) -> anyhow::Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(result)?);
        return Ok(());
    }

    if !result.enabled {
        if result.already_in_state {
            println!("{} Log forwarding was already off", "●".yellow());
        } else {
            println!("{} Log forwarding stopped", "✓".green().bold());
            println!("  {}", "Your nodes are otherwise unchanged.".dimmed());
        }
        return Ok(());
    }

    if result.already_in_state {
        println!(
            "{} Log forwarding was already on — settings updated",
            "●".yellow()
        );
    } else {
        println!("{} Log forwarding enabled", "✓".green().bold());
    }
    println!(
        "  Endpoint: {}   Level: {} and above",
        result.endpoint.cyan(),
        result.min_level.to_string().cyan()
    );

    print_node_lists(&result.nodes_forwarding, &result.nodes_skipped);

    if result.pending_daemon_start {
        println!(
            "\n{} The daemon is not running, so nothing is being shipped yet.",
            "●".yellow()
        );
        println!(
            "  Forwarding starts when you run: {}",
            "ant node daemon start".cyan()
        );
    }

    Ok(())
}

fn print_status(status: &LogForwardStatus, json_output: bool) -> anyhow::Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }

    if status.enabled {
        let state = if status.active {
            "on".green().bold()
        } else {
            "on (not yet started)".yellow().bold()
        };
        println!("Log forwarding: {state}");
    } else {
        println!("Log forwarding: {}", "off".dimmed().bold());
        println!(
            "  Enable it with: {}",
            "ant node logs forward enable --token <token>".cyan()
        );
        return Ok(());
    }

    println!(
        "  Endpoint: {}   Level: {} and above",
        status.endpoint.cyan(),
        status.min_level.to_string().cyan()
    );
    if let Some(fingerprint) = &status.token_fingerprint {
        println!("  Token:    {}", fingerprint.dimmed());
    }

    print_node_lists(&status.nodes_forwarding, &status.nodes_skipped);

    let stats = &status.stats;
    println!("\n  {}", "Delivery".bold());
    println!(
        "    Forwarded: {}   Batches: {} sent, {} failed",
        stats.events_forwarded.to_string().cyan(),
        stats.batches_sent.to_string().cyan(),
        stats.batches_failed.to_string().cyan(),
    );
    if stats.events_dropped_by_level > 0 || stats.events_dropped_by_overflow > 0 {
        println!(
            "    Dropped:   {} below level, {} to keep memory bounded",
            stats.events_dropped_by_level.to_string().dimmed(),
            stats.events_dropped_by_overflow.to_string().dimmed(),
        );
    }
    if let Some(error) = &stats.last_error {
        println!("    {} {}", "Last error:".red(), error.red());
    }

    Ok(())
}

fn print_node_lists(
    forwarding: &[ant_core::node::daemon::forward::ForwardingNode],
    skipped: &[ant_core::node::daemon::forward::SkippedNode],
) {
    if forwarding.is_empty() {
        println!("\n  {} No nodes have logging enabled.", "●".yellow());
    } else {
        println!("\n  {} ({})", "Forwarding".bold(), forwarding.len());
        for node in forwarding {
            println!(
                "    {} {} ({})",
                "●".green(),
                node.service.bold(),
                node.node_id.to_string().dimmed()
            );
        }
    }

    if !skipped.is_empty() {
        println!("\n  {} ({})", "Not forwarding".bold(), skipped.len());
        for node in skipped {
            println!(
                "    {} {} ({}) — {}",
                "○".yellow(),
                node.service.bold(),
                node.node_id.to_string().dimmed(),
                node.reason.dimmed()
            );
        }
        println!(
            "\n  {}",
            "Node logging is off unless a node was added with --log-dir-path.".dimmed()
        );
    }
}
