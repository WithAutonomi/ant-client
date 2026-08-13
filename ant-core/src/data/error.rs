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

    /// A remote node rejected a chunk PUT at the application layer.
    ///
    /// The node responded with a structured `ProtocolError`, so the
    /// transport round-trip succeeded — this is an application-level
    /// rejection (payment-failed, storage/disk-full, quote-stale,
    /// merkle-pool-rejected), NOT evidence the client is sending too
    /// fast. It therefore classifies as `Outcome::ApplicationError`
    /// (see `classify_error`) and does not push the adaptive store
    /// limiter down. The structured `source` is preserved (rather than
    /// flattened into `Protocol`) so the controller — and a future
    /// full-node skip-list (V2-469) — can key on the reason.
    #[error("remote PUT rejected for {address}: {source}")]
    RemotePut {
        /// Hex-encoded chunk address the rejection was for.
        address: String,
        /// The structured remote rejection reason.
        source: ant_protocol::ProtocolError,
    },

    /// A chunk PUT missed its close-group quorum, and the shortfall was
    /// caused by close-group **dial/relay churn** — dead or stale relayed
    /// peer addresses that could not be dialled — with **no** evidence of
    /// local backpressure (no PUT-response timeouts among the failures).
    ///
    /// This is remote peer churn (the same dead relayed DHT addresses as
    /// V2-551), NOT evidence the client is sending too fast. More local send
    /// concurrency neither causes nor fixes it, so it classifies as
    /// `Outcome::ApplicationError` (see `classify_error`) and does NOT push
    /// the adaptive store limiter down (V2-554). Distinct from
    /// [`Error::InsufficientPeers`], which a close-group shortfall keeps when
    /// any PUT-response *timeout* is present (genuine local backpressure that
    /// must still cut the cap). Like `InsufficientPeers`, it is a recoverable
    /// quorum shortfall and is deferred/retried, not fatal.
    #[error("close-group PUT shortfall (dial churn): {0}")]
    CloseGroupShortfall(String),

    /// Invalid data received.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// The requested record does not exist on the network.
    ///
    /// A well-formed address with nothing stored at it — e.g. a `DataMap`
    /// chunk lookup or a reconstruction fetch that came back empty from
    /// every queried peer. Distinct from [`Error::InvalidData`], which means
    /// content WAS retrieved but is malformed or fails integrity checks, so
    /// callers can show "not found — check the address" instead of a
    /// caller-bug message.
    #[error("not found: {0}")]
    NotFound(String),

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

    /// The network refused to quote this client because it settles payments
    /// under superseded rules.
    ///
    /// Deliberately terminal. A client that reaches this would pay an amount
    /// every storer rejects, and merkle payments are not refundable, so
    /// retrying or falling back to an older request shape would convert a
    /// clean refusal into destroyed money. The message is the storer's own
    /// wording, which already tells the user how to upgrade and that nothing
    /// has been charged.
    #[error("{0}")]
    ClientUpdateRequired(String),

    /// A storer declined to quote because *it* settles under older rules than
    /// this client.
    ///
    /// The opposite of [`Self::ClientUpdateRequired`] and deliberately not
    /// terminal. Nothing is wrong with this client, so the upload should use a
    /// different peer and say nothing to the user. During a client-first
    /// rollout most of the fleet is briefly in this state. If too few peers
    /// remain the operation fails for lack of quotes, which is the correct
    /// outcome: it fails before any payment rather than after.
    #[error("{0}")]
    StorerUpdateRequired(String),

    /// BLS signature verification failed.
    #[error("signature verification failed: {0}")]
    SignatureVerification(String),

    /// Self-encryption operation failed.
    #[error("encryption error: {0}")]
    Encryption(String),

    /// The operation was cancelled by the caller rather than failing.
    ///
    /// Returned, for example, by streaming downloads when the consumer drops
    /// its receiver (a client disconnect) — distinct from a transport
    /// [`Error::Network`] failure, since nothing went wrong on the wire.
    #[error("operation cancelled: {0}")]
    Cancelled(String),

    /// Data already exists on the network — no payment needed.
    #[error("already stored on network")]
    AlreadyStored,

    /// A peer's quote `pub_key` does not BLAKE3-hash to the peer ID. The
    /// storer would reject any `ProofOfPayment` containing this quote, so
    /// the client drops the response before payment.
    #[error("bad quote binding from peer {peer_id}: {detail}")]
    BadQuoteBinding {
        /// The peer ID we got the quote from (claimed identity).
        peer_id: String,
        /// Diagnostic detail (e.g. "BLAKE3(pub_key) = …, peer_id = …").
        detail: String,
    },

    /// ADR-0004: a quote's commitment binding does not hold — its price is not
    /// `calculate_price(committed_key_count)`, its `(count, pin)` shape is
    /// incoherent, or a shipped commitment does not match the pinned count/hash.
    /// The storer's arithmetic gate would reject such a quote, so the client
    /// drops it before paying ("the client pays nothing it cannot resolve").
    #[error("bad quote commitment from peer {peer_id}: {detail}")]
    BadQuoteCommitment {
        /// The peer ID we got the quote from.
        peer_id: String,
        /// Diagnostic detail (which binding rule failed).
        detail: String,
    },

    /// Not enough disk space for the operation.
    #[error("insufficient disk space: {0}")]
    InsufficientDiskSpace(String),

    /// An external-signer merkle preparation was handed more addresses than a
    /// single merkle tree can hold.
    ///
    /// The wallet path splits an oversized upload into several trees and pays
    /// each in its own transaction. The external-signer contract is one
    /// prepared batch → one signature → one payment, so that split has no
    /// representation there. Raised before any candidate collection or
    /// on-chain spend, rather than silently paying under a different model.
    #[error(
        "merkle batch of {addresses} addresses exceeds the {max_leaves}-leaf limit of a single \
         merkle tree; external signing cannot span multiple payment transactions"
    )]
    MerkleBatchTooLarge {
        /// Number of addresses the caller asked to prepare.
        addresses: usize,
        /// Maximum leaves one merkle tree can hold (`MAX_LEAVES`).
        max_leaves: usize,
    },

    /// Cost estimation could not reach a representative quote.
    ///
    /// Returned by [`crate::data::Client::estimate_upload_cost`] when every
    /// sampled chunk address reported `AlreadyStored`, so the network price
    /// for the remainder of the file cannot be inferred from a sample.
    /// The attached message describes how many addresses were tried.
    #[error("cost estimation inconclusive: {0}")]
    CostEstimationInconclusive(String),

    /// Upload partially succeeded -- some chunks stored, some failed after retries.
    ///
    /// The `stored` addresses can be used for progress tracking and resume.
    #[error(
        "partial upload: {stored_count}/{total_chunks} stored, {failed_count} failed: {reason}"
    )]
    PartialUpload {
        /// Addresses of successfully stored chunks.
        stored: Vec<[u8; 32]>,
        /// Number of successfully stored chunks.
        stored_count: usize,
        /// Addresses and error messages of chunks that failed after retries.
        failed: Vec<([u8; 32], String)>,
        /// Number of failed chunks.
        failed_count: usize,
        /// Total number of chunks the upload was attempting to store.
        total_chunks: usize,
        /// On-chain spend incurred so far. Boxed to keep the `Error` enum small
        /// (the variant is returned in `Result` across the crate; without the
        /// box the two cost fields would trip `clippy::result_large_err`).
        spend: Box<PartialUploadSpend>,
        /// Root cause description.
        reason: String,
    },
}

/// On-chain spend recorded on a [`Error::PartialUpload`].
///
/// A partial upload still spends money for the chunks it paid for. In the
/// single-node path payment precedes store, so this includes a failed wave's
/// chunks; surfacing it lets the caller report real spend rather than silently
/// dropping it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialUploadSpend {
    /// Storage cost paid on-chain so far, in atto-tokens.
    pub storage_cost_atto: String,
    /// Gas cost paid on-chain so far, in wei.
    pub gas_cost_wei: u128,
}

// ant-node is only linked when the `devnet` feature is on, so the
// blanket `From` impl follows that gate. LocalDevnet maps node errors
// to `Error::Network` via this conversion; default builds never see it.
#[cfg(feature = "devnet")]
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
    fn test_display_not_found() {
        let err = Error::NotFound("DataMap chunk not found at abcd".to_string());
        assert_eq!(
            err.to_string(),
            "not found: DataMap chunk not found at abcd"
        );
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
    fn test_display_close_group_shortfall() {
        let err = Error::CloseGroupShortfall("Stored on 3 peers, need 4".to_string());
        assert_eq!(
            err.to_string(),
            "close-group PUT shortfall (dial churn): Stored on 3 peers, need 4"
        );
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
    fn test_display_cancelled() {
        let err = Error::Cancelled("download stream receiver dropped".to_string());
        assert_eq!(
            err.to_string(),
            "operation cancelled: download stream receiver dropped"
        );
    }

    #[test]
    fn test_display_insufficient_disk_space() {
        let err = Error::InsufficientDiskSpace("need 100 MB but only 10 MB available".to_string());
        assert_eq!(
            err.to_string(),
            "insufficient disk space: need 100 MB but only 10 MB available"
        );
    }

    #[test]
    fn test_display_merkle_batch_too_large() {
        let err = Error::MerkleBatchTooLarge {
            addresses: 257,
            max_leaves: 256,
        };
        assert_eq!(
            err.to_string(),
            "merkle batch of 257 addresses exceeds the 256-leaf limit of a single merkle tree; \
             external signing cannot span multiple payment transactions"
        );
    }

    #[test]
    fn test_display_cost_estimation_inconclusive() {
        let err = Error::CostEstimationInconclusive(
            "sampled 5 addresses, all already stored".to_string(),
        );
        assert_eq!(
            err.to_string(),
            "cost estimation inconclusive: sampled 5 addresses, all already stored"
        );
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }
}
