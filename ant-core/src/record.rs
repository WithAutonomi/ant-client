//! Content-address verification shared by every transport and cache.
pub(crate) fn verify(address: &[u8; 32], content: &[u8]) -> Result<(), String> {
    let actual = blake3::hash(content);
    if actual.as_bytes() != address {
        return Err(format!(
            "BLAKE3 mismatch: expected {}, received {}",
            hex::encode(address),
            actual.to_hex()
        ));
    }
    Ok(())
}
