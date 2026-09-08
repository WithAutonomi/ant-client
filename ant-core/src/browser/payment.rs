//! Verification and payment planning shared by native and browser clients.

use super::protocol::normalize_hex;
pub use super::protocol::{BrowserCommitmentArtifact, BrowserQuoteArtifact};
use ant_protocol::evm::{Amount, PaymentQuote, RewardsAddress};
#[cfg(test)]
use ant_protocol::payment::commitment::commitment_hash;
use ant_protocol::payment::commitment::{StorageCommitment, MAX_COMMITMENT_SIDECAR_BYTES};
#[cfg(test)]
use saorsa_transport::webrtc::calculate_price_wei;
use serde::{Deserialize, Serialize};

pub use saorsa_transport::webrtc::payment_quote_hash;

#[cfg(test)]
const PAYMENT_MULTIPLIER: u128 = crate::payment_policy::SINGLE_NODE_PAYMENT_MULTIPLIER as u128;
#[cfg(test)]
const PRICE_BASELINE_WEI: u128 = 3_906_250_000_000_000;

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

/// Select the quote paid for one record using the native client's median policy.
///
/// Quotes must already have passed [`verify_storage_quote`] for distinct,
/// eligible peers requiring payment for the same record. Input order breaks
/// ties; one through seven quotes are supported. The selected issuer receives
/// three times its price. Transport-specific storage-quorum checks remain
/// with the caller.
pub fn select_storage_quote(
    mut quotes: Vec<VerifiedStorageQuote>,
) -> Result<VerifiedStorageQuote, StorageQuoteError> {
    let prices = quotes
        .iter()
        .map(|quote| parse_decimal_amount(&quote.quote.price, "quote price"))
        .collect::<Result<Vec<_>, _>>()?;
    let plan = crate::payment_policy::SingleNodePaymentPlan::from_prices(&prices)
        .map_err(|error| StorageQuoteError(error.to_string()))?;
    let paid = plan.paid_quote();
    let mut selected = quotes.swap_remove(paid.quote_index);
    selected.amount = paid.amount.to_string();
    Ok(selected)
}

/// Sum verified decimal quote amounts without exposing integer arithmetic to
/// JavaScript or a wallet adapter.
pub fn storage_payment_total(quotes: &[VerifiedStorageQuote]) -> Result<String, StorageQuoteError> {
    quotes
        .iter()
        .try_fold(Amount::ZERO, |total, quote| {
            let amount = parse_decimal_amount(&quote.amount, "storage payment amount")?;
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
    let public_key = decode_unbounded_hex(&quote.public_key, "quote public key")?;
    let signature = decode_unbounded_hex(&quote.signature, "quote signature")?;
    let price = parse_decimal_amount(&quote.price, "quote price")?;
    let rewards = normalize_hex(&quote.rewards_address, 20).map_err(StorageQuoteError)?;
    quote.rewards_address.clone_from(&rewards);
    let commitment_pin = quote
        .commitment_pin
        .as_deref()
        .map(|pin| normalize_hex(pin, 32).map_err(StorageQuoteError))
        .transpose()?;
    quote.commitment_pin.clone_from(&commitment_pin);
    let native_quote = PaymentQuote {
        content: xor_name::XorName(decode_hex_array(&quote.content, "quote content")?),
        timestamp: std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(quote.timestamp_secs))
            .ok_or_else(|| StorageQuoteError("quote timestamp out of range".into()))?,
        price,
        rewards_address: RewardsAddress::from(decode_hex_array::<20>(
            &rewards,
            "quote rewards address",
        )?),
        pub_key: public_key.clone(),
        signature: signature.clone(),
        committed_key_count: quote.committed_key_count,
        commitment_pin: commitment_pin
            .as_deref()
            .map(|pin| decode_hex_array(pin, "commitment pin"))
            .transpose()?,
    };
    let sidecar = if quote.committed_key_count > 0 {
        quote
            .commitment
            .as_ref()
            .map(|artifact| decode_unbounded_hex(&artifact.encoded, "storage commitment sidecar"))
            .transpose()?
    } else {
        None
    };
    crate::quote_validation::validate_quote::<_, StorageCommitment>(
        &decode_hex_array(&expected_peer_id, "peer ID")?,
        &decode_hex_array(&expected_address, "content address")?,
        &crate::quote_validation::QuoteFields {
            public_key: &public_key,
            content: &decode_hex_array(&quote.content, "quote content")?,
            price,
            committed_key_count: quote.committed_key_count,
            commitment_pin: commitment_pin
                .as_deref()
                .map(|pin| decode_hex_array(pin, "commitment pin"))
                .transpose()?,
        },
        || ant_protocol::payment::verify_quote_signature(&native_quote),
        sidecar.as_deref(),
    )
    .map_err(|error| StorageQuoteError(error.to_string()))?;
    let quote_hash = hex::encode(native_quote.hash());
    if normalize_hex(&quote.quote_hash, 32).map_err(StorageQuoteError)? != quote_hash {
        return Err(StorageQuoteError(
            "storage quote hash does not match its signed fields".to_string(),
        ));
    }
    quote.quote_hash.clone_from(&quote_hash);

    if quote.committed_key_count > 0 {
        // The browser wire duplicates the sidecar fields; only this envelope
        // consistency check is adapter-specific. Admission was checked above.
        if let Some(artifact) = quote.commitment.as_mut() {
            normalize_commitment_artifact(artifact)?;
        }
    }

    let amount = crate::payment_policy::enhanced_payment_amount(price)
        .map_err(|error| StorageQuoteError(error.to_string()))?;
    Ok(VerifiedStorageQuote {
        quote,
        quote_hash,
        rewards_address: format!("0x{rewards}"),
        amount: amount.to_string(),
    })
}

#[cfg(test)]
fn canonical_quote_bytes(
    quote: &BrowserQuoteArtifact,
    price: u128,
    rewards: &str,
    commitment_pin: Option<&str>,
) -> Result<Vec<u8>, StorageQuoteError> {
    let content = decode_hex_array::<32>(&quote.content, "quote content")?;
    let rewards = decode_hex_array::<20>(rewards, "quote rewards address")?;
    let commitment_pin = commitment_pin
        .map(|pin| decode_hex_array::<32>(pin, "storage commitment pin"))
        .transpose()?;
    Ok(PaymentQuote::bytes_for_signing(
        xor_name::XorName(content),
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(quote.timestamp_secs),
        &Amount::from(price),
        &RewardsAddress::from(rewards),
        quote.committed_key_count,
        &commitment_pin,
    ))
}

fn normalize_commitment_artifact(
    artifact: &mut BrowserCommitmentArtifact,
) -> Result<(), StorageQuoteError> {
    let encoded = decode_unbounded_hex(&artifact.encoded, "storage commitment sidecar")?;
    if encoded.len() > MAX_COMMITMENT_SIDECAR_BYTES {
        return Err(StorageQuoteError(
            "storage commitment sidecar exceeds the protocol limit".to_string(),
        ));
    }
    let commitment: StorageCommitment = rmp_serde::from_slice(&encoded).map_err(|error| {
        StorageQuoteError(format!(
            "storage commitment sidecar is not valid MessagePack: {error}"
        ))
    })?;
    let root = normalize_hex(&artifact.root, 32).map_err(StorageQuoteError)?;
    let peer_id = normalize_hex(&artifact.sender_peer_id, 32).map_err(StorageQuoteError)?;
    let public_key =
        decode_unbounded_hex(&artifact.sender_public_key, "storage commitment public key")?;
    let signature = decode_unbounded_hex(&artifact.signature, "storage commitment signature")?;
    if commitment.root != decode_hex_array::<32>(&root, "storage commitment root")?
        || commitment.key_count != artifact.key_count
        || commitment.sender_peer_id
            != decode_hex_array::<32>(&peer_id, "storage commitment peer ID")?
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
    Ok(())
}

fn parse_decimal_amount(value: &str, label: &str) -> Result<Amount, StorageQuoteError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(StorageQuoteError(format!("invalid {label}")));
    }
    value
        .parse::<Amount>()
        .map_err(|_| StorageQuoteError(format!("{label} exceeds the supported protocol range")))
}

fn decode_unbounded_hex(value: &str, label: &str) -> Result<Vec<u8>, StorageQuoteError> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if (value.len() & 1) != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StorageQuoteError(format!("invalid {label}")));
    }
    hex::decode(value).map_err(|error| StorageQuoteError(format!("invalid {label}: {error}")))
}

fn decode_hex_array<const LENGTH: usize>(
    value: &str,
    label: &str,
) -> Result<[u8; LENGTH], StorageQuoteError> {
    let decoded = hex::decode(value).map_err(|error| StorageQuoteError(error.to_string()))?;
    decoded.try_into().map_err(|bytes: Vec<u8>| {
        StorageQuoteError(format!(
            "expected {LENGTH} bytes for {label}, received {}",
            bytes.len()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ant_protocol::pqc::api::ml_dsa_65;
    use saorsa_transport::webrtc::{storage_commitment_bytes_for_signing, DOMAIN_COMMITMENT};

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

    fn bound_quote(key_count: u32) -> (BrowserQuoteArtifact, String, String) {
        let content = [0x31; 32];
        let rewards = [0x42; 20];
        let root = [0x53; 32];
        let timestamp = 1_775_000_001;
        let (public_key, secret_key) = ml_dsa_65().generate_keypair().expect("keypair");
        let public_key = public_key.to_bytes();
        let peer_id = blake3::hash(&public_key).into();
        let mut commitment = StorageCommitment {
            root,
            key_count,
            sender_peer_id: peer_id,
            sender_public_key: public_key.clone(),
            signature: Vec::new(),
        };
        let commitment_payload = storage_commitment_bytes_for_signing(
            &commitment.root,
            commitment.key_count,
            &commitment.sender_peer_id,
            &commitment.sender_public_key,
        );
        commitment.signature = ml_dsa_65()
            .sign_with_context(&secret_key, &commitment_payload, DOMAIN_COMMITMENT)
            .expect("commitment signature")
            .to_bytes();
        let native_commitment = ant_protocol::payment::StorageCommitment {
            root: commitment.root,
            key_count: commitment.key_count,
            sender_peer_id: commitment.sender_peer_id,
            sender_public_key: commitment.sender_public_key.clone(),
            signature: commitment.signature.clone(),
        };
        assert!(ant_protocol::payment::verify_commitment_signature(
            &native_commitment
        ));
        assert_eq!(
            commitment_hash(&commitment),
            ant_protocol::payment::commitment_hash(&native_commitment)
        );
        let encoded = rmp_serde::to_vec(&commitment).expect("MessagePack commitment");
        assert_eq!(
            encoded,
            rmp_serde::to_vec(&native_commitment).expect("native MessagePack commitment")
        );
        let pin = hex::encode(commitment_hash(&commitment).expect("commitment hash"));
        let peer_id = hex::encode(peer_id);
        let price = calculate_price_wei(key_count);
        assert_eq!(
            ant_protocol::evm::Amount::from(price),
            ant_protocol::payment::calculate_price(key_count as usize)
        );
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
        let native_quote = ant_protocol::evm::PaymentQuote {
            content: xor_name::XorName(content),
            timestamp: std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(timestamp),
            price: ant_protocol::evm::Amount::from(price),
            rewards_address: ant_protocol::evm::RewardsAddress::from(rewards),
            pub_key: public_key.clone(),
            signature: Vec::new(),
            committed_key_count: key_count,
            commitment_pin: Some(
                hex::decode(&pin)
                    .expect("pin")
                    .try_into()
                    .expect("32-byte pin"),
            ),
        };
        assert_eq!(payload, native_quote.bytes_for_sig());
        let signature = ml_dsa_65()
            .sign(&secret_key, &payload)
            .expect("quote signature")
            .to_bytes();
        quote.signature = hex::encode(&signature);
        quote.quote_hash = hex::encode(payment_quote_hash(&payload, &public_key, &signature));
        let native_quote = ant_protocol::evm::PaymentQuote {
            signature: signature.clone(),
            ..native_quote
        };
        assert_eq!(quote.quote_hash, hex::encode(native_quote.hash()));
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
        let (quote, content, peer_id) = bound_quote(23);
        let verified =
            verify_storage_quote(quote.clone(), &content, &peer_id).expect("valid bound quote");
        assert_eq!(
            storage_payment_total(&[verified]).expect("payment total"),
            (calculate_price_wei(23) * PAYMENT_MULTIPLIER).to_string()
        );

        let mut tampered = quote;
        tampered.commitment.as_mut().expect("commitment").root = hex::encode([0x99; 32]);
        let error = verify_storage_quote(tampered, &content, &peer_id)
            .expect_err("sidecar mismatch must fail");
        assert!(error.to_string().contains("sidecar differs"));
    }

    fn native_quote(quote: &BrowserQuoteArtifact) -> ant_protocol::evm::PaymentQuote {
        use ant_protocol::evm::{Amount, PaymentQuote, RewardsAddress};
        PaymentQuote {
            content: xor_name::XorName(decode_hex_array(&quote.content, "content").unwrap()),
            timestamp: std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(quote.timestamp_secs),
            price: Amount::from(quote.price.parse::<Amount>().unwrap()),
            rewards_address: RewardsAddress::from(
                decode_hex_array::<20>(&quote.rewards_address, "rewards").unwrap(),
            ),
            pub_key: hex::decode(&quote.public_key).unwrap(),
            signature: hex::decode(&quote.signature).unwrap(),
            committed_key_count: quote.committed_key_count,
            commitment_pin: quote
                .commitment_pin
                .as_deref()
                .map(|pin| decode_hex_array(pin, "pin").unwrap()),
        }
    }

    #[test]
    fn native_and_browser_validation_accept_and_reject_identical_signed_artifacts() {
        use ant_protocol::transport::PeerId;
        let (baseline, content, peer) = baseline_quote();
        let (bound, _, bound_peer) = bound_quote(23);
        let mut extra_baseline_sidecar = baseline.clone();
        extra_baseline_sidecar.commitment = bound.commitment.clone();
        extra_baseline_sidecar.commitment.as_mut().unwrap().encoded = "c1".into();
        let mut missing = bound.clone();
        missing.commitment = None;
        let mut corrupt = bound.clone();
        corrupt.commitment.as_mut().unwrap().encoded = "c1".into();
        let mut wrong_key = baseline.clone();
        wrong_key.public_key = bound.public_key.clone();
        let mut bad_signature = baseline.clone();
        bad_signature.signature = "00".repeat(3309);
        let mut wrong_content = baseline.clone();
        wrong_content.content = "01".repeat(32);
        for (quote, peer, expected) in [
            (baseline, peer.clone(), true),
            (extra_baseline_sidecar, peer.clone(), true),
            (bound, bound_peer.clone(), true),
            (missing, bound_peer.clone(), false),
            (corrupt, bound_peer, false),
            (wrong_key, peer.clone(), false),
            (bad_signature, peer.clone(), false),
            (wrong_content, peer, false),
        ] {
            let native = native_quote(&quote);
            let sidecar = quote
                .commitment
                .as_ref()
                .map(|artifact| hex::decode(&artifact.encoded).unwrap());
            let native_result = crate::data::client::quote::classify_quote_response(
                &PeerId::from_bytes(decode_hex_array(&peer, "peer").unwrap()),
                &decode_hex_array(&content, "content").unwrap(),
                &rmp_serde::to_vec(&native).unwrap(),
                false,
                sidecar,
            );
            assert_eq!(native_result.is_ok(), expected);
            assert_eq!(
                verify_storage_quote(quote, &content, &peer).is_ok(),
                expected
            );
        }
    }

    #[test]
    fn browser_and_native_adapters_select_the_same_signed_quote_and_payment() {
        use crate::data::client::batch::SingleNodeQuotePayment;
        for (counts, expected_index) in [
            (vec![1_000_000, 0, 0, 0, 0, 0, 0], 4),
            (vec![0, 6000, 6000, 6000, 6000, 6000, 6000], 3),
            (vec![6000, 1000, 4000, 2000], 2),
            (vec![23, 23, 23, 23], 2),
            (vec![23], 0),
        ] {
            let verified = counts
                .iter()
                .map(|&count| {
                    let (quote, address, peer) = if count == 0 {
                        baseline_quote()
                    } else {
                        bound_quote(count)
                    };
                    verify_storage_quote(quote, &address, &peer).expect("valid signed quote")
                })
                .collect::<Vec<_>>();
            let expected_hash = verified[expected_index].quote_hash.clone();
            let native_quotes = verified
                .iter()
                .map(|verified| native_quote(&verified.quote))
                .collect();
            let native = SingleNodeQuotePayment::from_quotes(native_quotes).expect("native plan");
            let browser = select_storage_quote(verified).expect("browser plan");
            let paid = native
                .quotes
                .iter()
                .filter(|quote| !quote.amount.is_zero())
                .collect::<Vec<_>>();
            assert_eq!(paid.len(), 1);
            assert_eq!(hex::encode(paid[0].quote_hash), expected_hash);
            assert_eq!(browser.quote_hash, expected_hash);
            assert_eq!(browser.amount, native.total_amount().to_string());
            assert_eq!(
                browser.amount,
                (calculate_price_wei(counts[expected_index]) * 3).to_string()
            );
            assert_eq!(native.quotes.len(), counts.len());
        }
    }
}
