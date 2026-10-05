//! Bounded plaintext reads with 64-bit positions and caller-owned destinations.
use super::*;
use crate::client_engine::files::{FileIndex, OrderedHasher};
use bytes::Bytes;
use std::ops::Range;

/// Plaintext one `pipeTo` window may hold. A window's whole chunks are
/// fetched concurrently in one deferred retry pass, then written in order;
/// nothing more is fetched while those writes are pending.
const PIPE_WINDOW_BYTES: u64 = 32 * 1024 * 1024;
const STREAM_HINT: &str =
    "use openPublicFile/openPrivateFile and reader.pipeTo(writable) to stream to disk";
const CLOSED_READER: &str = "browser file reader is closed";
const ABORTED: &str = "download aborted";
const ABORT_EVENT: &str = "abort";
const PIPE_OPTION_NAMES: [&str; 4] = ["start", "end", "signal", "onProgress"];
const DOWNLOAD_OPTION_NAMES: [&str; 3] = ["concurrency", "maxMemoryBytes", "onProgress"];

fn file_position(value: f64, label: &str) -> Result<u64, String> {
    if !value.is_finite()
        || value < 0.0
        || value.fract() != 0.0
        || value > super::super::manifest::MAX_BROWSER_FILE_POSITION as f64
    {
        return Err(format!("{label} must be a nonnegative safe integer"));
    }
    Ok(value as u64)
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

/// Complete-download options: `{ concurrency?, maxMemoryBytes?, onProgress? }`.
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
}

/// Validated complete-download options.
pub(super) struct DownloadSettings {
    pub(super) concurrency: usize,
    pub(super) memory_budget: Option<u64>,
    pub(super) progress: ProgressReporter,
}

impl DownloadSettings {
    /// `options` is an options object. A number is the legacy concurrency
    /// argument, which pairs with the positional `legacy_progress` callback.
    pub(super) fn from_js(
        options: JsValue,
        legacy_progress: Option<js_sys::Function>,
    ) -> Result<Self, String> {
        let (options, progress) = match options.as_f64() {
            Some(concurrency) => (
                DownloadOptions {
                    concurrency: Some(concurrency),
                    ..DownloadOptions::default()
                },
                legacy_progress,
            ),
            None => {
                let mut options: DownloadOptions =
                    options_from_js(options, &DOWNLOAD_OPTION_NAMES)?;
                let progress =
                    optional_function(std::mem::take(&mut options.on_progress), "onProgress")?;
                if progress.is_some() && legacy_progress.is_some() {
                    return Err("pass onProgress in the options or as an argument, not both".into());
                }
                (options, progress.or(legacy_progress))
            }
        };
        let concurrency = match options.concurrency {
            None => usize::MAX,
            Some(value) => match file_position(value, "download concurrency") {
                Ok(0) | Err(_) => {
                    return Err("download concurrency must be a positive integer".into())
                }
                Ok(value) => usize::try_from(value).unwrap_or(usize::MAX),
            },
        };
        Ok(Self {
            concurrency,
            memory_budget: options
                .max_memory_bytes
                .map(|value| file_position(value, "memory budget"))
                .transpose()?,
            progress: ProgressReporter::from_js(progress),
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

/// Stops one reader operation when its reader closes or its AbortSignal fires.
struct Cancellation {
    closed: watch::Receiver<bool>,
    abort: Option<AbortListener>,
}

impl Cancellation {
    fn new(closed: watch::Receiver<bool>, signal: JsValue) -> Result<Self, JsValue> {
        let abort = if signal.is_undefined() || signal.is_null() {
            None
        } else {
            Some(AbortListener::new(signal)?)
        };
        Ok(Self { closed, abort })
    }

    /// Fail with the abort reason, or the closed-reader error.
    fn check(&self) -> Result<(), JsValue> {
        if let Some(reason) = self.abort.as_ref().and_then(AbortListener::reason) {
            return Err(reason);
        }
        if *self.closed.borrow() {
            return Err(JsValue::from_str(CLOSED_READER));
        }
        Ok(())
    }

    /// Run `operation` unless it is cancelled first. Cancelling drops it,
    /// which stops its in-flight fetches and retry waits at once.
    async fn run<T>(
        &mut self,
        operation: impl Future<Output = Result<T, String>>,
    ) -> Result<T, JsValue> {
        self.check()?;
        let finished = {
            let Self { closed, abort } = &mut *self;
            let closed = closed.wait_for(|closed| *closed);
            let aborted = async move {
                match abort {
                    Some(abort) => {
                        let _ = (&mut abort.fired).await;
                    }
                    None => futures_util::future::pending::<()>().await,
                }
            };
            futures_util::pin_mut!(operation, closed, aborted);
            match select(operation, select(closed, aborted)).await {
                Either::Left((result, _)) => Some(result),
                Either::Right(_) => None,
            }
        };
        match finished {
            Some(result) => result.map_err(|error| JsValue::from_str(&error)),
            None => Err(self
                .check()
                .err()
                .unwrap_or_else(|| JsValue::from_str(CLOSED_READER))),
        }
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
        let start =
            file_position(start, "range start").map_err(|error| JsValue::from_str(&error))?;
        let length =
            file_position(length, "range length").map_err(|error| JsValue::from_str(&error))?;
        if length > MAX_BROWSER_RANGE_BYTES as u64 {
            return Err(JsValue::from_str(&format!(
                "browser range reads are limited to {MAX_BROWSER_RANGE_BYTES} bytes"
            )));
        }
        let mut cancel = Cancellation::new(self.closed.subscribe(), JsValue::UNDEFINED)?;
        let bytes = cancel
            .run(async {
                self.shared
                    .data_download_indexed_range(&self.index, start, length as usize)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await?;
        Ok(Uint8Array::from(bytes.as_ref()))
    }

    /// Write the file to a WritableStream (including a file-system writable).
    /// `options` is `{ start?, end?, signal?, onProgress? }`: an optional
    /// half-open byte range, an AbortSignal that cancels this operation, and
    /// a `(bytesWritten, totalBytes)` callback run after each write.
    ///
    /// Whole chunks are fetched a window at a time, and every write is awaited
    /// before the next window is fetched. Writes are at most 4 MiB. Closes the
    /// destination on success, aborts it on any failure (including invalid
    /// options), and always releases the writer lock. Returns the BLAKE3 and
    /// byte count of the written range. Closing the reader also cancels.
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
                    let _ = JsFuture::from(Promise::resolve(&promise)).await;
                }
            }
        }
        result
    }

    /// Close the reader: cancel its operations and drop its records from the
    /// client's shared cache, leaving other readers' records in place. Use an
    /// AbortSignal to cancel a single pipeTo without closing the reader.
    pub fn close(&self) {
        self.closed.send_replace(true);
        let cache = self.shared.chunk_cache();
        for address in self.index.record_addresses() {
            cache.remove(&address.0);
        }
    }
}

impl BrowserFileReader {
    pub(super) fn new(
        shared: Rc<crate::data::Client>,
        mut file: PublicFileDescriptor,
        index: FileIndex,
    ) -> Self {
        file.chunks = Vec::new();
        Self {
            shared,
            file,
            index,
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
        let options: PipeOptions = options_from_js(options, &PIPE_OPTION_NAMES)
            .map_err(|error| JsValue::from_str(&error))?;
        let progress = optional_function(options.on_progress, "onProgress")
            .map_err(|error| JsValue::from_str(&error))?;
        let mut cancel = Cancellation::new(self.closed.subscribe(), options.signal)?;
        cancel.check()?;
        let range = self
            .stream_range(options.start, options.end)
            .map_err(|error| JsValue::from_str(&error))?;
        let write = method(writer, "write")?;
        let total = range.end - range.start;
        let mut written = 0u64;
        let mut hasher = blake3::Hasher::new();
        let chunks = self.index.chunks_overlapping(range.clone());
        let mut next = chunks.start;
        while next < chunks.end {
            let window = next..self.index.window_end(next, chunks.end, PIPE_WINDOW_BYTES);
            for plaintext in self
                .fetch_window(window.clone(), &range, &mut cancel)
                .await?
            {
                for piece in plaintext.chunks(MAX_BROWSER_RANGE_BYTES) {
                    // Checked before each write, never after the last one:
                    // a finished destination is closed, not aborted.
                    cancel.check()?;
                    hasher.update(piece);
                    JsFuture::from(Promise::resolve(
                        &write.call1(writer, &Uint8Array::from(piece))?,
                    ))
                    .await?;
                    written += piece.len() as u64;
                    if let Some(callback) = &progress {
                        callback.call2(
                            &JsValue::NULL,
                            &JsValue::from_f64(written as f64),
                            &JsValue::from_f64(total as f64),
                        )?;
                    }
                }
            }
            next = window.end;
        }
        JsFuture::from(Promise::resolve(&method(writer, "close")?.call0(writer)?)).await?;
        serde_wasm_bindgen::to_value(&PipeResult {
            bytes_written: written,
            hash: hasher.finalize().to_hex().to_string(),
        })
        .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Validate a `pipeTo` range against the file.
    fn stream_range(&self, start: Option<f64>, end: Option<f64>) -> Result<Range<u64>, String> {
        let start = file_position(start.unwrap_or(0.0), "stream start")?;
        let end = end
            .map(|end| file_position(end, "stream end"))
            .transpose()?
            .unwrap_or(self.file.size);
        if start > end || end > self.file.size {
            return Err("stream range is outside the file".into());
        }
        Ok(start..end)
    }

    /// Fetch one window of whole chunks, returning each chunk's plaintext
    /// trimmed to the `pipeTo` range, in file order.
    async fn fetch_window(
        &self,
        window: Range<usize>,
        range: &Range<u64>,
        cancel: &mut Cancellation,
    ) -> Result<Vec<Bytes>, JsValue> {
        let mut plaintexts = vec![None; window.len()];
        cancel
            .run(async {
                self.shared
                    .data_download_indexed_chunks(
                        &self.index,
                        window.clone(),
                        usize::MAX,
                        |chunk, plaintext| {
                            plaintexts[chunk - window.start] = Some(plaintext);
                            Ok(())
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())
            })
            .await?;
        window
            .zip(plaintexts)
            .map(|(chunk, plaintext)| {
                let plaintext =
                    plaintext.ok_or_else(|| JsValue::from_str("window chunk missing"))?;
                let span = self.index.chunk_span(chunk);
                let from = range.start.max(span.start) - span.start;
                let to = range.end.min(span.end) - span.start;
                Ok(plaintext.slice(from as usize..to as usize))
            })
            .collect()
    }

    /// Allocate output in JS, then copy each decrypted chunk into it as it
    /// arrives. The whole file is fetched in one deferred retry pass at the
    /// requested concurrency; WASM holds only the records in flight. A caller
    /// can impose its own memory budget; allocation failures recommend the
    /// disk-backed path without downloading the file first.
    pub(super) async fn collect(
        &self,
        concurrency: usize,
        budget: Option<u64>,
        progress: &ProgressReporter,
    ) -> Result<(Uint8Array, String), String> {
        if budget.is_some_and(|limit| self.file.size > limit) {
            return Err(format!(
                "file exceeds the download memory budget; {STREAM_HINT}"
            ));
        }
        let constructor = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("Uint8Array"))
            .map_err(js_error_message)?
            .dyn_into::<js_sys::Function>()
            .map_err(js_error_message)?;
        let args = Array::new();
        args.push(&JsValue::from_f64(self.size()));
        let output = js_sys::Reflect::construct(&constructor, &args)
            .map_err(|error| {
                format!(
                    "cannot allocate download buffer: {}; {STREAM_HINT}",
                    js_error_message(error)
                )
            })?
            .unchecked_into::<Uint8Array>();
        // Reflect calls preserve Number offsets above u32::MAX and catch JS
        // allocation exceptions instead of turning them into WASM traps.
        let set = method(output.as_ref(), "set").map_err(js_error_message)?;
        let subarray = method(output.as_ref(), "subarray").map_err(js_error_message)?;
        let read_back = |chunk: usize| -> Result<Vec<u8>, String> {
            let span = self.index.chunk_span(chunk);
            subarray
                .call2(
                    output.as_ref(),
                    &JsValue::from_f64(span.start as f64),
                    &JsValue::from_f64(span.end as f64),
                )
                .map(|view| view.unchecked_into::<Uint8Array>().to_vec())
                .map_err(js_error_message)
        };
        let total = self.index.chunk_count();
        let mut hasher = OrderedHasher::new(total);
        let mut completed = 0usize;
        progress.report(&format!("Downloaded chunk {completed}/{total}"));
        self.shared
            .data_download_indexed_chunks(&self.index, 0..total, concurrency, |chunk, plaintext| {
                set.call2(
                    output.as_ref(),
                    &Uint8Array::from(plaintext.as_ref()),
                    &JsValue::from_f64(self.index.chunk_span(chunk).start as f64),
                )
                .map_err(js_error_message)?;
                hasher.complete(chunk, &plaintext, &read_back)?;
                completed += 1;
                progress.report(&format!("Downloaded chunk {completed}/{total}"));
                Ok(())
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok((output, hasher.finish()?))
    }
}
