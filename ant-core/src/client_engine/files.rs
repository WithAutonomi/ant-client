//! Async data workflows over a verified record fetcher. No runtime, sockets,
//! filesystem or JS dependencies. Cryptography and recursive map semantics stay
//! in self_encryption, the same implementation used by the native client.
use bytes::Bytes;
use futures_util::StreamExt;
use self_encryption::{DataMap, EncryptedChunk, XorName};
use std::{cell::RefCell, collections::HashMap, future::Future};

pub(crate) type RecordRequest = (usize, [u8; 32]);

#[derive(Debug)]
pub(crate) enum ReadError<E> {
    Fetch(E),
    Invalid(String),
}
impl<E: std::fmt::Display> std::fmt::Display for ReadError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fetch(e) => e.fmt(f),
            Self::Invalid(e) => e.fmt(f),
        }
    }
}

/// Encode and address a public DataMap using the canonical record format.
pub(crate) fn public_map_record(map: &DataMap) -> Result<([u8; 32], Bytes), String> {
    let bytes = rmp_serde::to_vec(map)
        .map(Bytes::from)
        .map_err(|e| format!("Failed to serialize DataMap: {e}"))?;
    Ok((ant_protocol::compute_address(&bytes), bytes))
}

pub(crate) fn decode_map(bytes: &[u8]) -> Result<DataMap, String> {
    rmp_serde::from_slice(bytes).map_err(|e| format!("Failed to deserialize DataMap: {e}"))
}

pub(crate) async fn fetch_records<E, F, Fut, C>(
    requests: Vec<RecordRequest>,
    fetch: &F,
    cap: &C,
) -> Result<Vec<(usize, Bytes)>, ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
{
    let results = super::rolling_unordered(
        requests,
        |(index, address)| async move {
            let bytes = fetch(address).await.map_err(ReadError::Fetch)?;
            crate::record::verify(&address, &bytes).map_err(ReadError::Invalid)?;
            Ok((index, bytes))
        },
        cap,
    );
    futures_util::pin_mut!(results);
    let mut ordered = Vec::new();
    while let Some(result) = results.next().await {
        ordered.push(result?);
    }
    ordered.sort_by_key(|(index, _)| *index);
    Ok(ordered)
}

/// Drive the native recursive resolver with async batches. On a cache miss,
/// suspend the synchronous resolver, fetch the whole missing level, and replay
/// with verified cached bytes. This preserves child-level KDF and depth checks
/// in self_encryption without block_on/block_in_place or a second resolver.
pub(crate) async fn resolve<E, F, Fut, C>(
    map: &DataMap,
    fetch: &F,
    cap: &C,
) -> Result<DataMap, ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
{
    let mut cache = HashMap::<[u8; 32], Bytes>::new();
    loop {
        let missing = RefCell::new(Vec::new());
        let result = self_encryption::get_root_data_map_parallel(map.clone(), &|batch| {
            let requests: Vec<_> = batch
                .iter()
                .filter(|(_, hash)| !cache.contains_key(&hash.0))
                .map(|(index, hash)| (*index, hash.0))
                .collect();
            if !requests.is_empty() {
                *missing.borrow_mut() = requests;
                return Err(self_encryption::Error::Generic(
                    "async record fetch required".into(),
                ));
            }
            // Preserve the resolver's positional batch contract, independent
            // of network completion order and DataMap chunk indices.
            Ok(batch
                .iter()
                .map(|(index, hash)| (*index, cache[&hash.0].clone()))
                .collect())
        });
        let requests = missing.into_inner();
        if requests.is_empty() {
            return result.map_err(|e| ReadError::Invalid(e.to_string()));
        }
        // Position, not the externally supplied chunk index, identifies a
        // response so malformed duplicate indices cannot misassociate bytes.
        let addresses: Vec<_> = requests.iter().map(|(_, address)| *address).collect();
        let records =
            fetch_records(addresses.iter().copied().enumerate().collect(), fetch, cap).await?;
        for (position, bytes) in records {
            cache.insert(addresses[position], bytes);
        }
    }
}

pub(crate) async fn download<E, F, Fut, C, S, SF>(
    map: &DataMap,
    fetch: &F,
    cap: &C,
    sleep: &S,
    retryable: fn(&E) -> bool,
) -> Result<Bytes, ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
    S: Fn(std::time::Duration) -> SF,
    SF: Future<Output = ()>,
{
    let root = resolve(map, fetch, cap).await?;
    let requests = root
        .infos()
        .iter()
        .enumerate()
        .map(|(index, info)| (index, info.dst_hash.0))
        .collect();
    let chunks = fetch_records_deferred(requests, fetch, cap, sleep, retryable)
        .await?
        .into_iter()
        .map(|(_, content)| EncryptedChunk { content })
        .collect::<Vec<_>>();
    self_encryption::decrypt(&root, &chunks).map_err(|e| ReadError::Invalid(e.to_string()))
}

/// Validated root-map index. File positions are independent of the platform's
/// pointer width; only individual chunk buffers and indices use `usize`.
/// Build once per reader so each seek is O(log chunks + overlapping chunks).
pub(crate) struct FileIndex {
    infos: Vec<self_encryption::ChunkInfo>,
    hashes: Vec<XorName>,
    offsets: Vec<u64>,
}

impl FileIndex {
    pub(crate) fn new(map: &DataMap) -> Result<Self, String> {
        if map.is_child() {
            return Err("range reads require a resolved root DataMap".into());
        }
        let mut infos = map.infos().to_vec();
        infos.sort_by_key(|info| info.index);
        if infos.len() < 3 {
            return Err("DataMap requires at least three chunks".into());
        }
        let mut offsets = Vec::with_capacity(infos.len() + 1);
        offsets.push(0u64);
        for (index, info) in infos.iter().enumerate() {
            if info.index != index {
                return Err("DataMap chunk indices must be contiguous".into());
            }
            if info.src_size == 0 || info.src_size > self_encryption::MAX_CHUNK_SIZE {
                return Err("invalid DataMap plaintext chunk size".into());
            }
            let end = offsets[index]
                .checked_add(info.src_size as u64)
                .ok_or("DataMap plaintext size overflow")?;
            offsets.push(end);
        }
        let hashes = infos.iter().map(|info| info.src_hash).collect();
        Ok(Self {
            infos,
            hashes,
            offsets,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        *self.offsets.last().expect("index includes zero offset")
    }

    fn range_records(&self, start: u64, length: usize) -> (usize, Vec<RecordRequest>) {
        let end = start.saturating_add(length as u64).min(self.size());
        if start >= end {
            return (0, Vec::new());
        }
        let first = self.offsets.partition_point(|offset| *offset <= start) - 1;
        let last = self.offsets.partition_point(|offset| *offset < end);
        let records = self.infos[first..last]
            .iter()
            .map(|info| (info.index, info.dst_hash.0))
            .collect();
        ((end - start) as usize, records)
    }
}

#[cfg(test)]
fn range_records(
    map: &DataMap,
    start: usize,
    length: usize,
) -> Result<(usize, Vec<RecordRequest>), String> {
    Ok(FileIndex::new(map)?.range_records(start as u64, length))
}

pub(crate) async fn read_range<E, F, Fut, C, S, SF>(
    root: &DataMap,
    start: u64,
    length: usize,
    fetch: &F,
    cap: &C,
    sleep: &S,
    retryable: fn(&E) -> bool,
) -> Result<Bytes, ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
    S: Fn(std::time::Duration) -> SF,
    SF: Future<Output = ()>,
{
    let index = FileIndex::new(root).map_err(ReadError::Invalid)?;
    read_indexed_range(&index, start, length, fetch, cap, sleep, retryable).await
}

pub(crate) async fn read_indexed_range<E, F, Fut, C, S, SF>(
    index: &FileIndex,
    start: u64,
    length: usize,
    fetch: &F,
    cap: &C,
    sleep: &S,
    retryable: fn(&E) -> bool,
) -> Result<Bytes, ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
    S: Fn(std::time::Duration) -> SF,
    SF: Future<Output = ()>,
{
    let (length, required) = index.range_records(start, length);
    if length == 0 {
        return Ok(Bytes::new());
    }
    let records = fetch_records_deferred(required, fetch, cap, sleep, retryable).await?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|e| ReadError::Invalid(format!("cannot allocate range: {e}")))?;
    let end = start + length as u64;
    for (position, encrypted) in records {
        let info = &index.infos[position];
        // The dependency's get_range uses usize file offsets. Its public
        // per-chunk primitive has no whole-file arithmetic and retains the
        // canonical KDF (original chunk index and neighbouring hashes).
        let plaintext = self_encryption::decrypt_chunk(position, &encrypted, &index.hashes, 0)
            .map_err(|e| ReadError::Invalid(e.to_string()))?;
        if plaintext.len() != info.src_size {
            return Err(ReadError::Invalid(
                "decrypted chunk size differs from DataMap".into(),
            ));
        }
        crate::record::verify(&info.src_hash.0, &plaintext).map_err(ReadError::Invalid)?;
        let chunk_start = index.offsets[position];
        let from = start.saturating_sub(chunk_start) as usize;
        let to = (end - chunk_start).min(info.src_size as u64) as usize;
        output.extend_from_slice(&plaintext[from..to]);
    }
    if output.len() != length {
        return Err(ReadError::Invalid(format!(
            "range returned {} bytes, expected {length}",
            output.len()
        )));
    }
    Ok(Bytes::from(output))
}

/// Native file-download retry rounds: retry missing records together after the
/// first batch settles, immediately once, then after 15 and 45 seconds. The
/// caller supplies typed fatal errors and a runtime-specific timer.
pub(crate) async fn deferred_batch<K, E, F, Fut, C, S, SF>(
    requests: Vec<(usize, K)>,
    fetch: F,
    cap: C,
    sleep: S,
    exhausted: impl Fn(K) -> E,
) -> Result<Vec<(usize, Bytes)>, E>
where
    K: Copy,
    F: Fn(usize, K, usize) -> Fut,
    Fut: Future<Output = Result<(usize, Result<Bytes, K>), E>>,
    C: Fn() -> usize,
    S: Fn(std::time::Duration) -> SF,
    SF: Future<Output = ()>,
{
    let mut remaining = requests;
    let mut results = Vec::new();
    for (round, delay) in [0, 0, 15, 45].into_iter().enumerate() {
        if remaining.is_empty() {
            break;
        }
        if delay > 0 {
            sleep(std::time::Duration::from_secs(delay)).await;
        }
        let input = std::mem::take(&mut remaining);
        let pending =
            super::rolling_unordered(input, |(index, key)| fetch(index, key, round + 1), &cap);
        futures_util::pin_mut!(pending);
        while let Some(result) = pending.next().await {
            let (index, content) = result?;
            match content {
                Ok(bytes) => results.push((index, bytes)),
                Err(key) => remaining.push((index, key)),
            }
        }
    }
    if let Some((_, key)) = remaining.first() {
        return Err(exhausted(*key));
    }
    results.sort_by_key(|(index, _)| *index);
    Ok(results)
}

async fn fetch_records_deferred<E, F, Fut, C, S, SF>(
    requests: Vec<RecordRequest>,
    fetch: &F,
    cap: &C,
    sleep: &S,
    retryable: fn(&E) -> bool,
) -> Result<Vec<(usize, Bytes)>, ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
    S: Fn(std::time::Duration) -> SF,
    SF: Future<Output = ()>,
{
    // Preserve the adapter's final typed fetch error across deferred rounds.
    let errors = std::sync::Mutex::new(HashMap::new());
    deferred_batch(
        requests,
        |index, address, _| {
            let errors = &errors;
            async move {
                match fetch(address).await {
                    Ok(bytes) => {
                        crate::record::verify(&address, &bytes).map_err(ReadError::Invalid)?;
                        Ok((index, Ok(bytes)))
                    }
                    Err(error) if !retryable(&error) => Err(ReadError::Fetch(error)),
                    Err(error) => {
                        errors
                            .lock()
                            .expect("fetch errors lock")
                            .insert(address, error);
                        Ok((index, Err(address)))
                    }
                }
            }
        },
        cap,
        sleep,
        |address| {
            ReadError::Fetch(
                errors
                    .lock()
                    .expect("fetch errors lock")
                    .remove(&address)
                    .expect("deferred record has a fetch error"),
            )
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn fixture() -> (Bytes, DataMap, HashMap<[u8; 32], Bytes>) {
        let content = Bytes::from((0..18000).map(|i| (i * 37) as u8).collect::<Vec<_>>());
        let (map, chunks) = self_encryption::encrypt(content.clone()).unwrap();
        let records = chunks
            .into_iter()
            .map(|chunk| (*blake3::hash(&chunk.content).as_bytes(), chunk.content))
            .collect();
        (content, map, records)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_nested_resolution_matches_native_without_blocking_runtime() {
        let (content, root, mut records) = fixture();
        let mut published = root.clone();
        // Build two wrapper levels with the public encryption API. That API
        // uses KDF level 0, so label these child maps with exactly that level.
        // Native self_encryption resolves these same maps below.
        for _ in 0..2 {
            let (wrapper, chunks) =
                self_encryption::encrypt(Bytes::from(published.to_bytes().unwrap())).unwrap();
            for chunk in chunks {
                records.insert(*blake3::hash(&chunk.content).as_bytes(), chunk.content);
            }
            published = DataMap::with_child(wrapper.infos().to_vec(), 0);
        }
        let native = self_encryption::get_root_data_map(published.clone(), &mut |hash| {
            Ok(records[&hash.0].clone())
        })
        .unwrap();
        assert_eq!(native, root);
        let fetch = |address| {
            let bytes = records[&address].clone();
            async move {
                tokio::task::yield_now().await;
                Ok::<_, String>(bytes)
            }
        };
        assert_eq!(resolve(&published, &fetch, &|| 3).await.unwrap(), native);
        assert_eq!(
            download(&published, &fetch, &|| 3, &|_| async {}, |_| true)
                .await
                .unwrap(),
            content
        );
    }

    #[tokio::test]
    async fn native_shrunk_map_download_matches_self_encryption() {
        let content = Bytes::from(vec![17; 3 * self_encryption::MAX_CHUNK_SIZE + 1]);
        let (map, chunks) = self_encryption::encrypt(content.clone()).unwrap();
        assert!(map.is_child());
        let records: HashMap<_, _> = chunks
            .iter()
            .map(|c| (*blake3::hash(&c.content).as_bytes(), c.content.clone()))
            .collect();
        let actual = download(
            &map,
            &|address| {
                let bytes = records[&address].clone();
                async move { Ok::<_, String>(bytes) }
            },
            &|| 3,
            &|_| async {},
            |_| true,
        )
        .await
        .unwrap();
        assert_eq!(actual, self_encryption::decrypt(&map, &chunks).unwrap());
        assert_eq!(actual, content);
    }

    #[tokio::test]
    async fn ranges_match_plaintext_and_fetch_only_overlapping_records() {
        let (content, map, records) = fixture();
        let boundary = map.infos()[0].src_size;
        for (start, length) in [
            (0, 1),
            (boundary - 1, 2),
            (boundary, boundary),
            (content.len() - 1, 50),
            (content.len(), 3),
            (usize::MAX, 5),
            (0, 0),
        ] {
            let seen = Mutex::new(Vec::new());
            let fetch = |address| {
                seen.lock().unwrap().push(address);
                let bytes = records[&address].clone();
                async move { Ok::<_, String>(bytes) }
            };
            let actual = read_range(
                &map,
                start as u64,
                length,
                &fetch,
                &|| 3,
                &|_| async {},
                |_| true,
            )
            .await
            .unwrap();
            let expected =
                &content[start.min(content.len())..start.saturating_add(length).min(content.len())];
            assert_eq!(actual.as_ref(), expected);
            let (_, required) = range_records(&map, start, length).unwrap();
            assert_eq!(seen.lock().unwrap().len(), required.len());
        }
    }

    #[tokio::test]
    async fn deferred_rounds_retry_only_missing_records_and_preserve_order() {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let sleeps = Mutex::new(Vec::new());
        let result = deferred_batch(
            vec![(0, 0u8), (1, 1), (2, 2)],
            |index, key, attempt| {
                attempts.lock().unwrap().push((key, attempt));
                async move {
                    Ok::<_, ()>((
                        index,
                        if key == 1 && attempt < 4 {
                            Err(key)
                        } else {
                            Ok(Bytes::from(vec![key]))
                        },
                    ))
                }
            },
            || 2,
            |delay| {
                sleeps.lock().unwrap().push(delay.as_secs());
                async {}
            },
            |_| (),
        )
        .await
        .unwrap();
        assert_eq!(
            result.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(*sleeps.lock().unwrap(), vec![15, 45]);
        assert_eq!(
            attempts
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, _)| *key == 0)
                .count(),
            1
        );
        assert_eq!(
            attempts
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, _)| *key == 1)
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn corrupt_records_are_rejected_and_fetch_errors_keep_their_type() {
        let (_, map, _) = fixture();
        let error = download(
            &map,
            &|_| async { Ok::<_, u8>(Bytes::from_static(b"corrupt")) },
            &|| 2,
            &|_| async {},
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ReadError::Invalid(message) if message.contains("BLAKE3 mismatch"))
        );
        let error = download(
            &map,
            &|_| async { Err::<Bytes, _>(42u8) },
            &|| 2,
            &|_| async {},
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadError::Fetch(42)));
        let child = DataMap::with_child(map.infos().to_vec(), 1);
        let error = resolve(&child, &|_| async { Err::<Bytes, _>(23u8) }, &|| 2)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::Fetch(23)));
    }

    #[tokio::test]
    async fn fatal_fetch_errors_do_not_enter_deferred_rounds() {
        let (_, map, _) = fixture();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let error = download(
            &map,
            &|_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async { Err::<Bytes, _>(42u8) }
            },
            &|| 1,
            &|_| async { panic!("fatal errors must not sleep") },
            |_| false,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadError::Fetch(42)));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn malformed_range_maps_are_rejected() {
        let (_, map, _) = fixture();
        let mut infos = map.infos().to_vec();
        infos[1].index = 0;
        assert!(range_records(&DataMap::new(infos), 0, 1).is_err());
        let mut infos = map.infos().to_vec();
        infos[0].src_size = usize::MAX;
        assert!(range_records(&DataMap::new(infos), 0, 1).is_err());
    }

    #[test]
    fn indexed_ranges_cross_four_gib_without_pointer_width_arithmetic() {
        let size = self_encryption::MAX_CHUNK_SIZE;
        let infos = (0..1030)
            .map(|index| self_encryption::ChunkInfo {
                index,
                src_size: size,
                src_hash: XorName([1; 32]),
                dst_hash: XorName([2; 32]),
            })
            .collect();
        let index = FileIndex::new(&DataMap::new(infos)).unwrap();
        assert_eq!(index.size(), 1030 * size as u64);
        let start = (1u64 << 32) - 4;
        let (length, records) = index.range_records(start, 8);
        assert_eq!(length, 8);
        assert_eq!(records[0].0 as u64, start / size as u64);
        let boundary = 1026 * size as u64;
        assert!(boundary > u32::MAX as u64);
        let (length, records) = index.range_records(boundary - 1, 2);
        assert_eq!(length, 2);
        assert_eq!(
            records.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![1025, 1026]
        );
        assert_eq!(index.range_records(index.size() - 1, 10).0, 1);
        assert_eq!(index.range_records(u64::MAX, 10).0, 0);
    }

    #[tokio::test]
    async fn ranges_reject_mismatched_plaintext_size() {
        let (_, map, records) = fixture();
        let mut infos = map.infos().to_vec();
        infos[0].src_size += 1;
        let fetch = |address| {
            let bytes = records[&address].clone();
            async move { Ok::<_, String>(bytes) }
        };
        let error = read_range(
            &DataMap::new(infos),
            0,
            1,
            &fetch,
            &|| 1,
            &|_| async {},
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("chunk size differs"));
    }
}
