//! Direct transport test seam, compiled only with `test-utils`.
//! Production callers own their connections through `BrowserNetworkClient`.
//! This helper drives the real authentication, request admission, framing and
//! verification code without network discovery masking a single-node failure.

use super::*;

/// Authenticate one test node without creating a second public client API.
#[wasm_bindgen]
pub async fn test_connect_node(endpoint: JsValue) -> Result<TestNodeSession, JsValue> {
    let endpoint: BrowserEndpointInput = serde_wasm_bindgen::from_value(endpoint)
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    let endpoint = parse_webrtc_direct_multiaddr(endpoint.multiaddr())
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    let inner = Rc::new(BrowserNodeClientCore::new(endpoint));
    inner
        .hello()
        .await
        .map_err(|error| JsValue::from_str(&error))?;
    Ok(TestNodeSession {
        generation: inner.generation.get(),
        inner,
    })
}

#[derive(Debug, Serialize)]
struct BrowserChunk {
    #[serde(with = "serde_bytes")]
    content: Vec<u8>,
    hash: String,
}

/// Test handle bound to one authenticated transport generation.
#[wasm_bindgen(js_name = TestNodeSession)]
pub struct TestNodeSession {
    inner: Rc<BrowserNodeClientCore>,
    generation: u64,
}

impl TestNodeSession {
    async fn active(&self) -> Result<LockedBrowserClient<'_>, JsValue> {
        let mut client = self
            .inner
            .lock_before(&TransferDeadline::new(RPC_ADMISSION_TIMEOUT))
            .await
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        if self.generation != self.inner.generation.get()
            || !self.inner.is_connected()
            || self.inner.hello.borrow().is_none()
        {
            return Err(JsValue::from_str(
                "session closed; call test_connect_node() again",
            ));
        }
        client._guard.take();
        let rpc = self
            .inner
            .current_rpc()
            .ok_or_else(|| JsValue::from_str("session closed"))?;
        let slot = rpc
            .admit(RPC_ADMISSION_TIMEOUT)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        if self.generation != self.inner.generation.get() {
            return Err(JsValue::from_str("session closed"));
        }
        client.slot.replace(Some(slot));
        client.authenticated_generation = Some(self.generation);
        Ok(client)
    }
}

#[wasm_bindgen(js_class = TestNodeSession)]
impl TestNodeSession {
    /// Authenticate the connected node.
    pub async fn hello(&self) -> Result<JsValue, JsValue> {
        let hello = self
            .active()
            .await?
            .hello()
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        hello
            .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Request nodes closest to a 32-byte target.
    #[wasm_bindgen(js_name = findNode)]
    pub async fn find_node(&self, target: &str, count: usize) -> Result<JsValue, JsValue> {
        let nodes = self
            .active()
            .await?
            .find_node(target, count)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&nodes).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Retrieve and BLAKE3-verify one content-addressed record.
    #[wasm_bindgen(js_name = getChunk)]
    pub async fn get_chunk(&self, address: &str) -> Result<JsValue, JsValue> {
        let (content, hash) = self
            .active()
            .await?
            .get_chunk(address)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&BrowserChunk { content, hash })
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Close the test connection.
    pub fn close(&self) {
        self.inner.close();
    }
}

// Legacy per-node record operations are used only by transport regression tests.
impl LockedBrowserClient<'_> {
    pub(super) async fn get_chunk(&self, address: &str) -> Result<(Vec<u8>, String), String> {
        self.try_get_chunk(address)
            .await?
            .ok_or_else(|| format!("chunk {address} was not found on this node"))
    }

    async fn try_get_chunk(&self, address: &str) -> Result<Option<(Vec<u8>, String)>, String> {
        let address = crate::browser::protocol::normalize_hex(address, 32)?;
        let response = self
            .request(
                BrowserRequestBody::GetChunk {
                    address: address.clone(),
                },
                &[],
            )
            .await?;
        if response.header.status == BrowserResponseStatus::NotFound {
            return Ok(None);
        }
        let BrowserResponseBody::Chunk {
            address: response_address,
            size,
        } = response.header.body
        else {
            return Err("expected a CHUNK response".to_string());
        };
        if response_address.to_ascii_lowercase() != address {
            return Err("node returned a different chunk address".to_string());
        }
        if size != response.content.len() {
            return Err("chunk metadata size does not match its content".to_string());
        }
        crate::browser::verify_record(&address, &response.content)
            .map_err(|error| error.to_string())?;
        Ok(Some((response.content, address)))
    }

    pub(super) async fn quote_chunk(
        &self,
        address: &str,
        size: usize,
    ) -> Result<(BrowserQuoteArtifact, bool), String> {
        let address = crate::browser::protocol::normalize_hex(address, 32)?;
        if size > crate::browser::protocol::MAX_BROWSER_RECORD_BYTES {
            return Err(format!("invalid chunk size {size}"));
        }
        let response = self
            .request(
                BrowserRequestBody::QuoteChunk {
                    address: address.clone(),
                    size: u64::try_from(size).map_err(|_| format!("invalid chunk size {size}"))?,
                },
                &[],
            )
            .await?;
        let BrowserResponseBody::StorageQuote {
            address: response_address,
            already_stored,
            quote,
        } = response.header.body
        else {
            return Err("expected a STORAGE_QUOTE response".to_string());
        };
        if response_address.to_ascii_lowercase() != address {
            return Err("node returned a quote for a different chunk address".to_string());
        }
        Ok((quote, already_stored))
    }

    pub(super) async fn put_chunk_typed(
        &self,
        address: &str,
        content: &[u8],
        quote: BrowserQuoteArtifact,
        transaction_hash: &str,
    ) -> Result<(String, bool), RpcError> {
        let address = crate::browser::protocol::normalize_hex(address, 32)?;
        let transaction_hash = crate::browser::protocol::normalize_hex(transaction_hash, 32)?;
        crate::browser::verify_record(&address, content).map_err(|error| error.to_string())?;
        let response = self
            .request_typed(
                BrowserRequestBody::PutChunk {
                    address: address.clone(),
                    quote: Box::new(quote),
                    transaction_hash,
                },
                content,
            )
            .await?;
        let BrowserResponseBody::ChunkStored {
            address: response_address,
            already_stored,
        } = response.header.body
        else {
            return Err("expected a CHUNK_STORED response".to_string().into());
        };
        if response_address.to_ascii_lowercase() != address {
            return Err("node stored a different chunk address".to_string().into());
        }
        Ok((address, already_stored))
    }
}
