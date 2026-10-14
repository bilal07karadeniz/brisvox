//! The session task of the [`S2sStage`](super::S2sStage): owns the provider session, the epoch,
//! the response state, the tools and every frame the stage emits (see the module docs of
//! [`super`]).

use std::collections::VecDeque;
use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::tools::{ToolEngine, ToolRegistry};
use brisvox_agent::{Assembled, CallError, Context, InvalidCall, Message, ToolCall, ToolCallId};
use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, ErrorFrame, Frame, SystemFrame,
};
use brisvox_core::{Clock, FrameSender, SharedClock, TurnEpoch, TurnEvent};
use brisvox_providers::s2s::{
    S2sConfig, S2sControl, S2sEvent, S2sResponseStatus, S2sSession, S2sTurnDetection,
};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{Boxed, Cmd, S2sStageConfig, S2sStats, S2sTurnMode, TaskParts, bump};
use crate::events::{EngineEvent, EngineEvents};
use crate::playout::{PlayoutBook, RoundKey};
use crate::tts_audio::OutputFramer;

/// Caller audio kept before the local VAD fires, sent when it does ([`S2sTurnMode::Local`]).
const PRE_ROLL: Duration = Duration::from_millis(300);

/// A session that lived at least this long before dying was healthy: its death does not count
/// against the reconnect budget.
const HEALTHY_SESSION: Duration = Duration::from_secs(10);

/// A seeded session that dies within this long of opening is taken to have refused its seed
/// (a history shape the provider does not accept closes the socket): the next session is opened
/// without history rather than failing the same way again.
const SEED_GRACE: Duration = Duration::from_secs(5);

/// The caller counts as speaking while loud frames arrived within this long (the provider may not
/// report speech starts, so a rotation must not cut an utterance it never announced).
const QUIET_WINDOW: Duration = Duration::from_millis(700);

/// Mean absolute amplitude (16-bit PCM) from which a caller frame counts as voice.
const VOICE_LEVEL: u32 = 500;

/// Longest reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Items of recent responses kept for truncation and the local mirror.
const MAX_ITEMS: usize = 32;

/// Calls remembered for de-duplication (the adapter de-duplicates too).
const MAX_SEEN_CALLS: usize = 256;

/// Calls of one response the stage will run.
const MAX_CALLS_PER_RESPONSE: usize = 16;

/// What the model is told when the tools of the turn will not run any more.
const TOOLS_UNAVAILABLE: &str = "The tools could not be used any more for this turn (limit reached or temporarily unavailable). Answer the caller now without calling any tool; apologise briefly if you cannot help.";

/// Pause of the warm-up before the first connect.
const WARM_BUDGET: Duration = Duration::from_millis(300);

type ConnectResult = Option<brisvox_providers::Result<S2sSession>>;

/// The outcome of a replacement connect, with the length the mirror had when its seed was taken
/// (the replacement is only used while the mirror is still that long).
type StandbyResult = (ConnectResult, usize, bool);

/// Whether a provider accepts the history a new session is seeded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeedTrust {
    /// Seeds are used.
    Trusted,
    /// A seeded session died right after opening: the next session opens without history, and
    /// decides whether the seed was the cause.
    RetryUnseeded,
    /// An unseeded session lived where the seeded one did not: no seed is sent again.
    Refused,
}

/// How often a due rotation re-checks whether the bot is idle.
const ROTATE_POLL: Duration = Duration::from_millis(250);

struct Live {
    audio: mpsc::Sender<Bytes>,
    control: mpsc::Sender<S2sControl>,
    events: mpsc::Receiver<S2sEvent>,
    since: Instant,
    /// The session was opened with a history.
    seeded: bool,
}

/// One assistant audio item of a recent response.
#[derive(Debug, Clone)]
struct ItemRecord {
    item: Arc<str>,
    key: RoundKey,
    epoch: TurnEpoch,
    /// Model audio received for the item (what the server holds).
    received: Duration,
    transcript: String,
    /// The text reached the local context (played whole, or cut).
    resolved: bool,
    /// A truncation was decided for the item.
    truncated: bool,
}

struct PendingCall {
    provider_id: Arc<str>,
    name: Arc<str>,
    arguments: Arc<str>,
}

struct Resp {
    id: Arc<str>,
    epoch: TurnEpoch,
    /// The item whose audio is being framed, with its book utterance.
    current: Option<(Arc<str>, u64)>,
    calls: Vec<PendingCall>,
}

/// What a finished tool round hands back to the task.
struct ToolsDone {
    epoch: TurnEpoch,
    /// The provider call ids, in the order of the engine's results.
    provider_ids: Vec<Arc<str>>,
    execution: brisvox_agent::tools::Execution,
}

pub(super) async fn run(parts: TaskParts, downstream: FrameSender) {
    let mut task = Task::new(parts, downstream);
    task.main().await;
}

struct Task {
    cfg: S2sStageConfig,
    provider: Arc<dyn super::S2sConnect>,
    clock: SharedClock,
    timeline: brisvox_core::TimelineRecorder,
    book: Arc<PlayoutBook>,
    rates: crate::rates::CallRatesCell,
    offload: crate::Offload,
    hook: crate::turn_driver::ClearHook,
    tools: Option<(Arc<ToolRegistry>, Arc<ToolEngine>)>,
    events: Option<EngineEvents>,
    context: Arc<std::sync::Mutex<Context>>,
    stats: S2sStats,
    downstream: FrameSender,
    audio_rx: mpsc::Receiver<Bytes>,
    cmd_rx: mpsc::Receiver<Cmd>,

    // -- the provider session
    live: Option<Live>,
    connecting: Option<Boxed<'static, ConnectResult>>,
    reconnect_at: Option<Instant>,
    /// A replacement session being opened while the live one keeps serving (rotation).
    standby: Option<Boxed<'static, StandbyResult>>,
    /// When the live session is due to be replaced (its limit minus the margin).
    rotate_at: Option<Instant>,
    /// Failed replacement attempts in a row (backoff).
    rotate_fails: u32,
    seed_trust: SeedTrust,
    /// The pending `connecting` session carries a history.
    connecting_seeded: bool,
    /// When the caller's audio was last loud (see [`QUIET_WINDOW`]).
    last_voice: Option<Instant>,
    streak: u32,
    gave_up: bool,
    first_connect: bool,

    // -- turns
    epoch: TurnEpoch,
    user_speaking: bool,
    resp: Option<Resp>,
    /// A response was requested (or the provider owes one after the caller's turn ended) and has
    /// not started: the bot is busy, until the deadline gives up.
    pending_response: Option<TurnEpoch>,
    deadline: Option<Instant>,
    /// The epoch whose audio is on its way to, or playing at, the client.
    playing_epoch: Option<TurnEpoch>,
    /// The epoch a `TtsEnded` was already queued for.
    ended_epoch: Option<TurnEpoch>,
    tools_current: u32,
    tool_cancel: CancellationToken,
    tool_rounds: (TurnEpoch, u32),
    /// The epoch for which the model was already asked to answer after the tool-round limit
    /// (asked once per epoch, so a model that keeps calling tools cannot loop the stage).
    limit_answered: Option<TurnEpoch>,
    round_counter: (TurnEpoch, u32),
    next_call: u32,
    seen_calls: VecDeque<Arc<str>>,
    items: VecDeque<ItemRecord>,

    // -- audio
    in_framer: Option<OutputFramer>,
    out_framer: Option<OutputFramer>,
    out_rate: Option<brisvox_core::frame::SampleRate>,
    scratch: Vec<Bytes>,
    preroll: VecDeque<Bytes>,
    out: VecDeque<Frame>,
    out_audio: Duration,
    next_utterance: u64,
    seq: u64,
    /// Audio chunks taken from `audio_rx` so far (compared with the sequence on commands).
    audio_seen: u64,

    // -- helpers
    tool_tasks: JoinSet<ToolsDone>,
    jobs: JoinSet<()>,
}

async fn next_event(live: &mut Option<Live>) -> Option<S2sEvent> {
    match live {
        Some(live) => live.events.recv().await,
        None => pending().await,
    }
}

async fn poll_connect(connecting: &mut Option<Boxed<'static, ConnectResult>>) -> ConnectResult {
    match connecting {
        Some(future) => future.as_mut().await,
        None => pending().await,
    }
}

async fn poll_standby(standby: &mut Option<Boxed<'static, StandbyResult>>) -> StandbyResult {
    match standby {
        Some(future) => future.as_mut().await,
        None => pending().await,
    }
}

async fn wait_at(clock: &dyn Clock, at: Option<Instant>) {
    match at {
        Some(at) => clock.sleep_until(at).await,
        None => pending().await,
    }
}

async fn send_front(downstream: &FrameSender, frame: Option<Frame>) -> brisvox_core::Result<()> {
    match frame {
        Some(frame) => downstream.send(frame).await,
        None => pending().await,
    }
}

fn control_frame(epoch: TurnEpoch, kind: ControlKind) -> Frame {
    Frame::Control(ControlFrame { epoch, kind })
}

/// Duration of `bytes` of PCM16 mono at `rate_hz`.
fn pcm_duration(bytes: usize, rate_hz: u32) -> Duration {
    let samples = u64::try_from(bytes / 2).unwrap_or(u64::MAX);
    let nanos = samples
        .saturating_mul(1_000_000_000)
        .checked_div(u64::from(rate_hz.max(1)))
        .unwrap_or(0);
    Duration::from_nanos(nanos)
}

/// The leading part of `text` that corresponds to `heard` of `received` audio, cut at a word
/// boundary (the transcript is not aligned to the audio: this is an estimate, the model's own
/// history is cut with the exact duration).
fn cut_transcript(text: &str, heard: Duration, received: Duration) -> String {
    if received.is_zero() || heard >= received {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let share = heard.as_secs_f64() / received.as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let wanted = (chars.len() as f64 * share).floor();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut end = (wanted as usize).min(chars.len());
    // Mid-word: back up to the previous boundary.
    if chars.get(end).is_some_and(|c| !c.is_whitespace()) {
        while end > 0 && chars.get(end - 1).is_some_and(|c| !c.is_whitespace()) {
            end -= 1;
        }
    }
    chars
        .get(..end)
        .map(|prefix| prefix.iter().collect::<String>().trim_end().to_owned())
        .unwrap_or_default()
}

impl Task {
    fn new(parts: TaskParts, downstream: FrameSender) -> Self {
        let TaskParts {
            config,
            provider,
            clock,
            timeline,
            book,
            rates,
            offload,
            hook,
            tools,
            events,
            context,
            stats,
            audio_rx,
            cmd_rx,
        } = parts;
        Self {
            cfg: config,
            provider,
            clock,
            timeline,
            book,
            rates,
            offload,
            hook,
            tools,
            events,
            context,
            stats,
            downstream,
            audio_rx,
            cmd_rx,
            live: None,
            connecting: None,
            reconnect_at: None,
            standby: None,
            rotate_at: None,
            rotate_fails: 0,
            seed_trust: SeedTrust::Trusted,
            connecting_seeded: false,
            last_voice: None,
            streak: 0,
            gave_up: false,
            first_connect: true,
            epoch: TurnEpoch::ZERO,
            user_speaking: false,
            resp: None,
            pending_response: None,
            deadline: None,
            playing_epoch: None,
            ended_epoch: None,
            tools_current: 0,
            tool_cancel: CancellationToken::new(),
            tool_rounds: (TurnEpoch::ZERO, 0),
            limit_answered: None,
            round_counter: (TurnEpoch::ZERO, 0),
            next_call: 0,
            seen_calls: VecDeque::new(),
            items: VecDeque::new(),
            in_framer: None,
            out_framer: None,
            out_rate: None,
            scratch: Vec::new(),
            preroll: VecDeque::new(),
            out: VecDeque::new(),
            out_audio: Duration::ZERO,
            next_utterance: 1,
            seq: 0,
            audio_seen: 0,
            tool_tasks: JoinSet::new(),
            jobs: JoinSet::new(),
        }
    }

    // -- the loop ----------------------------------------------------------------------------

    async fn main(&mut self) {
        self.start_connect();
        loop {
            let deadline = self.deadline;
            let reconnect = self.reconnect_at;
            let rotate = self.rotate_at;
            let front = self.out.front().cloned();
            tokio::select! {
                biased;
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(cmd) => self.on_cmd(cmd).await,
                    None => break,
                },
                event = next_event(&mut self.live) => match event {
                    Some(event) => self.on_event(event).await,
                    None => self.session_lost("the session ended"),
                },
                Some(joined) = self.tool_tasks.join_next(), if !self.tool_tasks.is_empty() => {
                    self.on_tool_done(joined);
                }
                Some(_) = self.jobs.join_next(), if !self.jobs.is_empty() => {}
                connected = poll_connect(&mut self.connecting), if self.connecting.is_some() => {
                    self.on_connected(connected);
                }
                audio = self.audio_rx.recv() => match audio {
                    Some(audio) => {
                        self.audio_seen = self.audio_seen.saturating_add(1);
                        self.on_audio(audio).await;
                    }
                    None => break,
                },
                () = wait_at(&*self.clock, deadline), if deadline.is_some() => self.on_deadline(),
                () = wait_at(&*self.clock, reconnect), if reconnect.is_some() => self.start_connect(),
                standby = poll_standby(&mut self.standby), if self.standby.is_some() => {
                    self.on_standby(standby);
                }
                () = wait_at(&*self.clock, rotate), if rotate.is_some() => self.on_rotate_due(),
                sent = send_front(&self.downstream, front), if !self.out.is_empty() => {
                    if sent.is_err() {
                        break;
                    }
                    self.sent_front();
                }
            }
        }
    }

    fn publish(&self, event: EngineEvent) {
        if let Some(events) = &self.events {
            events.publish(event);
        }
    }

    fn ctx_push(&self, message: Message) {
        if let Ok(mut context) = self.context.lock() {
            context.push(message);
        }
    }

    /// The bot is doing something the caller could interrupt.
    fn bot_busy(&self) -> bool {
        self.resp.is_some()
            || self.pending_response.is_some()
            || self.tools_current > 0
            || self.playing_epoch.is_some()
    }

    // -- the session -------------------------------------------------------------------------

    fn s2s_config(&self) -> S2sConfig {
        let mut config = S2sConfig::new(Arc::clone(&self.cfg.instructions));
        config.voice.clone_from(&self.cfg.voice);
        config.language.clone_from(&self.cfg.language);
        config.input_rate = self.cfg.provider_rate;
        config.output_rate = self.cfg.provider_rate;
        // A provider that rotates sessions re-seeds the next one from the mirror, and the mirror
        // only learns what the caller said from the provider's transcripts: they are needed then,
        // whatever the options say.
        config.input_transcription =
            self.cfg.options.input_transcription || self.provider.session_limit().is_some();
        config.turn_detection = match self.cfg.options.turn {
            S2sTurnMode::Provider => S2sTurnDetection::Provider,
            S2sTurnMode::Local => S2sTurnDetection::Manual,
        };
        if let Some((registry, _)) = &self.tools {
            config.tools = crate::convert::to_provider_tools(registry.specs());
        }
        // A new session continues the conversation: the mirror is the source of truth (unless the
        // provider turned out to refuse a seed).
        if self.seed_trust == SeedTrust::Trusted
            && let Ok(context) = self.context.lock()
        {
            config.history = super::seed::seed_history(context.messages());
        }
        config
    }

    /// The connect (and, on the first one, the warm-up) as a future that owns its inputs.
    fn connect_future(&self, config: S2sConfig, warm: bool) -> Boxed<'static, ConnectResult> {
        let provider = Arc::clone(&self.provider);
        let clock = Arc::clone(&self.clock);
        let limit = self.cfg.connect_timeout;
        Box::pin(async move {
            if warm {
                // Best effort and short: a failed warm-up never stops the connect.
                let budget = limit.min(WARM_BUDGET);
                tokio::select! {
                    biased;
                    _ = provider.warm() => {}
                    () = clock.sleep(budget) => {}
                }
            }
            tokio::select! {
                biased;
                result = provider.connect(config) => Some(result),
                () = clock.sleep(limit) => None,
            }
        })
    }

    fn start_connect(&mut self) {
        self.reconnect_at = None;
        if self.gave_up || self.connecting.is_some() || self.live.is_some() {
            return;
        }
        let config = self.s2s_config();
        self.connecting_seeded = !config.history.is_empty();
        let warm = std::mem::take(&mut self.first_connect);
        self.connecting = Some(self.connect_future(config, warm));
    }

    fn on_connected(&mut self, result: ConnectResult) {
        self.connecting = None;
        match result {
            Some(Ok(session)) => {
                bump(&self.stats.counters.sessions_opened);
                let seeded = self.connecting_seeded;
                self.set_live(session, seeded);
            }
            Some(Err(error)) => {
                bump(&self.stats.counters.connect_failures);
                tracing::warn!(target: "brisvox::engine", %error, "s2s connect failed");
                self.schedule_reconnect();
            }
            None => {
                bump(&self.stats.counters.connect_failures);
                tracing::warn!(target: "brisvox::engine", "s2s connect timed out");
                self.schedule_reconnect();
            }
        }
    }

    /// Makes `session` the live one and arms its rotation timer.
    fn set_live(&mut self, session: S2sSession, seeded: bool) {
        let now = self.clock.now();
        self.live = Some(Live {
            audio: session.audio,
            control: session.control,
            events: session.events,
            since: now,
            seeded,
        });
        // Rotate a margin before the provider's limit (at most halfway, so a short limit does not
        // make the stage rotate in a loop).
        self.rotate_at = self.provider.session_limit().map(|limit| {
            let early = limit.saturating_sub(self.cfg.rotate_margin);
            brisvox_core::clock::deadline_after(now, early.max(limit / 2))
        });
        self.rotate_fails = 0;
    }

    /// The bot and the caller are quiet: a session can be swapped without cutting anything.
    fn idle(&self) -> bool {
        !self.bot_busy() && !self.user_speaking && !self.voice_recent()
    }

    /// Loud caller audio arrived within the [`QUIET_WINDOW`]: an utterance may be under way even
    /// if the provider has not announced it.
    fn voice_recent(&self) -> bool {
        self.last_voice
            .is_some_and(|at| self.clock.now().saturating_duration_since(at) < QUIET_WINDOW)
    }

    /// The live session reached its rotation time (or the provider announced its end).
    fn on_rotate_due(&mut self) {
        self.rotate_at = None;
        if self.live.is_none()
            || self.gave_up
            || self.standby.is_some()
            || self.connecting.is_some()
        {
            // Without a live session the reconnect path arms the next rotation.
            return;
        }
        if !self.idle() {
            self.rotate_at = Some(brisvox_core::clock::deadline_after(
                self.clock.now(),
                ROTATE_POLL,
            ));
            return;
        }
        // An unseeded session that outlived the grace where the seeded one did not proves the
        // seed was refused: stop sending it.
        if self.seed_trust == SeedTrust::RetryUnseeded
            && let Some(live) = &self.live
            && !live.seeded
            && self.clock.now().saturating_duration_since(live.since) >= SEED_GRACE
        {
            self.seed_trust = SeedTrust::Refused;
            tracing::warn!(target: "brisvox::engine", "s2s provider refuses history seeds; sessions are no longer seeded");
        }
        let config = self.s2s_config();
        let seeded = !config.history.is_empty();
        let seed_len = self.context.lock().map_or(0, |c| c.len());
        let connect = self.connect_future(config, false);
        self.standby = Some(Box::pin(async move { (connect.await, seed_len, seeded) }));
    }

    /// The replacement session is open (or failed): switch over if nothing happened meanwhile.
    fn on_standby(&mut self, (result, seed_len, seeded): StandbyResult) {
        self.standby = None;
        match result {
            Some(Ok(session)) => {
                let unchanged = self.context.lock().is_ok_and(|c| c.len() == seed_len);
                if self.live.is_some() && self.idle() && unchanged {
                    bump(&self.stats.counters.sessions_opened);
                    bump(&self.stats.counters.rotations);
                    tracing::info!(target: "brisvox::engine", "s2s session rotated");
                    // Dropping the old `Live` closes the old session.
                    self.set_live(session, seeded);
                } else {
                    // The conversation moved on while the replacement was opening: its seed is
                    // stale. Drop it and try again at the next quiet moment.
                    bump(&self.stats.counters.rotations_deferred);
                    self.rotate_at = Some(brisvox_core::clock::deadline_after(
                        self.clock.now(),
                        ROTATE_POLL,
                    ));
                }
            }
            failed => {
                bump(&self.stats.counters.rotation_failures);
                if let Some(Err(error)) = &failed {
                    tracing::warn!(target: "brisvox::engine", %error, "s2s rotation connect failed");
                }
                self.rotate_fails = self.rotate_fails.saturating_add(1);
                let exponent = self.rotate_fails.saturating_sub(1).min(8);
                let backoff = self
                    .cfg
                    .reconnect_backoff
                    .saturating_mul(1_u32 << exponent)
                    .min(MAX_BACKOFF);
                self.rotate_at = Some(brisvox_core::clock::deadline_after(
                    self.clock.now(),
                    backoff,
                ));
            }
        }
    }

    fn schedule_reconnect(&mut self) {
        self.streak = self.streak.saturating_add(1);
        if self.streak > self.cfg.max_reconnects {
            self.gave_up = true;
            self.error_frame(
                "the speech-to-speech session is gone and could not be re-established",
                true,
            );
            return;
        }
        let exponent = self.streak.saturating_sub(1).min(8);
        let backoff = self
            .cfg
            .reconnect_backoff
            .saturating_mul(1_u32 << exponent)
            .min(MAX_BACKOFF);
        self.reconnect_at = Some(brisvox_core::clock::deadline_after(
            self.clock.now(),
            backoff,
        ));
    }

    /// The session is dead: drop it, settle the state that depended on it, and reconnect.
    fn session_lost(&mut self, reason: &str) {
        let Some(live) = self.live.take() else {
            return;
        };
        bump(&self.stats.counters.session_failures);
        let lived = self.clock.now().saturating_duration_since(live.since);
        if lived >= HEALTHY_SESSION {
            self.streak = 0;
        }
        if lived < SEED_GRACE {
            match (self.seed_trust, live.seeded) {
                // A seeded session died at once: try again without the history.
                (SeedTrust::Trusted, true) => {
                    self.seed_trust = SeedTrust::RetryUnseeded;
                    tracing::warn!(target: "brisvox::engine", "a seeded s2s session died at once; the next one opens without history");
                }
                // The unseeded retry died too: the seed was not the cause.
                (SeedTrust::RetryUnseeded, false) => self.seed_trust = SeedTrust::Trusted,
                _ => {}
            }
        }
        // A replacement was being prepared for the dead session: the reconnect path takes over.
        self.standby = None;
        self.rotate_at = None;
        tracing::warn!(target: "brisvox::engine", reason, "s2s session lost");
        self.error_frame(
            &format!("the speech-to-speech session was lost: {reason}"),
            false,
        );
        // What the dead session owed us will not come.
        self.finish_current_item();
        self.resp = None;
        self.pending_response = None;
        self.deadline = None;
        self.preroll.clear();
        self.maybe_end_epoch();
        self.schedule_reconnect();
    }

    fn error_frame(&mut self, message: &str, fatal: bool) {
        self.out
            .push_back(Frame::System(SystemFrame::Error(Box::new(ErrorFrame {
                stage: "s2s",
                message: message.to_owned(),
                fatal,
            }))));
    }

    fn send_control(&mut self, control: S2sControl) -> bool {
        let Some(live) = &self.live else {
            return false;
        };
        if live.control.try_send(control).is_ok() {
            return true;
        }
        // Full means a stuck adapter, closed a dead one.
        self.session_lost("the control queue is full or closed");
        false
    }

    // -- caller audio ------------------------------------------------------------------------

    async fn ensure_in_framer(&mut self) -> bool {
        if self.in_framer.is_some() {
            return true;
        }
        let (from, to) = (self.cfg.model_rate, self.cfg.provider_rate);
        let samples = frame_samples(self.cfg.frame, to);
        let built = self
            .offload
            .run(self.cfg.offload_timeout, move || {
                OutputFramer::build_resampler(from, to)
            })
            .await;
        match built.map(|done| done.value) {
            Ok(Ok(resampler)) => {
                self.in_framer = Some(OutputFramer::with_resampler(to, samples, resampler));
                true
            }
            _ => {
                bump(&self.stats.counters.resampler_failures);
                false
            }
        }
    }

    async fn on_audio(&mut self, audio: Bytes) {
        if self.live.is_none() {
            bump(&self.stats.counters.audio_dropped_no_session);
            return;
        }
        if !self.ensure_in_framer().await {
            return;
        }
        let mut frames = std::mem::take(&mut self.scratch);
        if let Some(framer) = &mut self.in_framer
            && framer
                .push(&audio, &mut |frame| frames.push(frame))
                .is_err()
        {
            framer.reset();
        }
        for frame in frames.drain(..) {
            self.route_input(frame);
        }
        self.scratch = frames;
    }

    fn route_input(&mut self, frame: Bytes) {
        if is_voiced(&frame) {
            self.last_voice = Some(self.clock.now());
        }
        match self.cfg.options.turn {
            S2sTurnMode::Provider => self.send_audio(frame),
            S2sTurnMode::Local => {
                if self.user_speaking {
                    self.send_audio(frame);
                } else {
                    self.preroll.push_back(frame);
                    let keep =
                        usize::try_from(PRE_ROLL.as_nanos() / self.cfg.frame.as_nanos().max(1))
                            .unwrap_or(usize::MAX)
                            .max(1);
                    while self.preroll.len() > keep {
                        self.preroll.pop_front();
                    }
                }
            }
        }
    }

    fn send_audio(&mut self, frame: Bytes) {
        use tokio::sync::mpsc::error::TrySendError;
        let Some(live) = &self.live else {
            bump(&self.stats.counters.audio_dropped_no_session);
            return;
        };
        match live.audio.try_send(frame) {
            Ok(()) => bump(&self.stats.counters.audio_to_provider),
            Err(TrySendError::Full(_)) => bump(&self.stats.counters.audio_dropped_provider),
            Err(TrySendError::Closed(_)) => self.session_lost("the audio queue is closed"),
        }
    }

    // -- commands from the stage -------------------------------------------------------------

    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::UserStarted(at, queued) => {
                if self.cfg.options.turn == S2sTurnMode::Local {
                    self.catch_up(queued).await;
                    self.user_started(at).await;
                }
            }
            Cmd::UserStopped(at, queued) => {
                if self.cfg.options.turn == S2sTurnMode::Local {
                    self.catch_up(queued).await;
                    self.user_stopped(at).await;
                }
            }
            Cmd::BotEnded(epoch) => self.bot_ended(epoch),
        }
    }

    /// Handles the caller audio the stage queued before a command was issued (`queued` chunks
    /// in all). Commands and audio travel on separate queues and the loop polls commands first,
    /// so without this the last frames of an utterance would still sit in the audio queue when
    /// `UserStopped` is handled: they would miss the committed buffer and leak into the next
    /// turn's pre-roll. Only chunks that were queued before the command are taken, so this is
    /// bounded by the queue capacity.
    async fn catch_up(&mut self, queued: u64) {
        while self.audio_seen < queued {
            match self.audio_rx.try_recv() {
                Ok(audio) => {
                    self.audio_seen = self.audio_seen.saturating_add(1);
                    self.on_audio(audio).await;
                }
                Err(_) => break,
            }
        }
    }

    /// The client played the end of the response of `epoch`.
    fn bot_ended(&mut self, epoch: TurnEpoch) {
        if self.playing_epoch.is_some_and(|playing| playing <= epoch) {
            self.playing_epoch = None;
        }
        // The text of what was played whole reaches the local context.
        let mut texts = Vec::new();
        for record in &mut self.items {
            if record.epoch <= epoch && !record.resolved && !record.truncated {
                record.resolved = true;
                // Played to its end: nothing to cut in the model's history later.
                record.truncated = true;
                if !record.transcript.is_empty() {
                    texts.push(record.transcript.clone());
                }
            }
        }
        for text in texts {
            self.ctx_push(Message::assistant(text));
        }
    }

    // -- turns -------------------------------------------------------------------------------

    async fn user_started(&mut self, at: Instant) {
        self.user_speaking = true;
        // A request the model makes while this speech goes on is not answered by it (the
        // provider's stop event may arrive after the model's tool call).
        if let Some((_, engine)) = &self.tools {
            engine.confirmations().note_user_speech_started();
        }
        let busy = self.bot_busy();
        let previous = self.epoch;
        self.epoch = self.epoch.next();
        let epoch = self.epoch;
        self.deadline = None;
        self.publish(EngineEvent::UserStartedSpeaking { epoch });
        self.flush_late_notes();
        if self.cfg.options.turn == S2sTurnMode::Local {
            // The provider's buffer starts with the pre-roll.
            let frames: Vec<Bytes> = self.preroll.drain(..).collect();
            for frame in frames {
                self.send_audio(frame);
            }
        }
        if busy {
            self.timeline
                .interrupt(previous, epoch, self.clock.now(), Some(at));
            self.barge_in(previous).await;
        } else {
            self.timeline.mark(epoch, TurnEvent::UserSpeechStart, at);
        }
    }

    async fn user_stopped(&mut self, at: Instant) {
        self.user_speaking = false;
        let epoch = self.epoch;
        self.timeline.mark(epoch, TurnEvent::UserSpeechEnd, at);
        self.timeline
            .mark(epoch, TurnEvent::TurnEnd, self.clock.now());
        self.publish(EngineEvent::UserStoppedSpeaking { epoch });
        // The user's turn ended: a confirmation held before it can now be given.
        if let Some((_, engine)) = &self.tools {
            engine.confirmations().note_user_turn();
        }
        self.pending_response = Some(epoch);
        self.arm_deadline();
        if self.cfg.options.turn == S2sTurnMode::Local {
            // The history must be cut before the model answers.
            self.finish_jobs().await;
            self.flush_late_notes();
            if self.send_control(S2sControl::Commit) {
                bump(&self.stats.counters.response_requests);
                self.send_control(S2sControl::CreateResponse);
            }
        }
    }

    fn arm_deadline(&mut self) {
        self.deadline = Some(brisvox_core::clock::deadline_after(
            self.clock.now(),
            self.cfg.response_deadline,
        ));
    }

    fn on_deadline(&mut self) {
        self.deadline = None;
        if self.pending_response.take().is_some() {
            bump(&self.stats.counters.response_timeouts);
            self.error_frame("no response started in time", false);
            self.maybe_end_epoch();
        }
    }

    /// Waits for the truncation jobs, bounded.
    async fn finish_jobs(&mut self) {
        let limit = self
            .cfg
            .settle_wait
            .saturating_mul(2)
            .max(self.cfg.send_timeout);
        let clock = Arc::clone(&self.clock);
        let jobs = &mut self.jobs;
        let drain = async { while jobs.join_next().await.is_some() {} };
        tokio::select! {
            biased;
            () = drain => {}
            () = clock.sleep(limit) => {}
        }
    }

    /// Late results of side-effect tools go to the model as notes.
    fn flush_late_notes(&mut self) {
        let Some((_, engine)) = &self.tools else {
            return;
        };
        let notes = engine.take_late_notes();
        for note in notes {
            self.send_control(S2sControl::Note {
                text: Arc::from(note.text()),
            });
            self.ctx_push(note);
        }
    }

    /// A barge-in: the call moved to the new epoch, everything of the old one is cut.
    async fn barge_in(&mut self, old: TurnEpoch) {
        bump(&self.stats.counters.barge_ins);
        // A held confirmation does not survive a barge-in.
        if let Some((_, engine)) = &self.tools {
            engine.confirmations().clear();
        }
        let epoch = self.epoch;
        // 1. The transport: note the clear in the book, then Clear (the fast path).
        let hook = Arc::clone(&self.hook);
        let cleared = {
            let clock = Arc::clone(&self.clock);
            let limit = self.cfg.send_timeout;
            tokio::select! {
                biased;
                result = hook(epoch) => result.map_err(|_| ()),
                () = clock.sleep(limit) => Err(()),
            }
        };
        if cleared.is_err() {
            bump(&self.stats.counters.clear_failures);
        }
        // 2. The stages behind us.
        self.send_system(Frame::System(SystemFrame::Interrupt { new_epoch: epoch }))
            .await;
        // 3. Everything of ours that was not sent yet is stale.
        let before = self.out.len();
        self.out.retain(|frame| matches!(frame, Frame::System(_)));
        let discarded = before.saturating_sub(self.out.len());
        self.stats.counters.queued_discarded.fetch_add(
            u64::try_from(discarded).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.out_audio = Duration::ZERO;
        if let Some(framer) = &mut self.out_framer {
            framer.reset();
        }
        let was_generating = self.resp.is_some() || self.pending_response.is_some();
        self.resp = None;
        self.pending_response = None;
        self.playing_epoch = None;
        self.ended_epoch = None;
        self.tools_current = 0;
        self.tool_cancel.cancel();
        self.tool_cancel = CancellationToken::new();
        // 4. The provider: stop the response when the provider's VAD does not.
        if self.cfg.options.turn == S2sTurnMode::Local && was_generating {
            self.send_control(S2sControl::CancelResponse);
        }
        // 5. The model's history keeps only what the caller heard.
        self.start_truncation(old);
        self.publish(EngineEvent::BotInterrupted { epoch });
    }

    async fn send_system(&mut self, frame: Frame) {
        let clock = Arc::clone(&self.clock);
        let limit = self.cfg.send_timeout;
        let sent = tokio::select! {
            biased;
            sent = self.downstream.send(frame) => sent.is_ok(),
            () = clock.sleep(limit) => false,
        };
        if !sent {
            tracing::warn!(target: "brisvox::engine", "s2s: a system frame could not be sent");
        }
    }

    /// Spawns the job that cuts the model's history to what the caller heard of every item up
    /// to `old` that was not cut yet.
    fn start_truncation(&mut self, old: TurnEpoch) {
        let mut candidates = Vec::new();
        for record in &mut self.items {
            if record.epoch <= old && !record.truncated && !record.received.is_zero() {
                record.truncated = true;
                candidates.push(record.clone());
            }
        }
        if candidates.is_empty() {
            return;
        }
        // Without a session (a reconnect backoff) there is nothing to cut on the server, but the
        // job still records what the caller heard in the local context.
        let env = JobEnv {
            book: Arc::clone(&self.book),
            clock: Arc::clone(&self.clock),
            lead_margin: self.cfg.lead_margin,
            settle_wait: self.cfg.settle_wait,
            send_timeout: self.cfg.send_timeout,
            frame: self.cfg.frame,
            control: self.live.as_ref().map(|live| live.control.clone()),
            context: Arc::clone(&self.context),
            stats: self.stats.clone(),
        };
        self.jobs.spawn(truncate_job(env, old, candidates));
    }

    // -- provider events ---------------------------------------------------------------------

    async fn on_event(&mut self, event: S2sEvent) {
        match event {
            S2sEvent::SpeechStarted => {
                if self.cfg.options.turn == S2sTurnMode::Provider {
                    let at = self.clock.now();
                    self.user_started(at).await;
                }
            }
            S2sEvent::SpeechStopped => {
                if self.cfg.options.turn == S2sTurnMode::Provider {
                    let at = self.clock.now();
                    self.user_stopped(at).await;
                }
            }
            S2sEvent::ResponseStarted { response } => self.on_response_started(response),
            S2sEvent::Audio {
                response,
                item,
                pcm,
            } => {
                self.on_provider_audio(&response, &item, &pcm).await;
            }
            S2sEvent::AudioDone { response, item } => {
                if self.is_current(&response) {
                    if self
                        .resp
                        .as_ref()
                        .and_then(|r| r.current.as_ref())
                        .is_some_and(|(current, _)| *current == item)
                    {
                        self.finish_current_item();
                    }
                } else {
                    bump(&self.stats.counters.stale_events);
                }
            }
            S2sEvent::AssistantTranscript {
                response,
                item,
                text,
                is_final,
            } => self.on_transcript(&response, &item, &text, is_final),
            S2sEvent::UserTranscript { text } => {
                self.publish(EngineEvent::UserTranscript {
                    text: Arc::clone(&text),
                    is_final: true,
                });
                self.publish(EngineEvent::UserText {
                    epoch: self.epoch,
                    text: Arc::clone(&text),
                });
                self.ctx_push(Message::user(text));
            }
            S2sEvent::ToolCall {
                response,
                call_id,
                name,
                arguments,
            } => self.on_tool_call(&response, call_id, name, arguments),
            S2sEvent::ResponseDone { response, status } => {
                self.on_response_done(&response, status);
            }
            S2sEvent::Warning { code, message } => {
                bump(&self.stats.counters.warnings);
                tracing::warn!(target: "brisvox::engine", %code, %message, "s2s provider warning");
            }
            S2sEvent::SessionEnding { time_left } => {
                // The provider will close this session: replace it now, while it still works.
                tracing::info!(target: "brisvox::engine", ?time_left, "s2s session ending");
                if self.live.is_some() && self.standby.is_none() {
                    self.rotate_at = Some(self.clock.now());
                }
            }
            S2sEvent::Error(error) => {
                let reason = error.to_string();
                self.session_lost(&reason);
            }
        }
    }

    fn is_current(&self, response: &str) -> bool {
        self.resp.as_ref().is_some_and(|r| &*r.id == response)
    }

    fn on_response_started(&mut self, response: Arc<str>) {
        // A response that was not asked for after the caller started again is not ours to play:
        // in provider mode the provider cancels it, in local mode it is simply not current.
        self.finish_current_item();
        self.pending_response = None;
        self.deadline = None;
        self.resp = Some(Resp {
            id: response,
            epoch: self.epoch,
            current: None,
            calls: Vec::new(),
        });
        self.publish(EngineEvent::BotLlmStarted { epoch: self.epoch });
    }

    async fn on_provider_audio(&mut self, response: &str, item: &Arc<str>, pcm: &Bytes) {
        bump(&self.stats.counters.provider_audio_chunks);
        if !self.is_current(response) {
            bump(&self.stats.counters.stale_events);
            return;
        }
        let Some(epoch) = self.resp.as_ref().map(|r| r.epoch) else {
            return;
        };
        let starts_item = self
            .resp
            .as_ref()
            .and_then(|r| r.current.as_ref())
            .is_none_or(|(current, _)| current != item);
        if starts_item && !self.start_item(epoch, item).await {
            return;
        }
        let Some(utterance) = self
            .resp
            .as_ref()
            .and_then(|r| r.current.as_ref())
            .map(|(_, utterance)| *utterance)
        else {
            return;
        };
        let received = pcm_duration(pcm.len(), self.cfg.provider_rate.hz());
        if let Some(record) = self.items.iter_mut().rev().find(|r| r.item == *item) {
            record.received = record.received.saturating_add(received);
        }
        if self.out_audio > self.cfg.options.max_buffered_audio {
            bump(&self.stats.counters.output_overflow);
            self.book.mark_lost(utterance);
            return;
        }
        let mut frames = std::mem::take(&mut self.scratch);
        if let Some(framer) = &mut self.out_framer
            && framer.push(pcm, &mut |frame| frames.push(frame)).is_err()
        {
            framer.reset();
            self.book.mark_lost(utterance);
        }
        self.queue_audio(&mut frames, epoch);
        self.scratch = frames;
    }

    /// A new assistant item starts: finish the previous one and announce the utterance.
    async fn start_item(&mut self, epoch: TurnEpoch, item: &Arc<str>) -> bool {
        self.finish_current_item();
        if !self.prepare_out_framer().await {
            return false;
        }
        if self.round_counter.0 != epoch {
            self.round_counter = (epoch, 0);
        }
        let round = self.round_counter.1;
        self.round_counter.1 = round.saturating_add(1);
        let key = RoundKey::new(epoch, round);
        let utterance = self.next_utterance;
        self.next_utterance = self.next_utterance.saturating_add(1);
        self.book.register_round(key);
        self.book.register_utterance(key, utterance);
        if let Some(resp) = &mut self.resp {
            resp.current = Some((Arc::clone(item), utterance));
        }
        if self.items.len() >= MAX_ITEMS {
            self.items.pop_front();
        }
        self.items.push_back(ItemRecord {
            item: Arc::clone(item),
            key,
            epoch,
            received: Duration::ZERO,
            transcript: String::new(),
            resolved: false,
            truncated: false,
        });
        bump(&self.stats.counters.utterances);
        if self.playing_epoch != Some(epoch) {
            self.timeline
                .mark(epoch, TurnEvent::TtsFirstByte, self.clock.now());
        }
        self.playing_epoch = Some(epoch);
        self.ended_epoch = None;
        self.out.push_back(control_frame(
            epoch,
            ControlKind::UtteranceStarted { utterance },
        ));
        true
    }

    /// A framer from the provider rate to the call's current output rate.
    async fn prepare_out_framer(&mut self) -> bool {
        let out = self.rates.get().output;
        if self.out_rate == Some(out) && self.out_framer.is_some() {
            if let Some(framer) = &mut self.out_framer {
                framer.reset();
            }
            return true;
        }
        let from = self.cfg.provider_rate;
        let built = self
            .offload
            .run(self.cfg.offload_timeout, move || {
                OutputFramer::build_resampler(from, out)
            })
            .await;
        match built.map(|done| done.value) {
            Ok(Ok(resampler)) => {
                let samples = frame_samples(self.cfg.frame, out);
                self.out_framer = Some(OutputFramer::with_resampler(out, samples, resampler));
                self.out_rate = Some(out);
                true
            }
            _ => {
                bump(&self.stats.counters.resampler_failures);
                tracing::warn!(target: "brisvox::engine", "s2s: cannot build the output resampler");
                false
            }
        }
    }

    /// Ends the item being framed: the resampler's tail and the last partial frame.
    fn finish_current_item(&mut self) {
        let Some(resp) = &mut self.resp else {
            return;
        };
        let epoch = resp.epoch;
        if resp.current.take().is_none() {
            return;
        }
        let mut frames = std::mem::take(&mut self.scratch);
        if let Some(framer) = &mut self.out_framer
            && framer.finish(&mut |frame| frames.push(frame)).is_err()
        {
            framer.reset();
        }
        self.queue_audio(&mut frames, epoch);
        self.scratch = frames;
    }

    fn queue_audio(&mut self, frames: &mut Vec<Bytes>, epoch: TurnEpoch) {
        let Some(rate) = self.out_framer.as_ref().map(OutputFramer::output_rate) else {
            frames.clear();
            return;
        };
        for bytes in frames.drain(..) {
            self.seq = self.seq.saturating_add(1);
            let Ok(frame) = AudioFrame::new(bytes, rate, self.clock.now(), self.seq, epoch) else {
                continue;
            };
            self.out_audio = self.out_audio.saturating_add(frame.duration());
            bump(&self.stats.counters.frames_out);
            self.out.push_back(Frame::from(frame));
        }
    }

    /// The front of the output queue was accepted downstream.
    fn sent_front(&mut self) {
        if let Some(Frame::Data(DataFrame::Audio(audio))) = self.out.pop_front() {
            self.out_audio = self.out_audio.saturating_sub(audio.duration());
        }
    }

    fn on_transcript(&mut self, response: &str, item: &Arc<str>, text: &Arc<str>, is_final: bool) {
        if !self.is_current(response) {
            bump(&self.stats.counters.stale_events);
            return;
        }
        let epoch = self.resp.as_ref().map_or(self.epoch, |r| r.epoch);
        if let Some(record) = self.items.iter_mut().rev().find(|r| r.item == *item) {
            if is_final {
                record.transcript = text.to_string();
            } else {
                record.transcript.push_str(text);
            }
        }
        if !is_final && !text.is_empty() {
            self.publish(EngineEvent::BotLlmText {
                epoch,
                text: Arc::clone(text),
            });
        }
    }

    // -- tools -------------------------------------------------------------------------------

    fn on_tool_call(
        &mut self,
        response: &str,
        call_id: Arc<str>,
        name: Arc<str>,
        arguments: Arc<str>,
    ) {
        bump(&self.stats.counters.tool_calls);
        if self.seen_calls.iter().any(|seen| *seen == call_id) {
            return;
        }
        if self.seen_calls.len() >= MAX_SEEN_CALLS {
            self.seen_calls.pop_front();
        }
        self.seen_calls.push_back(Arc::clone(&call_id));
        if !self.is_current(response) {
            // The turn moved on: the call is not run, but the model's history gets an answer.
            bump(&self.stats.counters.tool_calls_stale);
            self.send_control(S2sControl::ToolResult {
                call_id,
                output: Arc::from(interrupted_output()),
            });
            return;
        }
        if let Some(resp) = &mut self.resp {
            if resp.calls.len() >= MAX_CALLS_PER_RESPONSE {
                self.send_control(S2sControl::ToolResult {
                    call_id,
                    output: Arc::from(r#"{"error":"too_many_calls"}"#),
                });
                return;
            }
            resp.calls.push(PendingCall {
                provider_id: call_id,
                name,
                arguments,
            });
        }
    }

    fn on_response_done(&mut self, response: &str, status: S2sResponseStatus) {
        if !self.is_current(response) {
            bump(&self.stats.counters.stale_events);
            return;
        }
        self.finish_current_item();
        let Some(resp) = self.resp.take() else {
            return;
        };
        self.publish(EngineEvent::BotLlmStopped { epoch: resp.epoch });
        let current = resp.epoch == self.epoch;
        match status {
            S2sResponseStatus::Completed => {
                if !resp.calls.is_empty() {
                    self.run_tools(resp, current);
                }
            }
            other => {
                if let S2sResponseStatus::Failed(message) = &other {
                    tracing::warn!(target: "brisvox::engine", %message, "s2s response failed");
                    self.error_frame(&format!("the response failed: {message}"), false);
                }
                for call in resp.calls {
                    self.send_control(S2sControl::ToolResult {
                        call_id: call.provider_id,
                        output: Arc::from(interrupted_output()),
                    });
                }
            }
        }
        self.maybe_end_epoch();
    }

    fn run_tools(&mut self, resp: Resp, current: bool) {
        let epoch = resp.epoch;
        if !current {
            bump(&self.stats.counters.tool_calls_stale);
            for call in resp.calls {
                self.send_control(S2sControl::ToolResult {
                    call_id: call.provider_id,
                    output: Arc::from(interrupted_output()),
                });
            }
            return;
        }
        if self.tool_rounds.0 != epoch {
            self.tool_rounds = (epoch, 0);
        }
        if self.tool_rounds.1 >= self.cfg.max_tool_rounds {
            bump(&self.stats.counters.tool_round_limit);
            for call in resp.calls {
                self.send_control(S2sControl::ToolResult {
                    call_id: call.provider_id,
                    output: Arc::from(r#"{"error":"tool_round_limit"}"#),
                });
            }
            self.answer_without_tools(epoch);
            return;
        }
        self.tool_rounds.1 = self.tool_rounds.1.saturating_add(1);
        bump(&self.stats.counters.tool_rounds);
        let Some((_, engine)) = self.tools.clone() else {
            for call in resp.calls {
                self.send_control(S2sControl::ToolResult {
                    call_id: call.provider_id,
                    output: Arc::from(r#"{"error":"no_tools"}"#),
                });
            }
            self.request_response();
            return;
        };
        let mut assembled = Vec::with_capacity(resp.calls.len());
        let mut provider_ids = Vec::with_capacity(resp.calls.len());
        let mut recorded = Vec::with_capacity(resp.calls.len());
        for call in resp.calls {
            let id = ToolCallId::new(epoch, self.next_call);
            self.next_call = self.next_call.saturating_add(1);
            let entry = match parse_arguments(&call.arguments) {
                Ok(arguments) => Assembled::Complete(ToolCall {
                    id,
                    provider_id: Some(Arc::clone(&call.provider_id)),
                    name: Arc::clone(&call.name),
                    arguments,
                    provider_data: None,
                }),
                Err(error) => Assembled::Invalid(InvalidCall {
                    index: u32::try_from(assembled.len()).unwrap_or(u32::MAX),
                    id,
                    provider_id: Some(Arc::clone(&call.provider_id)),
                    name: Some(Arc::clone(&call.name)),
                    raw_arguments: Arc::clone(&call.arguments),
                    provider_data: None,
                    error,
                }),
            };
            recorded.push(match &entry {
                Assembled::Complete(call) => call.clone(),
                Assembled::Invalid(invalid) => ToolCall {
                    id: invalid.id.clone(),
                    provider_id: invalid.provider_id.clone(),
                    name: Arc::clone(&call.name),
                    arguments: serde_json::Value::Object(serde_json::Map::new()),
                    provider_data: None,
                },
            });
            provider_ids.push(call.provider_id);
            assembled.push(entry);
        }
        self.ctx_push(Message::assistant_with_calls("", recorded));
        self.tools_current = self.tools_current.saturating_add(1);
        let cancel = self.tool_cancel.clone();
        self.tool_tasks.spawn(async move {
            let execution = engine.execute(assembled, epoch, &cancel).await;
            ToolsDone {
                epoch,
                provider_ids,
                execution,
            }
        });
    }

    /// The tools of this turn will not run any more (round limit): the model is told so and asked
    /// for a final answer, once per epoch. Without it the caller would hear silence.
    fn answer_without_tools(&mut self, epoch: TurnEpoch) {
        if self.limit_answered == Some(epoch) {
            return;
        }
        self.limit_answered = Some(epoch);
        self.send_control(S2sControl::Note {
            text: Arc::from(TOOLS_UNAVAILABLE),
        });
        self.request_response();
    }

    fn on_tool_done(&mut self, joined: Result<ToolsDone, tokio::task::JoinError>) {
        let Ok(done) = joined else {
            tracing::warn!(target: "brisvox::engine", "s2s: a tool round task failed");
            self.tools_current = self.tools_current.saturating_sub(1);
            self.maybe_end_epoch();
            return;
        };
        let ToolsDone {
            epoch,
            provider_ids,
            mut execution,
        } = done;
        let current = epoch == self.epoch;
        if current {
            self.tools_current = self.tools_current.saturating_sub(1);
        }
        for (message, call_id) in execution.results.iter().zip(provider_ids) {
            self.send_control(S2sControl::ToolResult {
                call_id,
                output: Arc::from(message.text()),
            });
        }
        if execution.budget_exhausted && current {
            // The engine answered without running the tools: the model must speak, not retry.
            self.send_control(S2sControl::Note {
                text: Arc::from(TOOLS_UNAVAILABLE),
            });
        }
        for note in &execution.late_notes {
            self.send_control(S2sControl::Note {
                text: Arc::from(note.text()),
            });
        }
        if let Ok(mut context) = self.context.lock() {
            execution.append_to(&mut context);
        } else {
            execution.mark_delivered();
        }
        if current {
            self.request_response();
        } else {
            bump(&self.stats.counters.tool_results_stale);
        }
        self.maybe_end_epoch();
    }

    fn request_response(&mut self) {
        self.flush_late_notes();
        if self.send_control(S2sControl::CreateResponse) {
            bump(&self.stats.counters.response_requests);
            self.pending_response = Some(self.epoch);
            self.arm_deadline();
        }
    }

    /// Tells the transport the response of the epoch is complete once nothing more is coming:
    /// no response generating or requested, no tool running.
    fn maybe_end_epoch(&mut self) {
        let Some(epoch) = self.playing_epoch else {
            return;
        };
        if self.resp.is_some()
            || self.pending_response.is_some()
            || self.tools_current > 0
            || self.ended_epoch == Some(epoch)
            || epoch != self.epoch
        {
            return;
        }
        self.finish_current_item();
        self.ended_epoch = Some(epoch);
        self.out
            .push_back(control_frame(epoch, ControlKind::TtsEnded));
    }
}

/// Whether a 16-bit little-endian PCM frame is loud enough to be the caller's voice.
fn is_voiced(frame: &[u8]) -> bool {
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for pair in frame.as_chunks::<2>().0 {
        let sample = i16::from_le_bytes(*pair);
        sum = sum.saturating_add(u64::from(sample.unsigned_abs()));
        count += 1;
    }
    count > 0 && sum / count >= u64::from(VOICE_LEVEL)
}

fn frame_samples(frame: Duration, rate: brisvox_core::frame::SampleRate) -> usize {
    let samples = frame.as_nanos().saturating_mul(u128::from(rate.hz())) / 1_000_000_000;
    usize::try_from(samples).unwrap_or(usize::MAX).max(1)
}

fn interrupted_output() -> &'static str {
    r#"{"error":"interrupted","detail":"the caller interrupted before the call ran"}"#
}

fn parse_arguments(raw: &str) -> Result<serde_json::Value, CallError> {
    if raw.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value @ serde_json::Value::Object(_)) => Ok(value),
        Ok(_) => Err(CallError::NotAnObject),
        Err(error) => Err(CallError::InvalidJson(error.to_string())),
    }
}

// ---------------------------------------------------------------------------------------------
// The truncation job
// ---------------------------------------------------------------------------------------------

struct JobEnv {
    book: Arc<PlayoutBook>,
    clock: SharedClock,
    lead_margin: Duration,
    settle_wait: Duration,
    send_timeout: Duration,
    frame: Duration,
    /// The session's control sender; `None` when the session is gone (only the local mirror is
    /// updated then).
    control: Option<mpsc::Sender<S2sControl>>,
    context: Arc<std::sync::Mutex<Context>>,
    stats: S2sStats,
}

/// Cuts the model's history to what the caller heard: waits (bounded) for the playout book to
/// settle after the clear, computes the heard duration of every item as the played-text
/// truncation does, and sends [`S2sControl::Truncate`] for the items that were not heard whole.
async fn truncate_job(env: JobEnv, old: TurnEpoch, candidates: Vec<ItemRecord>) {
    let _settled = env
        .book
        .wait_settled_through(old, &*env.clock, env.settle_wait)
        .await;
    for record in candidates {
        let heard = match env.book.snapshot(record.key) {
            Some(snapshot) if snapshot.kept_whole() => record.received,
            Some(snapshot) => snapshot
                .heard_position(env.lead_margin)
                .min(record.received),
            // The book no longer knows the round (evicted): do not guess a cut.
            None => record.received,
        };
        // Within one frame of everything the server holds counts as heard whole.
        let whole = heard.saturating_add(env.frame) >= record.received;
        if !record.resolved {
            let text = if whole {
                record.transcript.clone()
            } else {
                cut_transcript(&record.transcript, heard, record.received)
            };
            if !text.is_empty()
                && let Ok(mut context) = env.context.lock()
            {
                context.push(Message::assistant(text));
            }
        }
        if whole {
            bump(&env.stats.counters.truncations_skipped);
            continue;
        }
        let Some(control) = &env.control else {
            continue;
        };
        let command = S2sControl::Truncate {
            item: Arc::clone(&record.item),
            audio_end: heard,
        };
        let sent = tokio::select! {
            biased;
            sent = control.send(command) => sent.is_ok(),
            () = env.clock.sleep(env.send_timeout) => false,
        };
        if sent {
            bump(&env.stats.counters.truncations_sent);
        } else {
            bump(&env.stats.counters.truncation_failures);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_cut_transcript_ends_at_a_word_boundary() {
        let text = "one two three four";
        assert_eq!(cut_transcript(text, S * 2, S * 4), "one two");
        // Half of the characters lands inside "three": back up to the boundary.
        assert_eq!(cut_transcript(text, S * 3, S * 5), "one two");
        assert_eq!(cut_transcript(text, S * 4, S * 4), text);
        assert_eq!(cut_transcript(text, Duration::ZERO, S * 4), "");
        assert_eq!(cut_transcript(text, S, Duration::ZERO), text);
    }

    #[test]
    fn voice_is_told_from_silence_and_low_noise() {
        let frame = |amplitude: i16| -> Vec<u8> {
            (0..160)
                .flat_map(|i| (if i % 2 == 0 { amplitude } else { -amplitude }).to_le_bytes())
                .collect()
        };
        assert!(is_voiced(&frame(3_000)));
        assert!(!is_voiced(&frame(0)));
        assert!(!is_voiced(&frame(100)));
        assert!(!is_voiced(&[]));
        assert!(!is_voiced(&[7]));
    }

    #[test]
    fn pcm_durations() {
        assert_eq!(pcm_duration(48_000, 24_000), S);
        assert_eq!(pcm_duration(960, 24_000), Duration::from_millis(20));
    }
}
