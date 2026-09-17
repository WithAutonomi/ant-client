//! Native resolve-before-pay policy, independent of quote wire format.
//!
//! Adapters retain their protocol's amount and cryptographic types. The order
//! and meaning of the checks here come from the native client's quote gate.

use std::fmt::Display;

pub(crate) const ML_DSA_PUB_KEY_LEN: usize =
    ant_protocol::pqc::api::MlDsaVariant::MlDsa65.public_key_size();

pub(crate) trait QuotePrice: Copy + Eq + Display {
    fn for_key_count(count: u32) -> Self;
}

impl QuotePrice for ant_protocol::evm::Amount {
    fn for_key_count(count: u32) -> Self {
        ant_protocol::payment::calculate_price(count as usize)
    }
}

/// Protocol primitives supplied by each wire adapter; admission is shared.
pub(crate) trait Commitment: serde::de::DeserializeOwned {
    const MAX_KEY_COUNT: u32;
    const MAX_SIDECAR_BYTES: usize;
    fn peer(&self) -> &[u8; 32];
    fn public_key(&self) -> &[u8];
    fn key_count(&self) -> u32;
    fn verify_signature(&self) -> bool;
    fn hash(&self) -> Option<[u8; 32]>;
}

macro_rules! commitment_adapter {
    ($protocol:path) => {
        use $protocol as protocol;
        impl Commitment for protocol::StorageCommitment {
            const MAX_KEY_COUNT: u32 = protocol::MAX_COMMITMENT_KEY_COUNT;
            const MAX_SIDECAR_BYTES: usize = protocol::MAX_COMMITMENT_SIDECAR_BYTES;
            fn peer(&self) -> &[u8; 32] {
                &self.sender_peer_id
            }
            fn public_key(&self) -> &[u8] {
                &self.sender_public_key
            }
            fn key_count(&self) -> u32 {
                self.key_count
            }
            fn verify_signature(&self) -> bool {
                protocol::verify_commitment_signature(self)
            }
            fn hash(&self) -> Option<[u8; 32]> {
                protocol::commitment_hash(self)
            }
        }
    };
}

mod native {
    use super::Commitment;
    commitment_adapter!(ant_protocol::payment::commitment);
}

pub(crate) struct QuoteFields<'a, P> {
    pub(crate) public_key: &'a [u8],
    pub(crate) content: &'a [u8; 32],
    pub(crate) price: P,
    pub(crate) committed_key_count: u32,
    pub(crate) commitment_pin: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QuoteValidationError {
    #[error("{0}")]
    Binding(String),
    #[error("{0}")]
    Commitment(String),
}

pub(crate) fn peer_binding_is_valid(peer: &[u8; 32], public_key: &[u8]) -> bool {
    public_key.len() == ML_DSA_PUB_KEY_LEN && blake3::hash(public_key).as_bytes() == peer
}

pub(crate) fn validate_quote<P: QuotePrice, C: Commitment>(
    peer: &[u8; 32],
    expected_content: &[u8; 32],
    fields: &QuoteFields<'_, P>,
    verify_signature: impl FnOnce() -> bool,
    sidecar: Option<&[u8]>,
) -> Result<(), QuoteValidationError> {
    if !peer_binding_is_valid(peer, fields.public_key) {
        return Err(QuoteValidationError::Binding(format!(
            "BLAKE3(pub_key)={} pub_key_len={}",
            blake3::hash(fields.public_key).to_hex(),
            fields.public_key.len(),
        )));
    }
    if fields.content != expected_content {
        return Err(QuoteValidationError::Binding(
            "quote content does not match the requested address".into(),
        ));
    }
    if !verify_signature() {
        return Err(QuoteValidationError::Binding(
            "quote ML-DSA-65 signature is invalid".into(),
        ));
    }
    validate_commitment_binding::<P, C>(peer, fields, sidecar)
        .map_err(QuoteValidationError::Commitment)
}

pub(crate) fn validate_commitment_binding<P: QuotePrice, C: Commitment>(
    peer: &[u8; 32],
    fields: &QuoteFields<'_, P>,
    sidecar: Option<&[u8]>,
) -> Result<(), String> {
    let count = fields.committed_key_count;
    let pin = fields.commitment_pin;
    match (count, pin.is_some()) {
        (0, false) | (1.., true) => {}
        (1.., false) => {
            return Err(format!(
                "committed_key_count={count} > 0 but commitment_pin is None (unauditable count)"
            ))
        }
        (0, true) => {
            return Err("committed_key_count=0 with a commitment_pin (incoherent baseline)".into())
        }
    }
    if count > C::MAX_KEY_COUNT {
        return Err(format!(
            "committed_key_count={count} exceeds MAX_COMMITMENT_KEY_COUNT={}",
            C::MAX_KEY_COUNT
        ));
    }
    let expected = P::for_key_count(count);
    if fields.price != expected {
        return Err(format!(
            "price {} does not equal calculate_price(committed_key_count={count}) = {expected}",
            fields.price
        ));
    }
    // Native baseline semantics: there is no pin to resolve. Any unsolicited
    // sidecar is irrelevant to admission, including an unparseable one.
    let Some(pin) = pin else {
        return Ok(());
    };
    let Some(blob) = sidecar else {
        return Err("bound quote did not ship its commitment; the pin is unresolvable so the quote is dropped before payment".into());
    };
    if blob.len() > C::MAX_SIDECAR_BYTES {
        return Err(format!(
            "shipped commitment is {} bytes, exceeds MAX_COMMITMENT_SIDECAR_BYTES={}",
            blob.len(),
            C::MAX_SIDECAR_BYTES
        ));
    }
    let commitment: C = rmp_serde::from_slice(blob).map_err(|e| {
        format!("shipped commitment did not deserialize as a StorageCommitment: {e}")
    })?;
    if blake3::hash(commitment.public_key()).as_bytes() != peer || commitment.peer() != peer {
        return Err("shipped commitment is not bound to the quoting peer".into());
    }
    if !commitment.verify_signature() {
        return Err("shipped commitment has an invalid signature".into());
    }
    if commitment.hash() != Some(pin) {
        return Err("shipped commitment does not hash to the quote's pin".into());
    }
    if commitment.key_count() != count {
        return Err(format!(
            "shipped commitment attests key_count={} but the quote claims {count}",
            commitment.key_count()
        ));
    }
    Ok(())
}
