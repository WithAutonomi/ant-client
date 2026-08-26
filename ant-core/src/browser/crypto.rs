#[cfg(feature = "native")]
use ant_protocol::pqc::api::{ml_dsa_65, MlDsaPublicKey, MlDsaSignature, MlDsaVariant};
#[cfg(all(not(feature = "native"), feature = "browser-wasm"))]
use fips204::{
    ml_dsa_65,
    traits::{SerDes as _, Verifier as _},
};
use tiny_keccak::{Hasher as _, Keccak};

pub(crate) fn verify_ml_dsa_65(
    public_key: &[u8],
    signature: &[u8],
    message: &[u8],
    context: &[u8],
) -> bool {
    verify_ml_dsa_65_inner(public_key, signature, message, context).unwrap_or(false)
}

#[cfg(feature = "native")]
fn verify_ml_dsa_65_inner(
    public_key: &[u8],
    signature: &[u8],
    message: &[u8],
    context: &[u8],
) -> Result<bool, String> {
    let public_key = MlDsaPublicKey::from_bytes(MlDsaVariant::MlDsa65, public_key)
        .map_err(|error| error.to_string())?;
    let signature = MlDsaSignature::from_bytes(MlDsaVariant::MlDsa65, signature)
        .map_err(|error| error.to_string())?;
    ml_dsa_65()
        .verify_with_context(&public_key, message, &signature, context)
        .map_err(|error| error.to_string())
}

#[cfg(all(not(feature = "native"), feature = "browser-wasm"))]
fn verify_ml_dsa_65_inner(
    public_key: &[u8],
    signature: &[u8],
    message: &[u8],
    context: &[u8],
) -> Result<bool, String> {
    let public_key: [u8; ml_dsa_65::PK_LEN] = public_key
        .try_into()
        .map_err(|_| "invalid ML-DSA-65 public key length".to_string())?;
    let signature: [u8; ml_dsa_65::SIG_LEN] = signature
        .try_into()
        .map_err(|_| "invalid ML-DSA-65 signature length".to_string())?;
    let public_key =
        ml_dsa_65::PublicKey::try_from_bytes(public_key).map_err(ToString::to_string)?;
    Ok(public_key.verify(message, &signature, context))
}

#[cfg(not(any(feature = "native", feature = "browser-wasm")))]
fn verify_ml_dsa_65_inner(
    _public_key: &[u8],
    _signature: &[u8],
    _message: &[u8],
    _context: &[u8],
) -> Result<bool, String> {
    Err("ML-DSA verification requires the native or browser-wasm feature".to_string())
}

pub(crate) fn keccak256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Keccak::v256();
    for part in parts {
        hasher.update(part);
    }
    let mut output = [0u8; 32];
    hasher.finalize(&mut output);
    output
}
