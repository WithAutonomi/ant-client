//! Local devnet launcher for development and testing.
//!
//! Wraps [`ant_node::devnet::Devnet`] and `evmlib::testnet::Testnet` (Anvil)
//! to spin up a local network with EVM payments in a few lines of code.

use crate::data::client::ClientConfig;
use crate::data::error::{Error, Result};
use crate::data::Client;
use ant_node::core::MultiAddr;
use ant_node::devnet::{Devnet, DevnetConfig, DevnetEvmInfo, DevnetManifest};
use evmlib::testnet::Testnet;
use evmlib::wallet::Wallet;
use evmlib::Network as EvmNetwork;
use std::path::Path;
use std::time::SystemTime;
use tracing::info;

/// A local devnet with embedded Anvil EVM blockchain.
///
/// Keeps the `Testnet` (Anvil) instance alive for the entire lifetime
/// so that the EVM RPC endpoint remains available.
pub struct LocalDevnet {
    devnet: Devnet,
    // Stored to keep Anvil alive — dropping Testnet kills the Anvil process.
    _testnet: Testnet,
    manifest: DevnetManifest,
    evm_network: EvmNetwork,
    wallet_private_key: String,
    bootstrap: Vec<MultiAddr>,
}

impl LocalDevnet {
    /// Start a devnet with the given configuration.
    ///
    /// Spins up a local Anvil EVM blockchain, configures the devnet
    /// for EVM payment enforcement, and starts all nodes.
    ///
    /// # Errors
    ///
    /// Returns an error if the devnet fails to start or stabilize.
    pub async fn start(mut config: DevnetConfig) -> Result<Self> {
        info!("Starting local Anvil blockchain...");
        let testnet = Testnet::new().await;
        let network = testnet.to_network();
        let wallet_key = testnet.default_wallet_private_key();

        let (rpc_url, token_addr, payments_addr, merkle_addr) =
            extract_custom_network_info(&network)?;

        config.evm_network = Some(network.clone());

        info!("Anvil running at {rpc_url}");

        let mut devnet = Devnet::new(config)
            .await
            .map_err(|e| Error::Config(format!("devnet creation failed: {e}")))?;

        devnet
            .start()
            .await
            .map_err(|e| Error::Network(format!("devnet start failed: {e}")))?;

        let bootstrap = devnet.bootstrap_addrs();

        let evm_info = DevnetEvmInfo {
            rpc_url,
            wallet_private_key: wallet_key.clone(),
            payment_token_address: token_addr,
            data_payments_address: payments_addr,
            merkle_payments_address: merkle_addr,
        };

        let manifest = DevnetManifest {
            base_port: devnet.config().base_port,
            node_count: devnet.config().node_count,
            bootstrap: bootstrap.clone(),
            data_dir: devnet.config().data_dir.clone(),
            created_at: current_timestamp(),
            evm: Some(evm_info),
        };

        info!(
            "Devnet running: {node_count} nodes, bootstrap: {bootstrap:?}",
            node_count = devnet.config().node_count,
        );

        Ok(Self {
            devnet,
            _testnet: testnet,
            manifest,
            evm_network: network,
            wallet_private_key: wallet_key,
            bootstrap,
        })
    }

    /// Start a minimal devnet (5 nodes).
    ///
    /// # Errors
    ///
    /// Returns an error if the devnet fails to start.
    pub async fn start_minimal() -> Result<Self> {
        Self::start(DevnetConfig::minimal()).await
    }

    /// Start a small devnet (10 nodes).
    ///
    /// # Errors
    ///
    /// Returns an error if the devnet fails to start.
    pub async fn start_small() -> Result<Self> {
        Self::start(DevnetConfig::small()).await
    }

    /// Bootstrap peer addresses for connecting clients.
    #[must_use]
    pub fn bootstrap_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.bootstrap
            .iter()
            .filter_map(MultiAddr::socket_addr)
            .collect()
    }

    /// The custom EVM network (Anvil).
    #[must_use]
    pub fn evm_network(&self) -> &EvmNetwork {
        &self.evm_network
    }

    /// The funded wallet private key (hex, with 0x prefix).
    #[must_use]
    pub fn wallet_private_key(&self) -> &str {
        &self.wallet_private_key
    }

    /// The devnet manifest (serializable to JSON).
    #[must_use]
    pub fn manifest(&self) -> &DevnetManifest {
        &self.manifest
    }

    /// Write the manifest to a JSON file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub async fn write_manifest(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.manifest)
            .map_err(|e| Error::Serialization(format!("manifest serialization failed: {e}")))?;
        tokio::fs::write(path, json).await?;
        info!("Wrote manifest to {}", path.display());
        Ok(())
    }

    /// Create a funded client connected to this devnet, ready for uploads.
    ///
    /// Connects to bootstrap peers, creates a wallet from the funded key,
    /// and approves token spend.
    ///
    /// # Errors
    ///
    /// Returns an error if connection, wallet creation, or approval fails.
    pub async fn create_funded_client(&self) -> Result<Client> {
        let addrs = self.bootstrap_addrs();
        let client = Client::connect(&addrs, ClientConfig::default()).await?;

        let key = self.wallet_private_key.trim_start_matches("0x").to_string();
        let wallet = Wallet::new_from_private_key(self.evm_network.clone(), &key)
            .map_err(|e| Error::Payment(format!("wallet creation failed: {e}")))?;

        let client = client.with_wallet(wallet);
        client.approve_token_spend().await?;
        Ok(client)
    }

    /// Shut down the devnet and all nodes.
    ///
    /// # Errors
    ///
    /// Returns an error if shutdown fails.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.devnet
            .shutdown()
            .await
            .map_err(|e| Error::Network(format!("devnet shutdown failed: {e}")))?;
        info!("Devnet shut down");
        Ok(())
    }
}

/// Extract RPC URL, token address, payments address, and merkle address from a Custom network.
fn extract_custom_network_info(
    network: &EvmNetwork,
) -> Result<(String, String, String, Option<String>)> {
    match network {
        EvmNetwork::Custom(custom) => {
            let token = custom.payment_token_address;
            let payments = custom.data_payments_address;
            Ok((
                custom.rpc_url_http.to_string(),
                format!("{token:?}"),
                format!("{payments:?}"),
                custom
                    .merkle_payments_address
                    .map(|addr| format!("{addr:?}")),
            ))
        }
        _ => Err(Error::Config(
            "Anvil testnet returned non-Custom network".to_string(),
        )),
    }
}

/// Get a simple ISO-8601 timestamp string.
fn current_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();

    format!("{secs}")
}
