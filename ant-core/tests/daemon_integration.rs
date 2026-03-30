use std::net::IpAddr;

use ant_core::node::daemon::server;
use ant_core::node::registry::NodeRegistry;
use ant_core::node::types::{DaemonConfig, DaemonStatus};

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
