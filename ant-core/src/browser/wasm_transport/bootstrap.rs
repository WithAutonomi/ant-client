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
    // Readiness can shrink after closure/eviction, so length is not a version.
    pub revision: u64,
}

pub(super) struct Bootstrap {
    started: Cell<bool>,
    expected: RefCell<Option<BrowserPaymentNetwork>>,
    state: watch::Sender<State>,
    retry_at: Cell<Option<web_time::Instant>>,
}

impl Bootstrap {
    pub fn new() -> Self {
        let (state, _) = watch::channel(State::default());
        Self {
            started: Cell::new(false),
            expected: RefCell::new(None),
            state,
            retry_at: Cell::new(None),
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
        } else {
            self.started.set(true);
            self.expected.replace(expected.clone());
            core.pool.bootstrap_policy.configure(&core.seeds, expected);
            for (endpoint, entry) in core.pool.clients.borrow().iter() {
                core.pool.bootstrap_policy.register(endpoint, entry);
            }
        }
        // Readiness belongs to live pooled sessions, not a historic HELLO.
        self.state.send_modify(|state| {
            let before = state.ready.len();
            state
                .ready
                .retain(|hello| core.pool.has_authenticated_connection(&hello.endpoint));
            if state.ready.len() != before {
                state.revision = state.revision.wrapping_add(1);
            }
        });
        if self.state.borrow().pending != 0 {
            return Ok(self.state.subscribe());
        }
        let seeds: Vec<_> = core
            .seeds
            .iter()
            .filter(|endpoint| {
                core.pool
                    .bootstrap_policy
                    .check(&endpoint.multiaddr)
                    .is_ok()
                    && !self
                        .state
                        .borrow()
                        .ready
                        .iter()
                        .any(|hello| hello.endpoint == **endpoint)
            })
            .cloned()
            .collect();
        if seeds.is_empty() {
            return Ok(self.state.subscribe());
        }
        // A caller may start one new bounded batch after transient failures.
        // Concurrent callers share it; there is no autonomous retry loop.
        let now = web_time::Instant::now();
        let delay = self
            .retry_at
            .get()
            .map_or(Duration::ZERO, |at| at.saturating_duration_since(now));
        self.retry_at
            .set(Some(now + delay + Duration::from_secs(1)));
        self.state.send_modify(|state| {
            state.pending = seeds.len();
            state
                .failures
                .retain(|failure| !seeds.iter().any(|seed| seed.multiaddr == failure.peer_id));
        });
        let state = self.state.clone();
        let pool = Rc::clone(&core.pool);
        wasm_bindgen_futures::spawn_local(async move {
            let connect = async {
                if !delay.is_zero() {
                    crate::runtime::sleep(delay).await;
                }
                let attempts = seeds.into_iter().map(|endpoint| {
                    let pool = Rc::clone(&pool);
                    async move {
                        let result = async {
                            let client = pool.client(&endpoint).await?;
                            let hello = client.hello().await?;
                            pool.bootstrap_policy
                                .validate(&endpoint.multiaddr, &hello)?;
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
                            Ok(hello) => {
                                state.ready.push(hello);
                                state.revision = state.revision.wrapping_add(1);
                            }
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

/// Every pooled lane consults the same immutable seed policy. Revocation closes
/// both lanes, including RPCs and HELLOs already holding a lease or admission.
#[derive(Default)]
pub(super) struct Policies {
    seeds: RefCell<HashMap<String, SeedPolicy>>,
}

struct SeedPolicy {
    expected: Option<BrowserPaymentNetwork>,
    rejected: Option<String>,
    lanes: [Weak<BrowserNodeClientCore>; 2],
}

impl Policies {
    fn configure(&self, seeds: &[BrowserEndpoint], expected: Option<BrowserPaymentNetwork>) {
        let mut policies = self.seeds.borrow_mut();
        for seed in seeds {
            policies.insert(
                seed.multiaddr.clone(),
                SeedPolicy {
                    expected: expected.clone(),
                    rejected: None,
                    lanes: [Weak::new(), Weak::new()],
                },
            );
        }
    }

    pub fn register(&self, endpoint: &str, entry: &PoolEntry) {
        if let Some(policy) = self.seeds.borrow_mut().get_mut(endpoint) {
            policy.lanes = [
                Rc::downgrade(&entry.client),
                Rc::downgrade(&entry.data_client),
            ];
        }
    }

    pub fn check(&self, endpoint: &str) -> Result<(), String> {
        match self
            .seeds
            .borrow()
            .get(endpoint)
            .and_then(|policy| policy.rejected.as_ref())
        {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    pub fn validate(&self, endpoint: &str, hello: &BrowserHello) -> Result<(), String> {
        self.check(endpoint)?;
        let rejection = {
            let mut seeds = self.seeds.borrow_mut();
            let Some(policy) = seeds.get_mut(endpoint) else {
                return Ok(());
            };
            match validate(hello, policy.expected.as_ref()) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    policy.rejected = Some(error.clone());
                    (error, policy.lanes.clone())
                }
            }
        };
        for lane in rejection.1.iter().filter_map(Weak::upgrade) {
            lane.close();
            if let Some(pool) = &lane.pool_availability {
                pool.notify_waiters();
            }
        }
        Err(rejection.0)
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
            if let Some(hello) = state
                .ready
                .iter()
                .find_map(|hello| self.pool.authenticated_hello(&hello.endpoint))
            {
                return Ok(hello);
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
