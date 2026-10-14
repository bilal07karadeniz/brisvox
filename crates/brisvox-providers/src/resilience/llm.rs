//! [`ResilientLlm`]: an LLM stage with a breaker per provider and an optional secondary that
//! takes a request over when the primary fails before any content.

use std::sync::Arc;

use brisvox_core::clock::wait_until;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::attempt::{
    Attempted, Common, LegId, ResilienceProbe, ResilienceStats, SECONDARY_WARM_BUDGET, bump, order,
    with_timeout,
};
use super::breaker::Permit;
use super::{Leg, Resilience, is_provider_failure};
use crate::error::{Error, Result};
use crate::llm::{LLM_EVENT_CAPACITY, LlmEvent, LlmProvider, LlmRequest, LlmStream};

/// An [`LlmProvider`] over a primary and an optional secondary provider (see the
/// [module docs](super)). One instance per call.
#[derive(Debug)]
pub struct ResilientLlm<P, S = P> {
    primary: Leg<P>,
    secondary: Option<Leg<S>>,
    common: Arc<Common>,
}

impl<P: LlmProvider> ResilientLlm<P, P> {
    /// A call's LLM over `primary` alone (breaker and budgets; no failover).
    pub fn new(primary: Leg<P>, resilience: &Resilience) -> Self {
        let common = Common::new(
            resilience,
            primary.provider.name(),
            Arc::clone(&primary.breaker),
            None,
        );
        Self {
            primary,
            secondary: None,
            common: Arc::new(common),
        }
    }
}

impl<P: LlmProvider, S: LlmProvider> ResilientLlm<P, S> {
    /// The same call's LLM with `secondary` taking requests over from the primary.
    #[must_use]
    pub fn with_secondary<T: LlmProvider>(self, secondary: Leg<T>) -> ResilientLlm<P, T> {
        let common = self.common.with_secondary(Arc::clone(&secondary.breaker));
        ResilientLlm {
            primary: self.primary,
            secondary: Some(secondary),
            common: Arc::new(common),
        }
    }

    /// A handle on this call's counters and budget that stays readable after the wrapper was
    /// handed to the engine.
    pub fn probe(&self) -> ResilienceProbe {
        ResilienceProbe::new(Arc::clone(&self.common))
    }

    /// This call's counters.
    pub fn stats(&self) -> ResilienceStats {
        self.common.counters.snapshot()
    }

    /// Tokens left in this call's retry budget.
    pub fn budget_available(&self) -> u32 {
        self.common.session_budget().available()
    }
}

/// `request` on `leg`.
async fn stream_on<P: LlmProvider, S: LlmProvider>(
    primary: &Leg<P>,
    secondary: Option<&Leg<S>>,
    leg: LegId,
    request: LlmRequest,
) -> Result<LlmStream> {
    match (leg, secondary) {
        (LegId::Primary, _) => primary.provider.stream(request).await,
        (LegId::Secondary, Some(secondary)) => secondary.provider.stream(request).await,
        (LegId::Secondary, None) => Err(Error::Unavailable {
            provider: primary.provider.name(),
            reason: "no secondary",
        }),
    }
}

impl<P: LlmProvider, S: LlmProvider> LlmProvider for ResilientLlm<P, S> {
    fn name(&self) -> &'static str {
        self.primary.provider.name()
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        let issued = self.common.clock.now();
        let has_secondary = self.common.has_secondary();
        // A copy to re-issue, only while a leg is left to re-issue it to.
        let mut spare = has_secondary.then(|| request.clone());
        let mut slot = Some(request);
        let primary = &self.primary;
        let secondary = self.secondary.as_ref();
        let first = self
            .common
            .attempt(order(has_secondary, None), false, false, |leg| {
                let request = slot.take().or_else(|| spare.clone()).unwrap_or_default();
                stream_on(primary, secondary, leg, request)
            })
            .await?;
        if first.leg == LegId::Secondary {
            spare = None;
        }
        let (tx, rx) = mpsc::channel(LLM_EVENT_CAPACITY);
        let relay = Relay {
            primary: self.primary.clone(),
            secondary: self.secondary.clone(),
            common: Arc::clone(&self.common),
            spare,
            issued,
        };
        // Ends when the consumer drops the stream, or with the response.
        tokio::spawn(relay.run(first, tx));
        Ok(LlmStream::new(rx))
    }

    async fn warm(&self) -> Result<()> {
        let secondary = async {
            if let Some(secondary) = &self.secondary {
                // Bounded so the primary's warm-up result is not held up by the secondary's.
                let _ = with_timeout(
                    &self.common.clock,
                    SECONDARY_WARM_BUDGET,
                    "secondary warm-up",
                    secondary.provider.warm(),
                )
                .await;
            }
        };
        let (primary, ()) = tokio::join!(self.primary.provider.warm(), secondary);
        primary
    }
}

/// What the relay task of one response owns.
struct Relay<P, S> {
    primary: Leg<P>,
    secondary: Option<Leg<S>>,
    common: Arc<Common>,
    /// The request, while the secondary has not been tried.
    spare: Option<LlmRequest>,
    /// When the engine asked for the response.
    issued: Instant,
}

enum Step {
    Closed,
    Event(Option<LlmEvent>),
    FirstEventLate,
}

impl<P: LlmProvider, S: LlmProvider> Relay<P, S> {
    async fn run(mut self, first: Attempted<LlmStream>, tx: mpsc::Sender<LlmEvent>) {
        let Attempted {
            value: mut stream,
            leg: _,
            permit,
        } = first;
        let mut permit: Option<Permit> = Some(permit);
        let mut started = false;
        loop {
            let deadline = if started || self.spare.is_none() {
                None
            } else {
                Some(self.issued + self.common.config.llm_first_event)
            };
            let step = tokio::select! {
                biased;
                () = tx.closed() => Step::Closed,
                event = stream.next() => Step::Event(event),
                () = sleep_until(&self.common, deadline) => Step::FirstEventLate,
            };
            let error = match step {
                // The consumer dropped the stream: the attempt is abandoned (permit dropped).
                Step::Closed => return,
                Step::Event(Some(LlmEvent::Done {
                    finish,
                    usage,
                    provider_data,
                })) => {
                    if let Some(permit) = permit.take() {
                        permit.success();
                    }
                    let _ = tx
                        .send(LlmEvent::Done {
                            finish,
                            usage,
                            provider_data,
                        })
                        .await;
                    return;
                }
                Step::Event(Some(LlmEvent::Error(error))) => error,
                Step::Event(None) => Error::Disconnected {
                    provider: self.common.name,
                },
                Step::FirstEventLate => Error::Core(brisvox_core::Error::Timeout {
                    what: "llm first event",
                    after: self.common.config.llm_first_event,
                }),
                Step::Event(Some(event)) => {
                    // The response started: a half-open probe frees the breaker for other
                    // calls (on probation) instead of holding it until `Done`.
                    if let Some(permit) = permit.as_mut() {
                        permit.probation();
                    }
                    started = true;
                    if tx.send(event).await.is_err() {
                        return;
                    }
                    continue;
                }
            };
            let failure = is_provider_failure(&error);
            if let Some(permit) = permit.take()
                && failure
            {
                permit.failure();
            }
            let request = if started || !failure {
                None
            } else {
                self.spare.take()
            };
            let Some(request) = request else {
                let _ = tx.send(LlmEvent::Error(error)).await;
                return;
            };
            // Nothing was delivered yet: the secondary takes the request over.
            let mut slot = Some(request);
            let primary = &self.primary;
            let secondary = self.secondary.as_ref();
            let reissue = self
                .common
                .attempt([Some(LegId::Secondary), None], true, false, |leg| {
                    stream_on(primary, secondary, leg, slot.take().unwrap_or_default())
                });
            let next = tokio::select! {
                biased;
                () = tx.closed() => return,
                next = reissue => next,
            };
            match next {
                Ok(next) => {
                    bump(&self.common.counters.recoveries);
                    stream = next.value;
                    permit = Some(next.permit);
                }
                Err(_) => {
                    bump(&self.common.counters.recovery_failures);
                    // The primary's error is the one that explains the failed round.
                    let _ = tx.send(LlmEvent::Error(error)).await;
                    return;
                }
            }
        }
    }
}

/// Waits until `deadline` on the layer's clock; never completes for `None`.
pub(crate) async fn sleep_until(common: &Common, deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => wait_until(&*common.clock, deadline).await,
        None => std::future::pending().await,
    }
}
