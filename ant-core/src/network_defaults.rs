//! Release-bundled bootstrap multiaddresses and portable payment defaults.

use ant_protocol::{evm::Network, transport::MultiAddr};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::browser::{parse_webrtc_direct_multiaddr, BrowserPaymentNetwork};

const BOOTSTRAP: &str = include_str!("../resources/bootstrap_peers.toml");

/// Validated bootstrap addresses, separated by transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapSeeds {
    /// Native QUIC multiaddresses, including any supplied peer identity.
    pub quic: Vec<MultiAddr>,
    /// Complete certificate-pinned WebRTC Direct multiaddresses.
    pub webrtc: Vec<String>,
}

/// A configuration error detected before opening any network connection.
#[derive(Debug, thiserror::Error)]
#[error("invalid bootstrap configuration: {0}")]
pub struct NetworkDefaultsError(String);

/// Parse a QUIC multiaddress, accepting legacy socket addresses for migration.
pub fn parse_quic_seed(value: &str) -> Result<MultiAddr, NetworkDefaultsError> {
    let address = if let Ok(socket) = value.parse::<std::net::SocketAddr>() {
        MultiAddr::quic(socket)
    } else {
        value
            .parse::<MultiAddr>()
            .map_err(|e| NetworkDefaultsError(e.to_string()))?
    };
    if !address.is_quic() || address.socket_addr().is_none_or(|a| a.port() == 0) {
        return Err(NetworkDefaultsError(
            "expected a QUIC multiaddress with a nonzero port".into(),
        ));
    }
    Ok(address)
}

/// Read both transport lists. Legacy `peers` is an alias for `quic`.
pub fn parse_bootstrap_seeds(text: &str) -> Result<BootstrapSeeds, NetworkDefaultsError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        #[serde(default, alias = "peers")]
        quic: Vec<String>,
        #[serde(default)]
        webrtc: Vec<String>,
    }
    let input: Input = toml::from_str(text).map_err(|e| NetworkDefaultsError(e.to_string()))?;
    let mut seen = HashSet::new();
    let mut quic = Vec::new();
    for value in input.quic {
        let address = parse_quic_seed(&value)?;
        if !seen.insert(address.to_string()) {
            return Err(NetworkDefaultsError("duplicate QUIC seed".into()));
        }
        quic.push(address);
    }
    let mut webrtc = Vec::new();
    for value in input.webrtc {
        let address = parse_webrtc_direct_multiaddr(&value)
            .map_err(|e| NetworkDefaultsError(e.to_string()))?
            .multiaddr;
        if !seen.insert(address.clone()) {
            return Err(NetworkDefaultsError("duplicate WebRTC seed".into()));
        }
        webrtc.push(address);
    }
    Ok(BootstrapSeeds { quic, webrtc })
}

/// The same resource shipped by native release archives.
pub fn bundled_bootstrap_seeds() -> Result<BootstrapSeeds, NetworkDefaultsError> {
    parse_bootstrap_seeds(BOOTSTRAP)
}

/// Browser-only projection of the trusted mainnet configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserNetworkDefaults {
    pub id: String,
    pub seeds: Vec<String>,
    pub payment: BrowserPaymentNetwork,
    pub rpc_url: String,
}

/// Return defaults without connecting, even before WebRTC seeds are published.
pub fn browser_mainnet_defaults() -> Result<BrowserNetworkDefaults, NetworkDefaultsError> {
    let network = Network::ArbitrumOne;
    Ok(BrowserNetworkDefaults {
        id: "mainnet".into(),
        seeds: bundled_bootstrap_seeds()?.webrtc,
        payment: BrowserPaymentNetwork {
            chain_id: 42161,
            payment_token_address: network.payment_token_address().to_string().to_lowercase(),
            payment_vault_address: network.payment_vault_address().to_string().to_lowercase(),
        },
        rpc_url: network.rpc_url().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_defaults_share_evm_identity_and_exclude_quic() {
        let seeds = bundled_bootstrap_seeds().unwrap();
        assert!(!seeds.quic.is_empty());
        let browser = browser_mainnet_defaults().unwrap();
        assert_eq!(browser.seeds, seeds.webrtc);
        assert_eq!(browser.payment.chain_id, 42161);
        assert_eq!(browser.rpc_url, Network::ArbitrumOne.rpc_url().as_str());
        assert_eq!(
            browser.payment.payment_vault_address,
            Network::ArbitrumOne
                .payment_vault_address()
                .to_string()
                .to_lowercase()
        );
    }

    #[test]
    fn preserves_quic_peer_pins_and_legacy_sockets() {
        let pin = format!("/ip6/::1/udp/10000/quic/p2p/{}", "ab".repeat(32));
        let seeds = parse_bootstrap_seeds(&format!("quic = [\"{pin}\"]\nwebrtc = []")).unwrap();
        assert_eq!(seeds.quic[0].to_string(), pin);
        let legacy = parse_bootstrap_seeds("peers = [\"127.0.0.1:10000\"]").unwrap();
        assert_eq!(legacy.quic[0].to_string(), "/ip4/127.0.0.1/udp/10000/quic");
    }

    #[test]
    fn keeps_valid_webrtc_seeds_separate_and_rejects_duplicates() {
        use base64::Engine;
        let mut hash = vec![0x12, 0x20];
        hash.extend([0xbb; 32]);
        let cert = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);
        let endpoint = format!(
            "/ip4/127.0.0.1/udp/24000/webrtc-direct/certhash/u{cert}/p2p/{}",
            "ab".repeat(32)
        );
        let input =
            format!("quic = [\"/ip4/127.0.0.1/udp/10000/quic\"]\nwebrtc = [\"{endpoint}\"]");
        let parsed = parse_bootstrap_seeds(&input).unwrap();
        assert_eq!(parsed.quic.len(), 1);
        assert_eq!(parsed.webrtc, vec![endpoint.clone()]);
        assert!(
            parse_bootstrap_seeds(&format!("webrtc = [\"{endpoint}\", \"{endpoint}\"]")).is_err()
        );
        assert!(parse_bootstrap_seeds(&format!("quic = [\"{endpoint}\"]")).is_err());
    }

    #[test]
    fn rejects_wrong_transport_duplicates_and_invalid_seeds() {
        for input in [
            "quic = [\"/ip4/127.0.0.1/tcp/10000\"]",
            "quic = [\"127.0.0.1:0\"]",
            "quic = [\"invalid\"]",
            "quic = [\"127.0.0.1:10000\", \"/ip4/127.0.0.1/udp/10000/quic\"]",
            "webrtc = [\"/ip4/127.0.0.1/udp/10000/quic\"]",
            "webrtc = [\"127.0.0.1:10000\"]",
        ] {
            assert!(parse_bootstrap_seeds(input).is_err(), "{input}");
        }
    }
}
