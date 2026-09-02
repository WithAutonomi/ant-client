//! Browser-facing WebRTC Direct wire profile.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr as _;

/// Current browser request/response protocol version.
pub const BROWSER_PROTOCOL_VERSION: u16 = 4;
/// Protocol name authenticated by the node HELLO response.
pub const BROWSER_PROTOCOL_NAME: &str = "autonomi.web.poc.v4";
/// Ordered WebRTC DataChannel label used by Autonomi nodes.
pub const WEBRTC_DIRECT_DATA_CHANNEL: &str = "autonomi.web.v4";
/// Maximum content carried by one browser protocol frame.
pub const MAX_BROWSER_RECORD_BYTES: usize = 4 * 1024 * 1024;
/// Maximum JSON header carried by one browser protocol frame.
pub const MAX_BROWSER_HEADER_BYTES: usize = 64 * 1024;
/// Maximum complete browser response frame.
pub const MAX_BROWSER_RESPONSE_BYTES: usize =
    4 + MAX_BROWSER_HEADER_BYTES + MAX_BROWSER_RECORD_BYTES;
/// Maximum accepted WebRTC Direct multiaddress length.
pub const MAX_WEBRTC_DIRECT_MULTIADDR_LENGTH: usize = 2048;
/// DataChannel message size shared with the native WebRTC Direct listener.
pub const WEBRTC_WRITE_CHUNK_BYTES: usize = 16 * 1024;

const SHA2_256_MULTIHASH_CODE: u8 = 0x12;
const SHA2_256_MULTIHASH_LENGTH: u8 = 32;

/// Errors produced by browser address, framing, and identity validation.
#[derive(Debug, thiserror::Error)]
pub enum BrowserProtocolError {
    /// An address or hexadecimal identifier is malformed.
    #[error("invalid browser endpoint: {0}")]
    Endpoint(String),
    /// A request or response frame is malformed or exceeds a bound.
    #[error("invalid browser frame: {0}")]
    Frame(String),
    /// A node identity response failed authentication.
    #[error("invalid node HELLO: {0}")]
    Identity(String),
}

/// Manifest-compatible wrapper around a WebRTC Direct multiaddress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserEndpoint {
    /// `/ip4|ip6/.../udp/.../webrtc-direct/certhash/.../p2p/...` address.
    pub multiaddr: String,
}

/// Parsed, certificate-pinned direct endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebRtcDirectEndpoint {
    /// Canonical input multiaddress.
    pub multiaddr: String,
    /// `ip4` or `ip6`.
    #[serde(rename = "hostProtocol")]
    pub host_protocol: String,
    /// Literal IP address.
    pub host: String,
    /// UDP listener port.
    pub port: u16,
    /// Lowercase 32-byte ANT peer ID.
    #[serde(rename = "peerId")]
    pub peer_id: String,
    /// SHA-256 DTLS certificate digest.
    #[serde(rename = "certificateHash")]
    pub certificate_hash: [u8; 32],
}

/// Endpoint accepted from either a raw multiaddress or manifest object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum BrowserEndpointInput {
    /// Raw multiaddress string.
    Multiaddr(String),
    /// Manifest endpoint object.
    Structured(BrowserEndpoint),
}

impl BrowserEndpointInput {
    /// Return the contained multiaddress.
    #[must_use]
    pub fn multiaddr(&self) -> &str {
        match self {
            Self::Multiaddr(value) => value,
            Self::Structured(value) => &value.multiaddr,
        }
    }
}

/// Parse and validate a signaling-free WebRTC Direct multiaddress.
pub fn parse_webrtc_direct_multiaddr(
    multiaddr: &str,
) -> Result<WebRtcDirectEndpoint, BrowserProtocolError> {
    let multiaddr = multiaddr.trim();
    if multiaddr.is_empty()
        || multiaddr.len() > MAX_WEBRTC_DIRECT_MULTIADDR_LENGTH
        || !multiaddr.starts_with('/')
    {
        return Err(BrowserProtocolError::Endpoint(
            "invalid WebRtcDirect multiaddress length or prefix".to_string(),
        ));
    }
    let segments = multiaddr.split('/').collect::<Vec<_>>();
    if segments.len() != 10 {
        return Err(BrowserProtocolError::Endpoint(
            "WebRtcDirect multiaddress is incomplete".to_string(),
        ));
    }

    let host_protocol = segments[1];
    let host = segments[2];
    match host_protocol {
        "ip4" => {
            Ipv4Addr::from_str(host).map_err(|error| {
                BrowserProtocolError::Endpoint(format!("invalid IPv4 address {host}: {error}"))
            })?;
        }
        "ip6" => {
            Ipv6Addr::from_str(host).map_err(|error| {
                BrowserProtocolError::Endpoint(format!("invalid IPv6 address {host}: {error}"))
            })?;
        }
        _ => {
            return Err(BrowserProtocolError::Endpoint(
                "WebRTC Direct multiaddresses must use a literal IP address".to_string(),
            ));
        }
    }
    if segments[3] != "udp" {
        return Err(BrowserProtocolError::Endpoint(
            "WebRtcDirect multiaddress must use UDP".to_string(),
        ));
    }
    let port = segments[4].parse::<u16>().map_err(|error| {
        BrowserProtocolError::Endpoint(format!(
            "WebRtcDirect multiaddress has an invalid UDP port: {error}"
        ))
    })?;
    if port == 0 {
        return Err(BrowserProtocolError::Endpoint(
            "WebRtcDirect multiaddress has an invalid UDP port".to_string(),
        ));
    }
    if segments[5] != "webrtc-direct" {
        return Err(BrowserProtocolError::Endpoint(
            "WebRTC Direct multiaddress must contain /webrtc-direct".to_string(),
        ));
    }
    if segments[6] != "certhash" || segments[7].is_empty() {
        return Err(BrowserProtocolError::Endpoint(
            "WebRTC Direct multiaddress must contain exactly one certhash".to_string(),
        ));
    }
    let certificate_hash = decode_certificate_multihash(segments[7])?;
    if segments[8] != "p2p" {
        return Err(BrowserProtocolError::Endpoint(
            "WebRtcDirect multiaddress must end with /p2p/<peer-id>".to_string(),
        ));
    }
    let peer_id = normalize_hex(segments[9], 32).map_err(BrowserProtocolError::Endpoint)?;

    Ok(WebRtcDirectEndpoint {
        multiaddr: multiaddr.to_string(),
        host_protocol: host_protocol.to_string(),
        host: host.to_string(),
        port,
        peer_id,
        certificate_hash,
    })
}

fn decode_certificate_multihash(value: &str) -> Result<[u8; 32], BrowserProtocolError> {
    let encoded = value.strip_prefix('u').ok_or_else(|| {
        BrowserProtocolError::Endpoint(
            "certificate multihash must use base64url multibase (`u`)".to_string(),
        )
    })?;
    let decoded = URL_SAFE_NO_PAD.decode(encoded).map_err(|error| {
        BrowserProtocolError::Endpoint(format!(
            "certificate multihash is not valid unpadded base64url: {error}"
        ))
    })?;
    if decoded.len() != 34
        || decoded[0] != SHA2_256_MULTIHASH_CODE
        || decoded[1] != SHA2_256_MULTIHASH_LENGTH
    {
        return Err(BrowserProtocolError::Endpoint(
            "certificate multihash must contain a 32-byte SHA-256 digest".to_string(),
        ));
    }
    decoded[2..].try_into().map_err(|_| {
        BrowserProtocolError::Endpoint(
            "certificate multihash must contain a 32-byte SHA-256 digest".to_string(),
        )
    })
}

/// Normalize a fixed-width hexadecimal wire field.
pub fn normalize_hex(value: &str, bytes: usize) -> Result<String, String> {
    let normalized = value
        .trim()
        .strip_prefix("0x")
        .or_else(|| value.trim().strip_prefix("0X"))
        .unwrap_or(value.trim())
        .replace(':', "");
    if normalized.len() != bytes.saturating_mul(2)
        || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!("expected {bytes} hexadecimal bytes"));
    }
    Ok(normalized.to_ascii_lowercase())
}

/// Decode a fixed-width hexadecimal wire field.
pub fn decode_hex(value: &str, bytes: usize) -> Result<Vec<u8>, String> {
    let normalized = normalize_hex(value, bytes)?;
    hex::decode(normalized).map_err(|error| error.to_string())
}

/// A decoded response frame retaining its JSON header and binary content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserResponseFrame {
    /// Untrusted JSON response header after structural validation.
    pub header: Value,
    /// Binary response body.
    #[serde(with = "serde_bytes")]
    pub content: Vec<u8>,
}

/// Parse one complete length-prefixed browser response.
pub fn parse_response_frame(frame: &[u8]) -> Result<BrowserResponseFrame, BrowserProtocolError> {
    let (header, content_offset, frame_length) = parse_response_header(frame)?;
    if frame.len() != frame_length {
        return Err(BrowserProtocolError::Frame(format!(
            "response length mismatch: declared {} content bytes",
            header["content_length"]
        )));
    }
    Ok(BrowserResponseFrame {
        header,
        content: frame[content_offset..].to_vec(),
    })
}

/// Determine a frame's complete length once its JSON header is available.
pub fn response_frame_length(frame: &[u8]) -> Result<Option<usize>, BrowserProtocolError> {
    if frame.len() < 4 {
        return Ok(None);
    }
    let header_length = u32::from_be_bytes(
        frame[..4]
            .try_into()
            .map_err(|_| BrowserProtocolError::Frame("missing header length".to_string()))?,
    ) as usize;
    validate_header_length(header_length)?;
    if frame.len() < 4 + header_length {
        return Ok(None);
    }
    parse_response_header(frame).map(|(_, _, length)| Some(length))
}

fn parse_response_header(frame: &[u8]) -> Result<(Value, usize, usize), BrowserProtocolError> {
    if frame.len() < 4 {
        return Err(BrowserProtocolError::Frame(
            "response ended before its four-byte header length".to_string(),
        ));
    }
    let header_length = u32::from_be_bytes(
        frame[..4]
            .try_into()
            .map_err(|_| BrowserProtocolError::Frame("missing header length".to_string()))?,
    ) as usize;
    validate_header_length(header_length)?;
    let content_offset = 4 + header_length;
    if content_offset > frame.len() {
        return Err(BrowserProtocolError::Frame(
            "response ended inside its JSON header".to_string(),
        ));
    }
    let header: Value = serde_json::from_slice(&frame[4..content_offset])
        .map_err(|error| BrowserProtocolError::Frame(format!("invalid response JSON: {error}")))?;
    if header.get("version").and_then(Value::as_u64) != Some(u64::from(BROWSER_PROTOCOL_VERSION)) {
        return Err(BrowserProtocolError::Frame(format!(
            "unsupported response version {}",
            header.get("version").unwrap_or(&Value::Null)
        )));
    }
    let content_length = header
        .get("content_length")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value <= MAX_BROWSER_RECORD_BYTES)
        .ok_or_else(|| {
            BrowserProtocolError::Frame("invalid response content length".to_string())
        })?;
    let frame_length = content_offset
        .checked_add(content_length)
        .ok_or_else(|| BrowserProtocolError::Frame("response length overflow".to_string()))?;
    Ok((header, content_offset, frame_length))
}

fn validate_header_length(header_length: usize) -> Result<(), BrowserProtocolError> {
    if header_length == 0 || header_length > MAX_BROWSER_HEADER_BYTES {
        return Err(BrowserProtocolError::Frame(format!(
            "invalid response header length {header_length}"
        )));
    }
    Ok(())
}

/// Encode one JSON-header-plus-binary request frame.
pub fn encode_request_frame(
    request_id: u64,
    request_type: &str,
    mut fields: Map<String, Value>,
    content: &[u8],
) -> Result<Vec<u8>, BrowserProtocolError> {
    if content.len() > MAX_BROWSER_RECORD_BYTES {
        return Err(BrowserProtocolError::Frame(format!(
            "request content must be at most {MAX_BROWSER_RECORD_BYTES} bytes"
        )));
    }
    fields.insert("version".to_string(), Value::from(BROWSER_PROTOCOL_VERSION));
    fields.insert("request_id".to_string(), Value::from(request_id));
    fields.insert("content_length".to_string(), Value::from(content.len()));
    fields.insert("type".to_string(), Value::from(request_type));
    let header = serde_json::to_vec(&fields)
        .map_err(|error| BrowserProtocolError::Frame(error.to_string()))?;
    validate_header_length(header.len())?;
    let capacity = 4usize
        .checked_add(header.len())
        .and_then(|size| size.checked_add(content.len()))
        .ok_or_else(|| BrowserProtocolError::Frame("request length overflow".to_string()))?;
    let header_length = u32::try_from(header.len())
        .map_err(|_| BrowserProtocolError::Frame("request header length overflow".to_string()))?;
    let mut frame = Vec::with_capacity(capacity);
    frame.extend_from_slice(&header_length.to_be_bytes());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(content);
    Ok(frame)
}

/// Metadata returned by an authenticated node's encrypted HELLO response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserHello {
    /// Response discriminator.
    #[serde(rename = "type")]
    pub response_type: String,
    /// Browser protocol name.
    pub protocol: String,
    /// Lowercase peer ID.
    pub peer_id: String,
    /// Direct endpoint authenticated by the enclosing post-quantum session.
    pub endpoint: BrowserEndpoint,
    /// Maximum node record size.
    #[serde(default)]
    pub max_chunk_size: usize,
    /// Advertised browser operations.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Advertised payment network, retained as protocol JSON.
    #[serde(default)]
    pub payment: Value,
}

/// Validate metadata received inside an authenticated post-quantum session.
pub fn validate_hello_metadata(
    hello: &BrowserHello,
    expected_endpoint: &WebRtcDirectEndpoint,
) -> Result<String, BrowserProtocolError> {
    if hello.response_type != "hello" {
        return Err(BrowserProtocolError::Identity(
            "expected a HELLO response".to_string(),
        ));
    }
    if hello.protocol != BROWSER_PROTOCOL_NAME {
        return Err(BrowserProtocolError::Identity(format!(
            "unsupported browser protocol {}",
            hello.protocol
        )));
    }
    let peer_id = normalize_hex(&hello.peer_id, 32).map_err(BrowserProtocolError::Identity)?;
    let advertised = parse_webrtc_direct_multiaddr(&hello.endpoint.multiaddr)
        .map_err(|error| BrowserProtocolError::Identity(error.to_string()))?;
    if advertised.multiaddr != expected_endpoint.multiaddr || advertised.peer_id != peer_id {
        return Err(BrowserProtocolError::Identity(
            "node advertised a different WebRTC Direct endpoint".to_string(),
        ));
    }
    if peer_id != expected_endpoint.peer_id {
        return Err(BrowserProtocolError::Identity(format!(
            "endpoint identity mismatch: expected {}, received {peer_id}",
            expected_endpoint.peer_id
        )));
    }
    Ok(peer_id)
}

/// Build the certificate-pinned ICE-lite SDP answer for a direct endpoint.
pub fn server_answer_sdp(
    endpoint: &WebRtcDirectEndpoint,
    server_ufrag: &str,
) -> Result<String, BrowserProtocolError> {
    validate_v2_server_ufrag(server_ufrag)?;
    let ip_version = if endpoint.host_protocol == "ip4" {
        "IP4"
    } else {
        "IP6"
    };
    let fingerprint = endpoint
        .certificate_hash
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    Ok(format!(
        "v=0\r\no=- 0 0 IN {ip_version} {host}\r\ns=-\r\nt=0 0\r\na=ice-lite\r\nm=application {port} UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN {ip_version} {host}\r\na=mid:0\r\na=ice-options:ice2\r\na=ice-ufrag:{credential}\r\na=ice-pwd:{credential}\r\na=fingerprint:sha-256 {fingerprint}\r\na=setup:passive\r\na=sctp-port:5000\r\na=max-message-size:{WEBRTC_WRITE_CHUNK_BYTES}\r\na=candidate:1467250027 1 UDP 1467250027 {host} {port} typ host\r\na=end-of-candidates\r\n",
        host = endpoint.host,
        port = endpoint.port,
        credential = server_ufrag,
    ))
}

/// Read the effective browser-generated ICE password from a local SDP offer.
pub fn ice_password_from_sdp(sdp: &str) -> Result<String, BrowserProtocolError> {
    if sdp.is_empty() {
        return Err(BrowserProtocolError::Frame(
            "browser created an empty WebRTC offer".to_string(),
        ));
    }

    let mut passwords = sdp
        .lines()
        .filter_map(|line| line.strip_prefix("a=ice-pwd:"));
    let password = passwords.next().ok_or_else(|| {
        BrowserProtocolError::Frame(
            "browser local description did not contain an ICE password".to_string(),
        )
    })?;
    if !is_valid_ice_pwd(password) {
        return Err(BrowserProtocolError::Frame(
            "browser local description contained an invalid ICE password".to_string(),
        ));
    }
    if passwords.any(|other| other != password) {
        return Err(BrowserProtocolError::Frame(
            "browser local description contained multiple ICE passwords".to_string(),
        ));
    }
    Ok(password.to_string())
}

/// Build the v2 server username fragment that carries the browser's ICE password.
pub fn v2_server_ice_credential(client_pwd: &str) -> Result<String, BrowserProtocolError> {
    if !is_valid_ice_pwd(client_pwd) {
        return Err(BrowserProtocolError::Endpoint(
            "invalid browser ICE password for WebRTC Direct v2".to_string(),
        ));
    }
    let server_ufrag = format!("saorsa+webrtc+v2/{client_pwd}");
    if !is_valid_ice_ufrag(&server_ufrag) {
        return Err(BrowserProtocolError::Endpoint(
            "browser ICE password is too long for WebRTC Direct v2".to_string(),
        ));
    }
    Ok(server_ufrag)
}

fn validate_v2_server_ufrag(value: &str) -> Result<(), BrowserProtocolError> {
    let client_pwd = value.strip_prefix("saorsa+webrtc+v2/").ok_or_else(|| {
        BrowserProtocolError::Endpoint(
            "unsupported Saorsa WebRTC Direct connection profile".to_string(),
        )
    })?;
    if !is_valid_ice_ufrag(value) || !is_valid_ice_pwd(client_pwd) {
        return Err(BrowserProtocolError::Endpoint(
            "invalid Saorsa WebRTC Direct v2 ICE credential".to_string(),
        ));
    }
    Ok(())
}

fn is_ice_char_string(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

fn is_valid_ice_ufrag(value: &str) -> bool {
    (4..=256).contains(&value.len()) && is_ice_char_string(value)
}

fn is_valid_ice_pwd(value: &str) -> bool {
    (22..=256).contains(&value.len()) && is_ice_char_string(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> String {
        let mut multihash = vec![0x12, 0x20];
        multihash.extend([0x11; 32]);
        format!(
            "/ip4/127.0.0.1/udp/24000/webrtc-direct/certhash/u{}/p2p/{}",
            URL_SAFE_NO_PAD.encode(multihash),
            "ab".repeat(32)
        )
    }

    #[test]
    fn parses_literal_certificate_pinned_endpoint() {
        let parsed = parse_webrtc_direct_multiaddr(&endpoint()).expect("parse endpoint");
        assert_eq!(parsed.host_protocol, "ip4");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 24000);
        assert_eq!(parsed.certificate_hash, [0x11; 32]);
        assert!(parse_webrtc_direct_multiaddr(&endpoint().replacen(
            "/ip4/127.0.0.1",
            "/dns/node.example",
            1
        ))
        .is_err());
    }

    #[test]
    fn response_frame_round_trip() {
        let mut fields = Map::new();
        fields.insert("address".to_string(), Value::from("11".repeat(32)));
        let frame =
            encode_request_frame(9, "get_chunk", fields, &[1, 2, 3]).expect("encode request");
        let parsed = parse_response_frame(&frame).expect("parse response-shaped frame");
        assert_eq!(parsed.header["request_id"], 9);
        assert_eq!(parsed.content, vec![1, 2, 3]);
    }

    #[test]
    fn hello_metadata_must_match_the_authenticated_endpoint() {
        let expected = parse_webrtc_direct_multiaddr(&endpoint()).expect("parse endpoint");
        let mut hello = BrowserHello {
            response_type: "hello".to_string(),
            protocol: BROWSER_PROTOCOL_NAME.to_string(),
            peer_id: "ab".repeat(32),
            endpoint: BrowserEndpoint {
                multiaddr: endpoint(),
            },
            max_chunk_size: MAX_BROWSER_RECORD_BYTES,
            capabilities: vec!["get_chunk".to_string()],
            payment: Value::Null,
        };
        assert_eq!(
            validate_hello_metadata(&hello, &expected).expect("validate HELLO"),
            "ab".repeat(32)
        );

        hello.peer_id = "cd".repeat(32);
        assert!(validate_hello_metadata(&hello, &expected).is_err());
    }

    #[test]
    fn synthesizes_pinned_v2_answer_without_mutating_the_offer() {
        let endpoint = parse_webrtc_direct_multiaddr(&endpoint()).expect("parse endpoint");
        let offer = "v=0\r\na=ice-ufrag:browserUfrag\r\na=ice-pwd:browserClientPassword1234\r\n";
        let password = ice_password_from_sdp(offer).expect("read ICE password");
        assert_eq!(password, "browserClientPassword1234");
        let credential = v2_server_ice_credential(&password).expect("v2 server credential");
        let answer = server_answer_sdp(&endpoint, &credential).expect("answer");
        assert!(answer.contains("a=ice-lite"));
        assert!(answer.contains("a=fingerprint:sha-256 11:11:11:11"));
        assert!(answer.contains(&format!("a=ice-ufrag:{credential}")));
        assert!(answer.contains(&format!("a=ice-pwd:{credential}")));
        assert!(offer.contains("a=ice-ufrag:browserUfrag"));
        assert!(offer.contains("a=ice-pwd:browserClientPassword1234"));
    }

    #[test]
    fn rejects_malformed_or_ambiguous_v2_ice_passwords() {
        assert!(ice_password_from_sdp("v=0\r\n").is_err());
        assert!(ice_password_from_sdp("a=ice-pwd:too-short\r\n").is_err());
        assert!(ice_password_from_sdp(
            "a=ice-pwd:browserClientPassword1234\r\na=ice-pwd:differentBrowserPassword12\r\n"
        )
        .is_err());
        assert!(v2_server_ice_credential(&format!("{}\r\na=candidate:x", "a".repeat(22))).is_err());
        assert!(v2_server_ice_credential(&"a".repeat(240)).is_err());
    }
}
