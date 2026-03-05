use clap::Args;

use ant_core::node::daemon::client;
use ant_core::node::types::DaemonConfig;

#[derive(Args)]
pub struct StartArgs {
    /// Start a specific node by service name (e.g., node1). If omitted, starts all nodes.
    #[arg(long)]
    pub service_name: Option<String>,
}

impl StartArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let config = DaemonConfig::default();

        // Verify daemon is running
        let status = client::status(&config).await?;
        if !status.running {
            anyhow::bail!("The daemon is not running. Start it first with: ant node daemon start");
        }

        match self.service_name {
            Some(ref name) => self.start_single(&config, name, json_output).await,
            None => self.start_all(&config, json_output).await,
        }
    }

    async fn start_single(
        &self,
        config: &DaemonConfig,
        service_name: &str,
        json_output: bool,
    ) -> anyhow::Result<()> {
        // Look up node ID by service name via the registry
        let registry = ant_core::node::registry::NodeRegistry::load(&config.registry_path)?;
        let node = registry
            .find_by_service_name(service_name)
            .ok_or_else(|| anyhow::anyhow!("No node found with service name '{service_name}'"))?;
        let node_id = node.id;

        let result = client::start_node(config, node_id).await?;

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!(
                "Node {} ({}) started (PID {})",
                result.service_name, result.node_id, result.pid
            );
        }

        Ok(())
    }

    async fn start_all(&self, config: &DaemonConfig, json_output: bool) -> anyhow::Result<()> {
        let result = client::start_all_nodes(config).await?;

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            if !result.started.is_empty() {
                println!("Started {} node(s):", result.started.len());
                for node in &result.started {
                    println!(
                        "  {} ({}) — PID {}",
                        node.service_name, node.node_id, node.pid
                    );
                }
            }
            if !result.already_running.is_empty() {
                println!("Already running: {} node(s)", result.already_running.len());
                for id in &result.already_running {
                    println!("  Node {id}");
                }
            }
            if !result.failed.is_empty() {
                println!("Failed to start {} node(s):", result.failed.len());
                for fail in &result.failed {
                    println!(
                        "  {} ({}) — {}",
                        fail.service_name, fail.node_id, fail.error
                    );
                }
            }
            if result.started.is_empty()
                && result.already_running.is_empty()
                && result.failed.is_empty()
            {
                println!("No nodes registered. Add nodes first with: ant node add");
            }
        }

        Ok(())
    }
}
