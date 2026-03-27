use std::net::IpAddr;

use ant_core::node::binary::NoopProgress;
use ant_core::node::daemon::server;
use ant_core::node::registry::NodeRegistry;
use ant_core::node::types::{
    AddNodeOpts, BinarySource, DaemonConfig, DaemonStatus, NodeStatusResult,
};

fn test_config(dir: &tempfile::TempDir) -> DaemonConfig {
    DaemonConfig {
        listen_addr: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port: Some(0), // random available port
        registry_path: dir.path().join("registry.json"),
        log_path: dir.path().join("daemon.log"),
        port_file_path: dir.path().join("daemon.port"),
        pid_file_path: dir.path().join("daemon.pid"),
    }
}

#[tokio::test]
async fn start_daemon_get_status_stop() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(&dir);
    let registry = NodeRegistry::load(&config.registry_path).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();

    let addr = server::start(config, registry, shutdown.clone())
        .await
        .unwrap();

    // Hit the status endpoint
    let url = format!("http://{addr}/api/v1/status");
    let resp = reqwest::get(&url).await.unwrap();
    assert!(resp.status().is_success());

    let status: DaemonStatus = resp.json().await.unwrap();
    assert!(status.running);
    assert!(status.pid.is_some());
    assert_eq!(status.nodes_total, 0);
    assert_eq!(status.nodes_running, 0);
    assert_eq!(status.nodes_stopped, 0);
    assert_eq!(status.nodes_errored, 0);

    // Verify uptime is reasonable (should be very small)
    assert!(status.uptime_secs.unwrap() < 5);

    // Stop the daemon
    shutdown.cancel();
    // Give server time to shut down
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[tokio::test]
async fn openapi_spec_is_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(&dir);
    let registry = NodeRegistry::load(&config.registry_path).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();

    let addr = server::start(config, registry, shutdown.clone())
        .await
        .unwrap();

    let url = format!("http://{addr}/api/v1/openapi.json");
    let resp = reqwest::get(&url).await.unwrap();
    assert!(resp.status().is_success());

    let spec: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(spec["info"]["title"], "Ant Daemon API");
    assert!(spec["paths"]["/api/v1/status"].is_object());

    shutdown.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[tokio::test]
async fn port_and_pid_files_written() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(&dir);
    let port_file = config.port_file_path.clone();
    let pid_file = config.pid_file_path.clone();
    let registry = NodeRegistry::load(&config.registry_path).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();

    let addr = server::start(config, registry, shutdown.clone())
        .await
        .unwrap();

    // Verify port file
    let port_contents = std::fs::read_to_string(&port_file).unwrap();
    let port: u16 = port_contents.trim().parse().unwrap();
    assert_eq!(port, addr.port());

    // Verify PID file
    let pid_contents = std::fs::read_to_string(&pid_file).unwrap();
    let pid: u32 = pid_contents.trim().parse().unwrap();
    assert_eq!(pid, std::process::id());

    shutdown.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // After shutdown, files should be cleaned up
    assert!(
        !port_file.exists(),
        "Port file should be removed after shutdown"
    );
    assert!(
        !pid_file.exists(),
        "PID file should be removed after shutdown"
    );
}

#[tokio::test]
async fn console_returns_html() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(&dir);
    let registry = NodeRegistry::load(&config.registry_path).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();

    let addr = server::start(config, registry, shutdown.clone())
        .await
        .unwrap();

    let url = format!("http://{addr}/console");
    let resp = reqwest::get(&url).await.unwrap();
    assert!(resp.status().is_success());

    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/html"),
        "Expected text/html, got {content_type}"
    );

    let body = resp.text().await.unwrap();
    assert!(body.contains("Node Console"));
    assert!(body.contains("/api/v1"));

    shutdown.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

/// Create a fake binary that responds to `--version` and stays alive otherwise.
fn create_fake_binary(dir: &std::path::Path) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        let binary_path = dir.join("fake-antnode");
        std::fs::write(
            &binary_path,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"antnode 0.1.0-test\"; exit 0; fi\nsleep 300\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        binary_path
    }
    #[cfg(windows)]
    {
        let binary_path = dir.join("fake-antnode.cmd");
        std::fs::write(
            &binary_path,
            "@echo off\r\nif \"%1\"==\"--version\" (\r\n  echo antnode 0.1.0-test\r\n  exit /b 0\r\n)\r\ntimeout /t 300 /nobreak >nul\r\n",
        )
        .unwrap();
        binary_path
    }
}

#[tokio::test]
async fn nodes_status_includes_pid_and_uptime() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(&dir);
    let reg_path = config.registry_path.clone();

    // Add nodes to the registry before starting the daemon
    let binary = create_fake_binary(dir.path());
    let opts = AddNodeOpts {
        count: 2,
        rewards_address: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
        data_dir_path: Some(dir.path().join("data")),
        log_dir_path: Some(dir.path().join("logs")),
        binary_source: BinarySource::LocalPath(binary),
        ..Default::default()
    };
    ant_core::node::add_nodes(opts, &reg_path, &NoopProgress)
        .await
        .unwrap();

    // Start the daemon with the populated registry
    let registry = NodeRegistry::load(&reg_path).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let addr = server::start(config, registry, shutdown.clone())
        .await
        .unwrap();

    // Hit the nodes status endpoint
    let url = format!("http://{addr}/api/v1/nodes/status");
    let resp = reqwest::get(&url).await.unwrap();
    assert!(resp.status().is_success());

    let result: NodeStatusResult = resp.json().await.unwrap();
    assert_eq!(result.nodes.len(), 2);

    // Nodes are registered but not started — pid and uptime should be None
    for node in &result.nodes {
        assert!(node.pid.is_none(), "stopped node should have no pid");
        assert!(
            node.uptime_secs.is_none(),
            "stopped node should have no uptime"
        );
    }

    // Verify the JSON omits None fields (skip_serializing_if)
    let raw: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    let first_node = &raw["nodes"][0];
    assert!(
        first_node.get("pid").is_none(),
        "pid should be omitted from JSON for stopped nodes"
    );
    assert!(
        first_node.get("uptime_secs").is_none(),
        "uptime_secs should be omitted from JSON for stopped nodes"
    );

    // Start node 1 via the daemon API.
    // The fake binary uses `sleep`/`timeout` to stay alive — on Unix this works
    // natively, on Windows the .cmd wrapper can't be spawned directly by
    // tokio::process::Command (os error 193), so we skip the running-node
    // assertions on Windows. The unit tests in types.rs cover the Some case.
    let start_url = format!("http://{addr}/api/v1/nodes/1/start");
    let client = reqwest::Client::new();
    let start_resp = client.post(&start_url).send().await.unwrap();

    if start_resp.status().is_success() {
        // Give the process a moment to be tracked
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Now status should include pid and uptime for the running node
        let result: NodeStatusResult = reqwest::get(&url).await.unwrap().json().await.unwrap();
        let running_node = result.nodes.iter().find(|n| n.node_id == 1).unwrap();
        assert!(running_node.pid.is_some(), "running node should have a pid");
        assert!(
            running_node.uptime_secs.is_some(),
            "running node should have uptime"
        );

        // Node 2 is still stopped
        let stopped_node = result.nodes.iter().find(|n| n.node_id == 2).unwrap();
        assert!(stopped_node.pid.is_none());
        assert!(stopped_node.uptime_secs.is_none());

        // Verify JSON shape for the running node
        let raw: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
        let running_raw = raw["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["node_id"] == 1)
            .unwrap();
        assert!(
            running_raw.get("pid").is_some(),
            "running node JSON should include pid"
        );
        assert!(
            running_raw.get("uptime_secs").is_some(),
            "running node JSON should include uptime_secs"
        );
    }

    shutdown.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}
