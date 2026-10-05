//! Bounded plaintext reads with 64-bit positions and caller-owned destinations.
use super::*;
use crate::browser::manifest::MAX_SAFE_JS_INTEGER;
use crate::client_engine::files::{ordered_pipeline, FileIndex, OrderedHasher};
use bytes::Bytes;
use std::ops::Range;
use web_time::Instant;

/// Plaintext a `pipeTo` holds at once: chunks in flight or fetched but not yet
/// written. Fetching continues while writes are pending until this is reached.
const PIPE_BUFFER_BYTES: u64 = 32 * 1024 * 1024;
const STREAM_HINT: &str =
    "use openPublicFile/openPrivateFile and reader.pipeTo(writable) to stream to disk";
const CLOSED_READER: &str = "browser file reader is closed";
const ABORTED: &str = "download aborted";
const ABORT_EVENT: &str = "abort";
const POSITIVE_CONCURRENCY: &str = "download concurrency must be a positive integer";
const PIPE_OPTION_NAMES: [&str; 4] = ["start", "end", "signal", "onProgress"];
const DOWNLOAD_OPTION_NAMES: [&str; 4] = ["concurrency", "maxMemoryBytes", "onProgress", "signal"];
/// JavaScript's ToUint32 wraps numbers modulo 2^32.
const UINT32_RANGE: f64 = u32::MAX as f64 + 1.0;
/// Longest stretch of read-back hashing between event-loop turns. Yielding
/// after every chunk would pay the browser's nested-timer clamp each time.
const HASH_SLICE: Duration = Duration::from_millis(20);

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

/// `value >>> 0`, which wasm-bindgen applied to the numeric concurrency
/// argument before downloads took an options object.
fn legacy_concurrency(value: &JsValue) -> usize {
    if value.is_symbol() || value.is_bigint() {
        return 0;
    }
    // Unary `+`, the ToNumber step of `>>>`; it throws only for the excluded types.
    let number = value.unchecked_into_f64();
    if !number.is_finite() {
        return 0;
    }
    number.trunc().rem_euclid(UINT32_RANGE) as usize
}

fn js_string(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

fn method(object: &JsValue, name: &str) -> Result<js_sys::Function, JsValue> {
    js_sys::Reflect::get(object, &JsValue::from_str(name))?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| JsValue::from_str(&format!("destination has no {name} method")))
}

/// An optional callback option: absent, or a function.
fn optional_function(value: JsValue, label: &str) -> Result<Option<js_sys::Function>, String> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value
        .dyn_into::<js_sys::Function>()
        .map(Some)
        .map_err(|_| format!("{label} must be a function"))
}

/// Deserialize an optional options object; `undefined` and `null` select
/// defaults. serde_wasm_bindgen reads only declared fields, so a misspelt
/// option is rejected here instead of being silently ignored.
fn options_from_js<T: Default + serde::de::DeserializeOwned>(
    value: JsValue,
    known: &[&str],
) -> Result<T, String> {
    if value.is_undefined() || value.is_null() {
        return Ok(T::default());
    }
    if let Some(object) = value.dyn_ref::<js_sys::Object>() {
        for key in js_sys::Object::keys(object) {
            let key = key.as_string().unwrap_or_default();
            if !known.contains(&key.as_str()) {
                return Err(format!(
                    "unknown option `{key}`, expected one of {}",
                    known.join(", ")
                ));
            }
        }
    }
    serde_wasm_bindgen::from_value(value).map_err(|error| error.to_string())
}

/// Let the page handle events between long stretches of synchronous work.
async fn yield_to_event_loop() {
    TimeoutFuture::new(0).await;
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
    /// AbortSignal that cancels this operation without closing the reader.
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

/// Complete-download options:
/// `{ concurrency?, maxMemoryBytes?, onProgress?, signal? }`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadOptions {
    /// Upper bound on concurrent record fetches.
    concurrency: Option<f64>,
    /// Largest output buffer to allocate; larger files fail before transfer.
    max_memory_bytes: Option<f64>,
    /// Called with progress messages.
    #[serde(default, with = "serde_wasm_bindgen::preserve")]
    on_progress: JsValue,
    /// AbortSignal that cancels the download.
    #[serde(default, with = "serde_wasm_bindgen::preserve")]
    signal: JsValue,
}

/// Validated complete-download options.
pub(super) struct DownloadSettings {
    pub(super) concurrency: usize,
    pub(super) memory_budget: Option<u64>,
    pub(super) progress: ProgressReporter,
    /// AbortSignal, or `undefined` when the caller passed none.
    pub(super) signal: JsValue,
}

impl DownloadSettings {
    /// `options` is an options object. Any other value is the legacy
    /// concurrency argument, coerced as before, which pairs with the
    /// positional `legacy_progress` callback.
    pub(super) fn from_js(
        options: Option<JsValue>,
        legacy_progress: Option<js_sys::Function>,
    ) -> Result<Self, String> {
        let options = options.unwrap_or(JsValue::UNDEFINED);
        if !options.is_undefined() && !options.is_object() {
            let concurrency = legacy_concurrency(&options);
            if concurrency == 0 {
                return Err(POSITIVE_CONCURRENCY.into());
            }
            return Ok(Self {
                concurrency,
                memory_budget: None,
                progress: ProgressReporter::from_js(legacy_progress),
                signal: JsValue::UNDEFINED,
            });
        }
        let options: DownloadOptions = options_from_js(options, &DOWNLOAD_OPTION_NAMES)?;
        let progress = optional_function(options.on_progress, "onProgress")?;
        if progress.is_some() && legacy_progress.is_some() {
            return Err("pass onProgress in the options or as an argument, not both".into());
        }
        let concurrency = match options.concurrency {
            None => usize::MAX,
            Some(value) => match safe_integer(value, "download concurrency") {
                Ok(0) | Err(_) => return Err(POSITIVE_CONCURRENCY.into()),
                Ok(value) => usize::try_from(value).unwrap_or(usize::MAX),
            },
        };
        Ok(Self {
            concurrency,
            memory_budget: options
                .max_memory_bytes
                .map(|value| safe_integer(value, "memory budget"))
                .transpose()?,
            progress: ProgressReporter::from_js(progress.or(legacy_progress)),
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

/// Stops an operation when its AbortSignal fires or, for a reader
/// operation, when the reader closes.
pub(super) struct Cancellation {
    closed: Option<watch::Receiver<bool>>,
    abort: Option<AbortListener>,
}

impl Cancellation {
    /// `closed` follows the reader an operation belongs to, if any.
    pub(super) fn new(
        closed: Option<watch::Receiver<bool>>,
        signal: JsValue,
    ) -> Result<Self, JsValue> {
        let abort = if signal.is_undefined() || signal.is_null() {
            None
        } else {
            Some(AbortListener::new(signal)?)
        };
        Ok(Self { closed, abort })
    }

    /// Stop following the reader; only the AbortSignal can cancel from here.
    fn ignore_reader(&mut self) {
        self.closed = None;
    }

    /// Fail with the abort reason, or the closed-reader error.
    fn check(&self) -> Result<(), JsValue> {
        if let Some(reason) = self.abort.as_ref().and_then(AbortListener::reason) {
            return Err(reason);
        }
        if self.closed.as_ref().is_some_and(|closed| *closed.borrow()) {
            return Err(JsValue::from_str(CLOSED_READER));
        }
        Ok(())
    }

    /// Run `operation` unless it is cancelled first. Cancellation is polled
    /// before the operation, so once the reader closes or the signal aborts
    /// the operation never runs again: it is dropped, stopping its in-flight
    /// fetches and retry waits, before it can finish or refill the cache.
    pub(super) async fn run<T>(
        &mut self,
        operation: impl Future<Output = Result<T, JsValue>>,
    ) -> Result<T, JsValue> {
        self.check()?;
        let finished = {
            let Self { closed, abort } = &mut *self;
            let closed = async move {
                match closed {
                    Some(closed) => {
                        let _ = closed.wait_for(|closed| *closed).await;
                    }
                    None => futures_util::future::pending::<()>().await,
                }
            };
            let aborted = async move {
                match abort {
                    Some(abort) => {
                        let _ = (&mut abort.fired).await;
                    }
                    None => futures_util::future::pending::<()>().await,
                }
            };
            futures_util::pin_mut!(operation, closed, aborted);
            match select(select(closed, aborted), operation).await {
                Either::Left(_) => None,
                Either::Right((result, _)) => Some(result),
            }
        };
        finished.unwrap_or_else(|| {
            Err(self
                .check()
                .err()
                .unwrap_or_else(|| JsValue::from_str(CLOSED_READER)))
        })
    }
}

/// Random-access file reader. Content memory is bounded independently of file
/// size. The resolved chunk map and seek index remain resident in memory.
#[wasm_bindgen(js_name = BrowserFileReader)]
pub struct BrowserFileReader {
    shared: Rc<crate::data::Client>,
    /// Descriptor without its chunk list; the index holds the chunk metadata.
    file: PublicFileDescriptor,
    index: FileIndex,
    closed: watch::Sender<bool>,
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
    /// Closing the reader rejects a read in progress.
    #[wasm_bindgen(js_name = readRange)]
    pub async fn read_range(&self, start: f64, length: f64) -> Result<Uint8Array, JsValue> {
        let start = safe_integer(start, "range start").map_err(js_string)?;
        let length = safe_integer(length, "range length").map_err(js_string)?;
        if length > MAX_BROWSER_RANGE_BYTES as u64 {
            return Err(JsValue::from_str(&format!(
                "browser range reads are limited to {MAX_BROWSER_RANGE_BYTES} bytes"
            )));
        }
        let mut cancel = Cancellation::new(Some(self.closed.subscribe()), JsValue::UNDEFINED)?;
        let bytes = cancel
            .run(async {
                self.shared
                    .data_download_indexed_range(&self.index, start, length as usize)
                    .await
                    .map_err(js_string)
            })
            .await?;
        Ok(Uint8Array::from(bytes.as_ref()))
    }

    /// Write the file to a WritableStream (including a file-system writable).
    /// `options` is `{ start?, end?, signal?, onProgress? }`: an optional
    /// half-open byte range, an AbortSignal that cancels this operation, and
    /// a `(bytesWritten, totalBytes)` callback run after each write.
    ///
    /// Chunks are fetched concurrently and written in order, at most 4 MiB per
    /// write, each awaited before the next. Fetching continues during writes
    /// until 32 MiB of chunks are in flight or waiting, and each missing record
    /// is retried on its own schedule while the others continue. Closes the
    /// destination on success, aborts it on any failure (including invalid
    /// options), and always releases the writer lock. Returns the BLAKE3 and
    /// byte count of the written range. Closing the reader also cancels until
    /// every byte is written; after that only the AbortSignal can, and a
    /// destination that has started closing may already be committed.
    #[wasm_bindgen(js_name = pipeTo)]
    pub async fn pipe_to(
        &self,
        writable: JsValue,
        options: Option<JsValue>,
    ) -> Result<JsValue, JsValue> {
        let writer = Writer(method(&writable, "getWriter")?.call0(&writable)?);
        let result = self
            .pipe_to_writer(&writer.0, options.unwrap_or(JsValue::UNDEFINED))
            .await;
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

    /// Close the reader and cancel its operations. Cached records stay in the
    /// client's shared cache, which is bounded and may serve other readers.
    /// Use an AbortSignal to cancel a single pipeTo without closing.
    pub fn close(&self) {
        self.closed.send_replace(true);
    }
}

impl BrowserFileReader {
    pub(super) fn new(shared: Rc<crate::data::Client>, resolved: ResolvedBrowserFile) -> Self {
        Self {
            shared,
            file: resolved.file,
            index: resolved.index,
            closed: watch::Sender::new(false),
        }
    }

    /// The descriptor of a completely read file, with its chunk list and hash.
    pub(super) fn into_descriptor(self, hash: String) -> PublicFileDescriptor {
        let mut file = self.file;
        file.chunks = self
            .index
            .chunk_infos()
            .map(|info| super::super::browser_chunk_info(&info))
            .collect();
        file.blake3 = hash;
        file
    }

    async fn pipe_to_writer(&self, writer: &JsValue, options: JsValue) -> Result<JsValue, JsValue> {
        let options: PipeOptions =
            options_from_js(options, &PIPE_OPTION_NAMES).map_err(js_string)?;
        let progress = optional_function(options.on_progress, "onProgress").map_err(js_string)?;
        let mut cancel = Cancellation::new(Some(self.closed.subscribe()), options.signal)?;
        cancel.check()?;
        let range = self
            .stream_range(options.start, options.end)
            .map_err(js_string)?;
        let write = method(writer, "write")?;
        let (write, progress) = (&write, &progress);
        let total = range.end - range.start;
        let mut hasher = blake3::Hasher::new();
        cancel
            .run(ordered_pipeline(
                self.index.chunks_overlapping(range.clone()),
                |chunk| {
                    let span = self.index.chunk_span(chunk);
                    span.end - span.start
                },
                PIPE_BUFFER_BYTES,
                &|| self.shared.controller().fetch.current(),
                |chunk| async move {
                    self.shared
                        .data_download_indexed_chunk(&self.index, chunk)
                        .await
                        .map_err(js_string)
                },
                |chunk, plaintext: Bytes| {
                    let span = self.index.chunk_span(chunk);
                    let from = range.start.max(span.start);
                    let to = range.end.min(span.end);
                    let plaintext =
                        plaintext.slice((from - span.start) as usize..(to - span.start) as usize);
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
            ))
            .await?;
        // Every byte is written, so closing the reader no longer cancels: it
        // may be closed from the final progress callback.
        cancel.ignore_reader();
        cancel
            .run(async {
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

    /// Validate a `pipeTo` range against the file.
    fn stream_range(&self, start: Option<f64>, end: Option<f64>) -> Result<Range<u64>, String> {
        let start = safe_integer(start.unwrap_or(0.0), "stream start")?;
        let end = end
            .map(|end| safe_integer(end, "stream end"))
            .transpose()?
            .unwrap_or(self.file.size);
        if start > end || end > self.file.size {
            return Err("stream range is outside the file".into());
        }
        Ok(start..end)
    }

    /// Allocate output in JS, then copy each decrypted chunk into it as it
    /// arrives. The whole file is fetched in one deferred retry pass at the
    /// requested concurrency; WASM holds only the records in flight. A caller
    /// can impose its own memory budget; allocation failures recommend the
    /// disk-backed path without downloading the file first.
    pub(super) async fn collect(
        &self,
        settings: &DownloadSettings,
        cancel: &mut Cancellation,
    ) -> Result<(Uint8Array, String), JsValue> {
        let progress = &settings.progress;
        if settings
            .memory_budget
            .is_some_and(|limit| self.file.size > limit)
        {
            return Err(js_string(format_args!(
                "file exceeds the download memory budget; {STREAM_HINT}"
            )));
        }
        let constructor =
            js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("Uint8Array"))?
                .dyn_into::<js_sys::Function>()?;
        let args = Array::new();
        args.push(&JsValue::from_f64(self.size()));
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
        let chunk_view = |chunk: usize| -> Result<Uint8Array, JsValue> {
            let span = self.index.chunk_span(chunk);
            Ok(subarray
                .call2(
                    output.as_ref(),
                    &JsValue::from_f64(span.start as f64),
                    &JsValue::from_f64(span.end as f64),
                )?
                .unchecked_into())
        };
        let total = self.index.chunk_count();
        let mut hasher = OrderedHasher::new(total);
        let mut completed = 0usize;
        progress.report(&format!("Downloaded chunk {completed}/{total}"));
        // The engine reports a sink failure as invalid data, so the output's
        // own error is kept here and returned instead.
        let mut output_error = None;
        let pass = cancel
            .run(async {
                self.shared
                    .data_download_indexed_chunks(
                        &self.index,
                        0..total,
                        settings.concurrency,
                        |chunk, plaintext| {
                            let mut place = || -> Result<(), JsValue> {
                                // Copies straight from WASM memory into the output.
                                chunk_view(chunk)?.copy_from(&plaintext);
                                hasher.complete(chunk, &plaintext);
                                // At most one earlier chunk per arrival keeps
                                // each callback short.
                                if let Some(earlier) = hasher.pending_read_back() {
                                    let view = chunk_view(earlier)?;
                                    hasher
                                        .read_back(earlier, &view.to_vec())
                                        .map_err(js_string)?;
                                }
                                Ok(())
                            };
                            place().map_err(|error| {
                                let message = js_error_message(error.clone());
                                output_error = Some(error);
                                message
                            })?;
                            completed += 1;
                            progress.report(&format!("Downloaded chunk {completed}/{total}"));
                            Ok(())
                        },
                    )
                    .await
                    .map_err(js_string)
            })
            .await;
        if let Some(error) = output_error {
            return Err(error);
        }
        pass?;
        // Hash the chunks that arrived ahead of file order, yielding to the
        // page between short slices so a large backlog cannot freeze it.
        let mut slice_start = Instant::now();
        while let Some(chunk) = hasher.pending_read_back() {
            cancel.check()?;
            hasher
                .read_back(chunk, &chunk_view(chunk)?.to_vec())
                .map_err(js_string)?;
            if slice_start.elapsed() >= HASH_SLICE {
                yield_to_event_loop().await;
                slice_start = Instant::now();
            }
        }
        Ok((output, hasher.finish().map_err(js_string)?))
    }
}
