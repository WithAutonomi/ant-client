//! Bootstrap authentication belongs to the network pool. A single bounded
//! worker retains healthy sessions and publishes each result without waiting
//! for slower seeds. Pool closure also cancels pending handshakes.
use super::*;
use futures_util::{stream, StreamExt};

#[derive(Clone, Default)]
pub(super) struct State {
    pub ready: Vec<BrowserHello>,
    pub failures: Vec<BrowserLookupFailure>,
    pub pending: usize,
}

pub(super) struct Bootstrap {
    started: Cell<bool>,
    expected: RefCell<Option<BrowserPaymentNetwork>>,
    state: watch::Sender<State>,
}

impl Bootstrap {
    pub fn new() -> Self {
        let (state, _) = watch::channel(State::default());
        Self {
            started: Cell::new(false),
            expected: RefCell::new(None),
            state,
        }
    }

    pub fn subscribe(
        &self,
        core: &BrowserNetworkCore,
        expected: Option<BrowserPaymentNetwork>,
    ) -> Result<watch::Receiver<State>, String> {
        if core.pool.availability.closed.get() {
            return Err("WebRTC client pool is closed".into());
        }
        if self.started.get() {
            if let Some(expected) = expected {
                if self.expected.borrow().as_ref() != Some(&expected) {
                    return Err("bootstrap payment policy cannot change after startup".into());
                }
            }
            return Ok(self.state.subscribe());
        }
        self.started.set(true);
        self.expected.replace(expected.clone());
        self.state.send_replace(State {
            pending: core.seeds.len(),
            ..State::default()
        });
        let state = self.state.clone();
        let seeds = core.seeds.clone();
        let pool = Rc::clone(&core.pool);
        wasm_bindgen_futures::spawn_local(async move {
            let connect = async {
                let attempts = seeds.into_iter().map(|endpoint| {
                    let pool = Rc::clone(&pool);
                    let expected = expected.clone();
                    async move {
                        let result = async {
                            let client = pool.client(&endpoint).await?;
                            let hello = client.hello().await?;
                            if let Err(error) = validate(&hello, expected.as_ref()) {
                                pool.rejected_bootstrap
                                    .borrow_mut()
                                    .insert(endpoint.multiaddr.clone(), error.clone());
                                client.close();
                                return Err(error);
                            }
                            Ok(hello)
                        }
                        .await;
                        (endpoint, result)
                    }
                });
                let mut attempts =
                    stream::iter(attempts).buffer_unordered(MAX_BOOTSTRAP_CONNECTIONS);
                while let Some((endpoint, result)) = attempts.next().await {
                    state.send_modify(|state| {
                        state.pending -= 1;
                        match result {
                            Ok(hello) => state.ready.push(hello),
                            Err(message) => state.failures.push(BrowserLookupFailure {
                                peer_id: endpoint.multiaddr,
                                message,
                            }),
                        }
                    });
                }
            };
            let _ = select(Box::pin(connect), Box::pin(pool.availability.wait_closed())).await;
        });
        Ok(self.state.subscribe())
    }
}

fn validate(hello: &BrowserHello, expected: Option<&BrowserPaymentNetwork>) -> Result<(), String> {
    if !hello
        .capabilities
        .iter()
        .any(|capability| capability == "chunk_protocol")
    {
        return Err("Bootstrap node does not support the shared storage protocol; upgrade ant-node to a version advertising chunk_protocol".into());
    }
    if let Some(expected) = expected {
        if hello.payment.chain_id != expected.chain_id
            || !hello
                .payment
                .payment_token_address
                .eq_ignore_ascii_case(&expected.payment_token_address)
            || !hello
                .payment
                .payment_vault_address
                .eq_ignore_ascii_case(&expected.payment_vault_address)
        {
            return Err("NETWORK_MISMATCH: Authenticated payment network does not match expectedPaymentNetwork".into());
        }
    }
    Ok(())
}

pub(super) fn candidate(hello: &BrowserHello) -> Result<BrowserLookupCandidate, String> {
    BrowserLookupCandidate::parse(BrowserNode {
        address_record: None,
        peer_record: None,
        peer_id: hello.peer_id.clone(),
        native_addresses: Vec::new(),
        reliability: 1.0,
        webrtc_direct: Some(hello.endpoint.clone()),
    })
}

pub(super) fn failure(state: &State) -> String {
    format!(
        "could not connect to any WebRtcDirect seed: {}",
        state
            .failures
            .iter()
            .map(|failure| failure.message.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    )
}

impl BrowserNetworkCore {
    pub(super) async fn connect(
        &self,
        expected: Option<BrowserPaymentNetwork>,
    ) -> Result<BrowserHello, String> {
        let mut updates = self.bootstrap.subscribe(self, expected)?;
        loop {
            let state = updates.borrow_and_update().clone();
            if self.pool.availability.closed.get() {
                return Err("WebRTC client pool is closed".into());
            }
            if let Some(hello) = state.ready.first() {
                return Ok(hello.clone());
            }
            if state.pending == 0 {
                return Err(failure(&state));
            }
            self.bootstrap_changed(&mut updates).await?;
        }
    }

    pub(super) async fn bootstrap_changed(
        &self,
        updates: &mut watch::Receiver<State>,
    ) -> Result<(), String> {
        match select(
            Box::pin(updates.changed()),
            Box::pin(self.pool.availability.wait_closed()),
        )
        .await
        {
            Either::Left((result, _)) => result.map_err(|_| "bootstrap worker closed".into()),
            Either::Right(_) => Err("WebRTC client pool is closed".into()),
        }
    }
}
