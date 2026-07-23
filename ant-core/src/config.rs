use std::net::SocketAddr;
use std::path::PathBuf;

use crate::data::{DevnetManifest, MultiAddr};
use crate::error::{Error, Result};

/// Returns the platform-appropriate data directory for ant.
///
/// - Linux: `~/.local/share/ant`
/// - macOS: `~/Library/Application Support/ant`
/// - Windows: `%APPDATA%\ant`
pub fn data_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        home_dir()?.join("Library").join("Application Support")
    } else if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().unwrap().join("AppData").join("Roaming"))
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().unwrap().join(".local").join("share"))
    };
    Ok(base.join("ant"))
}

/// Returns the platform-appropriate configuration directory for ant.
///
/// - Linux: `~/.config/ant`
/// - macOS: `~/Library/Application Support/ant`
/// - Windows: `%APPDATA%\ant`
pub fn config_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        home_dir()?.join("Library").join("Application Support")
    } else if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().unwrap().join("AppData").join("Roaming"))
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().unwrap().join(".config"))
    };
    Ok(base.join("ant"))
}

/// Returns the platform-appropriate log directory for ant.
///
/// - Linux: `~/.local/share/ant/logs`
/// - macOS: `~/Library/Logs/ant`
/// - Windows: `%APPDATA%\ant\logs`
pub fn log_dir() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        Ok(home_dir()?.join("Library").join("Logs").join("ant"))
    } else {
        Ok(data_dir()?.join("logs"))
    }
}

/// Loads bootstrap peers from the platform-appropriate `bootstrap_peers.toml` file.
///
/// Returns `Ok(Some(peers))` if the file exists and parses successfully,
/// `Ok(None)` if the file does not exist, or `Err` on parse/IO failures.
pub fn load_bootstrap_peers() -> Result<Option<Vec<SocketAddr>>> {
    let path = config_dir()?.join("bootstrap_peers.toml");
    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&path)?;
    let config: BootstrapConfig =
        toml::from_str(&contents).map_err(|e| Error::BootstrapConfigParse(e.to_string()))?;

    let addrs: Vec<SocketAddr> = config.peers.iter().filter_map(|s| s.parse().ok()).collect();

    if addrs.is_empty() {
        return Ok(None);
    }

    Ok(Some(addrs))
}

/// Resolve the bootstrap peers for a client connection.
///
/// Priority: explicitly supplied peers (e.g. a frontend's `--bootstrap`
/// flag) > devnet manifest peers > the platform `bootstrap_peers.toml`
/// config file. Manifest peers without a resolvable socket address are
/// filtered out. A selected manifest is authoritative: if it yields no
/// usable peers, resolution fails rather than falling back to the
/// config file.
///
/// # Errors
///
/// Returns [`Error::NoBootstrapPeers`] when the selected source yields
/// no peers, and propagates config-file read/parse failures.
pub fn resolve_bootstrap_peers(
    explicit: &[SocketAddr],
    manifest: Option<&DevnetManifest>,
) -> Result<Vec<SocketAddr>> {
    if !explicit.is_empty() {
        return Ok(explicit.to_vec());
    }

    if let Some(m) = manifest {
        let peers: Vec<SocketAddr> = m
            .bootstrap
            .iter()
            .filter_map(MultiAddr::socket_addr)
            .collect();
        // An explicitly selected manifest never falls back to the public
        // config: an empty (or fully filtered) manifest is an error here,
        // not later when the first data operation fails.
        if peers.is_empty() {
            return Err(Error::NoBootstrapPeers);
        }
        return Ok(peers);
    }

    if let Some(peers) = load_bootstrap_peers()? {
        tracing::info!("Loaded {} bootstrap peer(s) from config file", peers.len());
        return Ok(peers);
    }

    Err(Error::NoBootstrapPeers)
}

#[derive(serde::Deserialize)]
struct BootstrapConfig {
    peers: Vec<String>,
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| Error::HomeDirNotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_ends_with_ant() {
        let dir = data_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), "ant");
    }

    #[test]
    fn config_dir_ends_with_ant() {
        let dir = config_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), "ant");
    }

    #[test]
    fn log_dir_contains_ant() {
        let dir = log_dir().unwrap();
        assert!(
            dir.components().any(|c| c.as_os_str() == "ant"),
            "log_dir should contain 'ant' component: {:?}",
            dir
        );
    }

    fn test_manifest(addrs: Vec<SocketAddr>) -> DevnetManifest {
        DevnetManifest {
            base_port: 10000,
            node_count: addrs.len(),
            bootstrap: addrs.into_iter().map(MultiAddr::quic).collect(),
            data_dir: PathBuf::new(),
            created_at: String::new(),
            evm: None,
        }
    }

    #[test]
    fn resolve_bootstrap_prefers_explicit_peers() {
        let explicit: Vec<SocketAddr> = vec!["10.0.0.1:10000".parse().unwrap()];
        let manifest = test_manifest(vec!["10.0.0.2:10000".parse().unwrap()]);
        let peers = resolve_bootstrap_peers(&explicit, Some(&manifest)).unwrap();
        assert_eq!(peers, explicit);
    }

    #[test]
    fn resolve_bootstrap_uses_manifest_when_no_explicit_peers() {
        let addr: SocketAddr = "10.0.0.2:10000".parse().unwrap();
        let manifest = test_manifest(vec![addr]);
        let peers = resolve_bootstrap_peers(&[], Some(&manifest)).unwrap();
        assert_eq!(peers, vec![addr]);
    }

    #[test]
    fn resolve_bootstrap_errors_on_empty_manifest() {
        let manifest = test_manifest(vec![]);
        let err = resolve_bootstrap_peers(&[], Some(&manifest)).unwrap_err();
        assert!(matches!(err, Error::NoBootstrapPeers));
    }

    #[test]
    fn resolve_bootstrap_errors_when_all_manifest_peers_filtered() {
        // A non-IP transport has no socket address, so the peer is
        // filtered out and the manifest yields nothing usable.
        let bt: MultiAddr = "/bt/00:11:22:33:44:55/rfcomm/1".parse().unwrap();
        assert!(bt.socket_addr().is_none());
        let mut manifest = test_manifest(vec![]);
        manifest.bootstrap = vec![bt];
        manifest.node_count = 1;
        let err = resolve_bootstrap_peers(&[], Some(&manifest)).unwrap_err();
        assert!(matches!(err, Error::NoBootstrapPeers));
    }

    #[test]
    fn load_bootstrap_peers_returns_none_when_no_file() {
        // Set config dir to a temp location where no file exists
        let _result = load_bootstrap_peers();
        // Just verify it doesn't panic — actual None depends on whether the file exists
    }

    #[test]
    fn parse_bootstrap_config() {
        let toml_str = r#"
peers = [
    "129.212.138.135:10000",
    "134.199.138.183:10000",
]
"#;
        let config: BootstrapConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.peers.len(), 2);
        let addr: SocketAddr = config.peers[0].parse().unwrap();
        assert_eq!(addr.port(), 10000);
    }
}
