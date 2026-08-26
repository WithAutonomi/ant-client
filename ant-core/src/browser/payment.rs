//! Verification and payment planning shared by native and browser clients.

use super::crypto::{keccak256, verify_ml_dsa_65};
use super::protocol::normalize_hex;
use serde::{Deserialize, Serialize};

const PAYMENT_MULTIPLIER: u128 = 3;
const PRICE_BASELINE_WEI: u128 = 3_906_250_000_000_000;
const PRICE_COEFFICIENT_WEI: u128 = 35_156_250_000_000_000;
const PRICE_DIVISOR_SQUARED: u128 = 6_000 * 6_000;
const MAX_COMMITMENT_KEY_COUNT: u32 = 1_000_000;
const MAX_COMMITMENT_SIDECAR_BYTES: usize = 8 * 1024;
const DOMAIN_COMMITMENT: &[u8] = b"autonomi.ant.replication.storage_commitment.v1";
const DOMAIN_COMMITMENT_HASH: &[u8] = b"autonomi.ant.replication.commitment_hash.v1";

/// JSON-safe form of a node's signed storage commitment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserCommitmentArtifact {
    /// MessagePack-encoded native commitment.
    pub encoded: String,
    /// Merkle root.
    pub root: String,
    /// Number of committed keys.
    pub key_count: u32,
    /// Signing peer ID.
    pub sender_peer_id: String,
    /// ML-DSA-65 public key.
    pub sender_public_key: String,
    /// ML-DSA-65 signature.
    pub signature: String,
}

/// JSON-safe form of the native EVM payment quote returned over WebRTC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserQuoteArtifact {
    /// Quoting node peer ID.
    pub peer_id: String,
    /// Content address being quoted.
    pub content: String,
    /// Quote timestamp in seconds since the Unix epoch.
    pub timestamp_secs: u64,
    /// Decimal token price.
    pub price: String,
    /// Twenty-byte node rewards address.
    pub rewards_address: String,
    /// ML-DSA-65 public key.
    pub public_key: String,
    /// ML-DSA-65 signature.
    pub signature: String,
    /// Storage commitment key count used by the pricing curve.
    pub committed_key_count: u32,
    /// Optional pinned storage commitment.
    pub commitment_pin: Option<String>,
    /// Keccak-256 EVM payment quote hash.
    pub quote_hash: String,
    /// Optional resolved native storage commitment.
    pub commitment: Option<BrowserCommitmentArtifact>,
}

/// A quote that is safe to hand to a transaction signer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedStorageQuote {
    /// Original verified quote sent back to the selected storage nodes.
    pub quote: BrowserQuoteArtifact,
    /// Lowercase EVM quote hash without `0x`.
    #[serde(rename = "quoteHash")]
    pub quote_hash: String,
    /// Checksummed-independent lowercase rewards address with `0x`.
    #[serde(rename = "rewardsAddress")]
    pub rewards_address: String,
    /// Decimal amount paid after applying Autonomi's replication multiplier.
    pub amount: String,
}

/// Storage quote validation error.
#[derive(Debug, thiserror::Error)]
#[error("invalid storage quote: {0}")]
pub struct StorageQuoteError(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NativeStorageCommitment {
    root: [u8; 32],
    key_count: u32,
    sender_peer_id: [u8; 32],
    sender_public_key: Vec<u8>,
    signature: Vec<u8>,
}

/// Compute the EVM-facing `PaymentQuote` Keccak-256 hash.
#[must_use]
pub fn payment_quote_hash(signed_bytes: &[u8], public_key: &[u8], signature: &[u8]) -> [u8; 32] {
    keccak256(&[signed_bytes, public_key, signature])
}

/// Sum verified decimal quote amounts without exposing integer arithmetic to
/// JavaScript or a wallet adapter.
pub fn storage_payment_total(quotes: &[VerifiedStorageQuote]) -> Result<String, StorageQuoteError> {
    quotes
        .iter()
        .try_fold(0u128, |total, quote| {
            let amount = parse_decimal_u128(&quote.amount, "storage payment amount")?;
            total
                .checked_add(amount)
                .ok_or_else(|| StorageQuoteError("storage payment total overflow".to_string()))
        })
        .map(|total| total.to_string())
}

/// Fully verify a quote, its commitment, peer binding, price, and EVM hash.
pub fn verify_storage_quote(
    mut quote: BrowserQuoteArtifact,
    expected_address: &str,
    expected_peer_id: &str,
) -> Result<VerifiedStorageQuote, StorageQuoteError> {
    let expected_address = normalize_hex(expected_address, 32).map_err(StorageQuoteError)?;
    let expected_peer_id = normalize_hex(expected_peer_id, 32).map_err(StorageQuoteError)?;
    quote.content = normalize_hex(&quote.content, 32).map_err(StorageQuoteError)?;
    quote.peer_id = normalize_hex(&quote.peer_id, 32).map_err(StorageQuoteError)?;
    if quote.content != expected_address {
        return Err(StorageQuoteError(
            "storage quote is for a different chunk".to_string(),
        ));
    }
    if quote.peer_id != expected_peer_id {
        return Err(StorageQuoteError(
            "storage quote belongs to a different WebRtcDirect peer".to_string(),
        ));
    }
    if quote.committed_key_count > MAX_COMMITMENT_KEY_COUNT {
        return Err(StorageQuoteError(format!(
            "invalid committed key count {}",
            quote.committed_key_count
        )));
    }
    let public_key = decode_unbounded_hex(&quote.public_key, "quote public key")?;
    let signature = decode_unbounded_hex(&quote.signature, "quote signature")?;
    if blake3::hash(&public_key).to_hex().as_str() != quote.peer_id {
        return Err(StorageQuoteError(
            "storage quote public key is not bound to its peer ID".to_string(),
        ));
    }
    let price = parse_decimal_u128(&quote.price, "quote price")?;
    let expected_price = calculate_price(quote.committed_key_count);
    if price != expected_price {
        return Err(StorageQuoteError(
            "storage quote price is not bound to its committed key count".to_string(),
        ));
    }
    let rewards = normalize_hex(&quote.rewards_address, 20).map_err(StorageQuoteError)?;
    quote.rewards_address.clone_from(&rewards);
    let commitment_pin = quote
        .commitment_pin
        .as_deref()
        .map(|pin| normalize_hex(pin, 32).map_err(StorageQuoteError))
        .transpose()?;
    quote.commitment_pin.clone_from(&commitment_pin);
    let signed_bytes = canonical_quote_bytes(&quote, price, &rewards, commitment_pin.as_deref())?;
    if !verify_ml_dsa_65(&public_key, &signature, &signed_bytes, b"") {
        return Err(StorageQuoteError(
            "storage quote has an invalid ML-DSA-65 signature".to_string(),
        ));
    }
    let quote_hash = hex::encode(payment_quote_hash(&signed_bytes, &public_key, &signature));
    if normalize_hex(&quote.quote_hash, 32).map_err(StorageQuoteError)? != quote_hash {
        return Err(StorageQuoteError(
            "storage quote hash does not match its signed fields".to_string(),
        ));
    }
    quote.quote_hash.clone_from(&quote_hash);

    if quote.committed_key_count == 0 {
        if quote.commitment_pin.is_some() || quote.commitment.is_some() {
            return Err(StorageQuoteError(
                "baseline storage quote has an incoherent commitment".to_string(),
            ));
        }
    } else {
        let pin = commitment_pin
            .ok_or_else(|| StorageQuoteError("bound storage quote omitted its pin".to_string()))?;
        verify_commitment(
            quote.commitment.as_mut().ok_or_else(|| {
                StorageQuoteError("bound quote omitted its storage commitment".to_string())
            })?,
            &quote.peer_id,
            quote.committed_key_count,
            &pin,
        )?;
    }

    let amount = price
        .checked_mul(PAYMENT_MULTIPLIER)
        .ok_or_else(|| StorageQuoteError("storage payment amount overflow".to_string()))?;
    Ok(VerifiedStorageQuote {
        quote,
        quote_hash,
        rewards_address: format!("0x{rewards}"),
        amount: amount.to_string(),
    })
}

fn canonical_quote_bytes(
    quote: &BrowserQuoteArtifact,
    price: u128,
    rewards: &str,
    commitment_pin: Option<&str>,
) -> Result<Vec<u8>, StorageQuoteError> {
    let content =
        hex::decode(&quote.content).map_err(|error| StorageQuoteError(error.to_string()))?;
    let rewards = hex::decode(rewards).map_err(|error| StorageQuoteError(error.to_string()))?;
    let mut bytes = Vec::with_capacity(32 + 8 + 32 + 20 + 4 + 33);
    bytes.extend_from_slice(&content);
    bytes.extend_from_slice(&quote.timestamp_secs.to_le_bytes());
    bytes.extend_from_slice(&price.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(&rewards);
    bytes.extend_from_slice(&quote.committed_key_count.to_le_bytes());
    if let Some(pin) = commitment_pin {
        bytes.push(1);
        bytes.extend(hex::decode(pin).map_err(|error| StorageQuoteError(error.to_string()))?);
    } else {
        bytes.push(0);
    }
    Ok(bytes)
}

fn verify_commitment(
    artifact: &mut BrowserCommitmentArtifact,
    expected_peer_id: &str,
    expected_key_count: u32,
    expected_pin: &str,
) -> Result<(), StorageQuoteError> {
    let encoded = decode_unbounded_hex(&artifact.encoded, "storage commitment sidecar")?;
    if encoded.len() > MAX_COMMITMENT_SIDECAR_BYTES {
        return Err(StorageQuoteError(
            "storage commitment sidecar exceeds the protocol limit".to_string(),
        ));
    }
    let commitment: NativeStorageCommitment = rmp_serde::from_slice(&encoded).map_err(|error| {
        StorageQuoteError(format!(
            "storage commitment sidecar is not valid MessagePack: {error}"
        ))
    })?;
    let root = normalize_hex(&artifact.root, 32).map_err(StorageQuoteError)?;
    let peer_id = normalize_hex(&artifact.sender_peer_id, 32).map_err(StorageQuoteError)?;
    let public_key =
        decode_unbounded_hex(&artifact.sender_public_key, "storage commitment public key")?;
    let signature = decode_unbounded_hex(&artifact.signature, "storage commitment signature")?;
    if commitment.root != decode_array_32(&root)?
        || commitment.key_count != artifact.key_count
        || commitment.sender_peer_id != decode_array_32(&peer_id)?
        || commitment.sender_public_key != public_key
        || commitment.signature != signature
    {
        return Err(StorageQuoteError(
            "storage commitment sidecar differs from the verified commitment".to_string(),
        ));
    }
    artifact.root = root;
    artifact.sender_peer_id = peer_id.clone();
    artifact.sender_public_key = hex::encode(&public_key);
    artifact.signature = hex::encode(&signature);
    artifact.encoded = hex::encode(&encoded);
    if commitment.key_count != expected_key_count {
        return Err(StorageQuoteError(
            "storage commitment key count does not match quote".to_string(),
        ));
    }
    if peer_id != expected_peer_id
        || blake3::hash(&public_key).to_hex().as_str() != expected_peer_id
    {
        return Err(StorageQuoteError(
            "storage commitment belongs to a different peer".to_string(),
        ));
    }
    let signed_payload = commitment_signed_payload(&commitment)?;
    if !verify_ml_dsa_65(&public_key, &signature, &signed_payload, DOMAIN_COMMITMENT) {
        return Err(StorageQuoteError(
            "storage commitment has an invalid ML-DSA-65 signature".to_string(),
        ));
    }
    let postcard =
        postcard::to_allocvec(&commitment).map_err(|error| StorageQuoteError(error.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN_COMMITMENT_HASH);
    hasher.update(&postcard);
    if hasher.finalize().to_hex().as_str() != expected_pin {
        return Err(StorageQuoteError(
            "storage commitment does not resolve the quote pin".to_string(),
        ));
    }
    Ok(())
}

fn commitment_signed_payload(
    commitment: &NativeStorageCommitment,
) -> Result<Vec<u8>, StorageQuoteError> {
    let key_length = u32::try_from(commitment.sender_public_key.len())
        .map_err(|_| StorageQuoteError("storage commitment public key is too large".to_string()))?;
    let mut payload = Vec::with_capacity(32 + 4 + 32 + 4 + commitment.sender_public_key.len());
    payload.extend_from_slice(&commitment.root);
    payload.extend_from_slice(&commitment.key_count.to_le_bytes());
    payload.extend_from_slice(&commitment.sender_peer_id);
    payload.extend_from_slice(&key_length.to_le_bytes());
    payload.extend_from_slice(&commitment.sender_public_key);
    Ok(payload)
}

fn calculate_price(key_count: u32) -> u128 {
    let count = u128::from(key_count);
    PRICE_BASELINE_WEI
        + count
            .saturating_mul(count)
            .saturating_mul(PRICE_COEFFICIENT_WEI)
            / PRICE_DIVISOR_SQUARED
}

fn parse_decimal_u128(value: &str, label: &str) -> Result<u128, StorageQuoteError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(StorageQuoteError(format!("invalid {label}")));
    }
    value
        .parse::<u128>()
        .map_err(|_| StorageQuoteError(format!("{label} exceeds the supported protocol range")))
}

fn decode_unbounded_hex(value: &str, label: &str) -> Result<Vec<u8>, StorageQuoteError> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if (value.len() & 1) != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StorageQuoteError(format!("invalid {label}")));
    }
    hex::decode(value).map_err(|error| StorageQuoteError(format!("invalid {label}: {error}")))
}

fn decode_array_32(value: &str) -> Result<[u8; 32], StorageQuoteError> {
    let decoded = hex::decode(value).map_err(|error| StorageQuoteError(error.to_string()))?;
    decoded.try_into().map_err(|bytes: Vec<u8>| {
        StorageQuoteError(format!("expected 32 bytes, received {}", bytes.len()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ant_protocol::pqc::api::ml_dsa_65;

    fn baseline_quote() -> (BrowserQuoteArtifact, String, String) {
        let content = [0x31; 32];
        let rewards = [0x44; 20];
        let timestamp = 1_775_000_000;
        let (public_key, secret_key) = ml_dsa_65().generate_keypair().expect("keypair");
        let public_key = public_key.to_bytes();
        let peer_id = blake3::hash(&public_key).to_hex().to_string();
        let mut quote = BrowserQuoteArtifact {
            peer_id: peer_id.clone(),
            content: hex::encode(content),
            timestamp_secs: timestamp,
            price: PRICE_BASELINE_WEI.to_string(),
            rewards_address: hex::encode(rewards),
            public_key: hex::encode(&public_key),
            signature: String::new(),
            committed_key_count: 0,
            commitment_pin: None,
            quote_hash: String::new(),
            commitment: None,
        };
        let payload =
            canonical_quote_bytes(&quote, PRICE_BASELINE_WEI, &hex::encode(rewards), None)
                .expect("payload");
        let signature = ml_dsa_65()
            .sign(&secret_key, &payload)
            .expect("signature")
            .to_bytes();
        quote.signature = hex::encode(&signature);
        quote.quote_hash = hex::encode(payment_quote_hash(&payload, &public_key, &signature));
        (quote, hex::encode(content), peer_id)
    }

    fn bound_quote() -> (BrowserQuoteArtifact, String, String) {
        let content = [0x31; 32];
        let rewards = [0x42; 20];
        let root = [0x53; 32];
        let key_count = 23;
        let timestamp = 1_775_000_001;
        let (public_key, secret_key) = ml_dsa_65().generate_keypair().expect("keypair");
        let public_key = public_key.to_bytes();
        let peer_id = blake3::hash(&public_key).into();
        let mut commitment = NativeStorageCommitment {
            root,
            key_count,
            sender_peer_id: peer_id,
            sender_public_key: public_key.clone(),
            signature: Vec::new(),
        };
        let commitment_payload = commitment_signed_payload(&commitment).expect("payload");
        commitment.signature = ml_dsa_65()
            .sign_with_context(&secret_key, &commitment_payload, DOMAIN_COMMITMENT)
            .expect("commitment signature")
            .to_bytes();
        let encoded = rmp_serde::to_vec(&commitment).expect("MessagePack commitment");
        let postcard = postcard::to_allocvec(&commitment).expect("postcard commitment");
        let mut pin_hasher = blake3::Hasher::new();
        pin_hasher.update(DOMAIN_COMMITMENT_HASH);
        pin_hasher.update(&postcard);
        let pin = pin_hasher.finalize().to_hex().to_string();
        let peer_id = hex::encode(peer_id);
        let price = calculate_price(key_count);
        let mut quote = BrowserQuoteArtifact {
            peer_id: peer_id.clone(),
            content: hex::encode(content),
            timestamp_secs: timestamp,
            price: price.to_string(),
            rewards_address: hex::encode(rewards),
            public_key: hex::encode(&public_key),
            signature: String::new(),
            committed_key_count: key_count,
            commitment_pin: Some(pin.clone()),
            quote_hash: String::new(),
            commitment: Some(BrowserCommitmentArtifact {
                encoded: hex::encode(encoded),
                root: hex::encode(commitment.root),
                key_count,
                sender_peer_id: peer_id.clone(),
                sender_public_key: hex::encode(&public_key),
                signature: hex::encode(&commitment.signature),
            }),
        };
        let payload = canonical_quote_bytes(&quote, price, &hex::encode(rewards), Some(&pin))
            .expect("quote payload");
        let signature = ml_dsa_65()
            .sign(&secret_key, &payload)
            .expect("quote signature")
            .to_bytes();
        quote.signature = hex::encode(&signature);
        quote.quote_hash = hex::encode(payment_quote_hash(&payload, &public_key, &signature));
        (quote, hex::encode(content), peer_id)
    }

    #[test]
    fn payment_hash_matches_evmlib_vector() {
        assert_eq!(
            hex::encode(payment_quote_hash(&[0, 1], &[2], &[3])),
            "d98f2e8134922f73748703c8e7084d42f13d2fa1439936ef5a3abcf5646fe83f"
        );
    }

    #[test]
    fn verifies_baseline_quote_and_rejects_tampering() {
        let (quote, content, peer_id) = baseline_quote();
        let verified =
            verify_storage_quote(quote.clone(), &content, &peer_id).expect("valid quote");
        assert_eq!(verified.amount, (PRICE_BASELINE_WEI * 3).to_string());
        let mut tampered = quote;
        tampered.price = (PRICE_BASELINE_WEI + 1).to_string();
        assert!(verify_storage_quote(tampered, &content, &peer_id).is_err());
    }

    #[test]
    fn verifies_bound_commitment_and_exact_native_sidecar() {
        let (quote, content, peer_id) = bound_quote();
        let verified =
            verify_storage_quote(quote.clone(), &content, &peer_id).expect("valid bound quote");
        assert_eq!(
            storage_payment_total(&[verified]).expect("payment total"),
            (calculate_price(23) * PAYMENT_MULTIPLIER).to_string()
        );

        let mut tampered = quote;
        tampered.commitment.as_mut().expect("commitment").root = hex::encode([0x99; 32]);
        let error = verify_storage_quote(tampered, &content, &peer_id)
            .expect_err("sidecar mismatch must fail");
        assert!(error.to_string().contains("sidecar differs"));
    }
}
