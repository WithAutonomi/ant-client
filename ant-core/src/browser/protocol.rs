//! Browser-facing WebRTC Direct wire profile.
//!
//! The complete application contract lives in `saorsa-webrtc`; this module is
//! retained as a compatibility re-export for existing `ant-core` callers.

pub use saorsa_webrtc::{
    decode_hex, encode_request_frame, encode_response_frame, ice_password_from_sdp, normalize_hex,
    parse_request_frame, parse_request_header, parse_response_frame, parse_webrtc_direct_multiaddr,
    response_frame_length, server_answer_sdp, v2_server_ice_credential, validate_hello_metadata,
    BrowserCommitmentArtifact, BrowserEndpoint, BrowserEndpointInput, BrowserHello, BrowserNode,
    BrowserPaymentNetwork, BrowserProtocolError, BrowserQuoteArtifact, BrowserRequest,
    BrowserRequestBody, BrowserRequestFrame, BrowserResponse, BrowserResponseBody,
    BrowserResponseFrame, BrowserResponseStatus, WebRtcDirectEndpoint, BROWSER_PROTOCOL_NAME,
    BROWSER_PROTOCOL_VERSION, MAX_BROWSER_FRAME_BYTES, MAX_BROWSER_HEADER_BYTES,
    MAX_BROWSER_RECORD_BYTES, MAX_BROWSER_RESPONSE_BYTES, MAX_WEBRTC_DIRECT_MULTIADDR_LENGTH,
    WEBRTC_DIRECT_DATA_CHANNEL, WEBRTC_WRITE_CHUNK_BYTES,
};
