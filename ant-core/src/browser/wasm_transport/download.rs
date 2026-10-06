//! Bounded plaintext reads with 64-bit positions and caller-owned destinations.
use super::*;
use crate::browser::manifest::MAX_SAFE_JS_INTEGER;
use crate::client_engine::files::FileIndex;
use bytes::Bytes;
use futures_channel::mpsc;
use futures_util::{stream, SinkExt, StreamExt};
use std::ops::Range;

/// Plaintext a stream holds at once: chunks in flight, fetched, handed to the
/// writer or being written. Fetching continues during writes up to this bound.
const STREAM_BUFFER_BYTES: u64 = 32 * 1024 * 1024;
/// Chunks a stream holds outside its fetch buffer: one handed to the writer
/// and one being written.
const CHUNKS_OUTSIDE_FETCH_BUFFER: u64 = 2;
const STREAM_HINT: &str =
    "use openPublicFile/openPrivateFile and reader.pipeTo(writable) to stream to disk";
const CLOSED_READER: &str = "browser file reader is closed";
const ABORTED: &str = "download aborted";
const ABORT_EVENT: &str = "abort";
const POSITIVE_CONCURRENCY: &str = "download concurrency must be a positive integer";

/// A nonnegative integer that JavaScript represents exactly.
fn safe_integer(value: f64, label: &str) -> Result<u64, String> {
    if !value.is_finite()
        || value < 0.0
        || value.fract() != 0.0
        || value > MAX_SAFE_JS_INTEGER as f64
    {
        return Err(format!("{label} must be a nonnegative safe integer"));
    }
    Ok(value as u64)
}

fn js_string(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

fn method(object: &JsValue, name: &str) -> Result<js_sys::Function, JsValue> {
    js_sys::Reflect::get(object, &JsValue::from_str(name))?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| JsValue::from_str(&format!("destination has no {name} method")))
}

/// An optional options object; `undefined` and `null` select the defaults.
fn options_from_js<T: Default + serde::de::DeserializeOwned>(
    value: Option<JsValue>,
) -> Result<T, String> {
    match value {
        Some(value) if !value.is_undefined() && !value.is_null() => {
            serde_wasm_bindgen::from_value(value).map_err(|error| error.to_string())
        }
        _ => Ok(T::default()),
    }
}

/// Release the writer even if the Rust future is dropped before it completes.
struct Writer(JsValue);
impl Drop for Writer {
    fn drop(&mut self) {
        if let Ok(release) = method(&self.0, "releaseLock") {
            let _ = release.call0(&self.0);
        }
    }
}

/// `pipeTo` options, modelled on `ReadableStream.pipeTo`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PipeOptions {
    /// First byte to write.
    start: Option<f64>,
    /// Exclusive end of the written range; the file size by default.
    end: Option<f64>,
    /// AbortSignal that cancels this operation.
    #[serde(default, with = "serde_wasm_bindgen::preserve")]
    signal: JsValue,
    /// Called with `(bytesWritten, totalBytes)` after each write.
    #[serde(default, with = "serde_wasm_bindgen::preserve")]
    on_progress: JsValue,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PipeResult {
    bytes_written: u64,
    hash: String,
}

/// Complete-download options: `{ maxMemoryBytes?, signal? }`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadOptions {
    /// Largest output buffer to allocate; larger files fail before transfer.
    max_memory_bytes: Option<f64>,
    /// AbortSignal that cancels the download.
    #[serde(default, with = "serde_wasm_bindgen::preserve")]
    signal: JsValue,
}

/// Validated complete-download arguments.
pub(super) struct DownloadSettings {
    concurrency: usize,
    memory_budget: Option<u64>,
    pub(super) progress: ProgressReporter,
    /// AbortSignal, or `undefined` when the caller passed none.
    pub(super) signal: JsValue,
}

impl DownloadSettings {
    pub(super) fn new(
        concurrency: Option<usize>,
        on_progress: Option<js_sys::Function>,
        options: Option<JsValue>,
    ) -> Result<Self, String> {
        let concurrency = match concurrency {
            None => usize::MAX,
            Some(0) => return Err(POSITIVE_CONCURRENCY.into()),
            Some(concurrency) => concurrency,
        };
        let options: DownloadOptions = options_from_js(options)?;
        Ok(Self {
            concurrency,
            memory_budget: options
                .max_memory_bytes
                .map(|value| safe_integer(value, "memory budget"))
                .transpose()?,
            progress: ProgressReporter::from_js(on_progress),
            signal: options.signal,
        })
    }
}

/// An AbortSignal's `abort` event. Detaches its listener when dropped.
struct AbortListener {
    signal: JsValue,
    callback: Closure<dyn FnMut()>,
    fired: oneshot::Receiver<()>,
}

impl AbortListener {
    fn new(signal: JsValue) -> Result<Self, JsValue> {
        let (sender, fired) = oneshot::channel();
        let mut sender = Some(sender);
        let callback = Closure::<dyn FnMut()>::new(move || {
            if let Some(sender) = sender.take() {
                let _ = sender.send(());
            }
        });
        method(&signal, "addEventListener")
            .map_err(|_| JsValue::from_str("signal must be an AbortSignal"))?
            .call2(
                &signal,
                &JsValue::from_str(ABORT_EVENT),
                callback.as_ref().unchecked_ref(),
            )?;
        Ok(Self {
            signal,
            callback,
            fired,
        })
    }

    /// The signal's abort reason, once it has aborted.
    fn reason(&self) -> Option<JsValue> {
        let aborted = js_sys::Reflect::get(&self.signal, &JsValue::from_str("aborted"))
            .is_ok_and(|aborted| aborted.is_truthy());
        aborted.then(|| {
            js_sys::Reflect::get(&self.signal, &JsValue::from_str("reason"))
                .ok()
                .filter(|reason| !reason.is_undefined())
                .unwrap_or_else(|| JsValue::from_str(ABORTED))
        })
    }
}

impl Drop for AbortListener {
    fn drop(&mut self) {
        if let Ok(remove) = method(&self.signal, "removeEventListener") {
            let _ = remove.call2(
                &self.signal,
                &JsValue::from_str(ABORT_EVENT),
                self.callback.as_ref().unchecked_ref(),
            );
        }
    }
}

/// Stops an operation when its optional AbortSignal fires.
pub(super) struct Cancellation(Option<AbortListener>);

impl Cancellation {
    pub(super) fn new(signal: JsValue) -> Result<Self, JsValue> {
        if signal.is_undefined() || signal.is_null() {
            return Ok(Self(None));
        }
        AbortListener::new(signal).map(|listener| Self(Some(listener)))
    }

    /// Fail with the abort reason once the signal has aborted.
    fn check(&self) -> Result<(), JsValue> {
        match self.0.as_ref().and_then(AbortListener::reason) {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// Run `operation` unless the signal aborts first. The signal is polled
    /// before the operation, so an aborted operation is dropped, stopping its
    /// fetches and retry waits, without running again.
    pub(super) async fn run<T>(
        &mut self,
        operation: impl Future<Output = Result<T, JsValue>>,
    ) -> Result<T, JsValue> {
        self.check()?;
        let Some(abort) = self.0.as_mut() else {
            return operation.await;
        };
        let finished = {
            futures_util::pin_mut!(operation);
            match select(&mut abort.fired, operation).await {
                Either::Left(_) => None,
                Either::Right((result, _)) => Some(result),
            }
        };
        finished.unwrap_or_else(|| {
            Err(self
                .check()
                .err()
                .unwrap_or_else(|| JsValue::from_str(ABORTED)))
        })
    }
}

/// Fetch and decrypt `chunks` and hand each to `write` in file order. Fetching
/// continues while a write is pending, and the chunks in flight, fetched,
/// handed over and being written together hold at most `STREAM_BUFFER_BYTES`
/// of plaintext. Each missing record is retried on its own schedule, so retry
/// waits overlap with the other fetches.
async fn stream_chunks<W, WF>(
    shared: &crate::data::Client,
    index: &FileIndex,
    chunks: Range<usize>,
    concurrency: usize,
    mut write: W,
) -> Result<(), JsValue>
where
    W: FnMut(usize, Bytes) -> WF,
    WF: Future<Output = Result<(), JsValue>>,
{
    let buffered = (STREAM_BUFFER_BYTES / index.largest_chunk().max(1))
        .saturating_sub(CHUNKS_OUTSIDE_FETCH_BUFFER)
        .max(1);
    let buffered = usize::try_from(buffered)
        .unwrap_or(usize::MAX)
        .min(concurrency);
    // A zero-capacity channel holds one chunk per sender.
    let (mut handed, mut received) = mpsc::channel(0);
    let fetch = async move {
        let mut fetched = stream::iter(chunks)
            .map(|chunk| async move {
                let plaintext = shared.data_download_indexed_chunk(index, chunk).await;
                (chunk, plaintext)
            })
            .buffered(buffered);
        while let Some(fetched) = fetched.next().await {
            if handed.send(fetched).await.is_err() {
                return;
            }
        }
    };
    let deliver = async {
        while let Some((chunk, plaintext)) = received.next().await {
            write(chunk, plaintext.map_err(js_string)?).await?;
        }
        Ok(())
    };
    futures_util::pin_mut!(fetch, deliver);
    match select(fetch, deliver).await {
        // Every chunk has been handed over; finish writing them.
        Either::Left(((), deliver)) => deliver.await,
        Either::Right((delivered, _)) => delivered,
    }
}

/// Download a whole file into one JavaScript `Uint8Array`, writing each chunk
/// straight into it from WASM memory. A caller can impose its own memory
/// budget; allocation failures recommend the disk-backed path without
/// downloading the file first.
pub(super) async fn collect(
    shared: &crate::data::Client,
    index: &FileIndex,
    settings: &DownloadSettings,
    cancel: &mut Cancellation,
) -> Result<(Uint8Array, String), JsValue> {
    let size = index.size();
    if settings.memory_budget.is_some_and(|limit| size > limit) {
        return Err(js_string(format_args!(
            "file exceeds the download memory budget; {STREAM_HINT}"
        )));
    }
    let constructor = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("Uint8Array"))?
        .dyn_into::<js_sys::Function>()?;
    let args = Array::new();
    args.push(&JsValue::from_f64(size as f64));
    let output = js_sys::Reflect::construct(&constructor, &args)
        .map_err(|error| {
            js_string(format_args!(
                "cannot allocate download buffer: {}; {STREAM_HINT}",
                js_error_message(error)
            ))
        })?
        .unchecked_into::<Uint8Array>();
    // Reflect calls preserve Number offsets above u32::MAX and catch JS
    // allocation exceptions instead of turning them into WASM traps.
    let subarray = method(output.as_ref(), "subarray")?;
    let total = index.chunk_count();
    let mut written = 0usize;
    let mut hasher = blake3::Hasher::new();
    settings
        .progress
        .report(&format!("Downloaded chunk {written}/{total}"));
    cancel
        .run(stream_chunks(
            shared,
            index,
            0..total,
            settings.concurrency,
            |chunk, plaintext| {
                let span = index.chunk_span(chunk);
                let placed = subarray
                    .call2(
                        output.as_ref(),
                        &JsValue::from_f64(span.start as f64),
                        &JsValue::from_f64(span.end as f64),
                    )
                    .map(|view| {
                        view.unchecked_into::<Uint8Array>().copy_from(&plaintext);
                        hasher.update(&plaintext);
                        written += 1;
                        settings
                            .progress
                            .report(&format!("Downloaded chunk {written}/{total}"));
                    });
                std::future::ready(placed)
            },
        ))
        .await?;
    Ok((output, hasher.finalize().to_hex().to_string()))
}

/// The descriptor of a completely read file, with its chunk list and hash.
pub(super) fn completed_descriptor(
    mut file: PublicFileDescriptor,
    index: &FileIndex,
    hash: String,
) -> PublicFileDescriptor {
    file.chunks = index
        .chunk_infos()
        .map(|info| super::super::browser_chunk_info(&info))
        .collect();
    file.blake3 = hash;
    file
}

/// A validated `pipeTo` call.
struct PipePlan {
    range: Range<u64>,
    progress: Option<js_sys::Function>,
    cancel: Cancellation,
}

/// Random-access file reader. Content memory is bounded independently of file
/// size. The resolved chunk map and seek index remain resident in memory.
#[wasm_bindgen(js_name = BrowserFileReader)]
pub struct BrowserFileReader {
    shared: Rc<crate::data::Client>,
    /// Descriptor without its chunk list; the index holds the chunk metadata.
    file: PublicFileDescriptor,
    index: Rc<FileIndex>,
    /// Fetches ahead of sequential range reads; see `read_ahead`.
    pub(super) read_ahead: Rc<read_ahead::ReadAhead>,
    closed: Cell<bool>,
}

#[wasm_bindgen(js_class = BrowserFileReader)]
impl BrowserFileReader {
    /// Exact plaintext byte count as a JavaScript safe integer.
    #[wasm_bindgen(getter)]
    pub fn size(&self) -> f64 {
        self.file.size as f64
    }

    #[wasm_bindgen(getter, js_name = contentType)]
    pub fn content_type(&self) -> String {
        self.file.content_type.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn name(&self) -> String {
        self.file.name.clone()
    }

    /// Read up to 4 MiB. Positions above 4 GiB are supported without truncation.
    /// Fractions, negatives, and integers not exactly representable in JS fail.
    #[wasm_bindgen(js_name = readRange)]
    pub async fn read_range(&self, start: f64, length: f64) -> Result<Uint8Array, JsValue> {
        if self.closed.get() {
            return Err(JsValue::from_str(CLOSED_READER));
        }
        let start = safe_integer(start, "range start").map_err(js_string)?;
        let length = safe_integer(length, "range length").map_err(js_string)?;
        if length > MAX_BROWSER_RANGE_BYTES as u64 {
            return Err(JsValue::from_str(&format!(
                "browser range reads are limited to {MAX_BROWSER_RANGE_BYTES} bytes"
            )));
        }
        let lease = self.read_ahead.begin_read(start, length as usize);
        let bytes = self
            .shared
            .data_download_indexed_range(&self.index, start, length as usize, |address| {
                lease.record(address)
            })
            .await;
        if matches!(bytes, Err(crate::data::Error::Encryption(_))) {
            // The records do not decrypt as the DataMap declares, so reading
            // further ahead would only spend bandwidth and memory.
            self.read_ahead.close();
        }
        let bytes = bytes.map_err(js_string)?;
        Ok(Uint8Array::from(bytes.as_ref()))
    }

    /// Write the file to a WritableStream (including a file-system writable).
    /// `options` is `{ start?, end?, signal?, onProgress? }`: an optional
    /// half-open byte range, an AbortSignal that cancels the operation, and a
    /// `(bytesWritten, totalBytes)` callback run after each write.
    ///
    /// Chunks are fetched concurrently and written in order, at most 4 MiB per
    /// write, each awaited before the next; fetching continues during writes
    /// until 32 MiB of plaintext is held. Invalid calls reject without touching
    /// the destination. Otherwise the destination is closed on success and
    /// aborted on failure, and the writer lock is always released. Returns the
    /// BLAKE3 and byte count of the written range.
    #[wasm_bindgen(js_name = pipeTo)]
    pub async fn pipe_to(
        &self,
        writable: JsValue,
        options: Option<JsValue>,
    ) -> Result<JsValue, JsValue> {
        let plan = self.plan_pipe(options)?;
        let writer = Writer(method(&writable, "getWriter")?.call0(&writable)?);
        let result = self.pipe(&writer.0, plan).await;
        if let Err(error) = &result {
            if let Ok(abort) = method(&writer.0, "abort") {
                if let Ok(promise) = abort.call1(&writer.0, error) {
                    // Not awaited: an abort settles only once a stalled write
                    // does, and the rejection must not wait for that.
                    let settled = JsFuture::from(Promise::resolve(&promise));
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = settled.await;
                    });
                }
            }
        }
        result
    }

    /// Close the reader and release the records its read-ahead holds. Later
    /// calls fail; use an AbortSignal to cancel a pipeTo in progress. Records
    /// in the client's shared cache stay; that cache is bounded and may serve
    /// other readers.
    pub fn close(&self) {
        self.closed.set(true);
        self.read_ahead.close();
    }
}

impl Drop for BrowserFileReader {
    fn drop(&mut self) {
        self.read_ahead.close();
    }
}

impl BrowserFileReader {
    /// `streaming` treats every read as part of a sequential stream for
    /// read-ahead, as media playback needs.
    pub(super) fn new(
        shared: Rc<crate::data::Client>,
        resolved: ResolvedBrowserFile,
        read_ahead: &Rc<read_ahead::ReadAheadPool>,
        streaming: bool,
    ) -> Self {
        let index = Rc::new(resolved.index);
        Self {
            shared,
            file: resolved.file,
            read_ahead: read_ahead::ReadAhead::new(read_ahead, Rc::clone(&index), streaming),
            index,
            closed: Cell::new(false),
        }
    }

    /// Validate a `pipeTo` call before it touches the destination.
    fn plan_pipe(&self, options: Option<JsValue>) -> Result<PipePlan, JsValue> {
        if self.closed.get() {
            return Err(JsValue::from_str(CLOSED_READER));
        }
        let options: PipeOptions = options_from_js(options).map_err(js_string)?;
        let progress = if options.on_progress.is_undefined() || options.on_progress.is_null() {
            None
        } else {
            Some(
                options
                    .on_progress
                    .dyn_into::<js_sys::Function>()
                    .map_err(|_| JsValue::from_str("onProgress must be a function"))?,
            )
        };
        let start =
            safe_integer(options.start.unwrap_or(0.0), "stream start").map_err(js_string)?;
        let end = options
            .end
            .map(|end| safe_integer(end, "stream end"))
            .transpose()
            .map_err(js_string)?
            .unwrap_or(self.file.size);
        if start > end || end > self.file.size {
            return Err(JsValue::from_str("stream range is outside the file"));
        }
        Ok(PipePlan {
            range: start..end,
            progress,
            cancel: Cancellation::new(options.signal)?,
        })
    }

    async fn pipe(&self, writer: &JsValue, plan: PipePlan) -> Result<JsValue, JsValue> {
        let PipePlan {
            range,
            progress,
            mut cancel,
        } = plan;
        let write = method(writer, "write")?;
        let (write, progress) = (&write, &progress);
        let total = range.end - range.start;
        let mut hasher = blake3::Hasher::new();
        cancel
            .run(async {
                stream_chunks(
                    &self.shared,
                    &self.index,
                    self.index.chunks_overlapping(range.clone()),
                    usize::MAX,
                    |chunk, plaintext| {
                        let span = self.index.chunk_span(chunk);
                        let from = range.start.max(span.start);
                        let to = range.end.min(span.end);
                        let plaintext = plaintext
                            .slice((from - span.start) as usize..(to - span.start) as usize);
                        hasher.update(&plaintext);
                        let mut written = from - range.start;
                        async move {
                            for piece in plaintext.chunks(MAX_BROWSER_RANGE_BYTES) {
                                JsFuture::from(Promise::resolve(
                                    &write.call1(writer, &Uint8Array::from(piece))?,
                                ))
                                .await?;
                                written += piece.len() as u64;
                                if let Some(callback) = progress {
                                    callback.call2(
                                        &JsValue::NULL,
                                        &JsValue::from_f64(written as f64),
                                        &JsValue::from_f64(total as f64),
                                    )?;
                                }
                            }
                            Ok(())
                        }
                    },
                )
                .await?;
                JsFuture::from(Promise::resolve(&method(writer, "close")?.call0(writer)?))
                    .await
                    .map(drop)
            })
            .await?;
        serde_wasm_bindgen::to_value(&PipeResult {
            bytes_written: total,
            hash: hasher.finalize().to_hex().to_string(),
        })
        .map_err(js_string)
    }
}
