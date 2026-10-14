//! [`ResilientTts`]: a TTS stage with a breaker per provider, an optional secondary, mid-call
//! recovery that re-speaks unheard utterances, and the proactive refresh of idle sessions.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Instant;

use super::attempt::{
    Attempted, Common, LegId, ResilienceProbe, ResilienceStats, SECONDARY_WARM_BUDGET, bump, order,
    with_timeout,
};
use super::breaker::Permit;
use super::llm::sleep_until;
use super::{IdleRefresh, Leg, Resilience, is_provider_failure};
use crate::error::{Error, Result};
use crate::tts::{
    ContextId, TTS_CANCEL_CAPACITY, TTS_CHANNEL_CAPACITY, TtsConfig, TtsEvent, TtsInput,
    TtsProvider, TtsSession,
};

/// Utterances tracked per session at most (the engine flushes or cancels every one; this only
/// bounds a misbehaving caller). The oldest is forgotten first.
const MAX_TRACKED: usize = 64;

/// Cancelled utterances remembered per session (as the adapters do): text and flushes the engine
/// queued for them before the cancel are dropped when they are read after it.
const CANCELLED_MEMORY: usize = 64;

/// A [`TtsProvider`] over a primary and an optional secondary provider (see the
/// [module docs](super)). One instance per call.
#[derive(Debug)]
pub struct ResilientTts<P, S = P> {
    primary: Leg<P>,
    secondary: Option<Leg<S>>,
    common: Arc<Common>,
}

impl<P: TtsProvider> ResilientTts<P, P> {
    /// A call's TTS over `primary` alone (breaker, budgets, in-place recovery, idle refresh).
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

impl<P: TtsProvider, S: TtsProvider> ResilientTts<P, S> {
    /// The same call's TTS with `secondary` taking over from the primary.
    #[must_use]
    pub fn with_secondary<T: TtsProvider>(self, secondary: Leg<T>) -> ResilientTts<P, T> {
        let common = self.common.with_secondary(Arc::clone(&secondary.breaker));
        ResilientTts {
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
async fn connect_on<P: TtsProvider, S: TtsProvider>(
    primary: &Leg<P>,
    secondary: Option<&Leg<S>>,
    leg: LegId,
    config: TtsConfig,
) -> Result<TtsSession> {
    match (leg, secondary) {
        (LegId::Primary, _) => primary.provider.connect(config).await,
        (LegId::Secondary, Some(secondary)) => secondary.provider.connect(config).await,
        (LegId::Secondary, None) => Err(Error::Unavailable {
            provider: primary.provider.name(),
            reason: "no secondary",
        }),
    }
}

impl<P: TtsProvider, S: TtsProvider> TtsProvider for ResilientTts<P, S> {
    fn name(&self) -> &'static str {
        self.primary.provider.name()
    }

    async fn connect(&self, config: TtsConfig) -> Result<TtsSession> {
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
        let (input_tx, input_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::channel(TTS_CANCEL_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(TTS_CHANNEL_CAPACITY);
        let mut relay = Relay {
            primary: self.primary.clone(),
            secondary: self.secondary.clone(),
            common: Arc::clone(&self.common),
            config,
            live: None,
            standby: None,
            ledger: VecDeque::new(),
            cancelled: VecDeque::with_capacity(CANCELLED_MEMORY),
            outbound: VecDeque::new(),
            input_open: true,
            last_traffic: self.common.clock.now(),
            refresh_at: None,
        };
        relay.live = Some(relay.live_from(first));
        // Ends when the engine drops its handles, the session ends or a recovery fails.
        tokio::spawn(relay.run(input_rx, cancel_rx, events_tx));
        Ok(TtsSession {
            input: input_tx,
            cancel: cancel_tx,
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

    /// `None`: the wrapper refreshes idle sessions itself.
    fn idle_limit(&self) -> Option<Duration> {
        None
    }
}

type Connecting = Pin<Box<dyn Future<Output = Result<Attempted<TtsSession>>> + Send>>;

/// A provider session.
struct Live {
    /// `None` once the engine closed its input (the provider then finishes and closes events).
    input: Option<mpsc::Sender<TtsInput>>,
    cancel: mpsc::Sender<ContextId>,
    events: mpsc::Receiver<TtsEvent>,
    leg: LegId,
    /// Outcome not reported yet: the session has not proved healthy.
    permit: Option<Permit>,
    healthy_at: Instant,
    /// Idle time after which this session is replaced.
    refresh_after: Option<Duration>,
}

/// One utterance the provider has not finished (no `Flushed` or `Cancelled` yet).
struct Ctx {
    id: ContextId,
    /// Its text so far, for a replay (empty once it stopped being replayable).
    texts: Vec<Arc<str>>,
    bytes: usize,
    flush: bool,
    /// Audio or word timings of it were delivered: it cannot be replayed without repeating
    /// words the engine already has.
    started: bool,
    /// Its text fits the replay caps.
    replayable: bool,
    cancelled: bool,
}

/// The relay task of one wrapped session.
struct Relay<P, S> {
    primary: Leg<P>,
    secondary: Option<Leg<S>>,
    common: Arc<Common>,
    config: TtsConfig,
    live: Option<Live>,
    /// A refreshed session waiting for the live one to become idle.
    standby: Option<Live>,
    ledger: VecDeque<Ctx>,
    /// Recently cancelled utterances (at most [`CANCELLED_MEMORY`], oldest forgotten first). The
    /// cancel channel is read before the input, so text the engine queued before a cancel can
    /// be read after it: it must neither reach a provider nor make a ledger entry that nothing
    /// would ever close (that entry would block the idle refresh and be re-spoken by a
    /// recovery after the engine was told `Cancelled`).
    cancelled: VecDeque<ContextId>,
    /// Inputs waiting for room in the live session (the engine's input is not read meanwhile).
    outbound: VecDeque<TtsInput>,
    input_open: bool,
    /// Last input sent to or event received from the live session.
    last_traffic: Instant,
    /// When a failed refresh is tried again.
    refresh_at: Option<Instant>,
}

enum Step {
    EngineGone,
    Input(Option<TtsInput>),
    Cancel(ContextId),
    Event(Option<TtsEvent>),
    Room,
    Healthy,
    RefreshDue,
    Refreshed(Result<Attempted<TtsSession>>),
    Recovered(Result<Attempted<TtsSession>>),
}

async fn next_event(events: Option<&mut mpsc::Receiver<TtsEvent>>) -> Option<TtsEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

async fn room(input: Option<&mpsc::Sender<TtsInput>>, wanted: bool) {
    match input {
        Some(input) if wanted => {
            if input.reserve().await.is_err() {
                // A closed input shows up as the end of the events.
                std::future::pending::<()>().await;
            }
        }
        _ => std::future::pending().await,
    }
}

async fn connected(connecting: &mut Option<Connecting>) -> Result<Attempted<TtsSession>> {
    match connecting {
        Some(future) => future.await,
        None => std::future::pending().await,
    }
}

impl<P: TtsProvider, S: TtsProvider> Relay<P, S> {
    fn refresh_after(&self, leg: LegId) -> Option<Duration> {
        match self.common.config.idle_refresh {
            IdleRefresh::Off => None,
            IdleRefresh::After(after) => Some(after),
            IdleRefresh::Auto => {
                let limit = match (leg, &self.secondary) {
                    (LegId::Primary, _) => self.primary.provider.idle_limit(),
                    (LegId::Secondary, Some(secondary)) => secondary.provider.idle_limit(),
                    (LegId::Secondary, None) => None,
                };
                limit.map(|limit| limit * 4 / 5)
            }
        }
    }

    fn live_from(&self, attempted: Attempted<TtsSession>) -> Live {
        let Attempted {
            value: session,
            leg,
            mut permit,
        } = attempted;
        // Connected: a half-open probe frees the breaker for other calls (on probation).
        permit.probation();
        Live {
            input: Some(session.input),
            cancel: session.cancel,
            events: session.events,
            leg,
            permit: Some(permit),
            healthy_at: self.common.clock.now() + self.common.config.healthy_after,
            refresh_after: self.refresh_after(leg),
        }
    }

    fn connecting(&self, failed: Option<LegId>) -> Connecting {
        let primary = self.primary.clone();
        let secondary = self.secondary.clone();
        let common = Arc::clone(&self.common);
        let config = self.config.clone();
        Box::pin(async move {
            let order = order(common.has_secondary(), failed);
            common
                .attempt(order, failed.is_some(), true, |leg| {
                    connect_on(&primary, secondary.as_ref(), leg, config.clone())
                })
                .await
        })
    }

    /// Nothing is in flight on the live session.
    fn idle(&self) -> bool {
        self.live.is_some() && self.ledger.is_empty() && self.outbound.is_empty()
    }

    /// The ledger entry of `id`, made if missing (`None` only in the impossible case that the
    /// entry just made is gone).
    fn ctx(&mut self, id: ContextId) -> Option<&mut Ctx> {
        if let Some(index) = self.ledger.iter().position(|c| c.id == id) {
            return self.ledger.get_mut(index);
        }
        if self.ledger.len() >= MAX_TRACKED {
            self.ledger.pop_front();
        }
        let replayable = self.ledger.len() < self.common.config.tts_replay_contexts;
        self.ledger.push_back(Ctx {
            id,
            texts: Vec::new(),
            bytes: 0,
            flush: false,
            started: false,
            replayable,
            cancelled: false,
        });
        self.ledger.back_mut()
    }

    fn record_text(&mut self, id: ContextId, text: &Arc<str>) {
        let cap = self.common.config.tts_replay_bytes;
        let Some(ctx) = self.ctx(id) else { return };
        if !ctx.replayable {
            return;
        }
        let bytes = ctx.bytes.saturating_add(text.len());
        if bytes > cap {
            ctx.replayable = false;
            ctx.texts = Vec::new();
            ctx.bytes = 0;
        } else {
            ctx.texts.push(Arc::clone(text));
            ctx.bytes = bytes;
        }
    }

    fn forget(&mut self, id: ContextId) {
        self.ledger.retain(|c| c.id != id);
    }

    /// Remembers that `id` was cancelled (see the `cancelled` field).
    fn remember_cancel(&mut self, id: ContextId) {
        if self.cancelled.contains(&id) {
            return;
        }
        if self.cancelled.len() >= CANCELLED_MEMORY {
            self.cancelled.pop_front();
        }
        self.cancelled.push_back(id);
    }

    fn is_cancelled(&self, id: ContextId) -> bool {
        self.cancelled.contains(&id)
    }

    /// Queues `input` for the live session and sends what fits now.
    fn push_out(&mut self, input: TtsInput) {
        self.outbound.push_back(input);
        self.flush_out();
    }

    fn flush_out(&mut self) {
        let Some(sender) = self.live.as_ref().and_then(|l| l.input.as_ref()) else {
            return;
        };
        let mut sent = false;
        while let Some(input) = self.outbound.pop_front() {
            match sender.try_send(input) {
                Ok(()) => sent = true,
                Err(TrySendError::Full(input)) => {
                    self.outbound.push_front(input);
                    break;
                }
                // The session is ending: its events say so; the ledger keeps the text.
                Err(TrySendError::Closed(_)) => {
                    self.outbound.clear();
                    break;
                }
            }
        }
        if sent {
            self.last_traffic = self.common.clock.now();
        }
    }

    /// Swaps a waiting refreshed session in once the live one is idle.
    fn swap_if_idle(&mut self) {
        if self.standby.is_some() && self.idle() {
            self.live = self.standby.take();
            self.last_traffic = self.common.clock.now();
            bump(&self.common.counters.refreshes);
        }
    }

    /// Sends the ledger's replayable utterances to the (new) live session. `false` when one of
    /// them cannot be replayed (partly heard, or over the caps).
    fn replay(&mut self) -> bool {
        if self
            .ledger
            .iter()
            .any(|c| !c.cancelled && (c.started || !c.replayable))
        {
            return false;
        }
        let mut replayed = 0u64;
        for ctx in &self.ledger {
            if ctx.cancelled || self.cancelled.contains(&ctx.id) {
                continue;
            }
            for text in &ctx.texts {
                self.outbound.push_back(TtsInput::Text {
                    context: ctx.id,
                    text: Arc::clone(text),
                });
                replayed += 1;
            }
            if ctx.flush {
                self.outbound.push_back(TtsInput::Flush { context: ctx.id });
                replayed += 1;
            }
        }
        self.common
            .counters
            .replayed
            .fetch_add(replayed, Ordering::Relaxed);
        self.flush_out();
        true
    }

    #[allow(clippy::too_many_lines)]
    async fn run(
        mut self,
        mut input: mpsc::Receiver<TtsInput>,
        mut cancel: mpsc::Receiver<ContextId>,
        events: mpsc::Sender<TtsEvent>,
    ) {
        let mut recovery: Option<Connecting> = None;
        let mut refresh: Option<Connecting> = None;
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
            let refresh_due = match self.live.as_ref().and_then(|l| l.refresh_after) {
                Some(after)
                    if self.input_open
                        && self.idle()
                        && refresh.is_none()
                        && self.standby.is_none() =>
                {
                    Some(self.refresh_at.unwrap_or_else(|| self.last_traffic + after))
                }
                _ => None,
            };
            let want_room = !self.outbound.is_empty();
            let (live_input, live_events) = match self.live.as_mut() {
                Some(live) => (live.input.as_ref(), Some(&mut live.events)),
                None => (None, None),
            };
            let step = tokio::select! {
                biased;
                () = events.closed() => Step::EngineGone,
                Some(context) = cancel.recv(), if self.input_open => Step::Cancel(context),
                result = connected(&mut recovery) => Step::Recovered(result),
                event = next_event(live_events) => Step::Event(event),
                () = room(live_input, want_room) => Step::Room,
                message = input.recv(), if read_input => Step::Input(message),
                result = connected(&mut refresh) => Step::Refreshed(result),
                () = sleep_until(&self.common, healthy_at) => Step::Healthy,
                () = sleep_until(&self.common, refresh_due) => Step::RefreshDue,
            };
            match step {
                Step::EngineGone => return,
                Step::Room => self.flush_out(),
                Step::Healthy => {
                    if let Some(permit) = self.live.as_mut().and_then(|l| l.permit.take()) {
                        permit.success();
                    }
                }
                Step::RefreshDue => {
                    self.refresh_at = None;
                    refresh = Some(self.connecting(None));
                }
                Step::Refreshed(result) => {
                    refresh = None;
                    match result {
                        Ok(attempted) => {
                            let fresh = self.live_from(attempted);
                            self.standby = Some(fresh);
                            self.swap_if_idle();
                        }
                        Err(_) => {
                            bump(&self.common.counters.refresh_failures);
                            // Try again after an eighth of the period (still before the limit).
                            let after = self
                                .live
                                .as_ref()
                                .and_then(|l| l.refresh_after)
                                .unwrap_or(self.common.config.attempt_timeout);
                            self.refresh_at = Some(self.common.clock.now() + after / 8);
                        }
                    }
                }
                Step::Input(None) => {
                    self.input_open = false;
                    recovery = None;
                    refresh = None;
                    self.standby = None;
                    self.outbound.clear();
                    if let Some(live) = self.live.as_mut() {
                        // Closing the provider's input: it finishes and closes its events.
                        live.input = None;
                    }
                }
                // Queued by the engine before its cancel, read after it: dropped (the provider
                // would ignore it too, and a ledger entry for it would never be closed).
                Step::Input(Some(TtsInput::Text { context, .. } | TtsInput::Flush { context }))
                    if self.is_cancelled(context) => {}
                Step::Input(Some(TtsInput::Text { context, text })) => {
                    self.record_text(context, &text);
                    if self.live.is_some() {
                        self.push_out(TtsInput::Text { context, text });
                    }
                }
                Step::Input(Some(TtsInput::Flush { context })) => {
                    if let Some(ctx) = self.ctx(context) {
                        ctx.flush = true;
                    }
                    if self.live.is_some() {
                        self.push_out(TtsInput::Flush { context });
                    }
                }
                Step::Cancel(context) => {
                    self.remember_cancel(context);
                    // Text of it still queued must not reach the provider after the cancel.
                    self.outbound.retain(|i| match i {
                        TtsInput::Text { context: c, .. } | TtsInput::Flush { context: c } => {
                            *c != context
                        }
                    });
                    if let Some(live) = self.live.as_ref() {
                        if let Some(ctx) = self.ledger.iter_mut().find(|c| c.id == context) {
                            ctx.cancelled = true;
                        }
                        // Full: the provider is already cancelling (the engine's own rule).
                        let _ = live.cancel.try_send(context);
                        self.last_traffic = self.common.clock.now();
                    } else {
                        // No session to answer it: the layer acknowledges the cancel itself.
                        self.forget(context);
                        if events.send(TtsEvent::Cancelled { context }).await.is_err() {
                            return;
                        }
                    }
                }
                Step::Event(Some(TtsEvent::Error(error))) => {
                    if !self
                        .on_death(Some(error), &events, &mut recovery, &mut cause)
                        .await
                    {
                        return;
                    }
                }
                Step::Event(None) => {
                    if !self
                        .on_death(None, &events, &mut recovery, &mut cause)
                        .await
                    {
                        return;
                    }
                }
                Step::Event(Some(event)) => {
                    self.last_traffic = self.common.clock.now();
                    match &event {
                        TtsEvent::Audio { context, .. } | TtsEvent::Word { context, .. } => {
                            if let Some(permit) = self.live.as_mut().and_then(|l| l.permit.take()) {
                                permit.success();
                            }
                            if let Some(ctx) = self.ledger.iter_mut().find(|c| c.id == *context) {
                                ctx.started = true;
                            }
                        }
                        TtsEvent::Flushed { context } | TtsEvent::Cancelled { context } => {
                            let context = *context;
                            self.forget(context);
                        }
                        TtsEvent::Error(_) => {}
                    }
                    if events.send(event).await.is_err() {
                        return;
                    }
                    self.swap_if_idle();
                }
                Step::Recovered(result) => {
                    recovery = None;
                    match result {
                        Ok(attempted) => {
                            let fresh = self.live_from(attempted);
                            self.live = Some(fresh);
                            self.last_traffic = self.common.clock.now();
                            if !self.replay() {
                                bump(&self.common.counters.recovery_failures);
                                let error = cause.take().unwrap_or(Error::Disconnected {
                                    provider: self.common.name,
                                });
                                let _ = events.send(TtsEvent::Error(error)).await;
                                return;
                            }
                            cause = None;
                            bump(&self.common.counters.recoveries);
                        }
                        Err(_) => {
                            bump(&self.common.counters.recovery_failures);
                            let error = cause.take().unwrap_or(Error::Disconnected {
                                provider: self.common.name,
                            });
                            let _ = events.send(TtsEvent::Error(error)).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// The live session died (`error`, or its events ended). Returns `false` when the relay
    /// must end (the engine closed the session, or the death cannot be recovered: the error
    /// was passed on).
    async fn on_death(
        &mut self,
        error: Option<Error>,
        events: &mpsc::Sender<TtsEvent>,
        recovery: &mut Option<Connecting>,
        cause: &mut Option<Error>,
    ) -> bool {
        let Some(live) = self.live.take() else {
            return true;
        };
        self.outbound.clear();
        let failure = error.as_ref().is_none_or(is_provider_failure);
        if let Some(permit) = live.permit
            && failure
            && self.input_open
        {
            permit.failure();
        }
        if !self.input_open || !failure {
            if let Some(error) = error {
                let _ = events.send(TtsEvent::Error(error)).await;
            }
            return false;
        }
        let error = error.unwrap_or(Error::Disconnected {
            provider: self.common.name,
        });
        if self
            .ledger
            .iter()
            .any(|c| !c.cancelled && (c.started || !c.replayable))
        {
            // Part of an utterance was heard: the engine marks it lost, as without the layer.
            bump(&self.common.counters.recovery_failures);
            let _ = events.send(TtsEvent::Error(error)).await;
            return false;
        }
        // Cancels the dead session never acknowledged are acknowledged here.
        let cancelled: Vec<ContextId> = self
            .ledger
            .iter()
            .filter(|c| c.cancelled)
            .map(|c| c.id)
            .collect();
        for context in cancelled {
            self.forget(context);
            if events.send(TtsEvent::Cancelled { context }).await.is_err() {
                return false;
            }
        }
        if let Some(standby) = self.standby.take() {
            // A refreshed session is already open: it takes over at once.
            self.live = Some(standby);
            self.last_traffic = self.common.clock.now();
            bump(&self.common.counters.refreshes);
            if !self.replay() {
                bump(&self.common.counters.recovery_failures);
                let _ = events.send(TtsEvent::Error(error)).await;
                return false;
            }
            bump(&self.common.counters.recoveries);
            return true;
        }
        *cause = Some(error);
        *recovery = Some(self.connecting(Some(live.leg)));
        true
    }
}
