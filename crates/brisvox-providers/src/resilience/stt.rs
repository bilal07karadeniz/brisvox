//! [`ResilientStt`]: an STT stage with a breaker per provider, an optional secondary, and
//! mid-call recovery that replays the open utterance.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Instant;

use super::attempt::{
    Attempted, Common, LegId, ResilienceProbe, ResilienceStats, SECONDARY_WARM_BUDGET, bump, order,
    with_timeout,
};
use super::breaker::Permit;
use super::llm::sleep_until;
use super::{Leg, Resilience, is_provider_failure};
use crate::error::{Error, Result};
use crate::stt::{STT_CHANNEL_CAPACITY, SttConfig, SttEvent, SttInput, SttProvider, SttSession};

/// An [`SttProvider`] over a primary and an optional secondary provider (see the
/// [module docs](super)). One instance per call.
#[derive(Debug)]
pub struct ResilientStt<P, S = P> {
    primary: Leg<P>,
    secondary: Option<Leg<S>>,
    common: Arc<Common>,
}

impl<P: SttProvider> ResilientStt<P, P> {
    /// A call's STT over `primary` alone (breaker, budgets and in-place recovery).
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

impl<P: SttProvider, S: SttProvider> ResilientStt<P, S> {
    /// The same call's STT with `secondary` taking over from the primary.
    #[must_use]
    pub fn with_secondary<T: SttProvider>(self, secondary: Leg<T>) -> ResilientStt<P, T> {
        let common = self.common.with_secondary(Arc::clone(&secondary.breaker));
        ResilientStt {
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

/// Connects `leg`.
async fn connect_on<P: SttProvider, S: SttProvider>(
    primary: &Leg<P>,
    secondary: Option<&Leg<S>>,
    leg: LegId,
    config: SttConfig,
) -> Result<SttSession> {
    match (leg, secondary) {
        (LegId::Primary, _) => primary.provider.connect(config).await,
        (LegId::Secondary, Some(secondary)) => secondary.provider.connect(config).await,
        (LegId::Secondary, None) => Err(Error::Unavailable {
            provider: primary.provider.name(),
            reason: "no secondary",
        }),
    }
}

impl<P: SttProvider, S: SttProvider> SttProvider for ResilientStt<P, S> {
    fn name(&self) -> &'static str {
        self.primary.provider.name()
    }

    async fn connect(&self, config: SttConfig) -> Result<SttSession> {
        let primary = &self.primary;
        let secondary = self.secondary.as_ref();
        let first = self
            .common
            .attempt(
                order(self.common.has_secondary(), None),
                false,
                false,
                |leg| connect_on(primary, secondary, leg, config.clone()),
            )
            .await?;
        let (input_tx, input_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
        let bytes_per_second = usize::try_from(config.sample_rate.hz())
            .unwrap_or(usize::MAX)
            .saturating_mul(2);
        let bytes_of = |span: std::time::Duration| {
            usize::try_from(span.as_millis().saturating_mul(bytes_per_second as u128) / 1000)
                .unwrap_or(usize::MAX)
        };
        let replay_cap = bytes_of(self.common.config.stt_replay);
        let replay_tail = bytes_of(self.common.config.stt_replay_tail).min(replay_cap);
        let mut relay = Relay {
            primary: self.primary.clone(),
            secondary: self.secondary.clone(),
            common: Arc::clone(&self.common),
            config,
            live: None,
            replay: VecDeque::new(),
            replay_bytes: 0,
            replay_cap,
            replay_tail,
            finalize_pending: false,
            outbound: VecDeque::new(),
            input_open: true,
        };
        relay.go_live(first);
        // Ends when the engine drops its handles, the session ends or a recovery fails.
        tokio::spawn(relay.run(input_rx, events_tx));
        Ok(SttSession {
            input: input_tx,
            events: events_rx,
        })
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

type Recovery = Pin<Box<dyn Future<Output = Result<Attempted<SttSession>>> + Send>>;

/// The session in use.
struct Live {
    /// `None` once the engine closed its input (the provider then finishes and closes events).
    input: Option<mpsc::Sender<SttInput>>,
    events: mpsc::Receiver<SttEvent>,
    leg: LegId,
    /// Outcome not reported yet: the session has not proved healthy.
    permit: Option<Permit>,
    healthy_at: Instant,
}

/// The relay task of one wrapped session.
struct Relay<P, S> {
    primary: Leg<P>,
    secondary: Option<Leg<S>>,
    common: Arc<Common>,
    config: SttConfig,
    live: Option<Live>,
    /// Audio of the open utterance (since the last final segment), capped at `replay_cap` bytes
    /// (oldest dropped). `Bytes` clones: no copy.
    replay: VecDeque<Bytes>,
    replay_bytes: usize,
    replay_cap: usize,
    /// Bytes of `replay` kept at a final segment that does not end the utterance.
    replay_tail: usize,
    /// The engine sent `Finalize` and no `speech_final` answered it yet.
    finalize_pending: bool,
    /// Inputs waiting for room in the live session (the engine's input is not read meanwhile).
    outbound: VecDeque<SttInput>,
    input_open: bool,
}

enum Step {
    EngineGone,
    Input(Option<SttInput>),
    Event(Option<SttEvent>),
    Room,
    Healthy,
    Recovered(Result<Attempted<SttSession>>),
}

async fn next_event(events: Option<&mut mpsc::Receiver<SttEvent>>) -> Option<SttEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

async fn room(input: Option<&mpsc::Sender<SttInput>>, wanted: bool) {
    match input {
        Some(input) if wanted => {
            // A closed input shows up as the end of the events; the outbound item is then lost
            // with the session (and replayed if it was audio of the open utterance).
            if input.reserve().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        _ => std::future::pending().await,
    }
}

async fn recovered(recovery: &mut Option<Recovery>) -> Result<Attempted<SttSession>> {
    match recovery {
        Some(future) => future.await,
        None => std::future::pending().await,
    }
}

impl<P: SttProvider, S: SttProvider> Relay<P, S> {
    fn go_live(&mut self, attempted: Attempted<SttSession>) {
        let Attempted {
            value: session,
            leg,
            mut permit,
        } = attempted;
        // Connected: a half-open probe frees the breaker for other calls (on probation).
        permit.probation();
        self.live = Some(Live {
            input: Some(session.input),
            events: session.events,
            leg,
            permit: Some(permit),
            healthy_at: self.common.clock.now() + self.common.config.healthy_after,
        });
    }

    fn remember(&mut self, audio: &Bytes) {
        self.replay.push_back(audio.clone());
        self.replay_bytes = self.replay_bytes.saturating_add(audio.len());
        while self.replay_bytes > self.replay_cap {
            let Some(old) = self.replay.pop_front() else {
                self.replay_bytes = 0;
                break;
            };
            self.replay_bytes = self.replay_bytes.saturating_sub(old.len());
        }
    }

    fn forget_utterance(&mut self) {
        self.replay.clear();
        self.replay_bytes = 0;
    }

    /// Drops the replay audio a final segment covered, keeping the last `replay_tail` bytes
    /// (whole chunks, so a little more): the final lags the audio, its most recent part may not
    /// be transcribed yet.
    fn keep_tail(&mut self) {
        while let Some(front) = self.replay.front() {
            let rest = self.replay_bytes.saturating_sub(front.len());
            if rest < self.replay_tail {
                break;
            }
            self.replay_bytes = rest;
            self.replay.pop_front();
        }
    }

    /// Queues `input` for the live session and sends what fits now.
    fn push_out(&mut self, input: SttInput) {
        self.outbound.push_back(input);
        self.flush_out();
    }

    fn flush_out(&mut self) {
        let Some(sender) = self.live.as_ref().and_then(|l| l.input.as_ref()) else {
            return;
        };
        while let Some(input) = self.outbound.pop_front() {
            match sender.try_send(input) {
                Ok(()) => {}
                Err(TrySendError::Full(input)) => {
                    self.outbound.push_front(input);
                    return;
                }
                // The session is ending: its events say so; the replay buffer keeps the audio.
                Err(TrySendError::Closed(_)) => {
                    self.outbound.clear();
                    return;
                }
            }
        }
    }

    fn recovery(&self, failed: LegId) -> Recovery {
        let primary = self.primary.clone();
        let secondary = self.secondary.clone();
        let common = Arc::clone(&self.common);
        let config = self.config.clone();
        Box::pin(async move {
            let order = order(common.has_secondary(), Some(failed));
            common
                .attempt(order, true, true, |leg| {
                    connect_on(&primary, secondary.as_ref(), leg, config.clone())
                })
                .await
        })
    }

    async fn run(mut self, mut input: mpsc::Receiver<SttInput>, events: mpsc::Sender<SttEvent>) {
        let mut recovery: Option<Recovery> = None;
        // The error that killed the session being recovered.
        let mut cause: Option<Error> = None;
        loop {
            if !self.input_open && self.live.is_none() {
                return;
            }
            let read_input = self.input_open && self.outbound.is_empty();
            let healthy_at = self
                .live
                .as_ref()
                .and_then(|l| l.permit.as_ref().map(|_| l.healthy_at));
            let want_room = !self.outbound.is_empty();
            let (live_input, live_events) = match self.live.as_mut() {
                Some(live) => (live.input.as_ref(), Some(&mut live.events)),
                None => (None, None),
            };
            let step = tokio::select! {
                biased;
                () = events.closed() => Step::EngineGone,
                result = recovered(&mut recovery) => Step::Recovered(result),
                event = next_event(live_events) => Step::Event(event),
                () = room(live_input, want_room) => Step::Room,
                message = input.recv(), if read_input => Step::Input(message),
                () = sleep_until(&self.common, healthy_at) => Step::Healthy,
            };
            match step {
                Step::EngineGone => return,
                Step::Room => self.flush_out(),
                Step::Healthy => {
                    if let Some(permit) = self.live.as_mut().and_then(|l| l.permit.take()) {
                        permit.success();
                    }
                }
                Step::Input(None) => {
                    self.input_open = false;
                    recovery = None;
                    self.outbound.clear();
                    if let Some(live) = self.live.as_mut() {
                        // Closing the provider's input: it delivers what a Finalize asked for
                        // and closes its events.
                        live.input = None;
                    }
                }
                Step::Input(Some(SttInput::Audio(audio))) => {
                    self.remember(&audio);
                    if self.live.is_some() {
                        self.push_out(SttInput::Audio(audio));
                    }
                }
                Step::Input(Some(SttInput::Finalize)) => {
                    self.finalize_pending = true;
                    if self.live.is_some() {
                        self.push_out(SttInput::Finalize);
                    }
                }
                Step::Event(Some(SttEvent::Transcript {
                    text,
                    is_final,
                    speech_final,
                    from_finalize,
                    language,
                })) => {
                    if let Some(permit) = self.live.as_mut().and_then(|l| l.permit.take()) {
                        permit.success();
                    }
                    if speech_final {
                        // The utterance ended (the provider saw the speech end): nothing of it
                        // is replayed again.
                        self.forget_utterance();
                    } else if is_final {
                        // The segment is fixed, but it lags the audio: the tail is kept.
                        self.keep_tail();
                    }
                    if speech_final {
                        self.finalize_pending = false;
                    }
                    let event = SttEvent::Transcript {
                        text,
                        is_final,
                        speech_final,
                        from_finalize,
                        language,
                    };
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
                Step::Event(other) => {
                    let error = match other {
                        Some(SttEvent::Error(error)) => Some(error),
                        _ => None,
                    };
                    let Some(live) = self.live.take() else {
                        continue;
                    };
                    let failure = error.as_ref().is_none_or(is_provider_failure);
                    if let Some(permit) = live.permit
                        && failure
                        && self.input_open
                    {
                        permit.failure();
                    }
                    self.outbound.clear();
                    if !self.input_open || !failure {
                        // The engine closed the session (or a request error): pass the end on.
                        if let Some(error) = error {
                            let _ = events.send(SttEvent::Error(error)).await;
                        }
                        return;
                    }
                    cause = Some(error.unwrap_or(Error::Disconnected {
                        provider: self.common.name,
                    }));
                    recovery = Some(self.recovery(live.leg));
                }
                Step::Recovered(result) => {
                    recovery = None;
                    match result {
                        Ok(attempted) => {
                            bump(&self.common.counters.recoveries);
                            cause = None;
                            self.go_live(attempted);
                            let audio: Vec<SttInput> =
                                self.replay.iter().cloned().map(SttInput::Audio).collect();
                            let replayed = audio.len() + usize::from(self.finalize_pending);
                            self.outbound.extend(audio);
                            if self.finalize_pending {
                                self.outbound.push_back(SttInput::Finalize);
                            }
                            self.common.counters.replayed.fetch_add(
                                u64::try_from(replayed).unwrap_or(u64::MAX),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            self.flush_out();
                        }
                        Err(_) => {
                            bump(&self.common.counters.recovery_failures);
                            let error = cause.take().unwrap_or(Error::Disconnected {
                                provider: self.common.name,
                            });
                            let _ = events.send(SttEvent::Error(error)).await;
                            return;
                        }
                    }
                }
            }
        }
    }
}
