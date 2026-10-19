// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pointers from the browser (ADR-0016 in `ant-node`): read, resolve, create,
//! update, and hand over for good (ADR-0018), over the same client the native
//! API uses.
//!
//! Reads, the quorum and corroboration rules, the payment plan and the write
//! fan-out are the native client's own; only the wallet is the page's. The
//! owner key is handed in as the 32-byte FIPS 204 seed it derives from, so a
//! page keeps 32 bytes rather than a 4,032-byte secret key. The page is trusted
//! with that seed exactly as it is trusted with its wallet.

use super::upload_adapter::{paid_transactions, quote_transactions, wallet_quotes};
use super::*;
use crate::data::client::batch::{paid_proof_is_safely_fresh, ChunkPaymentPlan};
use crate::data::{
    ml_dsa_65, pointer_address, FinalState, FinalityStatus, MlDsaPublicKey, MlDsaSecretKey,
    Pointer, PointerFinality, PointerTarget, PointerTargetKind,
};
use ant_protocol::evm::{QuoteHash, TxHash};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// Length of an owner key seed.
const OWNER_SEED_LEN: usize = 32;

/// Format of the checkpoint `onPrepared` is handed and
/// [`recover_pointer_payment`] reads.
const POINTER_CHECKPOINT_VERSION: u64 = 1;

/// Largest checkpoint [`recover_pointer_payment`] reads. One signed record
/// and the quotes of one close group take a small fraction of it; a write
/// whose checkpoint would not fit is refused before anything is paid.
const MAX_POINTER_CHECKPOINT_BYTES: usize = 4 * 1024 * 1024;

/// How long a write waits on a page callback. An `onPrepared` that does not
/// settle in time fails the write before anything is paid; a paid state is
/// stored anyway once `onPaid` has had this long, since the payment is spent
/// either way.
#[cfg(not(feature = "test-utils"))]
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Short enough that a test can wait out a callback that never settles.
#[cfg(feature = "test-utils")]
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(1);

/// Ends every error a write returns before its wallet is invoked: nothing
/// was paid, and retrying cannot pay twice.
const NOT_SUBMITTED: &str = "payment was not submitted";

/// The pointer write option awaited with the checkpoint before paying.
const ON_PREPARED_OPTION: &str = "onPrepared";

/// The pointer write option handed each transaction the wallet broadcasts.
const ON_SUBMISSION_OPTION: &str = "onSubmission";

/// The pointer write option handed the paid state before storing it.
const ON_PAID_OPTION: &str = "onPaid";

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

/// A final state as JavaScript sees it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserFinalState {
    state_id: String,
    kind: String,
    kind_tag: u8,
    target: String,
    holders: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    transferred_to: Option<String>,
}

impl From<&FinalState> for BrowserFinalState {
    fn from(state: &FinalState) -> Self {
        let target = PointerTarget::from_raw_tag(state.kind_tag, state.target);
        Self {
            state_id: hex::encode(state.state_id),
            kind: kind_name(&target).to_string(),
            kind_tag: state.kind_tag,
            target: hex::encode(state.target),
            holders: state.holders,
            transferred_to: state.transferred_to().map(hex::encode),
        }
    }
}

/// Where a pointer stands with respect to finality, as JavaScript sees it.
///
/// `status` is `"open"`, `"settling"`, `"final"`, `"unconfirmed"` (final on a
/// majority, but not every peer answered) or `"forked"`. `states` holds every
/// final state seen, most-held first; `majority` the one reads return.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserPointerFinality {
    address: String,
    group: usize,
    answered: usize,
    status: &'static str,
    /// Decimal, for an open pointer: a counter can exceed JavaScript's safe
    /// integers.
    #[serde(skip_serializing_if = "Option::is_none")]
    counter: Option<String>,
    states: Vec<BrowserFinalState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    majority: Option<BrowserFinalState>,
}

impl From<&PointerFinality> for BrowserPointerFinality {
    fn from(finality: &PointerFinality) -> Self {
        let (status, counter, states, majority) = match &finality.status {
            FinalityStatus::Open { counter } => {
                ("open", counter.map(|c| c.to_string()), Vec::new(), None)
            }
            FinalityStatus::Settling(state) => ("settling", None, vec![state.into()], None),
            FinalityStatus::Final(state) => ("final", None, vec![state.into()], Some(state.into())),
            FinalityStatus::Unconfirmed(state) => {
                ("unconfirmed", None, vec![state.into()], Some(state.into()))
            }
            FinalityStatus::Forked { states, majority } => (
                "forked",
                None,
                states.iter().map(BrowserFinalState::from).collect(),
                majority.as_ref().map(BrowserFinalState::from),
            ),
        };
        Self {
            address: hex::encode(finality.address),
            group: finality.group,
            answered: finality.answered,
            status,
            counter,
            states,
            majority,
        }
    }
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
    /// `options` is `{ onPrepared, onSubmission, onPaid }`, each optional.
    /// An unknown option is refused before anything is paid.
    ///
    /// - `onPrepared` is awaited with an opaque checkpoint string, the signed
    ///   record and its payment plan (never the owner seed), before the
    ///   wallet is asked to pay. Persist it durably before it settles; if it
    ///   fails, or does not settle within 30 seconds, nothing is paid.
    /// - `onSubmission` is handed what the wallet reports for each
    ///   transaction it broadcasts, `{ transactionHash }` or
    ///   `{ transactionHashes }`, and the wallet waits on its promise. Keep
    ///   it: with the checkpoint, [`recover_pointer_payment`] rebuilds the
    ///   paid state once the transaction is confirmed, so a page lost before
    ///   the receipt never pays twice.
    /// - `onPaid` is called with `{ record, proof }` once the state is paid
    ///   for and before it is stored. Keep both: if storing fails, or the page
    ///   is lost, [`Self::store_paid_pointer`] stores the same state without
    ///   paying again.
    ///
    /// Nothing is paid unless `pay_for_quotes` is called. A write that fails
    /// before calling it says "payment was not submitted"; any later failure
    /// may have paid, and is recovered from what the callbacks were handed.
    #[wasm_bindgen(js_name = createPointer)]
    pub async fn create_pointer(
        &self,
        owner_seed: &[u8],
        target: &str,
        kind: &str,
        payment_network: JsValue,
        pay_for_quotes: js_sys::Function,
        options: Option<JsValue>,
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
                options,
            )
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        to_js(&written)
    }

    /// Point the pointer `owner_seed` controls at `target`, one past the
    /// counter the network serves (or create it), and pay for the new state.
    /// `options` is as for [`Self::create_pointer`].
    #[wasm_bindgen(js_name = updatePointer)]
    pub async fn update_pointer(
        &self,
        owner_seed: &[u8],
        target: &str,
        kind: &str,
        payment_network: JsValue,
        pay_for_quotes: js_sys::Function,
        options: Option<JsValue>,
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
                options,
            )
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        to_js(&written)
    }

    /// Hand the pointer `owner_seed` controls over to the pointer at
    /// `recipient`, for good, and pay for the final state. `options` is as
    /// for [`Self::create_pointer`].
    ///
    /// Readers of the pointer's address are redirected to `recipient`, which
    /// only its owner can move, and `owner_seed` can change nothing any more.
    /// Refused before anything is paid if the pointer is already final, or if
    /// `recipient` does not exist or leads back to this pointer. Check the
    /// result with [`Self::pointer_finality`] before relying on it.
    #[wasm_bindgen(js_name = transferPointer)]
    pub async fn transfer_pointer(
        &self,
        owner_seed: &[u8],
        recipient: &str,
        payment_network: JsValue,
        pay_for_quotes: js_sys::Function,
        options: Option<JsValue>,
    ) -> Result<JsValue, JsValue> {
        let payment_network = parse_payment_network(payment_network)?;
        let written = self
            .write_pointer(
                PointerWrite::Transfer,
                owner_seed,
                recipient,
                "pointer",
                &payment_network,
                &pay_for_quotes,
                options,
            )
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        to_js(&written)
    }

    /// Ask the whole close group whether the pointer at `address` is final,
    /// and on what. Only `status: "final"` means one final state, held by a
    /// majority of the group, with no rival.
    #[wasm_bindgen(js_name = pointerFinality)]
    pub async fn pointer_finality(&self, address: &str) -> Result<JsValue, JsValue> {
        let at = parse_lookup_key(address, "pointer address").map_err(|e| JsValue::from_str(&e))?;
        let finality = self
            .shared
            .pointer_finality(&at)
            .await
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        to_js(&BrowserPointerFinality::from(&finality))
    }

    /// Store a pointer state already paid for, from the `record` and `proof`
    /// an earlier write handed to its `onPaid` callback or
    /// [`recover_pointer_payment`] rebuilt. Pays nothing.
    ///
    /// Storers accept a proof only while its quotes are under a day old; a
    /// refusal of an older proof says it expired.
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
        let client = self.paying_client(&payment_network);
        let refused = |error: crate::data::Error| JsValue::from_str(&explain_refusal(proof, error));
        if let Err(error) = client.pointer_put_paid(&record, proof.to_vec()).await {
            // As for a transfer written the first time: a final state whose
            // acknowledgements were all lost may have landed anyway.
            if !record.is_terminal() {
                return Err(refused(error));
            }
            client
                .recover_final_write(&record, error)
                .await
                .map_err(refused)?;
        }
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
    /// The final state, handing the pointer over to the target pointer.
    Transfer,
}

/// The callbacks a pointer write takes in its trailing `options` argument.
#[derive(Default)]
struct PointerWriteOptions {
    /// Awaited with the recovery checkpoint before the wallet is asked to pay.
    on_prepared: Option<js_sys::Function>,
    /// Handed each transaction the wallet reports broadcasting.
    on_submission: Option<js_sys::Function>,
    /// Handed `{ record, proof }` once the state is paid for.
    on_paid: Option<js_sys::Function>,
}

impl PointerWriteOptions {
    /// Read `{ onPrepared, onSubmission, onPaid }`. An unknown option is
    /// refused rather than ignored: a misspelt `onPrepared` would otherwise
    /// pay with no checkpoint kept.
    fn parse(value: Option<JsValue>) -> Result<Self, String> {
        let Some(value) = value.filter(|value| !value.is_undefined() && !value.is_null()) else {
            return Ok(Self::default());
        };
        // A function is not an object here: earlier releases took a bare
        // `onPaid`, which must not now be read as an empty options object.
        if !value.is_object() {
            return Err("pointer write options must be an object".into());
        }
        reject_unknown_keys(
            &value,
            &[ON_PREPARED_OPTION, ON_SUBMISSION_OPTION, ON_PAID_OPTION],
        )?;
        Ok(Self {
            on_prepared: option_function(&value, ON_PREPARED_OPTION)?,
            on_submission: option_function(&value, ON_SUBMISSION_OPTION)?,
            on_paid: option_function(&value, ON_PAID_OPTION)?,
        })
    }
}

/// The function `options` holds at `name`, if any.
fn option_function(options: &JsValue, name: &str) -> Result<Option<js_sys::Function>, String> {
    let value =
        js_sys::Reflect::get(options, &JsValue::from_str(name)).map_err(js_error_message)?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value
        .dyn_into()
        .map(Some)
        .map_err(|_| format!("{name} must be a function"))
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
        options: Option<JsValue>,
    ) -> Result<BrowserPointerWrite, String> {
        let options = PointerWriteOptions::parse(options)?;
        let (owner, secret) = owner_keys(owner_seed)?;
        let target = parse_target(target, kind)?;
        let client = self.paying_client(payment_network);
        let record = match write {
            PointerWrite::Create => client.pointer_sign_create(&secret, &owner, target).await,
            PointerWrite::Update => client.pointer_sign_update(&secret, &owner, target).await,
            PointerWrite::Transfer => {
                client
                    .pointer_sign_transfer(&secret, &owner, target.address)
                    .await
            }
        }
        .map_err(|error| error.to_string())?;
        self.store_pointer(&client, &record, payment_network, pay_for_quotes, &options)
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
        options: &PointerWriteOptions,
    ) -> Result<BrowserPointerWrite, String> {
        let plan = client
            .prepare_pointer_payment(record)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(on_prepared) = &options.on_prepared {
            let checkpoint = PointerPaymentCheckpoint::encode(record, &plan)?;
            // Nothing is paid until the page has kept the checkpoint.
            crate::runtime::timeout(
                CALLBACK_TIMEOUT,
                call_and_wait(on_prepared, &JsValue::from_str(&checkpoint)),
            )
            .await
            .map_err(|_| {
                format!(
                    "onPrepared did not settle within {}s; {NOT_SUBMITTED}",
                    CALLBACK_TIMEOUT.as_secs()
                )
            })?
            .map_err(|error| {
                format!(
                    "onPrepared failed: {}; {NOT_SUBMITTED}",
                    js_error_message(error)
                )
            })?;
        }
        let (transactions, submission) = self
            .pay_plan(
                client,
                &plan,
                payment_network,
                pay_for_quotes,
                options.on_submission.as_ref(),
            )
            .await?;
        let proof = plan
            .proof(&transactions)
            .map_err(|error| error.to_string())?;
        if let Some(on_paid) = &options.on_paid {
            // The payment is spent whatever the page does with this, so a
            // callback that fails, or never settles, does not stop the state
            // being stored.
            let hand_over = async {
                let paid = paid_state(&record.to_bytes(), &proof)?;
                call_and_wait(on_paid, &paid).await
            };
            let _ = crate::runtime::timeout(CALLBACK_TIMEOUT, hand_over).await;
        }
        if let Err(error) = client.pointer_put_paid(record, proof).await {
            // A final state cannot be taken back, and a write whose
            // acknowledgements were all lost may have landed anyway; the
            // group says, as it does for a native transfer.
            if !record.is_terminal() {
                return Err(error.to_string());
            }
            client
                .recover_final_write(record, error)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(BrowserPointerWrite {
            pointer: BrowserPointer::from(record),
            transaction_hash: submission.transaction_hash,
            storage_cost_atto: submission.total_amount,
        })
    }

    /// Ask the page's wallet to pay one pointer state.
    ///
    /// The upload path journals each broadcast so that an interrupted payment
    /// is recovered rather than repeated. A pointer write keeps no journal of
    /// its own: the page keeps the `onPrepared` checkpoint and each broadcast
    /// the wallet reports, forwarded to `on_submission`, from which
    /// [`recover_pointer_payment`] rebuilds the paid state. Once paid,
    /// `onPaid` hands it the record and proof, and from then on nothing is
    /// paid twice.
    async fn pay_plan(
        &self,
        client: &crate::data::Client,
        plan: &ChunkPaymentPlan,
        payment_network: &BrowserPaymentNetwork,
        wallet: &js_sys::Function,
        on_submission: Option<&js_sys::Function>,
    ) -> Result<(HashMap<QuoteHash, TxHash>, BrowserPaymentSubmission), String> {
        let verified = wallet_quotes(std::slice::from_ref(plan)).map_err(|e| e.to_string())?;
        let network = serde_wasm_bindgen::to_value(payment_network).map_err(|e| e.to_string())?;
        let quotes = serde_wasm_bindgen::to_value(&verified).map_err(|e| e.to_string())?;
        let on_submission = on_submission.cloned();
        // The wallet waits on the promise this returns, so the page has kept
        // the transaction before the wallet goes on.
        let on_submission =
            Closure::<dyn FnMut(JsValue) -> Promise>::new(
                move |sent: JsValue| match &on_submission {
                    Some(callback) => match callback.call1(&JsValue::NULL, &sent) {
                        Ok(returned) => Promise::resolve(&returned),
                        Err(error) => Promise::reject(&error),
                    },
                    None => Promise::resolve(&JsValue::UNDEFINED),
                },
            )
            .into_js_value();
        // Checked again here, not only when quoting: `onPrepared` may have
        // taken long enough for another write to learn that this build
        // cannot settle.
        if let Some(refusal) = client.corroborated_settlement_refusal() {
            return Err(format!(
                "{}; {NOT_SUBMITTED}",
                crate::data::Error::ClientUpdateRequired(refusal)
            ));
        }
        if self.inner.pool.availability.closed.get() {
            return Err(format!("browser client is closed; {NOT_SUBMITTED}"));
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

/// Call a page callback with `argument`, waiting for it if it returns a
/// promise.
async fn call_and_wait(callback: &js_sys::Function, argument: &JsValue) -> Result<(), JsValue> {
    let returned = callback.call1(&JsValue::NULL, argument)?;
    JsFuture::from(Promise::resolve(&returned)).await?;
    Ok(())
}

/// A paid state as `onPaid` is handed it and [`BrowserNetworkClient::store_paid_pointer`]
/// takes it: `{ record, proof }`.
fn paid_state(record: &[u8], proof: &[u8]) -> Result<JsValue, JsValue> {
    let paid = js_sys::Object::new();
    js_sys::Reflect::set(
        &paid,
        &JsValue::from_str("record"),
        &Uint8Array::from(record),
    )?;
    js_sys::Reflect::set(&paid, &JsValue::from_str("proof"), &Uint8Array::from(proof))?;
    Ok(paid.into())
}

/// A storer's refusal of a paid state, saying so when the proof has expired:
/// that refusal is final, unlike a network failure worth retrying.
fn explain_refusal(proof: &[u8], error: crate::data::Error) -> String {
    match paid_proof_is_safely_fresh(proof, crate::runtime::system_time()) {
        Some(false) => format!(
            "paid proof expired: its quotes are older than storers accept, so this state \
             cannot be stored without paying again ({error})"
        ),
        _ => error.to_string(),
    }
}

/// What `onPrepared` is handed, as JSON: the signed record and the plan its
/// payment follows, each base64 so that their bytes are not spelt out as
/// JSON numbers, the plan in MessagePack. Holds no owner seed or secret key.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PointerPaymentCheckpoint {
    version: u64,
    record: String,
    plan: String,
}

impl PointerPaymentCheckpoint {
    /// The checkpoint string, refused if recovery could not read it back.
    fn encode(record: &Pointer, plan: &ChunkPaymentPlan) -> Result<String, String> {
        let plan = rmp_serde::to_vec_named(plan).map_err(|e| e.to_string())?;
        let checkpoint = serde_json::to_string(&Self {
            version: POINTER_CHECKPOINT_VERSION,
            record: URL_SAFE_NO_PAD.encode(record.to_bytes()),
            plan: URL_SAFE_NO_PAD.encode(plan),
        })
        .map_err(|e| e.to_string())?;
        if checkpoint.len() > MAX_POINTER_CHECKPOINT_BYTES {
            return Err(format!(
                "pointer checkpoint exceeds the {MAX_POINTER_CHECKPOINT_BYTES} bytes recovery \
                 reads; {NOT_SUBMITTED}"
            ));
        }
        Ok(checkpoint)
    }

    /// The record bytes and plan a checkpoint string holds.
    fn decode(checkpoint: &str) -> Result<(Vec<u8>, ChunkPaymentPlan), String> {
        let invalid =
            |error: &dyn std::fmt::Display| format!("invalid pointer checkpoint: {error}");
        if checkpoint.len() > MAX_POINTER_CHECKPOINT_BYTES {
            return Err(format!(
                "pointer checkpoint exceeds {MAX_POINTER_CHECKPOINT_BYTES} bytes"
            ));
        }
        let CheckpointVersion { version } =
            serde_json::from_str(checkpoint).map_err(|e| invalid(&e))?;
        if version != POINTER_CHECKPOINT_VERSION {
            return Err(format!("unsupported pointer checkpoint version {version}"));
        }
        let saved: Self = serde_json::from_str(checkpoint).map_err(|e| invalid(&e))?;
        let record = URL_SAFE_NO_PAD
            .decode(saved.record)
            .map_err(|e| invalid(&e))?;
        let plan = URL_SAFE_NO_PAD
            .decode(saved.plan)
            .map_err(|e| invalid(&e))?;
        let plan = rmp_serde::from_slice(&plan).map_err(|e| invalid(&e))?;
        Ok((record, plan))
    }
}

/// A checkpoint's version alone, read first so that a checkpoint from a
/// newer release is reported as unsupported rather than as malformed.
#[derive(Deserialize)]
struct CheckpointVersion {
    version: u64,
}

/// What recovery reads of a receipt, or of a submission the wallet reported:
/// `transactionHashes`, keyed by the quote each paid, or `transactionHash`,
/// one transaction paying the checkpoint's one quote, as the write itself
/// accepts. Other fields, such as `totalAmount`, are ignored.
#[derive(Deserialize)]
struct PointerPaymentReceipt {
    #[serde(default, rename = "transactionHashes")]
    transaction_hashes: HashMap<String, String>,
    #[serde(default, rename = "transactionHash")]
    transaction_hash: Option<String>,
}

/// Rebuild an already-paid state, `{ record, proof }` for
/// [`BrowserNetworkClient::store_paid_pointer`], from the checkpoint a write
/// handed `onPrepared` and the receipt of the payment that followed it, or
/// the submission `onSubmission` was handed: `{ transactionHash }` or
/// `{ transactionHashes: { <quote hash>: <transaction hash> } }`.
///
/// Does not quote, sign, submit payment, or contact the network. A keyed
/// receipt must name this checkpoint's quote; beyond that the transaction is
/// not checked, so confirm it on chain first. Only use an application's own
/// persisted checkpoint; this never authorizes fresh payment.
#[wasm_bindgen(js_name = recoverPointerPayment)]
pub fn recover_pointer_payment(checkpoint: &str, receipt: JsValue) -> Result<JsValue, JsValue> {
    let receipt: PointerPaymentReceipt = serde_wasm_bindgen::from_value(receipt)
        .map_err(|e| JsValue::from_str(&format!("invalid pointer payment receipt: {e}")))?;
    let (record, proof) =
        recover_paid_state(checkpoint, &receipt).map_err(|e| JsValue::from_str(&e))?;
    paid_state(&record, &proof)
}

/// The record and proof [`recover_pointer_payment`] returns.
fn recover_paid_state(
    checkpoint: &str,
    receipt: &PointerPaymentReceipt,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let (record_bytes, plan) = PointerPaymentCheckpoint::decode(checkpoint)?;
    let record = Pointer::from_bytes(&record_bytes).map_err(|e| e.to_string())?;
    if plan.address != record.state_id() || plan.data_size != record_bytes.len() as u64 {
        return Err("pointer record differs from its payment plan".into());
    }
    let verified = wallet_quotes(std::slice::from_ref(&plan)).map_err(|e| e.to_string())?;
    let transactions = quote_transactions(
        &verified,
        &receipt.transaction_hashes,
        receipt.transaction_hash.as_deref(),
    )
    .map_err(|e| format!("receipt does not pay this checkpoint's quote: {e}"))?;
    let proof = plan.proof(&transactions).map_err(|e| e.to_string())?;
    Ok((record_bytes, proof))
}
