//! Error types for data operations.

use thiserror::Error;

/// Result type alias using the data Error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur in data operations.
#[derive(Error, Debug)]
pub enum Error {
    /// Network operation failed.
    #[error("network error: {0}")]
    Network(String),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(String),

    /// Payment operation failed.
    #[error("payment error: {0}")]
    Payment(String),

    /// Protocol error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Invalid data received.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Cryptographic error.
    #[error("crypto error: {0}")]
    Crypto(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(String),

    /// Timeout waiting for a response.
    #[error("timeout: {0}")]
    Timeout(String),

    /// Insufficient peers for the operation.
    #[error("insufficient peers: {0}")]
    InsufficientPeers(String),

    /// BLS signature verification failed.
    #[error("signature verification failed: {0}")]
    SignatureVerification(String),

    /// Self-encryption operation failed.
    #[error("encryption error: {0}")]
    Encryption(String),

    /// Data already exists on the network — no payment needed.
    #[error("already stored on network")]
    AlreadyStored,
}

impl From<ant_node::Error> for Error {
    fn from(e: ant_node::Error) -> Self {
        Self::Network(e.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_display_network() {
        let err = Error::Network("connection refused".to_string());
        assert_eq!(err.to_string(), "network error: connection refused");
    }

    #[test]
    fn test_display_storage() {
        let err = Error::Storage("disk full".to_string());
        assert_eq!(err.to_string(), "storage error: disk full");
    }

    #[test]
    fn test_display_payment() {
        let err = Error::Payment("insufficient funds".to_string());
        assert_eq!(err.to_string(), "payment error: insufficient funds");
    }

    #[test]
    fn test_display_protocol() {
        let err = Error::Protocol("invalid message".to_string());
        assert_eq!(err.to_string(), "protocol error: invalid message");
    }

    #[test]
    fn test_display_invalid_data() {
        let err = Error::InvalidData("bad hash".to_string());
        assert_eq!(err.to_string(), "invalid data: bad hash");
    }

    #[test]
    fn test_display_serialization() {
        let err = Error::Serialization("decode failed".to_string());
        assert_eq!(err.to_string(), "serialization error: decode failed");
    }

    #[test]
    fn test_display_crypto() {
        let err = Error::Crypto("key mismatch".to_string());
        assert_eq!(err.to_string(), "crypto error: key mismatch");
    }

    #[test]
    fn test_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = Error::Io(io_err);
        assert_eq!(err.to_string(), "I/O error: file missing");
    }

    #[test]
    fn test_display_config() {
        let err = Error::Config("bad value".to_string());
        assert_eq!(err.to_string(), "configuration error: bad value");
    }

    #[test]
    fn test_display_timeout() {
        let err = Error::Timeout("30s elapsed".to_string());
        assert_eq!(err.to_string(), "timeout: 30s elapsed");
    }

    #[test]
    fn test_display_insufficient_peers() {
        let err = Error::InsufficientPeers("need 5, got 2".to_string());
        assert_eq!(err.to_string(), "insufficient peers: need 5, got 2");
    }

    #[test]
    fn test_display_signature_verification() {
        let err = Error::SignatureVerification("invalid sig".to_string());
        assert_eq!(
            err.to_string(),
            "signature verification failed: invalid sig"
        );
    }

    #[test]
    fn test_display_encryption() {
        let err = Error::Encryption("decrypt failed".to_string());
        assert_eq!(err.to_string(), "encryption error: decrypt failed");
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }
}
