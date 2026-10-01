//! Bounded plaintext reads with 64-bit positions and caller-owned destinations.
use super::*;
use crate::client_engine::files::FileIndex;

pub(super) fn file_position(value: f64, label: &str) -> Result<u64, String> {
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

/// Release the writer even if the Rust future is dropped before it completes.
struct Writer(JsValue);
impl Drop for Writer {
    fn drop(&mut self) {
        if let Ok(release) = method(&self.0, "releaseLock") {
            let _ = release.call0(&self.0);
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipeOptions {
    start: Option<f64>,
    end: Option<f64>,
}

#[derive(Serialize)]
struct PipeResult {
    #[serde(rename = "bytesWritten")]
    bytes_written: u64,
    hash: String,
}

/// Random-access file reader. Content memory is bounded independently of file
/// size. The resolved chunk map and seek index remain resident in memory.
#[wasm_bindgen(js_name = BrowserFileReader)]
pub struct BrowserFileReader {
    shared: Rc<crate::data::Client>,
    file: PublicFileDescriptor,
    index: FileIndex,
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
        let result = async {
            let start = file_position(start, "range start")?;
            let length = file_position(length, "range length")?;
            if length > MAX_BROWSER_RANGE_BYTES as u64 {
                return Err(format!(
                    "browser range reads are limited to {MAX_BROWSER_RANGE_BYTES} bytes"
                ));
            }
            self.read(start, length as usize, usize::MAX).await
        }
        .await
        .map_err(|error| JsValue::from_str(&error))?;
        Ok(Uint8Array::from(result.as_ref()))
    }

    /// Write the file to a WritableStream (including a file-system writable).
    /// Awaits every write before fetching the next range. Closes on success,
    /// aborts on failure, and always releases the writer lock. Optional `start`
    /// and exclusive `end` select a range. Returns its BLAKE3 and byte count.
    /// Calling close() cancels at the next read/write boundary.
    #[wasm_bindgen(js_name = pipeTo)]
    pub async fn pipe_to(
        &self,
        writable: JsValue,
        options: Option<JsValue>,
        on_progress: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let options: PipeOptions = options
            .filter(|value| !value.is_null())
            .map(serde_wasm_bindgen::from_value)
            .transpose()
            .map_err(|error| JsValue::from_str(&error.to_string()))?
            .unwrap_or_default();
        let start = file_position(options.start.unwrap_or(0.0), "stream start")
            .map_err(|error| JsValue::from_str(&error))?;
        let end = file_position(options.end.unwrap_or(self.size()), "stream end")
            .map_err(|error| JsValue::from_str(&error))?;
        if start > end || end > self.file.size {
            return Err(JsValue::from_str("stream range is outside the file"));
        }
        self.ensure_open()
            .map_err(|error| JsValue::from_str(&error))?;
        let writer = Writer(method(&writable, "getWriter")?.call0(&writable)?);
        let result = async {
            let write = method(&writer.0, "write")?;
            let mut position = start;
            let mut hasher = blake3::Hasher::new();
            while position < end {
                let length = (end - position).min(MAX_BROWSER_RANGE_BYTES as u64) as usize;
                let bytes = self
                    .read(position, length, usize::MAX)
                    .await
                    .map_err(|error| JsValue::from_str(&error))?;
                let value = Uint8Array::from(bytes.as_ref());
                hasher.update(&bytes);
                drop(bytes);
                JsFuture::from(Promise::resolve(&write.call1(&writer.0, &value)?)).await?;
                self.ensure_open()
                    .map_err(|error| JsValue::from_str(&error))?;
                position += length as u64;
                if let Some(callback) = &on_progress {
                    callback.call2(
                        &JsValue::NULL,
                        &JsValue::from_f64((position - start) as f64),
                        &JsValue::from_f64((end - start) as f64),
                    )?;
                }
            }
            self.ensure_open()
                .map_err(|error| JsValue::from_str(&error))?;
            JsFuture::from(Promise::resolve(
                &method(&writer.0, "close")?.call0(&writer.0)?,
            ))
            .await?;
            serde_wasm_bindgen::to_value(&PipeResult {
                bytes_written: end - start,
                hash: hasher.finalize().to_hex().to_string(),
            })
            .map_err(|error| JsValue::from_str(&error.to_string()))
        }
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

    /// Cancel this reader and release the shared encrypted-record cache.
    pub fn close(&self) {
        self.closed.set(true);
        self.shared.chunk_cache().clear();
    }
}

impl BrowserFileReader {
    pub(super) fn new(
        shared: Rc<crate::data::Client>,
        file: PublicFileDescriptor,
        map: &self_encryption::DataMap,
    ) -> Result<Self, String> {
        Ok(Self {
            shared,
            file,
            index: FileIndex::new(map)?,
            closed: Cell::new(false),
        })
    }

    pub(super) fn into_file(self) -> PublicFileDescriptor {
        self.file
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.closed.get() {
            Err("browser file reader is closed".into())
        } else {
            Ok(())
        }
    }

    async fn read(
        &self,
        start: u64,
        length: usize,
        concurrency: usize,
    ) -> Result<bytes::Bytes, String> {
        self.ensure_open()?;
        let bytes = self
            .shared
            .data_download_indexed_range(&self.index, start, length, concurrency)
            .await
            .map_err(|error| error.to_string())?;
        self.ensure_open()?;
        Ok(bytes)
    }

    /// Allocate output in JS, then copy bounded plaintext ranges into it. Never
    /// retain the whole ciphertext/plaintext in WASM or serialize a file-sized Vec.
    /// A caller can impose its own memory budget; allocation failures recommend
    /// the disk-backed path without downloading the file first.
    pub(super) async fn collect(
        &self,
        concurrency: usize,
        budget: Option<u64>,
        progress: &ProgressReporter,
    ) -> Result<(Uint8Array, String), String> {
        const STREAM_HINT: &str =
            "use openPublicFile/openPrivateFile and reader.pipeTo(writable) to stream to disk";
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
        let mut position = 0u64;
        let mut hasher = blake3::Hasher::new();
        while position < self.file.size {
            let length = (self.file.size - position).min(MAX_BROWSER_RANGE_BYTES as u64) as usize;
            let bytes = self.read(position, length, concurrency).await?;
            hasher.update(&bytes);
            set.call2(
                output.as_ref(),
                &Uint8Array::from(bytes.as_ref()),
                &JsValue::from_f64(position as f64),
            )
            .map_err(js_error_message)?;
            position += length as u64;
            progress.report(&format!("Downloaded {position}/{} bytes", self.file.size));
        }
        Ok((output, hasher.finalize().to_hex().to_string()))
    }
}
