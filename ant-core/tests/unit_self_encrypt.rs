//! Local self-encryption unit tests (no network required).
//!
//! These validate encryption correctness, `DataMap` integrity, and edge cases
//! using only in-memory chunk stores. Ported from ant-node's
//! `src/client/self_encrypt.rs` tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ant_core::data::compute_address;
use bytes::Bytes;
use self_encryption::{decrypt, encrypt, DataMap, EncryptedChunk};
use std::collections::HashMap;
use std::path::Path;
use xor_name::XorName;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encrypt in-memory data, returning the `DataMap` and a chunk store.
fn encrypt_local(data: &[u8]) -> (DataMap, HashMap<XorName, Bytes>) {
    let (data_map, chunks) = encrypt(Bytes::from(data.to_vec())).unwrap();
    let store: HashMap<XorName, Bytes> = data_map
        .infos()
        .iter()
        .zip(chunks)
        .map(|(info, chunk)| (info.dst_hash, chunk.content))
        .collect();
    (data_map, store)
}

/// Decrypt using a local chunk store. Panics on missing chunks.
fn decrypt_local(data_map: &DataMap, store: &HashMap<XorName, Bytes>) -> Bytes {
    let chunks: Vec<EncryptedChunk> = data_map
        .infos()
        .iter()
        .map(|info| EncryptedChunk {
            content: store
                .get(&info.dst_hash)
                .unwrap_or_else(|| panic!("Missing chunk {}", info.dst_hash))
                .clone(),
        })
        .collect();
    decrypt(data_map, &chunks).unwrap()
}

/// Try to decrypt, returning a Result for error-path tests.
fn try_decrypt_local(
    data_map: &DataMap,
    store: &HashMap<XorName, Bytes>,
) -> std::result::Result<Bytes, String> {
    let chunks: std::result::Result<Vec<EncryptedChunk>, String> = data_map
        .infos()
        .iter()
        .map(|info| {
            let content = store
                .get(&info.dst_hash)
                .ok_or_else(|| format!("Missing chunk {}", info.dst_hash))?;
            Ok(EncryptedChunk {
                content: content.clone(),
            })
        })
        .collect();
    let chunks = chunks?;
    decrypt(data_map, &chunks).map_err(|e| format!("Decryption failed: {e}"))
}

// ---------------------------------------------------------------------------
// Roundtrip tests
// ---------------------------------------------------------------------------

#[test]
fn test_encrypt_decrypt_roundtrip_small() {
    let original = vec![0xABu8; 4096];
    let (data_map, store) = encrypt_local(&original);

    assert!(
        !data_map.infos().is_empty(),
        "DataMap should have chunk identifiers"
    );

    let decrypted = decrypt_local(&data_map, &store);
    assert_eq!(decrypted.as_ref(), original.as_slice());
}

#[test]
fn test_encrypt_decrypt_roundtrip_medium() {
    let original: Vec<u8> = (0u8..=255).cycle().take(1_048_576).collect();
    let (data_map, store) = encrypt_local(&original);

    let decrypted = decrypt_local(&data_map, &store);
    assert_eq!(decrypted.as_ref(), original.as_slice());
}

// ---------------------------------------------------------------------------
// Encryption property tests
// ---------------------------------------------------------------------------

#[test]
fn test_encrypt_produces_encrypted_output() {
    let original = b"This is a known plaintext pattern for testing encryption";
    let mut data = Vec::new();
    for _ in 0..100 {
        data.extend_from_slice(original);
    }

    let (_data_map, store) = encrypt_local(&data);

    let plaintext_str = "This is a known plaintext pattern";
    for content in store.values() {
        let chunk_str = String::from_utf8_lossy(content);
        assert!(
            !chunk_str.contains(plaintext_str),
            "Encrypted chunk should not contain plaintext"
        );
    }
}

#[test]
fn test_cannot_recover_data_from_chunks_alone() {
    let original = vec![0x33u8; 8192];
    let (_data_map, store) = encrypt_local(&original);

    let mut concatenated = Vec::new();
    for content in store.values() {
        concatenated.extend_from_slice(content);
    }

    assert_ne!(
        concatenated, original,
        "Concatenated chunks should not match original data"
    );
}

#[test]
fn test_chunks_do_not_contain_plaintext_patterns() {
    let pattern = b"SENTINEL_PATTERN_12345";
    let mut data = Vec::with_capacity(pattern.len() * 500);
    for _ in 0..500 {
        data.extend_from_slice(pattern);
    }

    let (_data_map, store) = encrypt_local(&data);

    for content in store.values() {
        let found = content
            .windows(pattern.len())
            .any(|window| window == pattern);
        assert!(
            !found,
            "Encrypted chunks must not contain plaintext patterns"
        );
    }
}

#[test]
fn test_deterministic_encryption() {
    let data = vec![0xDDu8; 8192];
    let (data_map_a, store_a) = encrypt_local(&data);
    let (data_map_b, store_b) = encrypt_local(&data);

    assert_eq!(
        data_map_a.infos().len(),
        data_map_b.infos().len(),
        "Same content should produce same number of chunks"
    );

    for (a, b) in data_map_a.infos().iter().zip(data_map_b.infos().iter()) {
        assert_eq!(a.dst_hash, b.dst_hash, "Chunk addresses should match");
    }

    for key in store_a.keys() {
        assert!(
            store_b.contains_key(key),
            "Both stores should contain the same chunk addresses"
        );
    }
}

// ---------------------------------------------------------------------------
// DataMap tests
// ---------------------------------------------------------------------------

#[test]
fn test_data_map_serialization_roundtrip() {
    let original = vec![0xCDu8; 8192];
    let (data_map, _store) = encrypt_local(&original);

    let serialized = rmp_serde::to_vec(&data_map).unwrap();
    let deserialized: DataMap = rmp_serde::from_slice(&serialized).unwrap();

    assert_eq!(data_map.infos().len(), deserialized.infos().len());
}

#[test]
fn test_data_map_contains_correct_chunk_count() {
    let original = vec![0xEFu8; 1_048_576];
    let (data_map, store) = encrypt_local(&original);

    assert!(
        data_map.infos().len() >= 3,
        "Should have at least 3 chunk identifiers, got {}",
        data_map.infos().len()
    );

    for info in data_map.infos() {
        assert!(
            store.contains_key(&info.dst_hash),
            "Chunk store should contain chunk referenced by DataMap"
        );
    }
}

#[test]
fn test_encrypted_chunks_have_valid_addresses() {
    let original = vec![0x42u8; 8192];
    let (data_map, store) = encrypt_local(&original);

    for info in data_map.infos() {
        let content = store.get(&info.dst_hash).expect("Chunk should exist");
        let computed = compute_address(content);
        assert_eq!(
            computed, info.dst_hash.0,
            "BLAKE3(encrypted_content) should equal dst_hash"
        );
    }
}

// ---------------------------------------------------------------------------
// Decryption failure tests
// ---------------------------------------------------------------------------

#[test]
fn test_decryption_fails_without_correct_data_map() {
    let original = vec![0x11u8; 8192];
    let (_data_map, store) = encrypt_local(&original);

    let other = vec![0x22u8; 8192];
    let (wrong_data_map, _) = encrypt_local(&other);

    let result = try_decrypt_local(&wrong_data_map, &store);
    assert!(result.is_err(), "Decryption with wrong DataMap should fail");
}

#[test]
fn test_missing_chunk_fails_decryption() {
    let original = vec![0x44u8; 8192];
    let (data_map, mut store) = encrypt_local(&original);

    if let Some(info) = data_map.infos().first() {
        store.remove(&info.dst_hash);
    }

    let result = try_decrypt_local(&data_map, &store);
    assert!(
        result.is_err(),
        "Decryption should fail with a missing chunk"
    );
}

#[test]
fn test_tampered_chunk_detected() {
    let original = vec![0x55u8; 8192];
    let (data_map, mut store) = encrypt_local(&original);

    if let Some(info) = data_map.infos().first() {
        if let Some(content) = store.get_mut(&info.dst_hash) {
            let mut tampered = content.to_vec();
            if let Some(byte) = tampered.first_mut() {
                *byte ^= 0xFF;
            }
            *content = Bytes::from(tampered);
        }
    }

    // Tampered chunk: either decryption errors out or produces wrong data
    match try_decrypt_local(&data_map, &store) {
        Err(_) => {} // expected
        Ok(decrypted) => {
            assert_ne!(
                decrypted.as_ref(),
                original.as_slice(),
                "Tampered chunk should produce different data"
            );
        }
    }
}

#[test]
fn test_wrong_data_map_fails_decryption() {
    let original_a = vec![0x66u8; 8192];
    let (data_map_a, _store_a) = encrypt_local(&original_a);

    let original_b = vec![0x77u8; 8192];
    let (_data_map_b, store_b) = encrypt_local(&original_b);

    let result = try_decrypt_local(&data_map_a, &store_b);
    assert!(
        result.is_err(),
        "Decryption with mismatched DataMap should fail"
    );
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_file_below_min_encryptable_bytes_fails() {
    let tiny = vec![0xAA, 0xBB];
    let result = encrypt(Bytes::from(tiny));
    assert!(
        result.is_err(),
        "Encryption of 2-byte data should fail (below MIN_ENCRYPTABLE_BYTES)"
    );
}

#[test]
fn test_encrypt_at_minimum_size() {
    let data = vec![0xAAu8; 3];
    let (data_map, store) = encrypt_local(&data);

    assert!(
        !data_map.infos().is_empty(),
        "DataMap should have chunk identifiers for 3-byte data"
    );

    let decrypted = decrypt_local(&data_map, &store);
    assert_eq!(decrypted.as_ref(), data.as_slice());
}

#[test]
fn test_empty_data_fails() {
    let result = encrypt(Bytes::new());
    assert!(
        result.is_err(),
        "Encryption of empty data should fail (below MIN_ENCRYPTABLE_BYTES)"
    );
}

// ---------------------------------------------------------------------------
// Real file tests (ignored by default -- need test fixture files)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Requires ugly_files/kad.pdf to be present"]
fn test_encrypt_decrypt_pdf() {
    let pdf_path = Path::new("ugly_files/kad.pdf");
    if !pdf_path.exists() {
        return;
    }
    let data = std::fs::read(pdf_path).unwrap();
    let (data_map, store) = encrypt_local(&data);
    let decrypted = decrypt_local(&data_map, &store);
    assert_eq!(decrypted.as_ref(), data.as_slice());
}

#[test]
#[ignore = "Requires ugly_files/pylon.mp4 to be present"]
fn test_encrypt_decrypt_video() {
    let video_path = Path::new("ugly_files/pylon.mp4");
    if !video_path.exists() {
        return;
    }
    let data = std::fs::read(video_path).unwrap();
    let (data_map, store) = encrypt_local(&data);
    let decrypted = decrypt_local(&data_map, &store);
    assert_eq!(decrypted.as_ref(), data.as_slice());
}
