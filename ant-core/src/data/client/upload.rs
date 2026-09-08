// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Upload coordination shared by native and browser storage/wallet adapters.

use super::batch::{ChunkPaymentPlan, PaidChunk, WaveAggregateStats};
use super::merkle::PaymentMode;
use super::upload_state::UploadState;
use super::{adaptive::observe_op, classify_error, Client};
use crate::data::error::{Error, PartialUploadSpend, Result};
use ant_protocol::evm::{Amount, QuoteHash, TxHash};
use bytes::Bytes;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};

/// Content identity and size, independent of where the bytes are staged.
#[derive(Debug, Clone, Copy)]
pub struct UploadRecord {
    /// Canonical content address.
    pub address: [u8; 32],
    /// Expected byte length.
    pub size: u64,
    /// Stable index understood by the byte-source adapter.
    pub index: usize,
}

/// Confirmed wallet result; proof construction remains in Rust.
#[derive(Debug, Default)]
pub struct UploadPayment {
    /// Transactions indexed by quote hash.
    pub transactions: HashMap<QuoteHash, TxHash>,
    /// Actual storage spend for this submission.
    pub amount: Amount,
    /// Gas spend, when the adapter can report it.
    pub gas: u128,
}

/// Confirmed Merkle settlement; Rust constructs all record proofs.
#[derive(Debug)]
pub struct MerkleUploadPayment {
    /// Winning pool reported by the confirmed vault event.
    pub winner_pool: [u8; 32],
    /// Actual amount from that event.
    pub amount: Amount,
    /// Gas spend if known.
    pub gas: u128,
}

#[cfg(feature = "native")]
/// Native adapters permit futures to move between executor threads.
pub trait AdapterBounds: Sync {}
#[cfg(feature = "native")]
impl<T: Sync> AdapterBounds for T {}
#[cfg(not(feature = "native"))]
/// Browser adapters run on the local browser executor.
pub trait AdapterBounds {}
#[cfg(not(feature = "native"))]
impl<T> AdapterBounds for T {}

/// Only byte loading, wallet submission, endpoint admission, and persistence vary by platform.
#[cfg_attr(not(feature = "native"), async_trait::async_trait(?Send))]
#[cfg_attr(feature = "native", async_trait::async_trait)]
pub trait UploadAdapter: AdapterBounds {
    /// Load one previously described record.
    async fn load(&self, record: UploadRecord) -> Result<Bytes>;
    /// Submit the native single-node payment plans and await confirmation.
    async fn pay(&self, plans: &[ChunkPaymentPlan]) -> Result<UploadPayment>;
    /// Submit a prepared Merkle batch and await its confirmed winner.
    async fn pay_merkle(
        &self,
        _batch: &super::merkle::PreparedMerkleBatch,
    ) -> Result<MerkleUploadPayment> {
        Err(Error::Payment(
            "wallet adapter does not support Merkle payments".into(),
        ))
    }
    /// Check transport-specific endpoint capabilities before payment.
    async fn admit(&self, _plan: &mut ChunkPaymentPlan) -> Result<()> {
        Ok(())
    }
    /// Persist before submission and again before any paid data is loaded/stored.
    async fn checkpoint(
        &self,
        _state: &UploadState,
        _payment: Option<&UploadPayment>,
    ) -> Result<()> {
        Ok(())
    }
    /// Adapter resource ceiling; the adaptive controller still controls scheduling.
    fn quote_limit(&self) -> usize {
        usize::MAX
    }
    /// Report a completed store or an already-present record.
    fn stored(&self, _stored: usize, _total: usize) {}
    /// Report a completed quote.
    fn quoted(&self, _quoted: usize, _total: usize) {}
}

/// Completed upload accounting.
#[derive(Debug, Default)]
pub struct UploadOutcome {
    /// Successfully stored or already-present addresses, including repeated input records.
    pub addresses: Vec<[u8; 32]>,
    /// New storage spend during this invocation.
    pub amount: Amount,
    /// New gas spend during this invocation.
    pub gas: u128,
    /// Shared per-record retry statistics.
    pub stats: WaveAggregateStats,
    /// Effective payment mode.
    pub mode: PaymentMode,
}

impl Client {
    /// Drive record uploads using platform adapters and portable recovery state.
    pub async fn upload_records<A: UploadAdapter>(
        &self,
        records: Vec<UploadRecord>,
        state: &mut UploadState,
        adapter: &A,
        mode: PaymentMode,
    ) -> Result<UploadOutcome> {
        let original = records.iter().map(|r| r.address).collect::<Vec<_>>();
        let mut sizes = HashMap::new();
        let mut unique = Vec::new();
        for record in records {
            match sizes.insert(record.address, record.size) {
                Some(size) if size != record.size => {
                    return Err(Error::InvalidData(
                        "one content address has conflicting sizes".into(),
                    ))
                }
                Some(_) => {}
                None => unique.push(record),
            }
        }
        match self
            .upload_unique_records(unique, state, adapter, mode)
            .await
        {
            Ok(mut result) => {
                let stored = result.addresses.into_iter().collect::<HashSet<_>>();
                result.addresses = original
                    .iter()
                    .copied()
                    .filter(|address| stored.contains(address))
                    .collect();
                adapter.stored(result.addresses.len(), original.len());
                Ok(result)
            }
            Err(Error::PartialUpload {
                stored,
                failed,
                spend,
                reason,
                ..
            }) => {
                let stored_set = stored.into_iter().collect::<HashSet<_>>();
                let failures = failed.into_iter().collect::<HashMap<_, _>>();
                let stored = original
                    .iter()
                    .filter(|a| stored_set.contains(*a))
                    .copied()
                    .collect::<Vec<_>>();
                let failed = original
                    .iter()
                    .filter(|a| !stored_set.contains(*a))
                    .map(|a| {
                        (
                            *a,
                            failures
                                .get(a)
                                .cloned()
                                .unwrap_or_else(|| "upload interrupted before storage".into()),
                        )
                    })
                    .collect::<Vec<_>>();
                Err(Error::PartialUpload {
                    stored_count: stored.len(),
                    stored,
                    failed_count: failed.len(),
                    failed,
                    total_chunks: original.len(),
                    spend,
                    reason,
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn upload_unique_records<A: UploadAdapter>(
        &self,
        records: Vec<UploadRecord>,
        state: &mut UploadState,
        adapter: &A,
        mode: PaymentMode,
    ) -> Result<UploadOutcome> {
        let total = records.len();
        let mut outcome = UploadOutcome {
            mode: PaymentMode::Single,
            ..Default::default()
        };
        let mut unique = records;
        self.prepare_upload_merkle(&mut unique, state, adapter, mode, &mut outcome)
            .await?;
        self.store_upload_merkle(&mut unique, state, adapter, &mut outcome, total)
            .await?;
        let waves = unique
            .chunks(super::batch::PAYMENT_WAVE_SIZE)
            .collect::<Vec<_>>();
        let mut prefetched = None;
        for (wave_index, wave) in waves.iter().enumerate() {
            let plans = match prefetched.take() {
                Some(plans) => plans?,
                None => {
                    self.prepare_upload_wave(wave, state, adapter, total)
                        .await?
                }
            };
            let mut payable = Vec::new();
            for (record, plan) in &plans {
                if let Some(plan) = plan {
                    if !state.is_paid(&plan.address, crate::runtime::system_time()) {
                        state.prepare(plan.clone());
                        payable.push(plan.clone());
                    }
                } else {
                    outcome.addresses.push(record.address);
                    adapter.stored(outcome.addresses.len(), total);
                }
            }
            adapter.checkpoint(state, None).await?;
            let payment = if payable.is_empty() {
                UploadPayment::default()
            } else {
                adapter.pay(&payable).await?
            };
            let expected = payable.iter().try_fold(Amount::ZERO, |sum, plan| {
                sum.checked_add(plan.payment.total_amount())
                    .ok_or_else(|| Error::Payment("payment total overflow".into()))
            })?;
            if payment.amount != expected {
                return Err(Error::Payment(
                    "wallet reported a different payment total".into(),
                ));
            }
            state.confirm(
                &plans
                    .iter()
                    .filter_map(|(_, plan)| plan.as_ref().map(|p| p.address))
                    .collect::<Vec<_>>(),
                &payment.transactions,
                crate::runtime::system_time(),
            )?;
            outcome.amount = outcome
                .amount
                .checked_add(payment.amount)
                .ok_or_else(|| Error::Payment("payment total overflow".into()))?;
            outcome.gas = outcome.gas.saturating_add(payment.gas);
            adapter.checkpoint(state, Some(&payment)).await?;
            let max_size = wave.iter().map(|r| r.size as usize).max().unwrap_or(1);
            let recovery = &*state;
            let stores = crate::client_engine::rolling_unordered(
                plans
                    .into_iter()
                    .filter_map(|(record, plan)| plan.map(|plan| (record, plan))),
                |(record, plan)| async move {
                    let result = async {
                        let bytes = adapter.load(record).await?;
                        let prepared = plan.with_content(bytes)?;
                        let paid: PaidChunk = recovery
                            .reuse_prepared(&prepared, crate::runtime::system_time())
                            .ok_or_else(|| {
                                Error::Payment("paid proof expired before storage".into())
                            })?;
                        Ok::<_, Error>(
                            self.store_paid_chunks_with_events(vec![paid], None, 0, total)
                                .await,
                        )
                    }
                    .await;
                    (record.address, result)
                },
                || {
                    self.controller()
                        .store
                        .current()
                        .min(crate::client_engine::store_byte_bound(max_size))
                },
            )
            .collect::<Vec<_>>();
            let (results, next) = futures::join!(stores, async {
                match waves.get(wave_index + 1) {
                    Some(next) => Some(self.prepare_upload_wave(next, state, adapter, total).await),
                    None => None,
                }
            });
            prefetched = next;
            let mut failed = Vec::new();
            for (address, result) in results {
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        failed.push((address, error.to_string()));
                        continue;
                    }
                };
                outcome.stats.absorb(&result);
                outcome.addresses.extend(result.stored);
                failed.extend(result.failed);
                adapter.stored(outcome.addresses.len(), total);
            }
            if !failed.is_empty() {
                return Err(Error::PartialUpload {
                    stored_count: outcome.addresses.len(),
                    stored: outcome.addresses,
                    failed_count: failed.len(),
                    reason: failed
                        .iter()
                        .map(|(_, error)| error.as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                    failed,
                    total_chunks: total,
                    spend: Box::new(PartialUploadSpend {
                        storage_cost_atto: outcome.amount.to_string(),
                        gas_cost_wei: outcome.gas,
                    }),
                });
            }
        }
        Ok(outcome)
    }
    async fn prepare_upload_wave<A: UploadAdapter>(
        &self,
        wave: &[UploadRecord],
        state: &UploadState,
        adapter: &A,
        total: usize,
    ) -> Result<Vec<(UploadRecord, Option<ChunkPaymentPlan>)>> {
        let recovery = state;
        let plans = crate::client_engine::rolling_unordered(
            wave.iter().copied(),
            |record| async move {
                let cached_merkle = recovery.proof(&record.address).is_some_and(|bytes| {
                    ant_protocol::payment::deserialize_merkle_proof(bytes).is_ok()
                }) && recovery
                    .is_paid(&record.address, crate::runtime::system_time());
                let mut plan = if cached_merkle {
                    Some(ChunkPaymentPlan {
                        address: record.address,
                        data_size: record.size,
                        quoted_peers: self.put_target_peers(&record.address).await?,
                        payment: super::batch::SingleNodeQuotePayment { quotes: Vec::new() },
                        peer_quotes: Vec::new(),
                        commitment_sidecars: Vec::new(),
                    })
                } else {
                    match recovery.retained_plan(
                        &record.address,
                        record.size,
                        crate::runtime::system_time(),
                    ) {
                        Some(plan) => Some(plan),
                        None => {
                            observe_op(
                                &self.controller().quote,
                                || self.prepare_chunk_payment_plan(record.address, record.size),
                                classify_error,
                            )
                            .await?
                        }
                    }
                };
                if let Some(plan) = plan.as_mut() {
                    adapter.admit(plan).await?;
                }
                adapter.quoted(record.index + 1, total);
                Ok::<_, Error>((record, plan))
            },
            || self.controller().quote.current().min(adapter.quote_limit()),
        )
        .collect::<Vec<_>>()
        .await;
        let mut plans = plans.into_iter().collect::<Result<Vec<_>>>()?;
        plans.sort_by_key(|(record, _)| record.index);
        Ok(plans)
    }

    async fn store_upload_merkle<A: UploadAdapter>(
        &self,
        records: &mut Vec<UploadRecord>,
        state: &UploadState,
        adapter: &A,
        outcome: &mut UploadOutcome,
        total: usize,
    ) -> Result<()> {
        use super::merkle::{
            merkle_deferred_retry, merkle_store_with_retry, DEFERRED_ROUND_DELAYS_SECS,
        };
        let merkle = records
            .iter()
            .filter(|r| {
                state.is_paid(&r.address, crate::runtime::system_time())
                    && state
                        .proof(&r.address)
                        .is_some_and(|p| ant_protocol::payment::deserialize_merkle_proof(p).is_ok())
            })
            .map(|r| (r.address, *r))
            .collect::<HashMap<_, _>>();
        if merkle.is_empty() {
            return Ok(());
        }
        outcome.mode = PaymentMode::Merkle;
        let max_size = merkle.values().map(|r| r.size as usize).max().unwrap_or(1);
        let cap = || {
            self.controller()
                .store
                .current()
                .min(crate::client_engine::store_byte_bound(max_size))
        };
        let store_one = |address: [u8; 32]| {
            let merkle = &merkle;
            async move {
                let started = web_time::Instant::now();
                let record = *merkle
                    .get(&address)
                    .ok_or_else(|| Error::InvalidData("missing Merkle record".into()))?;
                let bytes = adapter.load(record).await?;
                if bytes.len() as u64 != record.size {
                    return Err(Error::InvalidData("staged record size changed".into()));
                }
                crate::record::verify(&address, &bytes).map_err(Error::InvalidData)?;
                let proof = state
                    .proof(&address)
                    .cloned()
                    .ok_or_else(|| Error::Payment("missing Merkle proof".into()))?;
                let peers = self.put_target_peers(&address).await?;
                observe_op(
                    &self.controller().store,
                    || self.chunk_put_to_close_group(bytes, proof, &peers),
                    classify_error,
                )
                .await?;
                Ok(started)
            }
        };
        let result = merkle_store_with_retry(
            merkle.keys().copied().collect(),
            cap,
            1,
            std::time::Duration::ZERO,
            None,
            outcome.addresses.len(),
            total,
            &store_one,
        )
        .await?;
        outcome.addresses.extend(result.stored_addresses);
        merge_stats(&mut outcome.stats, result.stats);
        let mut fatal = result.fatal.map(|e| e.to_string());
        let mut failed = result.failed_addresses;
        if fatal.is_none() && !failed.is_empty() {
            let result = merkle_deferred_retry(
                failed,
                &DEFERRED_ROUND_DELAYS_SECS,
                |_| cap(),
                None,
                outcome.addresses.len(),
                total,
                &store_one,
            )
            .await?;
            outcome.addresses.extend(result.stored_addresses);
            merge_stats(&mut outcome.stats, result.stats);
            failed = result.failed_addresses;
            fatal = result.fatal;
        }
        adapter.stored(outcome.addresses.len(), total);
        if fatal.is_some() || !failed.is_empty() {
            let landed = outcome.addresses.iter().copied().collect::<HashSet<_>>();
            let messages = failed.into_iter().collect::<HashMap<_, _>>();
            let failed = records
                .iter()
                .filter(|r| !landed.contains(&r.address))
                .map(|r| {
                    (
                        r.address,
                        messages.get(&r.address).cloned().unwrap_or_else(|| {
                            fatal
                                .clone()
                                .unwrap_or_else(|| "upload interrupted before storage".into())
                        }),
                    )
                })
                .collect::<Vec<_>>();
            return Err(Error::PartialUpload {
                stored: outcome.addresses.clone(),
                stored_count: outcome.addresses.len(),
                failed_count: failed.len(),
                failed,
                total_chunks: total,
                spend: Box::new(PartialUploadSpend {
                    storage_cost_atto: outcome.amount.to_string(),
                    gas_cost_wei: outcome.gas,
                }),
                reason: fatal.unwrap_or_else(|| {
                    "Merkle storage short of quorum after deferred retries".into()
                }),
            });
        }
        records.retain(|r| !merkle.contains_key(&r.address));
        Ok(())
    }

    async fn prepare_upload_merkle<A: UploadAdapter>(
        &self,
        records: &mut Vec<UploadRecord>,
        state: &mut UploadState,
        adapter: &A,
        mode: PaymentMode,
        outcome: &mut UploadOutcome,
    ) -> Result<()> {
        use super::merkle::{finalize_merkle_batch, merkle_batch_partitions, should_use_merkle};
        if state.pending_merkle.as_ref().is_some_and(|batch| {
            !super::upload_state::merkle_fresh(
                batch.merkle_payment_timestamp,
                crate::runtime::system_time(),
            )
        }) {
            state.pending_merkle = None;
        }
        if let Some(batch) = state.pending_merkle.take() {
            if batch
                .addresses()
                .iter()
                .any(|address| !records.iter().any(|r| r.address == *address))
            {
                state.pending_merkle = Some(batch);
                return Err(Error::InvalidData(
                    "pending Merkle batch belongs to different records".into(),
                ));
            }
            state.pending_merkle = Some(batch);
            let batch = state
                .pending_merkle
                .as_ref()
                .ok_or_else(|| Error::Payment("missing pending Merkle batch".into()))?;
            let payment = adapter.pay_merkle(batch).await?;
            let paid = finalize_merkle_batch(batch.clone(), payment.winner_pool)?;
            state.insert_merkle(paid);
            let receipt = UploadPayment {
                amount: payment.amount,
                gas: payment.gas,
                ..Default::default()
            };
            outcome.amount += receipt.amount;
            outcome.gas = outcome.gas.saturating_add(receipt.gas);
            outcome.mode = PaymentMode::Merkle;
            adapter.checkpoint(state, Some(&receipt)).await?;
        }
        let unpaid = records
            .iter()
            .filter(|r| !state.is_paid(&r.address, crate::runtime::system_time()))
            .copied()
            .collect::<Vec<_>>();
        if !should_use_merkle(unpaid.len(), mode) {
            return Ok(());
        }
        let entries = unpaid.iter().map(|r| (r.address, r.size)).collect();
        let plan = match self
            .plan_merkle_upload(entries, ant_protocol::DATA_TYPE_CHUNK, None)
            .await
        {
            Ok(plan) => plan,
            Err(Error::InsufficientPeers(_)) if mode == PaymentMode::Auto => return Ok(()),
            Err(error) => return Err(error),
        };
        let present = plan.already_stored.iter().copied().collect::<HashSet<_>>();
        records.retain(|r| !present.contains(&r.address));
        outcome
            .addresses
            .extend(plan.already_stored.iter().copied());
        if plan.to_upload.is_empty() {
            outcome.mode = PaymentMode::Merkle;
            return Ok(());
        }
        if !should_use_merkle(plan.to_upload.len(), mode) {
            return Ok(());
        }
        for addresses in merkle_batch_partitions(&plan.to_upload) {
            let batch = match self
                .prepare_merkle_batch_external(
                    addresses,
                    ant_protocol::DATA_TYPE_CHUNK,
                    plan.to_upload_avg_size(),
                )
                .await
            {
                Ok(batch) => batch,
                Err(Error::InsufficientPeers(_)) if mode == PaymentMode::Auto => return Ok(()),
                Err(error) => return Err(error),
            };
            state.pending_merkle = Some(batch);
            adapter.checkpoint(state, None).await?;
            let batch = state
                .pending_merkle
                .as_ref()
                .ok_or_else(|| Error::Payment("missing prepared Merkle batch".into()))?;
            let payment = adapter.pay_merkle(batch).await?;
            let paid = finalize_merkle_batch(batch.clone(), payment.winner_pool)?;
            state.insert_merkle(paid);
            let receipt = UploadPayment {
                amount: payment.amount,
                gas: payment.gas,
                ..Default::default()
            };
            outcome.amount = outcome
                .amount
                .checked_add(receipt.amount)
                .ok_or_else(|| Error::Payment("payment total overflow".into()))?;
            outcome.gas = outcome.gas.saturating_add(receipt.gas);
            outcome.mode = PaymentMode::Merkle;
            adapter.checkpoint(state, Some(&receipt)).await?;
        }
        Ok(())
    }
}

/// Native wallet and in-memory byte source for the shared upload coordinator.
pub(crate) struct MemoryUploadAdapter<'a> {
    pub client: &'a Client,
    pub chunks: &'a [Bytes],
    pub progress: Option<&'a tokio::sync::mpsc::Sender<super::file::UploadEvent>>,
    pub stored_offset: usize,
    pub file_total: usize,
    pub resume_key: Option<&'a str>,
}

#[cfg_attr(not(feature = "native"), async_trait::async_trait(?Send))]
#[cfg_attr(feature = "native", async_trait::async_trait)]
impl UploadAdapter for MemoryUploadAdapter<'_> {
    async fn load(&self, record: UploadRecord) -> Result<Bytes> {
        self.chunks
            .get(record.index)
            .cloned()
            .ok_or_else(|| Error::InvalidData("missing staged record".into()))
    }
    async fn pay(&self, plans: &[ChunkPaymentPlan]) -> Result<UploadPayment> {
        let wallet = self.client.require_wallet()?;
        let payments = plans
            .iter()
            .flat_map(|plan| {
                plan.payment
                    .quotes
                    .iter()
                    .map(|q| (q.quote_hash, q.rewards_address, q.amount))
            })
            .collect::<Vec<_>>();
        let (transactions, gas) = wallet.pay_for_quotes(payments).await.map_err(
            |ant_protocol::evm::PayForQuotesError(error, _)| Error::Payment(error.to_string()),
        )?;
        let amount = plans.iter().try_fold(Amount::ZERO, |sum, plan| {
            sum.checked_add(plan.payment.total_amount())
                .ok_or_else(|| Error::Payment("payment total overflow".into()))
        })?;
        Ok(UploadPayment {
            transactions: transactions.into_iter().collect(),
            amount,
            gas: gas.gas_cost_wei,
        })
    }
    async fn pay_merkle(
        &self,
        batch: &super::merkle::PreparedMerkleBatch,
    ) -> Result<MerkleUploadPayment> {
        let (winner_pool, amount, gas) = self
            .client
            .require_wallet()?
            .pay_for_merkle_tree(
                batch.depth,
                batch.pool_commitments.clone(),
                batch.merkle_payment_timestamp,
            )
            .await
            .map_err(|e| Error::Payment(e.to_string()))?;
        Ok(MerkleUploadPayment {
            winner_pool,
            amount,
            gas: gas.gas_cost_wei,
        })
    }
    async fn checkpoint(&self, state: &UploadState, payment: Option<&UploadPayment>) -> Result<()> {
        #[cfg(feature = "native")]
        if let (Some(key), Some(payment)) = (self.resume_key, payment) {
            super::cached_single::try_append_wave(
                key,
                state.proofs().clone(),
                &payment.amount.to_string(),
                payment.gas,
            );
        }
        #[cfg(not(feature = "native"))]
        let _ = (state, payment, self.resume_key);
        Ok(())
    }
    fn stored(&self, stored: usize, total: usize) {
        if let Some(progress) = self.progress {
            let _ = progress.try_send(super::file::UploadEvent::ChunkStored {
                stored: self.stored_offset + stored,
                total: self.file_total.max(total),
            });
        }
    }
    fn quoted(&self, quoted: usize, total: usize) {
        if let Some(progress) = self.progress {
            let _ = progress.try_send(super::file::UploadEvent::ChunkQuoted {
                quoted: self.stored_offset + quoted,
                total: self.file_total.max(total),
            });
        }
    }
}

fn merge_stats(total: &mut WaveAggregateStats, next: WaveAggregateStats) {
    total.chunk_attempts_total = total
        .chunk_attempts_total
        .saturating_add(next.chunk_attempts_total);
    total.store_durations_ms.extend(next.store_durations_ms);
    for (total, next) in total
        .retries_histogram
        .iter_mut()
        .zip(next.retries_histogram)
    {
        *total = total.saturating_add(next);
    }
}
