//! Provider outages for resilience tests: a provider behind an [`OutageSwitch`] refuses
//! every new connect or request while the switch is down, as a provider whose service is down
//! answers `503 Service Unavailable` at once, and never answers one while the switch hangs, as a
//! blackholed endpoint (the connect or request stays pending until its caller drops it: the
//! caller's own deadline is what ends it).
//!
//! Mid-session failures come from the mocks' own faults ([`StreamFault`](crate::mock_common::StreamFault),
//! [`LlmFault`](crate::mock_llm::LlmFault)); the switch models the outage of the whole service,
//! shared by every call that holds a clone of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_providers::{
    Error, LlmProvider, LlmRequest, LlmStream, Result, SttConfig, SttProvider, SttSession,
    TtsConfig, TtsProvider, TtsSession,
};

/// The status an outage answers with.
pub const OUTAGE_STATUS: u16 = 503;

#[derive(Debug, Default)]
struct SwitchState {
    down: AtomicBool,
    hanging: AtomicBool,
    refused: AtomicUsize,
    hung: AtomicUsize,
    passed: AtomicUsize,
}

/// Takes a provider down and up; clones share the state.
#[derive(Debug, Clone, Default)]
pub struct OutageSwitch {
    state: Arc<SwitchState>,
}

impl OutageSwitch {
    /// A switch that is up.
    pub fn new() -> Self {
        Self::default()
    }

    /// A switch that is down.
    pub fn down() -> Self {
        let switch = Self::new();
        switch.set_down(true);
        switch
    }

    /// Takes the provider down (`true`) or brings it back (`false`).
    pub fn set_down(&self, down: bool) {
        self.state.down.store(down, Ordering::SeqCst);
    }

    /// Whether the provider is down.
    pub fn is_down(&self) -> bool {
        self.state.down.load(Ordering::SeqCst)
    }

    /// Makes every new connect and request hang (`true`: pending until the caller drops it) or
    /// answer again (`false`). A switch that is down refuses rather than hangs.
    pub fn set_hanging(&self, hanging: bool) {
        self.state.hanging.store(hanging, Ordering::SeqCst);
    }

    /// Whether new connects and requests hang.
    pub fn is_hanging(&self) -> bool {
        self.state.hanging.load(Ordering::SeqCst)
    }

    /// Connects and requests that hung so far (each until its caller gave up).
    pub fn hung(&self) -> usize {
        self.state.hung.load(Ordering::SeqCst)
    }

    /// Connects and requests refused so far.
    pub fn refused(&self) -> usize {
        self.state.refused.load(Ordering::SeqCst)
    }

    /// Connects and requests let through so far.
    pub fn passed(&self) -> usize {
        self.state.passed.load(Ordering::SeqCst)
    }

    /// `Err` (counted) while down; pending until dropped (counted) while hanging; counts the
    /// pass otherwise.
    async fn check(&self, provider: &'static str) -> Result<()> {
        if self.is_down() {
            self.state.refused.fetch_add(1, Ordering::SeqCst);
            return Err(Error::Rejected {
                provider,
                status: OUTAGE_STATUS,
            });
        }
        if self.is_hanging() {
            self.state.hung.fetch_add(1, Ordering::SeqCst);
            // A blackholed endpoint: only the caller's deadline (dropping this future) ends it.
            return std::future::pending().await;
        }
        self.state.passed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// `inner` behind `switch`: connects and requests fail with `Rejected { 503 }` at once while the
/// switch is down and never complete while it hangs; otherwise everything passes through.
#[derive(Debug, Clone)]
pub struct Outage<P> {
    inner: P,
    switch: OutageSwitch,
}

impl<P> Outage<P> {
    /// `inner` behind `switch`.
    pub const fn new(inner: P, switch: OutageSwitch) -> Self {
        Self { inner, switch }
    }

    /// The provider behind the switch.
    pub const fn inner(&self) -> &P {
        &self.inner
    }

    /// The switch.
    pub const fn switch(&self) -> &OutageSwitch {
        &self.switch
    }
}

impl<P: SttProvider> SttProvider for Outage<P> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn connect(&self, config: SttConfig) -> Result<SttSession> {
        self.switch.check(self.inner.name()).await?;
        self.inner.connect(config).await
    }

    async fn warm(&self) -> Result<()> {
        self.inner.warm().await
    }
}

impl<P: LlmProvider> LlmProvider for Outage<P> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        self.switch.check(self.inner.name()).await?;
        self.inner.stream(request).await
    }

    async fn warm(&self) -> Result<()> {
        self.inner.warm().await
    }
}

impl<P: TtsProvider> TtsProvider for Outage<P> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn connect(&self, config: TtsConfig) -> Result<TtsSession> {
        self.switch.check(self.inner.name()).await?;
        self.inner.connect(config).await
    }

    async fn warm(&self) -> Result<()> {
        self.inner.warm().await
    }

    fn idle_limit(&self) -> Option<Duration> {
        self.inner.idle_limit()
    }
}
