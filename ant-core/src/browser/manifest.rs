//! Cross-platform validation for browser bootstrap and public-file metadata.

pub use super::protocol::BrowserPaymentNetwork;
use super::protocol::{normalize_hex, parse_webrtc_direct_multiaddr, BrowserEndpoint};
use super::BrowserChunkInfo;
use serde::{Deserialize, Serialize};

/// Current browser testnet manifest version.
pub const BROWSER_MANIFEST_VERSION: u16 = 6;
const MAX_DATA_MAP_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILE_CHUNKS: usize = 1024;

/// A validated WebRTC Direct bootstrap endpoint.
pub type BrowserManifestEndpoint = BrowserEndpoint;

/// Complete public-file metadata shared by native tooling and the web client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicFileDescriptor {
    /// Display and save-as filename.
    pub name: String,
    /// Public DataMap content address.
    pub address: String,
    /// Plaintext file size.
    pub size: usize,
    /// Browser MIME type.
    pub content_type: String,
    /// Whole-file plaintext BLAKE3 hash.
    pub blake3: String,
    /// Encoded public DataMap size.
    pub data_map_size: usize,
    /// Self-encryption chunk descriptors.
    pub chunks: Vec<BrowserChunkInfo>,
    /// Minimum confirmed record replica count.
    pub replicas: usize,
}

/// Validated bootstrap, payment, and public-file description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserManifest {
    /// Manifest schema version.
    pub version: u16,
    /// Network instance identifier.
    pub network_id: String,
    /// Optional manifest creation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Stable WebRTC Direct bootstrap addresses.
    pub endpoints: Vec<BrowserManifestEndpoint>,
    /// Storage payment configuration.
    pub payment: BrowserPaymentNetwork,
    /// Known public files offered by the manifest.
    #[serde(default)]
    pub files: Vec<PublicFileDescriptor>,
}

/// Manifest validation error.
#[derive(Debug, thiserror::Error)]
#[error("invalid browser manifest: {0}")]
pub struct BrowserManifestError(pub String);

/// Decode, validate, and normalize an untrusted browser manifest.
pub fn parse_browser_manifest(
    value: serde_json::Value,
) -> Result<BrowserManifest, BrowserManifestError> {
    let mut manifest: BrowserManifest =
        serde_json::from_value(value).map_err(|error| BrowserManifestError(error.to_string()))?;
    if manifest.version != BROWSER_MANIFEST_VERSION {
        return Err(BrowserManifestError(format!(
            "unsupported browser manifest version {}",
            manifest.version
        )));
    }
    if manifest.network_id.is_empty() {
        return Err(BrowserManifestError(
            "browser manifest has no network ID".to_string(),
        ));
    }
    if manifest.endpoints.is_empty() {
        return Err(BrowserManifestError(
            "browser manifest contains no WebRtcDirect endpoints".to_string(),
        ));
    }
    for endpoint in &mut manifest.endpoints {
        let parsed = parse_webrtc_direct_multiaddr(&endpoint.multiaddr)
            .map_err(|error| BrowserManifestError(error.to_string()))?;
        endpoint.multiaddr = parsed.multiaddr;
    }
    manifest.payment = validate_browser_payment_network(manifest.payment)?;
    for file in &mut manifest.files {
        normalize_file(file)?;
    }
    Ok(manifest)
}

/// Validate and normalize payment configuration supplied independently of a
/// manifest, such as to the browser network client WASM binding.
pub fn validate_browser_payment_network(
    mut payment: BrowserPaymentNetwork,
) -> Result<BrowserPaymentNetwork, BrowserManifestError> {
    // The JS SDK exposes chain IDs as numbers; reject identities that cannot
    // survive that boundary exactly.
    if payment.chain_id > 9_007_199_254_740_991 {
        return Err(BrowserManifestError(
            "payment chain ID exceeds JavaScript's safe integer range".to_string(),
        ));
    }
    payment.payment_token_address = format!(
        "0x{}",
        normalize_hex(&payment.payment_token_address, 20).map_err(BrowserManifestError)?
    );
    payment.payment_vault_address = format!(
        "0x{}",
        normalize_hex(&payment.payment_vault_address, 20).map_err(BrowserManifestError)?
    );
    Ok(payment)
}

/// Check upload capabilities and payment identity without comparing RPC providers.
#[cfg(any(all(target_arch = "wasm32", feature = "browser-wasm"), test))]
pub(crate) fn assert_upload_node(
    hello: &super::protocol::BrowserHello,
    expected: &BrowserPaymentNetwork,
) -> Result<(), String> {
    if !hello
        .capabilities
        .iter()
        .any(|value| value == "quote_chunk")
        || !hello.capabilities.iter().any(|value| value == "put_chunk")
    {
        return Err("node does not advertise paid browser uploads".to_string());
    }
    let advertised = &hello.payment;
    if advertised.chain_id != expected.chain_id
        || !advertised
            .payment_token_address
            .eq_ignore_ascii_case(&expected.payment_token_address)
        || !advertised
            .payment_vault_address
            .eq_ignore_ascii_case(&expected.payment_vault_address)
    {
        return Err("node advertises a different payment network than the client".to_string());
    }
    Ok(())
}

fn normalize_file(file: &mut PublicFileDescriptor) -> Result<(), BrowserManifestError> {
    if file.name.is_empty() {
        return Err(BrowserManifestError(
            "browser manifest file has no name".to_string(),
        ));
    }
    file.address = normalize_hex(&file.address, 32).map_err(BrowserManifestError)?;
    if !(self_encryption::MIN_ENCRYPTABLE_BYTES..=super::MAX_BROWSER_FILE_BYTES)
        .contains(&file.size)
    {
        return Err(BrowserManifestError(format!(
            "invalid public file size {}",
            file.size
        )));
    }
    file.blake3 = normalize_hex(&file.blake3, 32).map_err(BrowserManifestError)?;
    if !(1..=MAX_DATA_MAP_BYTES).contains(&file.data_map_size) {
        return Err(BrowserManifestError(format!(
            "invalid DataMap size {}",
            file.data_map_size
        )));
    }
    if !(3..=MAX_FILE_CHUNKS).contains(&file.chunks.len()) {
        return Err(BrowserManifestError(
            "public file has an invalid self-encryption chunk list".to_string(),
        ));
    }
    file.chunks.sort_by_key(|chunk| chunk.index);
    let mut reconstructed_size = 0usize;
    for (expected_index, chunk) in file.chunks.iter_mut().enumerate() {
        if chunk.index != expected_index {
            return Err(BrowserManifestError(
                "file chunk indices must be contiguous from zero".to_string(),
            ));
        }
        chunk.dst_hash = normalize_hex(&chunk.dst_hash, 32).map_err(BrowserManifestError)?;
        chunk.src_hash = normalize_hex(&chunk.src_hash, 32).map_err(BrowserManifestError)?;
        if chunk.src_size == 0 {
            return Err(BrowserManifestError(format!(
                "invalid plaintext chunk size {}",
                chunk.src_size
            )));
        }
        reconstructed_size = reconstructed_size
            .checked_add(chunk.src_size)
            .ok_or_else(|| BrowserManifestError("file size overflow".to_string()))?;
    }
    if reconstructed_size != file.size {
        return Err(BrowserManifestError(format!(
            "file chunk sizes total {reconstructed_size}, expected {}",
            file.size
        )));
    }
    if file.content_type.is_empty() {
        file.content_type = "application/octet-stream".to_string();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    fn endpoint() -> String {
        let mut multihash = vec![0x12, 0x20];
        multihash.extend([0xbb; 32]);
        format!(
            "/ip4/127.0.0.1/udp/22000/webrtc-direct/certhash/u{}/p2p/{}",
            URL_SAFE_NO_PAD.encode(multihash),
            "AA".repeat(32)
        )
    }

    #[test]
    fn upload_identity_checks_chain_and_both_contracts() {
        let payment = BrowserPaymentNetwork {
            chain_id: 31337,
            payment_token_address: format!("0x{}", "ab".repeat(20)),
            payment_vault_address: format!("0x{}", "cd".repeat(20)),
        };
        let mut hello = super::super::protocol::BrowserHello {
            response_type: "hello".into(),
            protocol: super::super::protocol::BROWSER_PROTOCOL_NAME.into(),
            peer_id: "aa".repeat(32),
            endpoint: BrowserEndpoint {
                multiaddr: endpoint(),
            },
            max_chunk_size: 4 * 1024 * 1024,
            capabilities: vec!["quote_chunk".into(), "put_chunk".into()],
            payment: payment.clone(),
        };
        assert!(assert_upload_node(&hello, &payment).is_ok());
        hello.payment.payment_token_address = payment.payment_token_address.to_uppercase();
        assert!(assert_upload_node(&hello, &payment).is_ok());
        hello.payment.chain_id = 1;
        assert!(assert_upload_node(&hello, &payment).is_err());
        hello.payment = payment.clone();
        hello.payment.payment_token_address = "11".repeat(20);
        assert!(assert_upload_node(&hello, &payment).is_err());
        hello.payment = payment.clone();
        hello.payment.payment_vault_address = "22".repeat(20);
        assert!(assert_upload_node(&hello, &payment).is_err());
        hello.payment = payment.clone();
        hello
            .capabilities
            .retain(|capability| capability != "put_chunk");
        assert!(assert_upload_node(&hello, &payment).is_err());
    }

    #[test]
    fn validates_and_normalizes_manifest() {
        let value = serde_json::json!({
            "version": 6,
            "network_id": "local-test",
            "created_at": "2026-08-03T00:00:00Z",
            "payment": {
                "chain_id": 31337,
                "payment_token_address": format!("0x{}", "11".repeat(20)),
                "payment_vault_address": format!("0x{}", "22".repeat(20)),
            },
            "endpoints": [{ "multiaddr": endpoint() }],
            "files": [{
                "name": "hello.txt",
                "address": "CC".repeat(32),
                "size": 12,
                "content_type": "text/plain",
                "blake3": "DD".repeat(32),
                "data_map_size": 128,
                "chunks": [
                    { "index": 2, "dst_hash": "13".repeat(32), "src_hash": "23".repeat(32), "src_size": 4 },
                    { "index": 0, "dst_hash": "11".repeat(32), "src_hash": "21".repeat(32), "src_size": 4 },
                    { "index": 1, "dst_hash": "12".repeat(32), "src_hash": "22".repeat(32), "src_size": 4 }
                ],
                "replicas": 5
            }]
        });
        let manifest = parse_browser_manifest(value).expect("valid manifest");
        assert_eq!(manifest.files[0].address, "cc".repeat(32));
        assert_eq!(manifest.files[0].chunks[0].index, 0);
        assert_eq!(manifest.payment.chain_id, 31337);
    }
}
