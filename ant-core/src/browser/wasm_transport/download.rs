//! Bounded plaintext reads with 64-bit positions and caller-owned destinations.
use super::*;
use crate::client_engine::files::ordered_pipeline;
use bytes::Bytes;
use self_encryption::DataMapIndex;
use std::ops::Range;
use web_sys::{AbortSignal, WritableStream, WritableStreamDefaultWriter};

/// Plaintext a stream holds at once: chunks in flight, fetched or being
/// written. Fetching continues during writes up to this bound.
const STREAM_BUFFER_BYTES: u64 = 32 * 1024 * 1024;
/// Plaintext a complete download copies back into WASM at a time to hash it.
const HASH_PIECE_BYTES: u64 = 4 * 1024 * 1024;
const STREAM_HINT: &str =
    "use openPublicFile/openPrivateFile and reader.pipeTo(writable) to stream to disk";
const CLOSED_READER: &str = "browser file reader is closed";
const ABORTED: &str = "download aborted";
const ABORT_EVENT: &str = "abort";
const POSITIVE_CONCURRENCY: &str = "download concurrency must be a positive integer";
const NOT_ABORT_SIGNAL: &str = "signal must be an AbortSignal";
const NOT_WRITABLE_STREAM: &str = "destination must be a WritableStream";
/// The output's constructor and view method, called through `Reflect` because
/// the `js_sys` bindings take `u32` lengths and offsets.
const UINT8_ARRAY: &str = "Uint8Array";
const SUBARRAY: &str = "subarray";

/// A nonnegative integer that JavaScript represents exactly.
fn safe_integer(value: f64, label: &str) -> Result<u64, String> {
    if value < 0.0 || !js_sys::Number::is_safe_integer(&JsValue::from_f64(value)) {
        return Err(format!("{label} must be a nonnegative safe integer"));
    }
    Ok(value as u64)
}

fn js_string(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

/// Progress of a complete download, which startup probes parse.
fn chunk_progress(written: usize, total: usize) -> String {
    format!("Downloaded chunk {written}/{total}")
}

/// An optional options object; `undefined` and `null` select the defaults.
pub(super) fn options_from_js<T: Default + serde::de::DeserializeOwned>(
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
struct Writer(WritableStreamDefaultWriter);
impl Drop for Writer {
    fn drop(&mut self) {
        self.0.release_lock();
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
    signal: AbortSignal,
    callback: Closure<dyn FnMut()>,
    fired: oneshot::Receiver<()>,
}

impl AbortListener {
    fn new(signal: JsValue) -> Result<Self, JsValue> {
        let signal = signal
            .dyn_into::<AbortSignal>()
            .map_err(|_| JsValue::from_str(NOT_ABORT_SIGNAL))?;
        let (sender, fired) = oneshot::channel();
        let mut sender = Some(sender);
        let callback = Closure::<dyn FnMut()>::new(move || {
            if let Some(sender) = sender.take() {
                let _ = sender.send(());
            }
        });
        signal.add_event_listener_with_callback(ABORT_EVENT, callback.as_ref().unchecked_ref())?;
        Ok(Self {
            signal,
            callback,
            fired,
        })
    }

    /// The signal's abort reason, once it has aborted.
    fn reason(&self) -> Option<JsValue> {
        self.signal.aborted().then(|| {
            let reason = self.signal.reason();
            if reason.is_undefined() {
                JsValue::from_str(ABORTED)
            } else {
                reason
            }
        })
    }
}

impl Drop for AbortListener {
    fn drop(&mut self) {
        let _ = self.signal.remove_event_listener_with_callback(
            ABORT_EVENT,
            self.callback.as_ref().unchecked_ref(),
        );
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
/// continues while a write is pending, within the client's adaptive fetch cap,
/// until the chunks in flight, fetched and being written hold
/// `STREAM_BUFFER_BYTES` of plaintext. Each missing record is retried on its
/// own schedule, so retry waits overlap with the other fetches; a record
/// waiting to retry still holds its place under the cap.
async fn stream_chunks<W, WF>(
    shared: &crate::data::Client,
    index: &DataMapIndex,
    chunks: Range<usize>,
    write: W,
) -> Result<(), JsValue>
where
    W: FnMut(usize, Bytes) -> WF,
    WF: Future<Output = Result<(), JsValue>>,
{
    ordered_pipeline(
        chunks,
        |chunk| {
            let span = index.chunk_range(chunk);
            span.end - span.start
        },
        STREAM_BUFFER_BYTES,
        &|| shared.controller().fetch.current(),
        |chunk| async move {
            shared
                .data_download_indexed_chunk(index, chunk)
                .await
                .map_err(js_string)
        },
        write,
    )
    .await
}

/// Download a whole file into one JavaScript `Uint8Array`. Chunks are fetched
/// as a complete download fetches them, in any order, and each is copied
/// straight into place from WASM memory; the file is hashed afterwards. A
/// caller can impose its own memory budget; allocation failures recommend the
/// disk-backed path without downloading the file first.
pub(super) async fn collect(
    shared: &crate::data::Client,
    index: &DataMapIndex,
    settings: &DownloadSettings,
    cancel: &mut Cancellation,
) -> Result<(Uint8Array, String), JsValue> {
    cancel.check()?;
    let size = index.file_size();
    if settings.memory_budget.is_some_and(|limit| size > limit) {
        return Err(js_string(format_args!(
            "file exceeds the download memory budget; {STREAM_HINT}"
        )));
    }
    // Reflect calls preserve Number offsets above u32::MAX and catch JS
    // allocation exceptions instead of turning them into WASM traps.
    let constructor = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str(UINT8_ARRAY))?
        .dyn_into::<js_sys::Function>()?;
    let output =
        js_sys::Reflect::construct(&constructor, &Array::of1(&JsValue::from_f64(size as f64)))
            .map_err(|error| {
                js_string(format_args!(
                    "cannot allocate download buffer: {}; {STREAM_HINT}",
                    js_error_message(error)
                ))
            })?
            .unchecked_into::<Uint8Array>();
    let subarray = js_sys::Reflect::get(&output, &JsValue::from_str(SUBARRAY))?
        .dyn_into::<js_sys::Function>()?;
    let view = |span: Range<u64>| {
        subarray
            .call2(
                &output,
                &JsValue::from_f64(span.start as f64),
                &JsValue::from_f64(span.end as f64),
            )
            .map(|view| view.unchecked_into::<Uint8Array>())
    };
    let total = index.infos().len();
    let written = Cell::new(0);
    // The output's own error, which the caller receives instead of a message.
    let output_error = RefCell::new(None);
    settings.progress.report(&chunk_progress(0, total));
    cancel
        .run(async {
            shared
                .data_download_indexed_chunks(index, settings.concurrency, |chunk, plaintext| {
                    let placed = view(index.chunk_range(chunk)).map_err(|error| {
                        output_error.replace(Some(error.clone()));
                        js_error_message(error)
                    })?;
                    placed.copy_from(&plaintext);
                    written.set(written.get() + 1);
                    settings
                        .progress
                        .report(&chunk_progress(written.get(), total));
                    Ok(())
                })
                .await
                .map_err(|error| output_error.take().unwrap_or_else(|| js_string(error)))
        })
        .await?;
    let hash = output_hash(size, view)?;
    Ok((output, hash))
}

/// BLAKE3 of a `size`-byte output, copied back into WASM through `view` in
/// `HASH_PIECE_BYTES` pieces.
fn output_hash(
    size: u64,
    view: impl Fn(Range<u64>) -> Result<Uint8Array, JsValue>,
) -> Result<String, JsValue> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; size.min(HASH_PIECE_BYTES) as usize];
    let mut start = 0;
    while start < size {
        let end = size.min(start + HASH_PIECE_BYTES);
        let piece = &mut buffer[..(end - start) as usize];
        view(start..end)?.copy_to(piece);
        hasher.update(piece);
        start = end;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// The descriptor of a completely read file, with its chunk list and hash.
pub(super) fn completed_descriptor(
    mut file: PublicFileDescriptor,
    index: &DataMapIndex,
    hash: String,
) -> PublicFileDescriptor {
    file.chunks = index
        .infos()
        .iter()
        .map(super::super::browser_chunk_info)
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
    index: Rc<DataMapIndex>,
    /// Fetches ahead of sequential range reads; see `read_ahead`.
    pub(super) read_ahead: Rc<read_ahead::ReadAhead>,
    closed: Cell<bool>,
    /// The file's swarm room, joined when the file was opened, and again by
    /// a read if this client has left it since.
    room: Option<(RoomKey, [u8; 32])>,
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
        self.join_room();
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
    /// aborted on failure, and the writer lock is always released. An abort
    /// while the destination is closing still rejects, but the destination may
    /// already be committed. Returns the BLAKE3 and byte count of the written
    /// range.
    #[wasm_bindgen(js_name = pipeTo)]
    pub async fn pipe_to(
        &self,
        writable: JsValue,
        options: Option<JsValue>,
    ) -> Result<JsValue, JsValue> {
        let plan = self.plan_pipe(options)?;
        let writable = writable
            .dyn_into::<WritableStream>()
            .map_err(|_| JsValue::from_str(NOT_WRITABLE_STREAM))?;
        let writer = Writer(writable.get_writer()?);
        let result = self.pipe(&writer.0, plan).await;
        if let Err(error) = &result {
            // Not awaited: an abort settles only once a stalled write does,
            // and the rejection must not wait for that.
            let settled = JsFuture::from(writer.0.abort_with_reason(error));
            wasm_bindgen_futures::spawn_local(async move {
                let _ = settled.await;
            });
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
            room: resolved.room,
        }
    }

    /// Joins the file's room again as a read starts, if reading other files
    /// has left it since the file was opened, which joined it.
    fn join_room(&self) {
        if let Some((room, secret)) = self.room {
            self.shared
                .join_swarm_room(room, secret, swarm_records(&self.index));
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

    async fn pipe(
        &self,
        writer: &WritableStreamDefaultWriter,
        plan: PipePlan,
    ) -> Result<JsValue, JsValue> {
        let PipePlan {
            range,
            progress,
            mut cancel,
        } = plan;
        let progress = &progress;
        let total = range.end - range.start;
        let mut hasher = blake3::Hasher::new();
        cancel
            .run(async {
                self.join_room();
                stream_chunks(
                    &self.shared,
                    &self.index,
                    self.index.chunks_overlapping(range.clone()),
                    |chunk, plaintext| {
                        let span = self.index.chunk_range(chunk);
                        let from = range.start.max(span.start);
                        let to = range.end.min(span.end);
                        let plaintext = plaintext
                            .slice((from - span.start) as usize..(to - span.start) as usize);
                        hasher.update(&plaintext);
                        let mut written = from - range.start;
                        async move {
                            for piece in plaintext.chunks(MAX_BROWSER_RANGE_BYTES) {
                                JsFuture::from(writer.write_with_chunk(&Uint8Array::from(piece)))
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
                JsFuture::from(writer.close()).await.map(drop)
            })
            .await?;
        serde_wasm_bindgen::to_value(&PipeResult {
            bytes_written: total,
            hash: hasher.finalize().to_hex().to_string(),
        })
        .map_err(js_string)
    }
}
