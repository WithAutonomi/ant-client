//! Async data workflows over a verified record fetcher. No runtime, sockets,
//! filesystem or JS dependencies. Whole-file decryption stays in
//! self_encryption, the same implementation used by the native client; nested
//! DataMaps and individual chunks use its KDF through `chunk_decrypt`, which
//! bounds decompression.
pub(crate) use super::chunk_decrypt::MIN_FILE_CHUNKS;
use bytes::{Buf, Bytes};
#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
use futures_util::future::{select, Either};
#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use self_encryption::{ChunkInfo, DataMap, EncryptedChunk, XorName};
use std::{collections::HashMap, future::Future, io::Read, ops::Range, time::Duration};

/// Deepest DataMap nesting resolved, as in self_encryption.
const MAX_DATA_MAP_DEPTH: usize = 100;
/// Content chunks of a resolved root DataMap are encrypted at KDF level zero.
const CONTENT_CHUNK_KDF_LEVEL: usize = 0;
/// Seconds to wait before each deferred fetch round: the first pass, one
/// immediate retry, then retries after 15 and 45 seconds.
const DEFERRED_ROUND_DELAYS_SECS: [u64; 4] = [0, 0, 15, 45];

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

/// Fetch records once each, handing every record to `on_record` as it
/// arrives. Records are not re-hashed: the fetcher verifies addresses, and
/// decryption authenticates every record.
async fn fetch_records<E, F, Fut, C>(
    requests: Vec<RecordRequest>,
    fetch: &F,
    cap: &C,
    mut on_record: impl FnMut(usize, Bytes) -> Result<(), ReadError<E>>,
) -> Result<(), ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
{
    let results = super::rolling_unordered(
        requests,
        |(index, address)| async move {
            let bytes = fetch(address).await.map_err(ReadError::Fetch)?;
            Ok((index, bytes))
        },
        cap,
    );
    futures_util::pin_mut!(results);
    while let Some(result) = results.next().await {
        let (index, bytes) = result?;
        on_record(index, bytes)?;
    }
    Ok(())
}

/// The decrypted chunks of a nested DataMap level read as one stream.
struct ChunksReader {
    remaining: std::vec::IntoIter<Bytes>,
    current: Bytes,
}

impl Read for ChunksReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while !self.current.has_remaining() {
            match self.remaining.next() {
                Some(next) => self.current = next,
                None => return Ok(0),
            }
        }
        let length = buf.len().min(self.current.remaining());
        self.current.copy_to_slice(&mut buf[..length]);
        Ok(length)
    }
}

/// Resolve a published, possibly nested DataMap to its root. Each level's
/// records are fetched together and decrypted with self_encryption's KDF at
/// that level, with decompression bounded by each chunk's declared size, so a
/// crafted record cannot expand. `max_map_bytes` bounds each decoded level's
/// declared size. Memory follows the data actually decrypted, never the sizes
/// a map declares. Records are placed by position, not by the externally
/// supplied chunk index, so malformed duplicate indices cannot misassociate
/// bytes.
pub(crate) async fn resolve<E, F, Fut, C>(
    map: &DataMap,
    fetch: &F,
    cap: &C,
    max_map_bytes: usize,
) -> Result<DataMap, ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
{
    let mut current = map.clone();
    for _ in 0..=MAX_DATA_MAP_DEPTH {
        let Some(kdf_level) = current.child() else {
            return Ok(current);
        };
        let infos = current.infos();
        if infos.iter().any(|info| info.src_size == 0) {
            return Err(ReadError::Invalid(
                "invalid nested DataMap chunk size".into(),
            ));
        }
        infos
            .iter()
            .try_fold(0usize, |total, info| total.checked_add(info.src_size))
            .filter(|total| *total <= max_map_bytes)
            .ok_or_else(|| {
                ReadError::Invalid(format!("nested DataMap exceeds {max_map_bytes} bytes"))
            })?;
        let mut chunks: Vec<Option<Bytes>> = vec![None; infos.len()];
        let src_hashes: Vec<XorName> = infos.iter().map(|info| info.src_hash).collect();
        let requests = infos
            .iter()
            .map(|info| info.dst_hash.0)
            .enumerate()
            .collect();
        fetch_records(requests, fetch, cap, |position, record| {
            let plaintext = super::chunk_decrypt::decrypt_chunk(
                position,
                &record,
                &src_hashes,
                kdf_level,
                infos[position].src_size,
            )
            .map_err(ReadError::Invalid)?;
            crate::record::verify(&src_hashes[position].0, &plaintext)
                .map_err(ReadError::Invalid)?;
            chunks[position] = Some(plaintext);
            Ok(())
        })
        .await?;
        // Decode straight from the chunks rather than joining them first.
        // These are DataMap::from_bytes' bincode options, for a reader.
        let chunks = ChunksReader {
            remaining: chunks.into_iter().flatten().collect::<Vec<_>>().into_iter(),
            current: Bytes::new(),
        };
        current = bincode::deserialize_from(chunks)
            .map_err(|e| ReadError::Invalid(format!("Failed to deserialize DataMap: {e}")))?;
    }
    Err(ReadError::Invalid(format!(
        "DataMap nesting exceeds {MAX_DATA_MAP_DEPTH} levels"
    )))
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
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    // The native whole-file path keeps no limit on nested DataMap levels.
    let root = resolve(map, fetch, cap, usize::MAX).await?;
    let requests = root
        .infos()
        .iter()
        .enumerate()
        .map(|(index, info)| (index, info.dst_hash.0))
        .collect();
    let mut records = Vec::new();
    // self_encryption matches records to the map by content hash and
    // authenticates each one, so they are not hashed here as well.
    fetch_records_deferred(requests, fetch, cap, sleep, retryable, |index, content| {
        records.push((index, content));
        Ok(())
    })
    .await?;
    records.sort_by_key(|(index, _)| *index);
    let chunks = records
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

    /// Decrypt one record and check it against the map's plaintext size and
    /// hash. Decompression stops at the size the map declares. The record is
    /// not re-hashed: the fetcher verifies addresses, and an altered record
    /// fails ChaCha20-Poly1305 authentication or the plaintext hash.
    fn decrypt(&self, chunk: usize, encrypted: &Bytes) -> Result<Bytes, String> {
        let span = self.chunk_span(chunk);
        // Per-chunk decryption avoids self_encryption's pointer-sized
        // whole-file range arithmetic and keeps the canonical KDF (original
        // chunk index and neighbouring hashes).
        let plaintext = super::chunk_decrypt::decrypt_chunk(
            chunk,
            encrypted,
            &self.src_hashes,
            CONTENT_CHUNK_KDF_LEVEL,
            // Each size was a usize in the DataMap this index came from.
            (span.end - span.start) as usize,
        )?;
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

    /// The size of the largest chunk.
    pub(crate) fn largest_chunk(&self) -> u64 {
        self.offsets
            .windows(2)
            .map(|bounds| bounds[1] - bounds[0])
            .max()
            .unwrap_or(0)
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

/// Fetch, verify and decrypt `chunks` in one deferred retry pass. Each
/// plaintext chunk goes to `sink` as soon as it is decrypted, in completion
/// order, so only records still in flight are held. A missing record is
/// retried alongside the rest instead of stalling chunks behind it.
pub(crate) async fn for_each_chunk<E, F, Fut, C, S, SF>(
    index: &FileIndex,
    chunks: Range<usize>,
    fetch: &F,
    cap: &C,
    sleep: &S,
    retryable: fn(&E) -> bool,
    mut sink: impl FnMut(usize, Bytes) -> Result<(), String>,
) -> Result<(), ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    let requests = chunks
        .map(|chunk| (chunk, index.dst_hashes[chunk].0))
        .collect();
    fetch_records_deferred(
        requests,
        fetch,
        cap,
        sleep,
        retryable,
        |chunk, encrypted| {
            let plaintext = index
                .decrypt(chunk, &encrypted)
                .map_err(ReadError::Invalid)?;
            sink(chunk, plaintext).map_err(ReadError::Invalid)
        },
    )
    .await
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

/// Whole-file BLAKE3 over chunks that complete out of order. Chunks are hashed
/// in file order. One that completes ahead of that order is read back from the
/// caller's output later, one chunk at a time, so no plaintext is retained
/// here and the caller decides when that work runs.
#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
pub(crate) struct OrderedHasher {
    hasher: blake3::Hasher,
    next: usize,
    completed: Vec<bool>,
}

#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
impl OrderedHasher {
    pub(crate) fn new(chunks: usize) -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            next: 0,
            completed: vec![false; chunks],
        }
    }

    /// Record `chunk` as complete, hashing it now if it is next in file order.
    pub(crate) fn complete(&mut self, chunk: usize, plaintext: &[u8]) {
        self.completed[chunk] = true;
        if chunk == self.next {
            self.hasher.update(plaintext);
            self.next += 1;
        }
    }

    /// The next chunk in file order, if it completed earlier and must now be
    /// read back to be hashed.
    pub(crate) fn pending_read_back(&self) -> Option<usize> {
        (self.completed.get(self.next) == Some(&true)).then_some(self.next)
    }

    /// Hash the read-back plaintext of [`Self::pending_read_back`].
    pub(crate) fn read_back(&mut self, chunk: usize, plaintext: &[u8]) -> Result<(), String> {
        if self.pending_read_back() != Some(chunk) {
            return Err(format!("chunk {chunk} is not waiting to be hashed"));
        }
        self.hasher.update(plaintext);
        self.next += 1;
        Ok(())
    }

    /// The hash of the whole file, once every chunk is complete.
    pub(crate) fn finish(self) -> Result<String, String> {
        if self.next != self.completed.len() {
            return Err(format!(
                "hashed {} of {} chunks",
                self.next,
                self.completed.len()
            ));
        }
        Ok(self.hasher.finalize().to_hex().to_string())
    }
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
    S: Fn(Duration) -> SF,
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
    S: Fn(Duration) -> SF,
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
    output.resize(length, 0);
    let chunks = index.chunks_overlapping(start..end);
    for_each_chunk(
        index,
        chunks,
        fetch,
        cap,
        sleep,
        retryable,
        |chunk, plaintext| {
            let span = index.chunk_span(chunk);
            let from = start.max(span.start);
            let to = end.min(span.end);
            output[(from - start) as usize..(to - start) as usize].copy_from_slice(
                &plaintext[(from - span.start) as usize..(to - span.start) as usize],
            );
            Ok(())
        },
    )
    .await?;
    Ok(Bytes::from(output))
}

/// Native file-download retry rounds: retry missing records together after the
/// first batch settles, immediately once, then after 15 and 45 seconds. The
/// caller supplies typed fatal errors and a runtime-specific timer.
#[cfg(any(feature = "native", test))]
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
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    let mut results = Vec::new();
    deferred_rounds(requests, fetch, cap, sleep, exhausted, |index, bytes| {
        results.push((index, bytes));
        Ok(())
    })
    .await?;
    results.sort_by_key(|(index, _)| *index);
    Ok(results)
}

/// The rounds behind [`deferred_batch`], handing each record to `on_record`
/// as it arrives instead of collecting them.
async fn deferred_rounds<K, E, F, Fut, C, S, SF>(
    requests: Vec<(usize, K)>,
    fetch: F,
    cap: C,
    sleep: S,
    exhausted: impl Fn(K) -> E,
    mut on_record: impl FnMut(usize, Bytes) -> Result<(), E>,
) -> Result<(), E>
where
    K: Copy,
    F: Fn(usize, K, usize) -> Fut,
    Fut: Future<Output = Result<(usize, Result<Bytes, K>), E>>,
    C: Fn() -> usize,
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    let mut remaining = requests;
    for (round, delay) in DEFERRED_ROUND_DELAYS_SECS.into_iter().enumerate() {
        if remaining.is_empty() {
            break;
        }
        if delay > 0 {
            sleep(Duration::from_secs(delay)).await;
        }
        let input = std::mem::take(&mut remaining);
        let pending =
            super::rolling_unordered(input, |(index, key)| fetch(index, key, round + 1), &cap);
        futures_util::pin_mut!(pending);
        while let Some(result) = pending.next().await {
            let (index, content) = result?;
            match content {
                Ok(bytes) => on_record(index, bytes)?,
                Err(key) => remaining.push((index, key)),
            }
        }
    }
    match remaining.first() {
        Some((_, key)) => Err(exhausted(*key)),
        None => Ok(()),
    }
}

/// Deferred retry rounds over records, preserving each record's last typed
/// fetch error. Records reach `on_record` unhashed: the fetcher verifies
/// addresses, and decryption authenticates every record.
async fn fetch_records_deferred<E, F, Fut, C, S, SF>(
    requests: Vec<RecordRequest>,
    fetch: &F,
    cap: &C,
    sleep: &S,
    retryable: fn(&E) -> bool,
    on_record: impl FnMut(usize, Bytes) -> Result<(), ReadError<E>>,
) -> Result<(), ReadError<E>>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: Future<Output = Result<Bytes, E>>,
    C: Fn() -> usize,
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    // Preserve the adapter's final typed fetch error across deferred rounds.
    let errors = std::sync::Mutex::new(HashMap::new());
    deferred_rounds(
        requests,
        |index, address, _| {
            let errors = &errors;
            async move {
                match fetch(address).await {
                    Ok(bytes) => Ok((index, Ok(bytes))),
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
        on_record,
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
        assert_eq!(
            resolve(&published, &fetch, &|| 3, usize::MAX)
                .await
                .unwrap(),
            native
        );
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
        // self_encryption finds no record matching the map's addresses.
        assert!(matches!(error, ReadError::Invalid(_)));
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
        let error = resolve(
            &child,
            &|_| async { Err::<Bytes, _>(23u8) },
            &|| 2,
            usize::MAX,
        )
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
        assert_eq!(index.largest_chunk(), size as u64);
        // Chunk sizes are not capped natively; browsers apply their own limit.
        let mut infos = infos;
        infos[1].src_size = 64 * size;
        let index = FileIndex::new(&DataMap::new(infos)).unwrap();
        assert_eq!(index.largest_chunk(), 64 * size as u64);
    }

    #[tokio::test]
    async fn nested_maps_resolve_within_limits_and_reject_expanding_records() {
        let content = Bytes::from(vec![17; 3 * self_encryption::MAX_CHUNK_SIZE + 1]);
        let (map, chunks) = self_encryption::encrypt(content).unwrap();
        assert!(map.is_child());
        let records: HashMap<_, _> = chunks
            .iter()
            .map(|c| (*blake3::hash(&c.content).as_bytes(), c.content.clone()))
            .collect();
        let fetch = |address| {
            let bytes = records[&address].clone();
            async move { Ok::<_, String>(bytes) }
        };
        let level_size: usize = map.infos().iter().map(|info| info.src_size).sum();
        let root = resolve(&map, &fetch, &|| 3, level_size).await.unwrap();
        assert!(!root.is_child());

        let error = resolve(&map, &fetch, &|| 3, level_size - 1)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::Invalid(message) if message.contains("exceeds")));

        // A record that decompresses past its declared size is cut off there.
        let mut infos = map.infos().to_vec();
        infos[0].src_size -= 1;
        let understated = DataMap::with_child(infos, map.child().unwrap());
        let error = resolve(&understated, &fetch, &|| 3, usize::MAX)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::Invalid(message) if message.contains("differs")));
    }

    #[tokio::test]
    async fn declared_nested_sizes_allocate_nothing_before_data_arrives() {
        const DECLARED_CHUNK: usize = 1 << 40;
        let infos = (0..MIN_FILE_CHUNKS)
            .map(|index| self_encryption::ChunkInfo {
                index,
                src_size: DECLARED_CHUNK,
                src_hash: XorName([index as u8; 32]),
                dst_hash: XorName([0xee - index as u8; 32]),
            })
            .collect();
        let map = DataMap::with_child(infos, 1);
        // Terabytes are declared, but the fetch fails before any data exists.
        let error = resolve(&map, &|_| async { Err::<Bytes, _>(7u8) }, &|| 3, usize::MAX)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::Fetch(7)));
    }

    #[tokio::test]
    async fn altered_content_records_fail_decryption() {
        let (_, map, records) = fixture();
        let fetch = |address| {
            let mut bytes = records[&address].to_vec();
            *bytes.last_mut().unwrap() ^= 1;
            async move { Ok::<_, String>(Bytes::from(bytes)) }
        };
        let error = read_range(&map, 0, 1, &fetch, &|| 1, &|_| async {}, |_| true)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::Invalid(message) if message.contains("decryption")));
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
        tokio::time::timeout(Duration::from_secs(5), pipeline)
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
        tokio::time::timeout(Duration::from_secs(5), pipeline)
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
        let sleep = |delay: Duration| {
            sleeps.borrow_mut().push(delay.as_secs());
            async {}
        };
        // One chunk on its own, as each pipeTo fetch is.
        let mut plaintext = None;
        for_each_chunk(
            &index,
            1..2,
            &fetch_after(4),
            &|| 1,
            &sleep,
            |_| true,
            |_, chunk| {
                plaintext = Some(chunk);
                Ok(())
            },
        )
        .await
        .unwrap();
        let span = index.chunk_span(1);
        assert_eq!(
            plaintext.unwrap(),
            content.slice(span.start as usize..span.end as usize)
        );
        assert_eq!(*sleeps.borrow(), vec![15, 45]);

        attempts.set(0);
        let error = for_each_chunk(
            &index,
            1..2,
            &fetch_after(5),
            &|| 1,
            &sleep,
            |_| true,
            |_, _| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadError::Fetch(message) if message == "missing"));
        assert_eq!(attempts.get(), DEFERRED_ROUND_DELAYS_SECS.len());

        attempts.set(0);
        let error = for_each_chunk(
            &index,
            1..2,
            &fetch_after(5),
            &|| 1,
            &sleep,
            |_| false,
            |_, _| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadError::Fetch(_)));
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test]
    async fn chunks_share_one_retry_pass_and_reach_the_sink_as_they_complete() {
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
        let mut received = Vec::new();
        for_each_chunk(
            &index,
            0..index.chunk_count(),
            &fetch,
            &|| MIN_FILE_CHUNKS,
            &|delay| {
                sleeps.lock().unwrap().push(delay.as_secs());
                async {}
            },
            |_| true,
            |chunk, plaintext| {
                received.push((chunk, plaintext));
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(most_in_flight.get(), MIN_FILE_CHUNKS);
        // Both missing records wait out the same 15-second round.
        assert_eq!(*sleeps.lock().unwrap(), vec![15]);
        // The available middle chunk is delivered without waiting for the others.
        assert_eq!(received[0].0, 1);
        let mut chunks: Vec<_> = received.iter().map(|(chunk, _)| *chunk).collect();
        chunks.sort_unstable();
        assert_eq!(chunks, (0..MIN_FILE_CHUNKS).collect::<Vec<_>>());
        for (chunk, plaintext) in received {
            let span = index.chunk_span(chunk);
            assert_eq!(
                plaintext,
                content.slice(span.start as usize..span.end as usize)
            );
        }
    }

    #[test]
    fn ordered_hash_matches_in_order_hash_for_any_completion_order() {
        let chunks: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 10 + i as usize]).collect();
        let expected = blake3::hash(&chunks.concat()).to_hex().to_string();
        // Only chunks that completed ahead of the frontier are read back.
        for (order, expected_read_backs) in [
            ([0, 1, 2, 3, 4], vec![]),
            ([4, 3, 2, 1, 0], vec![1, 2, 3, 4]),
            ([2, 0, 4, 1, 3], vec![2, 4]),
        ] {
            let mut hasher = OrderedHasher::new(chunks.len());
            let mut read_backs = Vec::new();
            let mut read_back = |hasher: &mut OrderedHasher| {
                let chunk = hasher.pending_read_back()?;
                read_backs.push(chunk);
                hasher.read_back(chunk, &chunks[chunk]).unwrap();
                Some(())
            };
            // As in a download: at most one read-back per completion, then
            // the rest once every chunk has arrived.
            for chunk in order {
                hasher.complete(chunk, &chunks[chunk]);
                read_back(&mut hasher);
            }
            while read_back(&mut hasher).is_some() {}
            assert_eq!(hasher.finish().unwrap(), expected);
            assert_eq!(read_backs, expected_read_backs);
        }
        let mut incomplete = OrderedHasher::new(chunks.len());
        incomplete.complete(1, &chunks[1]);
        assert_eq!(incomplete.pending_read_back(), None);
        assert!(incomplete.read_back(1, &chunks[1]).is_err());
        assert!(incomplete.finish().is_err());
    }

    #[tokio::test]
    async fn sink_errors_stop_a_chunk_pass() {
        let (_, map, records) = fixture();
        let index = FileIndex::new(&map).unwrap();
        let fetch = |address| {
            let bytes = records[&address].clone();
            async move { Ok::<_, String>(bytes) }
        };
        let mut delivered = 0;
        let error = for_each_chunk(
            &index,
            0..index.chunk_count(),
            &fetch,
            &|| 1,
            &|_| async {},
            |_| true,
            |_, _| {
                delivered += 1;
                Err("destination full".to_string())
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadError::Invalid(message) if message == "destination full"));
        assert_eq!(delivered, 1);
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
