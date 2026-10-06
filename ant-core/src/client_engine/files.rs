//! Async data workflows over a verified record fetcher. No runtime, sockets,
//! filesystem or JS dependencies. Cryptography and recursive map semantics stay
//! in self_encryption, the same implementation used by the native client.
use bytes::Bytes;
#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
use futures_util::future::{select, Either};
#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use self_encryption::{ChunkInfo, DataMap, EncryptedChunk, XorName};
use std::{cell::RefCell, collections::HashMap, future::Future, ops::Range};

/// self_encryption splits every encryptable file into at least three chunks.
pub(crate) const MIN_FILE_CHUNKS: usize = 3;
/// Content chunks of a resolved root DataMap are encrypted at KDF level zero.
const CONTENT_CHUNK_KDF_LEVEL: usize = 0;

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
    /// Plaintext hashes in chunk order. Each chunk's key derives from its
    /// neighbours, so decryption needs the whole list.
    src_hashes: Vec<XorName>,
    /// Encrypted record addresses in chunk order.
    dst_hashes: Vec<XorName>,
    /// Plaintext start of each chunk, followed by the file size.
    offsets: Vec<u64>,
}

impl FileIndex {
    /// Chunk sizes are taken from the map, so maps produced with another
    /// self-encryption chunk size remain readable.
    pub(crate) fn new(map: &DataMap) -> Result<Self, String> {
        if map.is_child() {
            return Err("range reads require a resolved root DataMap".into());
        }
        let mut infos: Vec<&ChunkInfo> = map.infos().iter().collect();
        infos.sort_by_key(|info| info.index);
        if infos.len() < MIN_FILE_CHUNKS {
            return Err(format!(
                "DataMap requires at least {MIN_FILE_CHUNKS} chunks"
            ));
        }
        let mut offsets = Vec::with_capacity(infos.len() + 1);
        offsets.push(0u64);
        for (index, info) in infos.iter().enumerate() {
            if info.index != index {
                return Err("DataMap chunk indices must be contiguous".into());
            }
            if info.src_size == 0 {
                return Err("invalid DataMap plaintext chunk size".into());
            }
            let end = offsets[index]
                .checked_add(info.src_size as u64)
                .ok_or("DataMap plaintext size overflow")?;
            offsets.push(end);
        }
        Ok(Self {
            src_hashes: infos.iter().map(|info| info.src_hash).collect(),
            dst_hashes: infos.iter().map(|info| info.dst_hash).collect(),
            offsets,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        *self.offsets.last().expect("index includes zero offset")
    }

    /// Half-open plaintext byte span of one chunk.
    pub(crate) fn chunk_span(&self, chunk: usize) -> Range<u64> {
        self.offsets[chunk]..self.offsets[chunk + 1]
    }

    /// Chunks overlapping a half-open byte range, clamped at EOF.
    pub(crate) fn chunks_overlapping(&self, range: Range<u64>) -> Range<usize> {
        let end = range.end.min(self.size());
        if range.start >= end {
            return 0..0;
        }
        let first = self
            .offsets
            .partition_point(|offset| *offset <= range.start)
            - 1;
        let last = self.offsets.partition_point(|offset| *offset < end);
        first..last
    }

    /// Decrypt one verified record and check it against the map's plaintext
    /// size and hash.
    fn decrypt(&self, chunk: usize, encrypted: &Bytes) -> Result<Bytes, String> {
        // The dependency's get_range uses usize file offsets. Its public
        // per-chunk primitive has no whole-file arithmetic and retains the
        // canonical KDF (original chunk index and neighbouring hashes).
        let plaintext = self_encryption::decrypt_chunk(
            chunk,
            encrypted,
            &self.src_hashes,
            CONTENT_CHUNK_KDF_LEVEL,
        )
        .map_err(|e| e.to_string())?;
        let span = self.chunk_span(chunk);
        if plaintext.len() as u64 != span.end - span.start {
            return Err("decrypted chunk size differs from DataMap".into());
        }
        crate::record::verify(&self.src_hashes[chunk].0, &plaintext)?;
        Ok(plaintext)
    }
}

/// Browser readers and complete downloads.
#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
impl FileIndex {
    pub(crate) fn chunk_count(&self) -> usize {
        self.dst_hashes.len()
    }

    /// Encrypted record address of one chunk.
    pub(crate) fn address(&self, chunk: usize) -> [u8; 32] {
        self.dst_hashes[chunk].0
    }

    /// The self-encryption chunk metadata in chunk order.
    pub(crate) fn chunk_infos(&self) -> impl Iterator<Item = ChunkInfo> + '_ {
        (0..self.chunk_count()).map(|index| {
            let span = self.chunk_span(index);
            ChunkInfo {
                index,
                dst_hash: self.dst_hashes[index],
                src_hash: self.src_hashes[index],
                // Each size was a usize in the DataMap this index came from.
                src_size: (span.end - span.start) as usize,
            }
        })
    }
}

/// Fetch, verify and decrypt `chunks` in one deferred retry pass, in chunk
/// order.
pub(crate) async fn read_chunks<E, F, Fut, C, S, SF>(
    index: &FileIndex,
    chunks: Range<usize>,
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
    let requests = chunks
        .map(|chunk| (chunk, index.dst_hashes[chunk].0))
        .collect();
    fetch_records_deferred(requests, fetch, cap, sleep, retryable)
        .await?
        .into_iter()
        .map(|(chunk, encrypted)| {
            let plaintext = index
                .decrypt(chunk, &encrypted)
                .map_err(ReadError::Invalid)?;
            Ok((chunk, plaintext))
        })
        .collect()
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

/// Half-open plaintext range. EOF and zero-length reads have the same
/// semantics on every platform.
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
    let end = start.saturating_add(length as u64).min(index.size());
    if start >= end {
        return Ok(Bytes::new());
    }
    // `end - start` is at most the requested usize length.
    let length = (end - start) as usize;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|e| ReadError::Invalid(format!("cannot allocate range: {e}")))?;
    let chunks = index.chunks_overlapping(start..end);
    for (chunk, plaintext) in read_chunks(index, chunks, fetch, cap, sleep, retryable).await? {
        let span = index.chunk_span(chunk);
        let from = start.max(span.start) - span.start;
        let to = end.min(span.end) - span.start;
        output.extend_from_slice(&plaintext[from as usize..to as usize]);
    }
    Ok(Bytes::from(output))
}

#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
enum PipelineEvent<T, X> {
    Written(Result<(), X>),
    Fetched(usize, Result<T, X>),
}

/// Fetch `items` concurrently and hand each result to `write` in item order.
/// Items launch in order while fewer than `cap()` are in flight and the cost
/// of launched-but-unwritten items stays within `budget`; one item can always
/// run. Fetching continues while a write is pending, so slow items and their
/// retry waits overlap with every other item the budget admits.
#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
pub(crate) async fn ordered_pipeline<T, X, F, FF, W, WF, C>(
    items: Range<usize>,
    cost: impl Fn(usize) -> u64,
    budget: u64,
    cap: &C,
    fetch: F,
    mut write: W,
) -> Result<(), X>
where
    F: Fn(usize) -> FF,
    FF: Future<Output = Result<T, X>>,
    W: FnMut(usize, T) -> WF,
    WF: Future<Output = Result<(), X>>,
    C: Fn() -> usize,
{
    let fetch = &fetch;
    let mut in_flight = FuturesUnordered::new();
    let mut ready = HashMap::new();
    let mut writing = None;
    let (mut next_launch, mut next_write) = (items.start, items.start);
    let mut outstanding = 0u64;
    while next_write < items.end {
        while next_launch < items.end
            && in_flight.len() < cap().max(1)
            && (outstanding == 0 || outstanding.saturating_add(cost(next_launch)) <= budget)
        {
            let item = next_launch;
            in_flight.push(async move { (item, fetch(item).await) });
            outstanding = outstanding.saturating_add(cost(item));
            next_launch += 1;
        }
        if writing.is_none() {
            if let Some(value) = ready.remove(&next_write) {
                writing = Some(Box::pin(write(next_write, value)));
            }
        }
        // `outstanding` covers the next item to write, so with nothing being
        // written that item is in flight.
        let event = match writing.as_mut() {
            Some(pending) if in_flight.is_empty() => PipelineEvent::Written(pending.await),
            Some(pending) => match select(pending, in_flight.next()).await {
                Either::Left((written, _)) => PipelineEvent::Written(written),
                Either::Right((fetched, _)) => {
                    let (item, value) = fetched.expect("items are in flight");
                    PipelineEvent::Fetched(item, value)
                }
            },
            None => {
                let (item, value) = in_flight
                    .next()
                    .await
                    .expect("the next item to write is in flight");
                PipelineEvent::Fetched(item, value)
            }
        };
        match event {
            PipelineEvent::Written(written) => {
                written?;
                writing = None;
                outstanding -= cost(next_write);
                next_write += 1;
            }
            PipelineEvent::Fetched(item, value) => {
                ready.insert(item, value?);
            }
        }
    }
    Ok(())
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
            let seen = std::sync::Mutex::new(Vec::new());
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
            let end = (start as u64).saturating_add(length as u64);
            let required = FileIndex::new(&map)
                .unwrap()
                .chunks_overlapping(start as u64..end);
            assert_eq!(seen.lock().unwrap().len(), required.len());
        }
    }

    #[tokio::test]
    async fn deferred_rounds_retry_only_missing_records_and_preserve_order() {
        let attempts = std::sync::Mutex::new(Vec::new());
        let sleeps = std::sync::Mutex::new(Vec::new());
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
        assert!(FileIndex::new(&DataMap::new(infos)).is_err());
        let mut infos = map.infos().to_vec();
        infos[0].src_size = 0;
        assert!(FileIndex::new(&DataMap::new(infos)).is_err());
        let mut infos = map.infos().to_vec();
        infos.iter_mut().for_each(|info| info.src_size = usize::MAX);
        assert!(FileIndex::new(&DataMap::new(infos)).is_err());
        let infos = map.infos()[..MIN_FILE_CHUNKS - 1].to_vec();
        assert!(FileIndex::new(&DataMap::new(infos)).is_err());
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
        let chunks = index.chunks_overlapping(start..start + 8);
        assert_eq!(chunks.start as u64, start / size as u64);
        let boundary = 1026 * size as u64;
        assert!(boundary > u32::MAX as u64);
        assert_eq!(
            index.chunks_overlapping(boundary - 1..boundary + 1),
            1025..1027
        );
        assert_eq!(index.chunk_span(1026), boundary..boundary + size as u64);
        assert_eq!(
            index.chunks_overlapping(index.size() - 1..u64::MAX),
            1029..1030
        );
        assert!(index.chunks_overlapping(u64::MAX - 10..u64::MAX).is_empty());
        assert!(index.chunks_overlapping(5..5).is_empty());
    }

    #[test]
    fn maps_with_larger_chunks_than_this_build_produces_are_indexed() {
        let size = 2 * self_encryption::MAX_CHUNK_SIZE;
        let infos: Vec<_> = (0..MIN_FILE_CHUNKS)
            .map(|index| self_encryption::ChunkInfo {
                index,
                src_size: size,
                src_hash: XorName([index as u8; 32]),
                dst_hash: XorName([0xff - index as u8; 32]),
            })
            .collect();
        let index = FileIndex::new(&DataMap::new(infos.clone())).unwrap();
        assert_eq!(index.size(), (MIN_FILE_CHUNKS * size) as u64);
        assert_eq!(index.chunk_infos().collect::<Vec<_>>(), infos);
        assert_eq!(
            index.dst_hashes,
            infos.iter().map(|info| info.dst_hash).collect::<Vec<_>>()
        );
        assert_eq!(index.address(2), infos[2].dst_hash.0);
        // Chunk sizes are not capped; any positive size is indexed.
        let mut infos = infos;
        infos[1].src_size = 64 * size;
        let index = FileIndex::new(&DataMap::new(infos)).unwrap();
        assert_eq!(index.size(), (MIN_FILE_CHUNKS + 63) as u64 * size as u64);
    }

    #[tokio::test]
    async fn altered_range_records_are_rejected() {
        let (_, map, records) = fixture();
        let fetch = |address| {
            let mut bytes = records[&address].to_vec();
            *bytes.last_mut().unwrap() ^= 1;
            async move { Ok::<_, String>(Bytes::from(bytes)) }
        };
        let error = read_range(&map, 0, 1, &fetch, &|| 1, &|_| async {}, |_| true)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ReadError::Invalid(message) if message.contains("BLAKE3 mismatch"))
        );
    }

    #[tokio::test]
    async fn a_missing_chunk_is_retried_on_its_own_schedule() {
        let (content, map, records) = fixture();
        let index = FileIndex::new(&map).unwrap();
        let attempts = std::cell::Cell::new(0);
        let sleeps = std::cell::RefCell::new(Vec::new());
        let fetch_after = |available_from: usize| {
            let (attempts, records) = (&attempts, &records);
            move |address: [u8; 32]| {
                attempts.set(attempts.get() + 1);
                let found = attempts.get() >= available_from;
                let bytes = records[&address].clone();
                async move {
                    if found {
                        Ok(bytes)
                    } else {
                        Err("missing".to_string())
                    }
                }
            }
        };
        let sleep = |delay: std::time::Duration| {
            sleeps.borrow_mut().push(delay.as_secs());
            async {}
        };
        // The first pass and three deferred retries.
        const ATTEMPTS: usize = 4;
        // One chunk on its own, as each pipeTo fetch is.
        let chunks = read_chunks(&index, 1..2, &fetch_after(ATTEMPTS), &|| 1, &sleep, |_| {
            true
        })
        .await
        .unwrap();
        let span = index.chunk_span(1);
        assert_eq!(
            chunks,
            vec![(1, content.slice(span.start as usize..span.end as usize))]
        );
        assert_eq!(*sleeps.borrow(), vec![15, 45]);

        attempts.set(0);
        let error = read_chunks(
            &index,
            1..2,
            &fetch_after(ATTEMPTS + 1),
            &|| 1,
            &sleep,
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadError::Fetch(message) if message == "missing"));
        assert_eq!(attempts.get(), ATTEMPTS);

        attempts.set(0);
        let error = read_chunks(
            &index,
            1..2,
            &fetch_after(ATTEMPTS + 1),
            &|| 1,
            &sleep,
            |_| false,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadError::Fetch(_)));
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test]
    async fn chunks_share_one_retry_pass() {
        let (content, map, records) = fixture();
        let index = FileIndex::new(&map).unwrap();
        let attempts = std::sync::Mutex::new(HashMap::<[u8; 32], usize>::new());
        let in_flight = std::cell::Cell::new(0);
        let most_in_flight = std::cell::Cell::new(0);
        let sleeps = std::sync::Mutex::new(Vec::new());
        // The first and last records are missing until their third attempt.
        let missing = [
            index.dst_hashes[0].0,
            index.dst_hashes[MIN_FILE_CHUNKS - 1].0,
        ];
        let fetch = |address: [u8; 32]| {
            let attempt = {
                let mut attempts = attempts.lock().unwrap();
                let attempt = attempts.entry(address).or_default();
                *attempt += 1;
                *attempt
            };
            let bytes = records[&address].clone();
            let (in_flight, most_in_flight) = (&in_flight, &most_in_flight);
            async move {
                in_flight.set(in_flight.get() + 1);
                most_in_flight.set(most_in_flight.get().max(in_flight.get()));
                tokio::task::yield_now().await;
                in_flight.set(in_flight.get() - 1);
                if missing.contains(&address) && attempt < 3 {
                    Err("missing".to_string())
                } else {
                    Ok(bytes)
                }
            }
        };
        let received = read_chunks(
            &index,
            0..index.chunk_count(),
            &fetch,
            &|| MIN_FILE_CHUNKS,
            &|delay| {
                sleeps.lock().unwrap().push(delay.as_secs());
                async {}
            },
            |_| true,
        )
        .await
        .unwrap();
        assert_eq!(most_in_flight.get(), MIN_FILE_CHUNKS);
        // Both missing records wait out the same 15-second round.
        assert_eq!(*sleeps.lock().unwrap(), vec![15]);
        assert_eq!(
            received.iter().map(|(chunk, _)| *chunk).collect::<Vec<_>>(),
            (0..MIN_FILE_CHUNKS).collect::<Vec<_>>()
        );
        for (chunk, plaintext) in received {
            let span = index.chunk_span(chunk);
            assert_eq!(
                plaintext,
                content.slice(span.start as usize..span.end as usize)
            );
        }
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

    #[tokio::test]
    async fn pipeline_writes_in_order_within_its_budget() {
        const ITEMS: usize = 6;
        const COST: u64 = 10;
        const BUDGET: u64 = 3 * COST;
        let launched = std::cell::Cell::new(0u64);
        let most_outstanding = std::cell::Cell::new(0u64);
        let mut written = Vec::new();
        ordered_pipeline(
            0..ITEMS,
            |_| COST,
            BUDGET,
            &|| ITEMS,
            |item| {
                launched.set(launched.get() + 1);
                async move {
                    // Later items finish first.
                    for _ in item..ITEMS {
                        tokio::task::yield_now().await;
                    }
                    Ok::<_, String>(item)
                }
            },
            |item, value| {
                assert_eq!(item, value);
                let outstanding = (launched.get() - written.len() as u64) * COST;
                most_outstanding.set(most_outstanding.get().max(outstanding));
                written.push(item);
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(written, (0..ITEMS).collect::<Vec<_>>());
        assert_eq!(most_outstanding.get(), BUDGET);
    }

    #[tokio::test]
    async fn pipeline_keeps_fetching_while_a_write_is_pending() {
        // One fetch at a time: the first write only finishes once the last
        // item has been fetched, so fetching must continue behind it.
        let (fetched_last, unblock) = tokio::sync::oneshot::channel();
        let fetched_last = std::cell::RefCell::new(Some(fetched_last));
        let unblock = std::cell::RefCell::new(Some(unblock));
        let mut written = Vec::new();
        let pipeline = ordered_pipeline(
            0..4,
            |_| 1,
            u64::MAX,
            &|| 1,
            |item| {
                if item == 3 {
                    let _ = fetched_last.borrow_mut().take().unwrap().send(());
                }
                async move { Ok::<_, String>(item) }
            },
            |item, _| {
                let unblock = (item == 0).then(|| unblock.borrow_mut().take().unwrap());
                written.push(item);
                async move {
                    if let Some(unblock) = unblock {
                        unblock.await.unwrap();
                    }
                    Ok(())
                }
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), pipeline)
            .await
            .expect("fetching stalled behind a pending write")
            .unwrap();
        assert_eq!(written, vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn a_slow_item_does_not_hold_back_fetches_behind_it() {
        // The first item stays in flight (as during a retry wait) until the
        // third has been fetched.
        let (fetched_third, released) = tokio::sync::oneshot::channel();
        let fetched_third = std::cell::RefCell::new(Some(fetched_third));
        let released = std::cell::RefCell::new(Some(released));
        let completed = std::cell::RefCell::new(Vec::new());
        let pipeline = ordered_pipeline(
            0..3,
            |_| 1,
            3,
            &|| 3,
            |item| {
                let released = (item == 0).then(|| released.borrow_mut().take().unwrap());
                let fetched_third = (item == 2).then(|| fetched_third.borrow_mut().take().unwrap());
                let completed = &completed;
                async move {
                    if let Some(released) = released {
                        released.await.unwrap();
                    }
                    completed.borrow_mut().push(item);
                    if let Some(fetched_third) = fetched_third {
                        let _ = fetched_third.send(());
                    }
                    Ok::<_, String>(item)
                }
            },
            |_, _| async { Ok(()) },
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), pipeline)
            .await
            .expect("a slow item held back the items behind it")
            .unwrap();
        assert_eq!(*completed.borrow(), vec![1, 2, 0]);
    }

    #[tokio::test]
    async fn pipeline_admits_an_oversized_item_and_stops_at_the_first_error() {
        let mut written = Vec::new();
        ordered_pipeline(
            0..2,
            |_| 100,
            10,
            &|| 4,
            |item| async move { Ok::<_, String>(item) },
            |item, _| {
                written.push(item);
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(written, vec![0, 1]);

        let error = ordered_pipeline(
            0..4,
            |_| 1,
            4,
            &|| 4,
            |item| async move {
                if item == 2 {
                    Err(format!("fetch {item}"))
                } else {
                    Ok(item)
                }
            },
            |_, _| async { Ok(()) },
        )
        .await
        .unwrap_err();
        assert_eq!(error, "fetch 2");
        let error = ordered_pipeline(
            0..4,
            |_| 1,
            4,
            &|| 4,
            |item| async move { Ok::<_, String>(item) },
            |item, _| async move {
                if item == 1 {
                    Err(format!("write {item}"))
                } else {
                    Ok(())
                }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error, "write 1");
    }
}
