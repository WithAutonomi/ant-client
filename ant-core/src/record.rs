//! Content-address verification shared by every transport and cache.
pub(crate) fn verify(address: &[u8; 32], content: &[u8]) -> Result<(), String> {
    let actual = ant_protocol::compute_address(content);
    if &actual != address {
        return Err(format!(
            "BLAKE3 mismatch: expected {}, received {}",
            hex::encode(address),
            hex::encode(actual)
        ));
    }
    Ok(())
}
