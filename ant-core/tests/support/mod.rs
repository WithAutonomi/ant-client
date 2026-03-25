//! Minimal test infrastructure for ant-core E2E tests.
//!
//! Spawns a small local testnet with `AntProtocol` handlers and an Anvil
//! EVM testnet for real on-chain payment verification.

// This module is compiled into every [[test]] binary separately.
// Each binary uses a different subset of methods, so Rust flags
// the unused ones as dead code. All items ARE used by at least
// one test binary.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::used_underscore_binding
)]

use ant_evm::RewardsAddress;
use ant_node::ant_protocol::MAX_WIRE_MESSAGE_SIZE;
use ant_node::core::{
    CoreNodeConfig, IPDiversityConfig, MlDsa65, MultiAddr, NodeIdentity, P2PEvent, P2PNode,
};
use ant_node::payment::{
    EvmVerifierConfig, PaymentVerifier, PaymentVerifierConfig, QuoteGenerator,
    QuotingMetricsTracker,
};
use ant_node::storage::{AntProtocol, LmdbStorage, LmdbStorageConfig};
use evmlib::testnet::Testnet;
use evmlib::wallet::Wallet;
use evmlib::Network as EvmNetwork;
use rand::Rng;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

const TEST_PORT_RANGE_MIN: u16 = 20_000;
const TEST_PORT_RANGE_MAX: u16 = 60_000;
const BOOTSTRAP_COUNT: usize = 2;
const SPAWN_DELAY_MS: u64 = 200;
const STABILIZATION_TIMEOUT_SECS: u64 = 180;

/// Test rewards address (20 bytes, all 0x01).
const TEST_REWARDS_ADDRESS: [u8; 20] = [0x01; 20];
/// Max records for quoting metrics.
const TEST_MAX_RECORDS: usize = 1280;
/// Initial records for quoting metrics.
const TEST_INITIAL_RECORDS: usize = 1000;

pub struct TestNode {
    pub p2p_node: Option<Arc<P2PNode>>,
    pub protocol: Option<Arc<AntProtocol>>,
    _handler_task: Option<tokio::task::JoinHandle<()>>,
}

pub struct MiniTestnet {
    pub nodes: Vec<TestNode>,
    _temp_dirs: Vec<tempfile::TempDir>,
    /// Keeps the Anvil process alive for the lifetime of the testnet.
    _testnet: Testnet,
    wallet: Wallet,
    evm_network: EvmNetwork,
}

impl MiniTestnet {
    /// Start a testnet with the given number of nodes.
    ///
    /// Use 6 for standard tests, 35+ for merkle tests (need 16 peers per pool).
    pub async fn start(node_count: usize) -> Self {
        // Start Anvil EVM testnet FIRST
        let testnet = Testnet::new().await;
        let evm_network = testnet.to_network();

        // Create funded wallet from the same Anvil instance
        let private_key = testnet.default_wallet_private_key();
        let wallet = Wallet::new_from_private_key(evm_network.clone(), &private_key)
            .expect("create funded wallet");

        let bootstrap_count = BOOTSTRAP_COUNT.min(node_count);
        let base_port = rand::thread_rng()
            .gen_range(TEST_PORT_RANGE_MIN..TEST_PORT_RANGE_MAX - node_count as u16);
        let mut nodes = Vec::with_capacity(node_count);
        let mut temp_dirs = Vec::with_capacity(node_count);
        let mut bootstrap_addrs = Vec::new();

        // Phase 1: Spawn bootstrap nodes
        for i in 0..bootstrap_count {
            let port = base_port + i as u16;
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

            let temp_dir = tempfile::TempDir::new().expect("create temp dir");
            let (node, protocol, handler) =
                Self::spawn_node(addr, &bootstrap_addrs, temp_dir.path(), &evm_network, i).await;

            bootstrap_addrs.push(addr);
            nodes.push(TestNode {
                p2p_node: Some(Arc::clone(&node)),
                protocol: Some(protocol),
                _handler_task: Some(handler),
            });
            temp_dirs.push(temp_dir);
            sleep(Duration::from_millis(SPAWN_DELAY_MS)).await;
        }

        // Phase 2: Spawn regular nodes
        for i in bootstrap_count..node_count {
            let port = base_port + i as u16;
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

            let temp_dir = tempfile::TempDir::new().expect("create temp dir");
            let (node, protocol, handler) =
                Self::spawn_node(addr, &bootstrap_addrs, temp_dir.path(), &evm_network, i).await;

            nodes.push(TestNode {
                p2p_node: Some(Arc::clone(&node)),
                protocol: Some(protocol),
                _handler_task: Some(handler),
            });
            temp_dirs.push(temp_dir);
            sleep(Duration::from_millis(SPAWN_DELAY_MS)).await;
        }

        // Phase 3: Wait for DHT convergence.
        // Every node's routing table must know about all other nodes
        // so that `find_closest_nodes` returns the full close group.
        // For merkle payments we need at least 16 peers per pool query.
        // For small networks, require all peers. For large ones, require at least 20.
        let min_routing_table_size = if node_count > 20 { 20 } else { node_count - 1 };
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(STABILIZATION_TIMEOUT_SECS);

        loop {
            let mut converged = true;
            for n in &nodes {
                if let Some(ref node) = n.p2p_node {
                    if node.dht().get_routing_table_size().await < min_routing_table_size {
                        converged = false;
                        break;
                    }
                }
            }

            if converged {
                break;
            }

            if tokio::time::Instant::now() > deadline {
                break;
            }

            sleep(Duration::from_millis(500)).await;
        }

        // Approve token spend for data payments contract
        let data_payments_address = evm_network.data_payments_address();
        wallet
            .approve_to_spend_tokens(*data_payments_address, evmlib::common::U256::MAX)
            .await
            .expect("approve data payment token spend");

        // Approve token spend for merkle payments contract (if deployed)
        if let Some(merkle_address) = evm_network.merkle_payments_address() {
            wallet
                .approve_to_spend_tokens(*merkle_address, evmlib::common::U256::MAX)
                .await
                .expect("approve merkle payment token spend");
        }

        Self {
            nodes,
            _temp_dirs: temp_dirs,
            _testnet: testnet,
            wallet,
            evm_network,
        }
    }

    pub fn node(&self, index: usize) -> Option<Arc<P2PNode>> {
        self.nodes.get(index).and_then(|n| n.p2p_node.clone())
    }

    /// Get a reference to the funded wallet for payment operations.
    pub fn wallet(&self) -> &Wallet {
        &self.wallet
    }

    /// Get the EVM network configuration (Anvil testnet).
    pub fn evm_network(&self) -> &EvmNetwork {
        &self.evm_network
    }

    #[allow(clippy::too_many_lines)]
    async fn spawn_node(
        listen_addr: SocketAddr,
        bootstrap_peers: &[SocketAddr],
        data_dir: &std::path::Path,
        evm_network: &EvmNetwork,
        node_index: usize,
    ) -> (Arc<P2PNode>, Arc<AntProtocol>, tokio::task::JoinHandle<()>) {
        // Generate ML-DSA-65 identity for this node
        let identity = Arc::new(NodeIdentity::generate().expect("generate node identity"));

        let mut core_config = CoreNodeConfig::builder()
            .port(listen_addr.port())
            .ipv6(false)
            .local(true)
            .max_message_size(MAX_WIRE_MESSAGE_SIZE)
            .build()
            .expect("create core config");

        core_config.bootstrap_peers = bootstrap_peers
            .iter()
            .map(|addr| MultiAddr::quic(*addr))
            .collect();
        core_config.connection_timeout = Duration::from_secs(5);
        core_config.node_identity = Some(Arc::clone(&identity));
        core_config.diversity_config = Some(IPDiversityConfig::permissive());

        let node = Arc::new(P2PNode::new(core_config).await.expect("create P2P node"));
        node.start().await.expect("start P2P node");

        // Create LMDB storage
        let storage_config = LmdbStorageConfig {
            root_dir: data_dir.to_path_buf(),
            verify_on_read: true,
            max_chunks: 0,
            max_map_size: 0,
        };
        let storage = Arc::new(
            LmdbStorage::new(storage_config)
                .await
                .expect("create storage"),
        );

        // Each node gets a unique rewards address so tests exercise
        // recipient-binding verification (the verifier checks that its own
        // address appears in the proof).
        let mut addr_bytes = TEST_REWARDS_ADDRESS;
        addr_bytes[19] = u8::try_from(node_index % 256).unwrap_or(0);
        let rewards_address = RewardsAddress::new(addr_bytes);

        // Create payment verifier with the Anvil EVM network
        let payment_config = PaymentVerifierConfig {
            evm: EvmVerifierConfig {
                network: evm_network.clone(),
            },
            cache_capacity: 1000,
            local_rewards_address: rewards_address,
        };
        let payment_verifier = Arc::new(PaymentVerifier::new(payment_config));
        let metrics_tracker = QuotingMetricsTracker::new(TEST_MAX_RECORDS, TEST_INITIAL_RECORDS);
        let mut quote_generator = QuoteGenerator::new(rewards_address, metrics_tracker);

        // Wire ML-DSA-65 signing so quotes are properly signed and verifiable
        let pub_key_bytes = identity.public_key().as_bytes().to_vec();
        let sk_bytes = identity.secret_key_bytes().to_vec();
        let sk = {
            use saorsa_pqc::pqc::types::MlDsaSecretKey;
            MlDsaSecretKey::from_bytes(&sk_bytes).expect("deserialize ML-DSA-65 secret key")
        };
        quote_generator.set_signer(pub_key_bytes, move |msg| {
            use saorsa_pqc::pqc::MlDsaOperations;
            let ml_dsa = MlDsa65::new();
            ml_dsa
                .sign(&sk, msg)
                .map_or_else(|_| vec![], |sig| sig.as_bytes().to_vec())
        });

        // Create protocol handler
        let protocol = Arc::new(AntProtocol::new(
            storage,
            payment_verifier,
            Arc::new(quote_generator),
        ));

        // Start message handler loop
        let handler_node = Arc::clone(&node);
        let handler_protocol = Arc::clone(&protocol);
        let handler = tokio::spawn(async move {
            let mut events = handler_node.subscribe_events();
            loop {
                match events.recv().await {
                    Ok(P2PEvent::Message {
                        topic,
                        source: Some(source_peer),
                        data,
                    }) => {
                        let protocol = Arc::clone(&handler_protocol);
                        let node = Arc::clone(&handler_node);
                        let topic_clone = topic.clone();
                        tokio::spawn(async move {
                            if topic_clone != ant_node::CHUNK_PROTOCOL_ID {
                                return;
                            }
                            match protocol.try_handle_request(&data).await {
                                Ok(Some(response_bytes)) => {
                                    if let Err(e) = node
                                        .send_message(
                                            &source_peer,
                                            &topic_clone,
                                            response_bytes.to_vec(),
                                            &[],
                                        )
                                        .await
                                    {
                                        eprintln!("ERROR: node {node_index} failed to send response to {source_peer}: {e}");
                                    }
                                }
                                Ok(None) => {
                                    // Non-request message (e.g. response) — nothing to reply
                                }
                                Err(e) => {
                                    eprintln!(
                                        "ERROR: node {node_index} try_handle_request failed: {e}"
                                    );
                                }
                            }
                        });
                    }
                    Ok(P2PEvent::Message { source: None, .. }) => {
                        eprintln!("WARNING: node {node_index} received message with no source");
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("WARNING: node {node_index} handler lagged, dropped {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        (node, protocol, handler)
    }

    /// Shut down a node by index, simulating a failure.
    ///
    /// Aborts the handler task and drops the P2P node reference so the
    /// transport shuts down. The slot remains in the `nodes` vec (as `None`).
    pub fn shutdown_node(&mut self, index: usize) {
        if let Some(node) = self.nodes.get_mut(index) {
            if let Some(task) = node._handler_task.take() {
                task.abort();
            }
            node.protocol = None;
            node.p2p_node = None;
        }
    }

    /// Count how many nodes are still running (have a P2P node).
    pub fn running_node_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.p2p_node.is_some()).count()
    }

    pub async fn teardown(self) {
        // 1. Abort handler tasks first so they stop processing messages
        for node in &self.nodes {
            if let Some(ref task) = node._handler_task {
                task.abort();
            }
        }

        // 2. Gracefully shut down each P2P node — this sends DHT leave
        //    messages, closes QUIC endpoints, and releases ports so the
        //    OS can reclaim them before the next sequential test starts.
        //    Without this, ports remain in TIME_WAIT and subsequent tests
        //    encounter "Peer not found" / "Stream error" transport failures.
        for node in &self.nodes {
            if let Some(ref p2p_node) = node.p2p_node {
                let _ = p2p_node.shutdown().await;
            }
        }
    }
}
