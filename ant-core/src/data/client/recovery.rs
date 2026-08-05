// recovery.rs — on-chain DataMap recovery via Arbitrum calldata.
// Query historical Recovery transactions and extract DataMap bytes.

use serde::{Deserialize, Serialize};

/// A recovery entry: one backed-up folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryEntry {
    /// Transaction hash on Arbitrum.
    pub tx_hash: String,
    /// Block number when the backup was created.
    pub block_number: u64,
    /// Timestamp of the backup.
    pub timestamp: String,
    /// Hash of the folder name (privacy-preserving).
    pub folder_hash: String,
    /// Size of the DataMap in bytes.
    pub datamap_size: usize,
}

/// List all recovery backups for a wallet address.
/// Queries the payment vault contract for RecoveryUpload events.
pub async fn list_recoveries(
    wallet_address: &str,
    _rpc_url: &str,
) -> Result<Vec<RecoveryEntry>, String> {
    // Stub: in production, query Arbitrum for RecoveryUpload events
    // indexed by wallet_address. For now, return empty — the contract
    // event emission is Phase 2 of the implementation.
    let _ = wallet_address;
    Ok(vec![])
}

/// Recover a DataMap from an on-chain transaction.
/// Given a tx hash, extracts the encrypted DataMap from calldata,
/// decrypts it with the user key, and returns the raw DataMap bytes.
pub async fn recover_datamap(
    tx_hash: &str,
    _user_key: &[u8],
    _rpc_url: &str,
) -> Result<Vec<u8>, String> {
    // Stub: in production, fetch tx receipt from Arbitrum,
    // extract calldata, parse Recovery payload, decrypt DataMap.
    // Returns the raw DataMap bytes for reconstruction.
    let _ = tx_hash;
    Err("recovery contract not yet deployed — Phase 2".into())
}

/// Parse a recovery calldata payload into (upload_id, encrypted_datamap).
pub fn parse_recovery_calldata(calldata: &[u8]) -> Result<(String, Vec<u8>), String> {
    if calldata.len() < 68 {
        return Err("calldata too short for recovery payload".into());
    }
    // First 4 bytes: function selector
    // Next 32 bytes: upload_id (padded)
    // Remaining: encrypted DataMap
    let upload_id_bytes = &calldata[4..36];
    let upload_id = hex::encode(upload_id_bytes);
    let datamap = calldata[36..].to_vec();
    Ok((upload_id, datamap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_recovery_calldata() {
        let mut calldata = vec![0u8; 100];
        calldata[0..4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]); // selector
        calldata[4..36].fill(0xAB); // upload_id
        calldata[36..].fill(0xCD); // datamap

        let (id, map) = parse_recovery_calldata(&calldata).unwrap();
        assert_eq!(id.len(), 64); // hex-encoded 32 bytes
        assert_eq!(map.len(), 64); // remaining bytes
    }
}
