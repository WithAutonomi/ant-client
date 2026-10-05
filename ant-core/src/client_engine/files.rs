//! Async data workflows over a verified record fetcher. No runtime, sockets,
//! filesystem or JS dependencies. Cryptography and recursive map semantics stay
//! in self_encryption, the same implementation used by the native client.
use bytes::Bytes;
use futures_util::StreamExt;
use self_encryption::{ChunkInfo, DataMap, EncryptedChunk, XorName};
use std::{cell::RefCell, collections::HashMap, future::Future, ops::Range, time::Duration};

/// self_encryption splits every encryptable file into at least three chunks.
pub(crate) const MIN_FILE_CHUNKS: usize = 3;
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
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    let root = resolve(map, fetch, cap).await?;
    let requests = root
        .infos()
        .iter()
        .enumerate()
        .map(|(index, info)| (index, info.dst_hash.0))
        .collect();
    let mut records = Vec::new();
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

/// Plaintext layout of a resolved root DataMap, in record order. Chunk sizes are
/// read from the native DataMap; checked sums avoid overflow on untrusted maps.
#[derive(Default)]
pub(crate) struct RecordLayout {
    /// Plaintext end offset and content address of each record.
    records: Vec<(usize, [u8; 32])>,
}

impl RecordLayout {
    pub(crate) fn new(map: &DataMap) -> Result<Self, String> {
        if map.is_child() {
            return Err("range reads require a resolved root DataMap".into());
        }
        let mut infos = map.infos().to_vec();
        infos.sort_by_key(|info| info.index);
        let mut end = 0usize;
        let mut records = Vec::with_capacity(infos.len());
        for (index, info) in infos.into_iter().enumerate() {
            if info.index != index {
                return Err("DataMap chunk indices must be contiguous".into());
            }
            end = end
                .checked_add(info.src_size)
                .ok_or("DataMap plaintext size overflow")?;
            records.push((end, info.dst_hash.0));
        }
        Ok(Self { records })
    }

    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    /// Plaintext size of the file.
    pub(crate) fn size(&self) -> usize {
        self.records.last().map_or(0, |(end, _)| *end)
    }

    pub(crate) fn address(&self, index: usize) -> [u8; 32] {
        self.records[index].1
    }

    /// Index of the record holding plaintext byte `offset`; `len()` at or past EOF.
    pub(crate) fn record_at(&self, offset: usize) -> usize {
        self.records.partition_point(|(end, _)| *end <= offset)
    }

    /// Records overlapping the plaintext range `[start, start + length)`.
    pub(crate) fn overlapping(&self, start: usize, length: usize) -> Range<usize> {
        let first = self.record_at(start);
        if length == 0 {
            return first..first;
        }
        let last = self.record_at(start.saturating_add(length - 1));
        first..(last + 1).min(self.len())
    }
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

/// Whole-file and windowed browser reads.
#[cfg(any(all(feature = "browser-wasm", target_arch = "wasm32"), test))]
impl FileIndex {
    pub(crate) fn chunk_count(&self) -> usize {
        self.dst_hashes.len()
    }

    /// End of a chunk window that starts at `first`, holds at most `max_bytes`
    /// of plaintext and stops at `limit`. A window always holds one chunk,
    /// however large it is.
    pub(crate) fn window_end(&self, first: usize, limit: usize, max_bytes: u64) -> usize {
        let budget_end = self.offsets[first].saturating_add(max_bytes);
        let fitting = self.offsets.partition_point(|offset| *offset <= budget_end) - 1;
        fitting.clamp(first + 1, limit)
    }

    /// Encrypted record addresses in chunk order.
    pub(crate) fn record_addresses(&self) -> &[XorName] {
        &self.dst_hashes
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

/// Whole-file BLAKE3 over chunks that complete out of order. Chunks are
/// hashed in file order; one that completes ahead of that frontier is read
/// back from the caller's output when the frontier reaches it, so no
/// plaintext is retained here.
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

    /// Record `chunk` as complete and hash every chunk the frontier can now
    /// pass. `read_back` returns the plaintext of an earlier-completed chunk.
    pub(crate) fn complete(
        &mut self,
        chunk: usize,
        plaintext: &[u8],
        mut read_back: impl FnMut(usize) -> Result<Vec<u8>, String>,
    ) -> Result<(), String> {
        self.completed[chunk] = true;
        while self.completed.get(self.next) == Some(&true) {
            if self.next == chunk {
                self.hasher.update(plaintext);
            } else {
                self.hasher.update(&read_back(self.next)?);
            }
            self.next += 1;
        }
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
    fn layout_maps_plaintext_ranges_to_records() {
        let infos = [10, 10, 10]
            .into_iter()
            .enumerate()
            .map(|(index, src_size)| self_encryption::ChunkInfo {
                index,
                dst_hash: XorName([index as u8 + 1; 32]),
                src_hash: XorName([0; 32]),
                src_size,
            })
            .collect();
        let layout = RecordLayout::new(&DataMap::new(infos)).unwrap();
        assert_eq!(layout.size(), 30);
        assert_eq!(layout.overlapping(0, 10), 0..1);
        assert_eq!(layout.overlapping(9, 2), 0..2);
        assert_eq!(layout.overlapping(10, 25), 1..3);
        assert_eq!(layout.overlapping(25, 0), 2..2);
        assert_eq!(layout.overlapping(30, 10), 3..3);
        assert_eq!(layout.address(1), [2; 32]);
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
            index.record_addresses(),
            infos.iter().map(|info| info.dst_hash).collect::<Vec<_>>()
        );
    }

    #[test]
    fn windows_hold_whole_chunks_within_their_byte_budget() {
        let size = self_encryption::MAX_CHUNK_SIZE;
        let infos = (0..10)
            .map(|index| self_encryption::ChunkInfo {
                index,
                src_size: size,
                src_hash: XorName([1; 32]),
                dst_hash: XorName([2; 32]),
            })
            .collect();
        let index = FileIndex::new(&DataMap::new(infos)).unwrap();
        let chunk = size as u64;
        assert_eq!(index.window_end(0, 10, 4 * chunk), 4);
        assert_eq!(index.window_end(0, 10, 4 * chunk + 1), 4);
        assert_eq!(index.window_end(2, 10, 4 * chunk - 1), 5);
        // A window always makes progress, even past its budget.
        assert_eq!(index.window_end(3, 10, 1), 4);
        assert_eq!(index.window_end(8, 10, 4 * chunk), 10);
        assert_eq!(index.window_end(0, 3, u64::MAX), 3);
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
            index.record_addresses()[0].0,
            index.record_addresses()[MIN_FILE_CHUNKS - 1].0,
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
            for chunk in order {
                hasher
                    .complete(chunk, &chunks[chunk], |earlier| {
                        read_backs.push(earlier);
                        Ok(chunks[earlier].clone())
                    })
                    .unwrap();
            }
            assert_eq!(hasher.finish().unwrap(), expected);
            assert_eq!(read_backs, expected_read_backs);
        }
        let mut incomplete = OrderedHasher::new(chunks.len());
        incomplete
            .complete(1, &chunks[1], |_| unreachable!("chunk 0 is missing"))
            .unwrap();
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
