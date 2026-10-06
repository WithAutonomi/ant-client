//! Per-chunk self-encryption decryption with bounded decompression.
//!
//! `self_encryption::decrypt_chunk` decompresses without an output limit, so a
//! crafted record could expand far past the size its DataMap declares before
//! that size can be checked. This performs the same steps — the BLAKE3 KDF over
//! the chunk and its two predecessors, the XOR pad, ChaCha20-Poly1305 and
//! Brotli — and stops decompressing one byte past the declared size. Tests pin
//! the result to `decrypt_chunk`, so a KDF change in the dependency fails them.

use bytes::Bytes;
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use self_encryption::XorName;
use std::io::Read;

/// self_encryption splits every encryptable file into at least three chunks:
/// each chunk's key depends on the two before it, wrapping around.
pub(crate) const MIN_FILE_CHUNKS: usize = 3;
/// Domain separation context of self_encryption's chunk KDF.
const KDF_CONTEXT: &str = "self_encryption/chunk/v2";
const HASH_SIZE: usize = 32;
const KEY_SIZE: usize = 32;
const NONCE_SIZE: usize = 12;
/// The KDF output left after the key and nonce, used as the XOR pad.
const PAD_SIZE: usize = 3 * HASH_SIZE - KEY_SIZE - NONCE_SIZE;
/// Three source hashes, the chunk index and the KDF level.
const KDF_INPUT_SIZE: usize = 3 * HASH_SIZE + 2 * std::mem::size_of::<u64>();
/// Brotli decoder input buffer.
const DECOMPRESS_BUFFER_BYTES: usize = 4096;

/// The per-chunk secrets self_encryption derives from the source hashes.
struct ChunkSecrets {
    pad: [u8; PAD_SIZE],
    key: [u8; KEY_SIZE],
    nonce: [u8; NONCE_SIZE],
}

/// Decrypt chunk `index` of a map with `src_hashes`, rejecting any record that
/// decompresses to anything other than `expected_len` bytes. Memory grows with
/// the output actually produced, which stops at `expected_len + 1` bytes; a
/// large declared size alone allocates nothing.
pub(super) fn decrypt_chunk(
    index: usize,
    content: &[u8],
    src_hashes: &[XorName],
    kdf_level: usize,
    expected_len: usize,
) -> Result<Bytes, String> {
    let secrets = chunk_secrets(index, src_hashes, kdf_level)?;
    // Unpad, then decrypt in the same buffer; the tag is truncated off.
    let mut compressed: Vec<u8> = content
        .iter()
        .zip(secrets.pad.iter().cycle())
        .map(|(byte, pad)| byte ^ pad)
        .collect();
    ChaCha20Poly1305::new(Key::from_slice(&secrets.key))
        .decrypt_in_place(Nonce::from_slice(&secrets.nonce), &[], &mut compressed)
        .map_err(|error| format!("chunk decryption failed: {error}"))?;
    let mut plaintext = Vec::with_capacity(expected_len.min(self_encryption::MAX_CHUNK_SIZE));
    brotli_decompressor::Decompressor::new(compressed.as_slice(), DECOMPRESS_BUFFER_BYTES)
        .take((expected_len as u64).saturating_add(1))
        .read_to_end(&mut plaintext)
        .map_err(|_| "chunk decompression failed".to_string())?;
    if plaintext.len() != expected_len {
        return Err("decrypted chunk size differs from DataMap".into());
    }
    Ok(Bytes::from(plaintext))
}

/// self_encryption's KDF: each chunk's key depends on its own source hash and
/// those of the two chunks before it, wrapping around at the start.
fn chunk_secrets(
    index: usize,
    src_hashes: &[XorName],
    kdf_level: usize,
) -> Result<ChunkSecrets, String> {
    let total = src_hashes.len();
    if total < MIN_FILE_CHUNKS || index >= total {
        return Err(format!("chunk {index} is outside a map of {total} chunks"));
    }
    let (previous, before_previous) = match index {
        0 => (total - 1, total - 2),
        1 => (0, total - 1),
        _ => (index - 1, index - 2),
    };
    let mut input = Vec::with_capacity(KDF_INPUT_SIZE);
    input.extend_from_slice(&src_hashes[index].0);
    input.extend_from_slice(&src_hashes[previous].0);
    input.extend_from_slice(&src_hashes[before_previous].0);
    input.extend_from_slice(&(index as u64).to_le_bytes());
    input.extend_from_slice(&(kdf_level as u64).to_le_bytes());
    let mut output = [0u8; PAD_SIZE + KEY_SIZE + NONCE_SIZE];
    let mut hasher = blake3::Hasher::new_derive_key(KDF_CONTEXT);
    hasher.update(&input);
    hasher.finalize_xof().fill(&mut output);
    let mut secrets = ChunkSecrets {
        pad: [0; PAD_SIZE],
        key: [0; KEY_SIZE],
        nonce: [0; NONCE_SIZE],
    };
    secrets.pad.copy_from_slice(&output[..PAD_SIZE]);
    secrets
        .key
        .copy_from_slice(&output[PAD_SIZE..PAD_SIZE + KEY_SIZE]);
    secrets
        .nonce
        .copy_from_slice(&output[PAD_SIZE + KEY_SIZE..]);
    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_self_encryption_for_every_chunk() {
        let content = Bytes::from((0..18_000).map(|i| (i * 37) as u8).collect::<Vec<_>>());
        let (map, chunks) = self_encryption::encrypt(content).unwrap();
        let src_hashes: Vec<_> = map.infos().iter().map(|info| info.src_hash).collect();
        for (index, info) in map.infos().iter().enumerate() {
            let chunk = chunks
                .iter()
                .find(|chunk| *blake3::hash(&chunk.content).as_bytes() == info.dst_hash.0)
                .unwrap();
            let expected =
                self_encryption::decrypt_chunk(index, &chunk.content, &src_hashes, 0).unwrap();
            let actual =
                decrypt_chunk(index, &chunk.content, &src_hashes, 0, info.src_size).unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn records_expanding_past_their_declared_size_are_rejected() {
        // Highly compressible chunks expand far beyond a falsely small size.
        let content = Bytes::from(vec![0u8; 3 * 1024 * 1024]);
        let (map, chunks) = self_encryption::encrypt(content).unwrap();
        let src_hashes: Vec<_> = map.infos().iter().map(|info| info.src_hash).collect();
        let info = &map.infos()[0];
        let chunk = chunks
            .iter()
            .find(|chunk| *blake3::hash(&chunk.content).as_bytes() == info.dst_hash.0)
            .unwrap();
        assert!(chunk.content.len() < info.src_size);
        for declared in [0, 1, info.src_size - 1, info.src_size + 1] {
            let error = decrypt_chunk(0, &chunk.content, &src_hashes, 0, declared).unwrap_err();
            assert!(error.contains("differs"), "{declared}: {error}");
        }
        assert!(decrypt_chunk(0, &chunk.content, &src_hashes, 1, info.src_size).is_err());
        assert!(decrypt_chunk(3, &chunk.content, &src_hashes, 0, info.src_size).is_err());
    }
}
