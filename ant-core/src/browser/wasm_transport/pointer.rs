// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pointers from the browser (ADR-0016 in `ant-node`): read, resolve, create
//! and update, over the same client the native API uses.
//!
//! Reads, the quorum and corroboration rules, the payment plan and the write
//! fan-out are the native client's own; only the wallet is the page's. The
//! owner key is handed in as the 32-byte FIPS 204 seed it derives from, so a
//! page keeps 32 bytes rather than a 4,032-byte secret key. The page is trusted
//! with that seed exactly as it is trusted with its wallet.

use super::upload_adapter::{paid_transactions, wallet_quotes};
use super::*;
use crate::data::client::batch::ChunkPaymentPlan;
use crate::data::{
    ml_dsa_65, pointer_address, MlDsaPublicKey, MlDsaSecretKey, Pointer, PointerTarget,
    PointerTargetKind,
};
use ant_protocol::evm::{QuoteHash, TxHash};

/// Length of an owner key seed.
const OWNER_SEED_LEN: usize = 32;

/// A pointer as JavaScript sees it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserPointer {
    address: String,
    /// Decimal: a counter can exceed JavaScript's safe integers.
    counter: String,
    kind: String,
    kind_tag: u8,
    target: String,
    state_id: String,
}

impl From<&Pointer> for BrowserPointer {
    fn from(record: &Pointer) -> Self {
        let target = record.target();
        Self {
            address: hex::encode(record.address()),
            counter: record.counter().to_string(),
            kind: kind_name(&target).to_string(),
            kind_tag: target.kind_tag(),
            target: hex::encode(target.address),
            state_id: hex::encode(record.state_id()),
        }
    }
}

/// Where a pointer chain ends.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserPointerTarget {
    kind: String,
    kind_tag: u8,
    target: String,
}

/// A stored pointer state and what it cost.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserPointerWrite {
    pointer: BrowserPointer,
    #[serde(skip_serializing_if = "Option::is_none")]
    transaction_hash: Option<String>,
    storage_cost_atto: String,
}

/// A target kind by name. A tag this build does not know is reported as
/// unknown, with its number in `kindTag`, rather than hidden: the record is
/// signed, so the bytes are authentic and the caller decides.
fn kind_name(target: &PointerTarget) -> &'static str {
    match target.kind() {
        Some(PointerTargetKind::Chunk) => "chunk",
        Some(PointerTargetKind::Pointer) => "pointer",
        None => "unknown",
    }
}

fn owner_keys(seed: &[u8]) -> Result<(MlDsaPublicKey, MlDsaSecretKey), String> {
    let seed: [u8; OWNER_SEED_LEN] = seed
        .try_into()
        .map_err(|_| format!("owner seed must be {OWNER_SEED_LEN} bytes"))?;
    Ok(ml_dsa_65().generate_keypair_from_seed(&seed))
}

fn parse_target(target: &str, kind: &str) -> Result<PointerTarget, String> {
    let kind = match kind {
        "chunk" => PointerTargetKind::Chunk,
        "pointer" => PointerTargetKind::Pointer,
        other => return Err(format!("unknown pointer target kind {other}")),
    };
    Ok(PointerTarget::new(
        kind,
        parse_lookup_key(target, "pointer target")?,
    ))
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    value
        .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
        .map_err(|error| JsValue::from_str(&error.to_string()))
}

/// The address of the pointer an owner seed controls, computed offline.
#[wasm_bindgen(js_name = pointerAddress)]
pub fn pointer_address_wasm(owner_seed: &[u8]) -> Result<String, JsValue> {
    let (owner, _) = owner_keys(owner_seed).map_err(|error| JsValue::from_str(&error))?;
    Ok(hex::encode(pointer_address(&owner)))
}

#[wasm_bindgen(js_class = BrowserNetworkClient)]
impl BrowserNetworkClient {
    /// Read a pointer, or `null` if the network holds none at `address`.
    ///
    /// The record is verified and must be named by at least two of the close
    /// group, as for a native read.
    #[wasm_bindgen(js_name = getPointer)]
    pub async fn get_pointer(&self, address: &str) -> Result<JsValue, JsValue> {
        let at = parse_lookup_key(address, "pointer address").map_err(|e| JsValue::from_str(&e))?;
        let record = self
            .shared
            .pointer_get(&at)
            .await
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        match record {
            Some(record) => to_js(&BrowserPointer::from(&record)),
            None => Ok(JsValue::NULL),
        }
    }

    /// Follow a chain of pointers to the target at its end.
    #[wasm_bindgen(js_name = resolvePointer)]
    pub async fn resolve_pointer(&self, address: &str) -> Result<JsValue, JsValue> {
        let at = parse_lookup_key(address, "pointer address").map_err(|e| JsValue::from_str(&e))?;
        let target = self
            .shared
            .pointer_resolve(&at)
            .await
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        to_js(&BrowserPointerTarget {
            kind: kind_name(&target).to_string(),
            kind_tag: target.kind_tag(),
            target: hex::encode(target.address),
        })
    }

    /// Create the pointer `owner_seed` controls, pointing at `target`, and pay
    /// for it through the same wallet callback file uploads take.
    ///
    /// `kind` is `"chunk"` or `"pointer"`. Refused before anything is paid if
    /// the pointer already exists.
    ///
    /// `on_paid`, if given, is called with `{ record, proof }` once the state
    /// is paid for and before it is stored. Keep both: if storing fails, or the
    /// page is lost, [`Self::store_paid_pointer`] stores the same state without
    /// paying again.
    #[wasm_bindgen(js_name = createPointer)]
    pub async fn create_pointer(
        &self,
        owner_seed: &[u8],
        target: &str,
        kind: &str,
        payment_network: JsValue,
        pay_for_quotes: js_sys::Function,
        on_paid: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let payment_network = parse_payment_network(payment_network)?;
        let written = self
            .write_pointer(
                PointerWrite::Create,
                owner_seed,
                target,
                kind,
                &payment_network,
                &pay_for_quotes,
                on_paid.as_ref(),
            )
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        to_js(&written)
    }

    /// Point the pointer `owner_seed` controls at `target`, one past the
    /// counter the network serves (or create it), and pay for the new state.
    /// `on_paid` is as for [`Self::create_pointer`].
    #[wasm_bindgen(js_name = updatePointer)]
    pub async fn update_pointer(
        &self,
        owner_seed: &[u8],
        target: &str,
        kind: &str,
        payment_network: JsValue,
        pay_for_quotes: js_sys::Function,
        on_paid: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let payment_network = parse_payment_network(payment_network)?;
        let written = self
            .write_pointer(
                PointerWrite::Update,
                owner_seed,
                target,
                kind,
                &payment_network,
                &pay_for_quotes,
                on_paid.as_ref(),
            )
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        to_js(&written)
    }

    /// Store a pointer state already paid for, from the `record` and `proof`
    /// an earlier write handed to its `on_paid` callback. Pays nothing.
    #[wasm_bindgen(js_name = storePaidPointer)]
    pub async fn store_paid_pointer(
        &self,
        record: &[u8],
        proof: &[u8],
        payment_network: JsValue,
    ) -> Result<JsValue, JsValue> {
        let payment_network = parse_payment_network(payment_network)?;
        let record = Pointer::from_bytes(record)
            .map_err(|error| JsValue::from_str(&format!("invalid pointer record: {error}")))?;
        self.paying_client(&payment_network)
            .pointer_put_paid(&record, proof.to_vec())
            .await
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        to_js(&BrowserPointer::from(&record))
    }
}

/// Which write a page asked for.
#[derive(Clone, Copy)]
enum PointerWrite {
    /// A new pointer, refused if one exists.
    Create,
    /// One past what the network serves, or a creation.
    Update,
}

impl BrowserNetworkClient {
    #[allow(clippy::too_many_arguments)] // Mirrors the JS arguments.
    async fn write_pointer(
        &self,
        write: PointerWrite,
        owner_seed: &[u8],
        target: &str,
        kind: &str,
        payment_network: &BrowserPaymentNetwork,
        pay_for_quotes: &js_sys::Function,
        on_paid: Option<&js_sys::Function>,
    ) -> Result<BrowserPointerWrite, String> {
        let (owner, secret) = owner_keys(owner_seed)?;
        let target = parse_target(target, kind)?;
        let client = self.paying_client(payment_network);
        let record = match write {
            PointerWrite::Create => client.pointer_sign_create(&secret, &owner, target).await,
            PointerWrite::Update => client.pointer_sign_update(&secret, &owner, target).await,
        }
        .map_err(|error| error.to_string())?;
        self.store_pointer(&client, &record, payment_network, pay_for_quotes, on_paid)
            .await
    }

    /// A client whose quotes and paid writes go only to nodes on the page's
    /// payment network, as an upload's do.
    fn paying_client(&self, payment_network: &BrowserPaymentNetwork) -> crate::data::Client {
        let mut network = SharedNetworkAdapter::new(Rc::clone(&self.inner));
        network.payment_network = Some(payment_network.clone());
        crate::data::Client::from_network(
            crate::data::Network::from_browser(Rc::new(network)),
            crate::data::ClientConfig::default(),
        )
        .with_shared_quote_state(&self.shared)
    }

    /// Quote the record's state, have the wallet pay it, then store it.
    async fn store_pointer(
        &self,
        client: &crate::data::Client,
        record: &Pointer,
        payment_network: &BrowserPaymentNetwork,
        pay_for_quotes: &js_sys::Function,
        on_paid: Option<&js_sys::Function>,
    ) -> Result<BrowserPointerWrite, String> {
        let plan = client
            .prepare_pointer_payment(record)
            .await
            .map_err(|error| error.to_string())?;
        let (transactions, submission) = self
            .pay_plan(&plan, payment_network, pay_for_quotes)
            .await?;
        let proof = plan
            .proof(&transactions)
            .map_err(|error| error.to_string())?;
        if let Some(on_paid) = on_paid {
            // The payment is spent whatever the page does with this, so a
            // callback that fails does not stop the state being stored.
            let _ = hand_over_paid(on_paid, record, &proof).await;
        }
        client
            .pointer_put_paid(record, proof)
            .await
            .map_err(|error| error.to_string())?;
        Ok(BrowserPointerWrite {
            pointer: BrowserPointer::from(record),
            transaction_hash: submission.transaction_hash,
            storage_cost_atto: submission.total_amount,
        })
    }

    /// Ask the page's wallet to pay one pointer state.
    ///
    /// The upload path journals each broadcast so that an interrupted payment
    /// is recovered rather than repeated. A pointer write pays one quote and
    /// keeps no journal of its own: once paid, the page is handed the record
    /// and proof through `on_paid`, and from then on nothing is paid twice. A
    /// page lost between the wallet's broadcast and its receipt pays again on
    /// retry.
    async fn pay_plan(
        &self,
        plan: &ChunkPaymentPlan,
        payment_network: &BrowserPaymentNetwork,
        wallet: &js_sys::Function,
    ) -> Result<(HashMap<QuoteHash, TxHash>, BrowserPaymentSubmission), String> {
        let verified = wallet_quotes(std::slice::from_ref(plan)).map_err(|e| e.to_string())?;
        let network = serde_wasm_bindgen::to_value(payment_network).map_err(|e| e.to_string())?;
        let quotes = serde_wasm_bindgen::to_value(&verified).map_err(|e| e.to_string())?;
        let on_submission = Closure::<dyn FnMut(JsValue) -> Promise>::new(|_: JsValue| {
            Promise::resolve(&JsValue::UNDEFINED)
        })
        .into_js_value();
        if self.inner.pool.availability.closed.get() {
            return Err("browser client is closed; payment was not submitted".to_string());
        }
        let returned = wallet
            .call4(
                &JsValue::NULL,
                &network,
                &quotes,
                &on_submission,
                &JsValue::UNDEFINED,
            )
            .map_err(js_error_message)?;
        let value = JsFuture::from(Promise::resolve(&returned))
            .await
            .map_err(js_error_message)?;
        let submission: BrowserPaymentSubmission =
            serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())?;
        let transactions = paid_transactions(&verified, &submission).map_err(|e| e.to_string())?;
        Ok((transactions, submission))
    }
}

/// Hand a paid state to the page as `{ record, proof }`, waiting for it if
/// the callback returns a promise.
async fn hand_over_paid(
    on_paid: &js_sys::Function,
    record: &Pointer,
    proof: &[u8],
) -> Result<(), JsValue> {
    let paid = js_sys::Object::new();
    js_sys::Reflect::set(
        &paid,
        &JsValue::from_str("record"),
        &Uint8Array::from(record.to_bytes().as_slice()),
    )?;
    js_sys::Reflect::set(&paid, &JsValue::from_str("proof"), &Uint8Array::from(proof))?;
    let returned = on_paid.call1(&JsValue::NULL, &paid)?;
    JsFuture::from(Promise::resolve(&returned)).await?;
    Ok(())
}
