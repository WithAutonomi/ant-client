//! Regenerate the compact native fixture used by the WASM large-file tests:
//! cargo run -p ant-core --release --example generate-browser-large-file
//!
//! Encrypts >4 GiB with bounded memory. Repeated plaintext compresses well;
//! all encrypted records fit in a small fixture without allocating the file.
use bytes::Bytes;
use std::collections::HashMap;

/// Plaintext byte repeated through the file; the tests check it on every read.
const FILL_BYTE: u8 = 0x5a;
/// Whole chunks past the 4 GiB position, so reads and seeks cross it.
const CHUNKS_PAST_4_GIB: u64 = 2;
/// A short final chunk, so the file does not end on a chunk boundary.
const TAIL_BYTES: u64 = 123;

fn main() {
    let size =
        (1u64 << 32) + CHUNKS_PAST_4_GIB * self_encryption::MAX_CHUNK_SIZE as u64 + TAIL_BYTES;
    let block = Bytes::from(vec![FILL_BYTE; self_encryption::MAX_CHUNK_SIZE]);
    let mut remaining = size as usize;
    let mut hasher = blake3::Hasher::new();
    let input = std::iter::from_fn(|| {
        if remaining == 0 {
            return None;
        }
        let length = remaining.min(block.len());
        remaining -= length;
        hasher.update(&block[..length]);
        Some(block.slice(..length))
    });
    let mut encryptor = self_encryption::stream_encrypt(size as usize, input).unwrap();
    let records: HashMap<_, _> = encryptor.chunks().map(|item| item.unwrap()).collect();
    let map = encryptor.into_datamap().unwrap();
    let root =
        self_encryption::get_root_data_map(map.clone(), &mut |hash| Ok(records[&hash].clone()))
            .unwrap();
    assert_eq!(root.original_file_size() as u64, size);
    let mut retained = records;
    let map_bytes = rmp_serde::to_vec(&map).unwrap();
    let address = *blake3::hash(&map_bytes).as_bytes();
    retained.insert(self_encryption::XorName(address), Bytes::from(map_bytes));
    let mut records: Vec<_> = retained.into_iter().map(|(hash, content)| {
        serde_json::json!({"address": hex::encode(hash.0), "content": hex::encode(content)})
    }).collect();
    records.sort_by(|a, b| a["address"].as_str().cmp(&b["address"].as_str()));
    let fixture = serde_json::json!({
        "size": size, "byte": FILL_BYTE, "hash": hasher.finalize().to_hex().to_string(),
        "address": hex::encode(address), "records": records,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("wasm-tests/fixtures/large-file.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&fixture).unwrap()).unwrap();
    eprintln!("Wrote {}-byte file fixture to {}", size, path.display());
}
