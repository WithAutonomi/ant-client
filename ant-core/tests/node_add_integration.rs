use std::collections::HashMap;
use std::path::PathBuf;

use ant_core::node::binary::NoopProgress;
use ant_core::node::registry::NodeRegistry;
use ant_core::node::types::{AddNodeOpts, BinarySource, PortRange};

/// Create a fake binary that responds to --version
fn create_fake_binary(dir: &std::path::Path) -> PathBuf {
    let binary_path = dir.join("fake-antnode");
    #[cfg(unix)]
    {
        std::fs::write(&binary_path, "#!/bin/sh\necho \"antnode 0.1.0-test\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    {
        std::fs::write(&binary_path, "@echo antnode 0.1.0-test\n").unwrap();
    }
    binary_path
}

#[tokio::test]
async fn add_nodes_creates_registry_and_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = create_fake_binary(tmp.path());
    let reg_path = tmp.path().join("registry.json");

    let opts = AddNodeOpts {
        count: 2,
        rewards_address: "0xintegration_test".to_string(),
        node_port: Some(PortRange::Range(12000, 12001)),
        metrics_port: Some(PortRange::Range(13000, 13001)),
        data_dir_path: Some(tmp.path().join("data")),
        log_dir_path: Some(tmp.path().join("logs")),
        binary_source: BinarySource::LocalPath(binary),
        ..Default::default()
    };

    let result = ant_core::node::add_nodes(opts, &reg_path, &NoopProgress)
        .await
        .unwrap();

    // Verify 2 nodes added
    assert_eq!(result.nodes_added.len(), 2);

    // Verify IDs are sequential
    assert_eq!(result.nodes_added[0].id, 1);
    assert_eq!(result.nodes_added[1].id, 2);

    // Verify ports assigned correctly
    assert_eq!(result.nodes_added[0].node_port, Some(12000));
    assert_eq!(result.nodes_added[1].node_port, Some(12001));
    assert_eq!(result.nodes_added[0].metrics_port, Some(13000));
    assert_eq!(result.nodes_added[1].metrics_port, Some(13001));

    // Verify directories were created
    for node in &result.nodes_added {
        assert!(
            node.data_dir.exists(),
            "data dir should exist: {:?}",
            node.data_dir
        );
        let log_dir = node.log_dir.as_ref().expect("log_dir should be set");
        assert!(log_dir.exists(), "log dir should exist: {:?}", log_dir);
    }

    // Verify registry file was written
    assert!(reg_path.exists());
    let reg = NodeRegistry::load(&reg_path).unwrap();
    assert_eq!(reg.len(), 2);
    assert_eq!(reg.next_id, 3);
}

#[tokio::test]
async fn add_then_remove_node() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = create_fake_binary(tmp.path());
    let reg_path = tmp.path().join("registry.json");

    // Add a node
    let opts = AddNodeOpts {
        count: 1,
        rewards_address: "0xremove_test".to_string(),
        data_dir_path: Some(tmp.path().join("data")),
        log_dir_path: Some(tmp.path().join("logs")),
        binary_source: BinarySource::LocalPath(binary),
        ..Default::default()
    };

    let result = ant_core::node::add_nodes(opts, &reg_path, &NoopProgress)
        .await
        .unwrap();
    let node_id = result.nodes_added[0].id;

    // Remove it
    let remove_result = ant_core::node::remove_node(node_id, &reg_path).unwrap();
    assert_eq!(remove_result.removed.rewards_address, "0xremove_test");

    // Verify registry is empty
    let reg = NodeRegistry::load(&reg_path).unwrap();
    assert!(reg.is_empty());
}

#[tokio::test]
async fn add_nodes_with_env_variables() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = create_fake_binary(tmp.path());
    let reg_path = tmp.path().join("registry.json");

    let opts = AddNodeOpts {
        count: 1,
        rewards_address: "0xenv_test".to_string(),
        data_dir_path: Some(tmp.path().join("data")),
        log_dir_path: Some(tmp.path().join("logs")),
        binary_source: BinarySource::LocalPath(binary),
        env_variables: vec![
            ("FOO".to_string(), "bar".to_string()),
            ("BAZ".to_string(), "qux".to_string()),
        ],
        bootstrap_peers: vec!["1.2.3.4:5000".to_string()],
        ..Default::default()
    };

    let result = ant_core::node::add_nodes(opts, &reg_path, &NoopProgress)
        .await
        .unwrap();

    let node = &result.nodes_added[0];
    let expected_env: HashMap<String, String> = [
        ("FOO".to_string(), "bar".to_string()),
        ("BAZ".to_string(), "qux".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(node.env_variables, expected_env);
    assert_eq!(node.bootstrap_peers, vec!["1.2.3.4:5000"]);
}
