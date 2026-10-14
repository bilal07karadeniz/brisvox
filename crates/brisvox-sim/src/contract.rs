//! Provider contract suite: generic checks that every STT, LLM and TTS adapter must pass.
//!
//! The suite talks to a provider only through the public provider traits and a small harness
//! trait per kind ([`SttHarness`], [`LlmHarness`], [`TtsHarness`]). The harness is the one place
//! that knows how to make a provider produce a scripted behaviour and how to inject a fault:
//! the sim mocks implement it directly (below); a real adapter (Deepgram, OpenAI,
//! Cartesia) implements it on top of a local mock server that speaks the provider's wire
//! protocol, configured with [`CONTRACT_TIMEOUT`] as its deadline.
//!
//! Every provider call (connect, stream, warm, each awaited event) is bounded on the injected
//! clock, and every check as a whole is bounded too, so a hang is a reported failure, never a
//! stuck test. Each check starts with [`reset`](SttHarness::reset). `tests/contract.rs` runs the
//! suite against the mocks and against deliberately broken doubles, and asserts that the matching
//! checks fail: a suite that passes everything vacuously is the failure mode this module exists
//! to prevent.
//!
//! What the timing checks tolerate, in one place: a cancel is judged against [`CANCEL_PROMPT`]
//! and [`CANCEL_SLACK_EVENTS`]. Output that was already handed to the caller before the cancel
//! (buffered in the event channel) is not counted; output produced after it is. The TTS check
//! reads what is buffered after a short settle, right before it cancels, so a synthesizer that
//! runs faster than real time (a burst of audio queued in the channel) is judged fairly. A
//! deadline is judged both ways: the `Timeout` error must come at the deadline, not before
//! ([`TIMEOUT_SLACK`] either side), and a stall shorter than the deadline (half of it) must be
//! survived, at connect, at the first event and, for events after the first, as an idle gap.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::Error as CoreError;
use brisvox_core::SharedClock;
use brisvox_core::clock::{deadline_after, wait_for};
use brisvox_core::frame::SampleRate;
use brisvox_providers::{
    ContextId, Error as ProviderError, FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream,
    Message, Role, SttConfig, SttEvent, SttInput, SttProvider, SttSession, ToolSpec, TtsConfig,
    TtsEvent, TtsInput, TtsProvider, TtsSession,
};
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::leak::TaskCounter;
use crate::mock_common::StreamFault;
use crate::mock_llm::{LlmFault, LlmScript, MockLlm};
use crate::mock_stt::{MockStt, SttUtterance};
use crate::mock_tts::MockTts;

/// The deadline every adapter under contract must enforce. The harness configures its provider
/// with it; the timeout checks expect a `Timeout` error within it (plus [`TIMEOUT_SLACK`]).
pub const CONTRACT_TIMEOUT: Duration = Duration::from_secs(2);

/// Slack past the deadline within which the `Timeout` error must be observed.
pub const TIMEOUT_SLACK: Duration = Duration::from_millis(100);

/// Length of an injected stall: far beyond any deadline, so only a deadline can end the wait.
pub const STALL: Duration = Duration::from_secs(3_600);

/// Quiet period before a leak check takes its baseline (the alive-task count must hold still for
/// this long), and the bound on the wait for leaked tasks to finish after a session ends.
pub const LEAK_BOUND: Duration = CONTRACT_TIMEOUT;

/// Events of the long LLM response used by the cancel check. The harness must make that
/// response slow (several seconds in total), or a cancel that is ignored finishes unobserved.
pub const CANCEL_RESPONSE_EVENTS: usize = 200;

/// How soon after a cancel the provider must have stopped: no further LLM response events are
/// produced, no STT transcript follows and the events close, and TTS acknowledges with
/// `Cancelled`.
pub const CANCEL_PROMPT: Duration = Duration::from_millis(500);

/// Events of a cancelled context that may still arrive after the cancel (in flight on the wire
/// or in the middle of being handed over). More than this means the output did not stop. What is
/// already buffered in the event channel before the cancel is sent does not count.
pub const CANCEL_SLACK_EVENTS: usize = 2;

/// Upper bound on the wait for any single check. A check that runs longer is a hang.
pub const CHECK_BOUND: Duration = Duration::from_secs(60);

/// Silence required after a TTS context ended or was cancelled: no late event may arrive
/// within it.
const QUIET_WINDOW: Duration = Duration::from_millis(500);

/// Time given to the caller's output that is already buffered, before a cancel is judged.
const SETTLE: Duration = Duration::from_millis(50);

/// How much slower a connect after `warm()` may be than a cold one before the check fails.
const WARM_TOLERANCE: Duration = Duration::from_millis(50);

/// Upper bound on events one check reads (a provider that never stops is a failure).
const MAX_EVENTS: usize = 1_000;

/// Audio bytes per millisecond: PCM16 mono at 16 kHz.
const BYTES_PER_MS: usize = 32;

/// Upper bound on the wait for one piece of progress: longer than the deadline, so an adapter
/// that honours its deadline is never mistaken for a hang.
fn progress_bound(timeout: Duration) -> Duration {
    timeout.saturating_mul(3)
}

/// A fault the suite injects into the next connection or response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The connect (or request) is refused with this HTTP status or close code. Must surface as
    /// `Error::Rejected { status }`.
    Reject {
        /// Status, e.g. 401, 429, 500 or 503.
        status: u16,
    },
    /// The connection drops after `after_events` events. Must surface as an `Error` event with
    /// `Error::Disconnected`, after which the stream ends.
    Disconnect {
        /// Events delivered before the drop.
        after_events: usize,
    },
    /// The provider sends something unparsable after `after_events` events. Must surface as
    /// `Error::Protocol`.
    Protocol {
        /// Events delivered before the violation.
        after_events: usize,
    },
    /// Event number `before_event` (0-based) arrives `duration` late.
    Stall {
        /// Index of the delayed event.
        before_event: usize,
        /// Extra delay.
        duration: Duration,
    },
    /// The connect (or request) takes `duration` longer. With an adapter deadline shorter than
    /// that, it must fail with `Core(Timeout)` within the deadline.
    ConnectStall {
        /// Extra delay of the connect.
        duration: Duration,
    },
}

/// What an STT utterance sounds like: interim hypotheses, then the final segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utterance {
    /// Interim hypotheses, in order, produced while audio arrives.
    pub interims: Vec<String>,
    /// Final segments, in order; the last one ends the utterance after a `Finalize`.
    pub finals: Vec<String>,
}

/// A tool call the scripted LLM response makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Complete JSON arguments.
    pub arguments: String,
}

/// A scripted LLM response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Text deltas, in order.
    pub text: Vec<String>,
    /// Tool calls, after the text.
    pub tool_calls: Vec<ToolCall>,
    /// Faults applied to this response.
    pub faults: Vec<Fault>,
}

/// Scripting and fault control for an STT adapter under contract.
pub trait SttHarness: Send + Sync {
    /// The provider under test.
    type P: SttProvider;

    /// The provider, configured with [`Self::io_timeout`] as its deadline.
    fn provider(&self) -> Arc<Self::P>;

    /// The clock the provider and the checks share (paused in simulations, real otherwise).
    fn clock(&self) -> SharedClock;

    /// The deadline the adapter must enforce. Defaults to [`CONTRACT_TIMEOUT`].
    fn io_timeout(&self) -> Duration {
        CONTRACT_TIMEOUT
    }

    /// Forgets scripted utterances and queued faults. The suite calls this before each check.
    fn reset(&self);

    /// Makes the next utterance heard by a session sound like `utterance`.
    fn script(&self, utterance: Utterance);

    /// Injects `fault` into the next session that connects.
    fn inject(&self, fault: Fault);
}

/// Scripting and fault control for an LLM adapter under contract.
pub trait LlmHarness: Send + Sync {
    /// The provider under test.
    type P: LlmProvider;

    /// The provider, configured with [`Self::io_timeout`] as its deadline.
    fn provider(&self) -> Arc<Self::P>;

    /// The clock the provider and the checks share.
    fn clock(&self) -> SharedClock;

    /// The deadline the adapter must enforce. Defaults to [`CONTRACT_TIMEOUT`].
    fn io_timeout(&self) -> Duration {
        CONTRACT_TIMEOUT
    }

    /// Forgets scripted responses. The suite calls this before each check.
    fn reset(&self);

    /// Makes the next response stream as `response` says, faults included. Responses of
    /// [`CANCEL_RESPONSE_EVENTS`] text deltas must take several seconds to stream, so that a
    /// cancel is observable.
    fn script(&self, response: Response);

    /// Response events (text deltas, tool-call fragments, `Done`) the upstream has handed to
    /// the adapter so far, cumulative across requests. Counts what the upstream sent, not what
    /// the caller read; the cancel check uses it to see whether generation continues after a
    /// cancel. It must be live: the check fails when a fully read response of three deltas and
    /// `Done` is not counted (at least 4), so a counter that is stubbed or not wired up cannot
    /// make the cancel check pass.
    fn events_sent(&self) -> usize;
}

/// Scripting and fault control for a TTS adapter under contract.
pub trait TtsHarness: Send + Sync {
    /// The provider under test.
    type P: TtsProvider;

    /// The provider, configured with [`Self::io_timeout`] as its deadline.
    fn provider(&self) -> Arc<Self::P>;

    /// The clock the provider and the checks share.
    fn clock(&self) -> SharedClock;

    /// The deadline the adapter must enforce. Defaults to [`CONTRACT_TIMEOUT`].
    fn io_timeout(&self) -> Duration {
        CONTRACT_TIMEOUT
    }

    /// Whether the provider reports word times ([`TtsEvent::Word`]). Defaults to `true`. A
    /// provider that offers no timestamps at all (Deepgram Aura) returns `false`: the word
    /// checks then demand the opposite, that no `Word` event is made up, and the checks that
    /// compare words with the audio are skipped.
    fn emits_words(&self) -> bool {
        true
    }

    /// Whether a refusal ([`Fault::Reject`]) and a stalled connection
    /// ([`Fault::ConnectStall`]) surface from `connect`. Defaults to `true`. A request-scoped
    /// provider (OpenAI-compatible `/audio/speech`: one HTTP request per text piece, nothing is
    /// opened by `connect`) returns `false`: the faults then apply to the first request of the
    /// session, and the checks demand the same errors as events after the first text
    /// (`Rejected { status }`, and `Core(Timeout)` at the deadline).
    fn faults_at_connect(&self) -> bool {
        true
    }

    /// Forgets queued faults. The suite calls this before each check.
    fn reset(&self);

    /// Injects `fault` into the next session that connects (or, for a provider whose
    /// [`faults_at_connect`](Self::faults_at_connect) is `false`, the first request of the next
    /// session).
    fn inject(&self, fault: Fault);
}

/// Outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Check name, e.g. `"cancel"`.
    pub name: &'static str,
    /// `None` when the check passed; otherwise what was observed instead.
    pub failure: Option<String>,
}

impl CheckResult {
    /// Whether the check passed.
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

/// Outcome of one suite run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractReport {
    /// `"stt"`, `"llm"` or `"tts"`.
    pub kind: &'static str,
    /// One result per check, in run order.
    pub checks: Vec<CheckResult>,
}

impl ContractReport {
    /// Whether every check passed.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(CheckResult::passed)
    }

    /// The result of the check called `name`.
    pub fn check(&self, name: &str) -> Option<&CheckResult> {
        self.checks.iter().find(|c| c.name == name)
    }

    /// The checks that failed.
    pub fn failures(&self) -> impl Iterator<Item = &CheckResult> + '_ {
        self.checks.iter().filter(|c| !c.passed())
    }
}

/// Runs the STT contract: streaming, cancel, reconnect, error mapping, timeout, warm-up and task
/// leaks.
pub async fn stt_contract<H: SttHarness>(h: &H) -> ContractReport {
    let clock = h.clock();
    ContractReport {
        kind: "stt",
        checks: vec![
            check("streaming", &clock, stt_streaming(h)).await,
            check("cancel", &clock, stt_cancel(h)).await,
            check("reconnect", &clock, stt_reconnect(h)).await,
            check("error_mapping", &clock, stt_error_mapping(h)).await,
            check("timeout", &clock, stt_timeout(h)).await,
            check("warm", &clock, stt_warm(h)).await,
            check("no_task_leak", &clock, stt_no_task_leak(h)).await,
        ],
    }
}

/// Runs the LLM contract: streaming, cancel, reconnect, error mapping, timeout, warm-up and task
/// leaks.
pub async fn llm_contract<H: LlmHarness>(h: &H) -> ContractReport {
    let clock = h.clock();
    ContractReport {
        kind: "llm",
        checks: vec![
            check("streaming", &clock, llm_streaming(h)).await,
            check("cancel", &clock, llm_cancel(h)).await,
            check("reconnect", &clock, llm_reconnect(h)).await,
            check("error_mapping", &clock, llm_error_mapping(h)).await,
            check("timeout", &clock, llm_timeout(h)).await,
            check("warm", &clock, llm_warm(h)).await,
            check("no_task_leak", &clock, llm_no_task_leak(h)).await,
        ],
    }
}

/// Runs the TTS contract: audio and words, flush, cancel, reconnect, error mapping, timeout,
/// warm-up and task leaks.
pub async fn tts_contract<H: TtsHarness>(h: &H) -> ContractReport {
    let clock = h.clock();
    ContractReport {
        kind: "tts",
        checks: vec![
            check("audio_and_words", &clock, tts_audio_and_words(h)).await,
            check("flush", &clock, tts_flush(h)).await,
            check("cancel", &clock, tts_cancel(h)).await,
            check("reconnect", &clock, tts_reconnect(h)).await,
            check("error_mapping", &clock, tts_error_mapping(h)).await,
            check("timeout", &clock, tts_timeout(h)).await,
            check("warm", &clock, tts_warm(h)).await,
            check("no_task_leak", &clock, tts_no_task_leak(h)).await,
        ],
    }
}

async fn check(
    name: &'static str,
    clock: &SharedClock,
    run: impl Future<Output = Result<()>>,
) -> CheckResult {
    let outcome = match within(clock, CHECK_BOUND, run).await {
        Some(outcome) => outcome,
        None => fail(format!(
            "the check did not finish within {CHECK_BOUND:?}: a provider call hangs"
        )),
    };
    CheckResult {
        name,
        failure: outcome.err().map(|e| e.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Assertion(message.into()))
}

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition { Ok(()) } else { fail(message) }
}

fn provider_error(context: &str, error: ProviderError) -> Error {
    Error::Assertion(format!("{context}: {error}"))
}

async fn send<T>(tx: &mpsc::Sender<T>, message: T) -> Result<()> {
    tx.send(message)
        .await
        .map_err(|_| Error::Assertion("the session closed while input was sent".to_owned()))
}

/// Runs `future` unless `bound` passes on `clock` first (`None`: the bound won).
async fn within<F: Future>(clock: &SharedClock, bound: Duration, future: F) -> Option<F::Output> {
    tokio::select! {
        output = future => Some(output),
        () = wait_for(&**clock, bound) => None,
    }
}

/// Awaits `future` for at most `bound` on `clock`: a call that never returns is a failure named
/// `what`.
async fn bounded<F: Future>(
    clock: &SharedClock,
    bound: Duration,
    what: &str,
    future: F,
) -> Result<F::Output> {
    match within(clock, bound, future).await {
        Some(output) => Ok(output),
        None => fail(format!(
            "{what} did not return within {bound:?}: the provider hangs"
        )),
    }
}

/// [`bounded`] for a provider call that can fail, with the provider error reported under `what`.
async fn bounded_call<T>(
    clock: &SharedClock,
    bound: Duration,
    what: &str,
    future: impl Future<Output = std::result::Result<T, ProviderError>>,
) -> Result<T> {
    bounded(clock, bound, what, future)
        .await?
        .map_err(|e| provider_error(what, e))
}

fn elapsed_since(clock: &SharedClock, start: tokio::time::Instant) -> Duration {
    clock.now().saturating_duration_since(start)
}

/// Checks that a timeout error arrived at the deadline: not later than the deadline plus
/// [`TIMEOUT_SLACK`], and not earlier than the deadline minus [`TIMEOUT_SLACK`] (an adapter with
/// its own, shorter deadline would cut off a slow but healthy provider).
fn ensure_timed_out(elapsed: Duration, timeout: Duration) -> Result<()> {
    ensure(
        elapsed <= timeout + TIMEOUT_SLACK,
        format!("the Timeout error came after {elapsed:?}, the deadline is {timeout:?}"),
    )?;
    ensure(
        elapsed + TIMEOUT_SLACK >= timeout,
        format!(
            "the Timeout error came after only {elapsed:?}, the deadline is {timeout:?}: the adapter gives up early"
        ),
    )
}

/// A delay that is long but still inside the deadline: a provider that is slow, not stuck, must
/// not be cut off.
fn slow_but_in_time(timeout: Duration) -> Duration {
    timeout / 2
}

// ---------------------------------------------------------------------------------------------
// STT
// ---------------------------------------------------------------------------------------------

fn stt_config() -> SttConfig {
    SttConfig {
        sample_rate: SampleRate::Hz16000,
        language: None,
    }
}

/// 20 ms of PCM16 mono at 16 kHz.
fn stt_chunk() -> SttInput {
    SttInput::Audio(Bytes::from(vec![0_u8; 640]))
}

fn stt_hello() -> Utterance {
    Utterance {
        interims: vec!["hello".to_owned(), "hello there".to_owned()],
        finals: vec!["hello there".to_owned()],
    }
}

async fn connect_stt<H: SttHarness>(h: &H) -> Result<SttSession> {
    let clock = h.clock();
    let provider = h.provider();
    bounded_call(
        &clock,
        progress_bound(h.io_timeout()),
        "connect",
        provider.connect(stt_config()),
    )
    .await
}

async fn timed_connect_stt<P: SttProvider>(
    clock: &SharedClock,
    bound: Duration,
    provider: &P,
) -> Result<(SttSession, Duration)> {
    let start = clock.now();
    let session = bounded_call(clock, bound, "connect", provider.connect(stt_config())).await?;
    Ok((session, elapsed_since(clock, start)))
}

async fn stt_event(
    clock: &SharedClock,
    bound: Duration,
    events: &mut mpsc::Receiver<SttEvent>,
) -> Result<SttEvent> {
    match within(clock, bound, events.recv()).await {
        None => fail("no event within the progress bound: the provider hangs"),
        Some(None) => fail("the event stream closed before the expected event"),
        Some(Some(event)) => Ok(event),
    }
}

/// Reads transcripts until the utterance ends, checking that the final one is `expected`.
async fn stt_expect_final(
    clock: &SharedClock,
    bound: Duration,
    events: &mut mpsc::Receiver<SttEvent>,
    expected: &str,
) -> Result<()> {
    for _ in 0..MAX_EVENTS {
        match stt_event(clock, bound, events).await? {
            SttEvent::Transcript {
                is_final: false, ..
            } => {}
            SttEvent::Transcript {
                text,
                is_final: true,
                speech_final,
                from_finalize,
            } => {
                ensure(
                    &*text == expected,
                    format!("final transcript is {text:?}, expected {expected:?}"),
                )?;
                return ensure(
                    speech_final && from_finalize,
                    format!("final {text:?} must be flagged from_finalize and speech_final"),
                );
            }
            SttEvent::Error(e) => return Err(provider_error("transcript", e)),
        }
    }
    fail("no speech_final transcript within the event budget")
}

async fn stt_streaming<H: SttHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    h.script(stt_hello());
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    // Interim results arrive while the caller is still speaking, before any Finalize.
    match stt_event(&clock, bound, &mut session.events).await? {
        SttEvent::Transcript {
            text,
            is_final: false,
            ..
        } if &*text == "hello" => {}
        other => {
            return fail(format!(
                "the first event must be the interim \"hello\" before any Finalize, got {other:?}"
            ));
        }
    }
    send(&session.input, stt_chunk()).await?;
    send(&session.input, SttInput::Finalize).await?;
    stt_expect_final(&clock, bound, &mut session.events, "hello there").await
}

async fn stt_cancel<H: SttHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    h.script(stt_hello());
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    match stt_event(&clock, bound, &mut session.events).await? {
        SttEvent::Transcript {
            is_final: false, ..
        } => {}
        other => {
            return fail(format!(
                "expected an interim before cancelling, got {other:?}"
            ));
        }
    }
    // Transcripts the provider had already handed over are read and discarded here: from the
    // caller's side they cannot be told apart from in-flight ones. Errors are not discarded.
    wait_for(&*clock, SETTLE).await;
    while let Ok(event) = session.events.try_recv() {
        if let SttEvent::Error(e) = event {
            return Err(provider_error("before the cancel", e));
        }
    }
    // Cancel: the caller hangs up without a Finalize. From here on the provider sends nothing
    // and closes its events promptly. It must not finalize the utterance on the way out.
    let SttSession { input, mut events } = session;
    drop(input);
    match within(&clock, CANCEL_PROMPT, events.recv()).await {
        None => fail(format!(
            "the events stayed open for {CANCEL_PROMPT:?} after the input was dropped: cancel is ignored"
        )),
        Some(None) => Ok(()),
        Some(Some(event)) => fail(format!(
            "{event:?} after the cancel: nothing may follow a hang-up"
        )),
    }
}

async fn stt_reconnect<H: SttHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    h.script(stt_hello());
    h.inject(Fault::Disconnect { after_events: 1 });
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    match stt_event(&clock, bound, &mut session.events).await? {
        SttEvent::Transcript {
            is_final: false, ..
        } => {}
        other => {
            return fail(format!(
                "expected the interim before the drop, got {other:?}"
            ));
        }
    }
    match stt_event(&clock, bound, &mut session.events).await? {
        SttEvent::Error(ProviderError::Disconnected { .. }) => {}
        other => {
            return fail(format!(
                "a dropped connection must surface as Disconnected, got {other:?}"
            ));
        }
    }
    match within(&clock, bound, session.events.recv()).await {
        None => return fail("the stream did not end after the drop"),
        Some(None) => {}
        Some(Some(event)) => return fail(format!("{event:?} after the drop")),
    }
    // Reconnect: a new session works end to end.
    drop(session);
    h.reset();
    h.script(stt_hello());
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    send(&session.input, SttInput::Finalize).await?;
    stt_expect_final(&clock, bound, &mut session.events, "hello there").await
}

async fn stt_error_mapping<H: SttHarness>(h: &H) -> Result<()> {
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    for status in [401_u16, 429, 500, 503] {
        h.reset();
        h.inject(Fault::Reject { status });
        let provider = h.provider();
        match bounded(&clock, bound, "connect", provider.connect(stt_config())).await? {
            Err(ProviderError::Rejected { status: got, .. }) if got == status => {}
            Err(other) => {
                return fail(format!(
                    "a refusal with status {status} must map to Rejected {{ status: {status} }}, got {other}"
                ));
            }
            Ok(_) => return fail(format!("connect was accepted despite a {status} refusal")),
        }
    }
    h.reset();
    h.script(stt_hello());
    h.inject(Fault::Protocol { after_events: 0 });
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    match stt_event(&clock, bound, &mut session.events).await? {
        SttEvent::Error(ProviderError::Protocol { .. }) => {}
        other => {
            return fail(format!(
                "a protocol violation must map to Protocol, got {other:?}"
            ));
        }
    }
    drop(session);
    h.reset();
    h.script(stt_hello());
    h.inject(Fault::Disconnect { after_events: 0 });
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    match stt_event(&clock, bound, &mut session.events).await? {
        SttEvent::Error(ProviderError::Disconnected { .. }) => Ok(()),
        other => fail(format!(
            "a dropped connection must map to Disconnected, got {other:?}"
        )),
    }
}

async fn stt_timeout<H: SttHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let timeout = h.io_timeout();
    let bound = progress_bound(timeout);
    // A connect that takes longer than the deadline.
    h.inject(Fault::ConnectStall { duration: STALL });
    let provider = h.provider();
    let start = clock.now();
    match bounded(&clock, bound, "connect", provider.connect(stt_config())).await? {
        Err(ProviderError::Core(CoreError::Timeout { .. })) => {
            ensure_timed_out(elapsed_since(&clock, start), timeout)?;
        }
        Err(other) => {
            return fail(format!(
                "a stalled connect must yield Core(Timeout), got {other}"
            ));
        }
        Ok(_) => return fail("a stalled connect produced a session"),
    }
    // A slow connect inside the deadline must succeed.
    h.reset();
    h.inject(Fault::ConnectStall {
        duration: slow_but_in_time(timeout),
    });
    drop(connect_stt(h).await?);
    // A session whose first event stalls.
    h.reset();
    h.script(stt_hello());
    h.inject(Fault::Stall {
        before_event: 0,
        duration: STALL,
    });
    let mut session = connect_stt(h).await?;
    let start = clock.now();
    send(&session.input, stt_chunk()).await?;
    stt_expect_timeout(&clock, bound, timeout, &mut session.events, start).await?;
    // A first event that is slow but inside the deadline must be delivered.
    h.reset();
    h.script(stt_hello());
    h.inject(Fault::Stall {
        before_event: 0,
        duration: slow_but_in_time(timeout),
    });
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    stt_expect_interim(&clock, bound, &mut session.events).await?;
    // An idle gap after the first event: the deadline applies to every event, not only to the
    // first one.
    h.reset();
    h.script(stt_hello());
    h.inject(Fault::Stall {
        before_event: 1,
        duration: STALL,
    });
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    stt_expect_interim(&clock, bound, &mut session.events).await?;
    let start = clock.now();
    stt_expect_timeout(&clock, bound, timeout, &mut session.events, start).await
}

/// Expects the next event to be a `Core(Timeout)` error at the deadline.
async fn stt_expect_timeout(
    clock: &SharedClock,
    bound: Duration,
    timeout: Duration,
    events: &mut mpsc::Receiver<SttEvent>,
    start: tokio::time::Instant,
) -> Result<()> {
    match within(clock, bound, events.recv()).await {
        None => fail(format!(
            "no event and no Timeout within {bound:?}: a stalled provider hangs"
        )),
        Some(None) => fail("the stream closed without a Timeout error"),
        Some(Some(SttEvent::Error(ProviderError::Core(CoreError::Timeout { .. })))) => {
            ensure_timed_out(elapsed_since(clock, start), timeout)
        }
        Some(Some(other)) => fail(format!(
            "a stalled provider must yield Core(Timeout), got {other:?}"
        )),
    }
}

/// Expects the next event to be an interim transcript.
async fn stt_expect_interim(
    clock: &SharedClock,
    bound: Duration,
    events: &mut mpsc::Receiver<SttEvent>,
) -> Result<()> {
    match stt_event(clock, bound, events).await? {
        SttEvent::Transcript {
            is_final: false, ..
        } => Ok(()),
        other => fail(format!("expected an interim transcript, got {other:?}")),
    }
}

async fn stt_warm<H: SttHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    let provider = h.provider();
    let (cold_session, cold) = timed_connect_stt(&clock, bound, &*provider).await?;
    drop(cold_session);
    bounded_call(&clock, bound, "warm", provider.warm()).await?;
    bounded_call(
        &clock,
        bound,
        "warm (second call, must be idempotent)",
        provider.warm(),
    )
    .await?;
    let (warm_session, warm) = timed_connect_stt(&clock, bound, &*provider).await?;
    drop(warm_session);
    ensure(
        warm <= cold + WARM_TOLERANCE,
        format!("connect after warm() took {warm:?}, a cold connect took {cold:?}"),
    )
}

async fn stt_no_task_leak<H: SttHarness>(h: &H) -> Result<()> {
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    h.reset();
    h.script(stt_hello());
    let counter = TaskCounter::start_settled(&*clock, LEAK_BOUND).await?;
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    send(&session.input, SttInput::Finalize).await?;
    stt_expect_final(&clock, bound, &mut session.events, "hello there").await?;
    drop(session);
    counter.wait_for_baseline(&*clock, LEAK_BOUND).await?;
    // A caller that hangs up mid-utterance must not leave the provider running.
    h.reset();
    h.script(stt_hello());
    let counter = TaskCounter::start_settled(&*clock, LEAK_BOUND).await?;
    let mut session = connect_stt(h).await?;
    send(&session.input, stt_chunk()).await?;
    stt_event(&clock, bound, &mut session.events).await?;
    drop(session);
    counter.wait_for_baseline(&*clock, LEAK_BOUND).await
}

// ---------------------------------------------------------------------------------------------
// LLM
// ---------------------------------------------------------------------------------------------

fn llm_request() -> LlmRequest {
    LlmRequest {
        messages: vec![Message::new(Role::User, "What is the weather in Paris?")],
        tools: vec![ToolSpec {
            name: Arc::from("get_weather"),
            description: Arc::from("Current weather for a city"),
            parameters_schema: Arc::from(r#"{"type":"object"}"#),
        }],
    }
}

fn text_response(parts: &[&str], faults: Vec<Fault>) -> Response {
    Response {
        text: parts.iter().map(|p| (*p).to_owned()).collect(),
        tool_calls: Vec::new(),
        faults,
    }
}

async fn llm_stream<H: LlmHarness>(h: &H) -> Result<LlmStream> {
    let clock = h.clock();
    let provider = h.provider();
    bounded_call(
        &clock,
        progress_bound(h.io_timeout()),
        "stream",
        provider.stream(llm_request()),
    )
    .await
}

async fn llm_event(
    clock: &SharedClock,
    bound: Duration,
    stream: &mut LlmStream,
) -> Result<LlmEvent> {
    match within(clock, bound, stream.next()).await {
        None => fail("no event within the progress bound: the provider hangs"),
        Some(None) => fail("the stream ended without Done or Error"),
        Some(Some(event)) => Ok(event),
    }
}

/// Reads events until `Done` and returns its finish reason.
async fn llm_until_done(
    clock: &SharedClock,
    bound: Duration,
    stream: &mut LlmStream,
) -> Result<FinishReason> {
    for _ in 0..MAX_EVENTS {
        match llm_event(clock, bound, stream).await? {
            LlmEvent::Done { finish, .. } => return Ok(finish),
            LlmEvent::Error(e) => return Err(provider_error("stream", e)),
            LlmEvent::TextDelta(_) | LlmEvent::ToolCallDelta { .. } => {}
        }
    }
    fail("no Done within the event budget")
}

async fn llm_streaming<H: LlmHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    h.script(Response {
        text: vec!["Hel".to_owned(), "lo, ".to_owned(), "world".to_owned()],
        tool_calls: vec![ToolCall {
            id: "call_1".to_owned(),
            name: "get_weather".to_owned(),
            arguments: r#"{"city":"Paris"}"#.to_owned(),
        }],
        faults: Vec::new(),
    });
    let mut stream = llm_stream(h).await?;
    let mut text = String::new();
    let mut arguments = String::new();
    let mut first_fragment = true;
    let mut finish = None;
    for _ in 0..MAX_EVENTS {
        match llm_event(&clock, bound, &mut stream).await? {
            LlmEvent::TextDelta(delta) => text.push_str(&delta),
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments: fragment,
            } => {
                ensure(index == 0, format!("tool call index {index}, expected 0"))?;
                if first_fragment {
                    ensure(
                        id.as_deref() == Some("call_1") && name.as_deref() == Some("get_weather"),
                        format!(
                            "the first fragment must carry the id and name, got {id:?} and {name:?}"
                        ),
                    )?;
                    first_fragment = false;
                } else {
                    ensure(
                        id.is_none() && name.is_none(),
                        "a later fragment repeats the call id or name",
                    )?;
                }
                arguments.push_str(&fragment);
            }
            LlmEvent::Done { finish: reason, .. } => {
                finish = Some(reason);
                break;
            }
            LlmEvent::Error(e) => return Err(provider_error("stream", e)),
        }
    }
    let Some(reason) = finish else {
        return fail("no Done within the event budget");
    };
    ensure(
        text == "Hello, world",
        format!("text deltas joined to {text:?}"),
    )?;
    ensure(
        arguments == r#"{"city":"Paris"}"#,
        format!("tool argument fragments joined to {arguments:?}"),
    )?;
    ensure(
        reason == FinishReason::ToolCalls,
        format!("finish reason {reason:?}, expected ToolCalls"),
    )?;
    match within(&clock, bound, stream.next()).await {
        None => fail("the stream did not end after Done"),
        Some(None) => Ok(()),
        Some(Some(event)) => fail(format!("{event:?} after Done")),
    }
}

async fn llm_cancel<H: LlmHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    // Control run: the check below trusts `events_sent`, so first prove that the counter is
    // live. A response read to the end (three deltas and `Done`) must have been counted.
    h.script(text_response(&["a", "b", "c"], Vec::new()));
    let control_before = h.events_sent();
    let mut control = llm_stream(h).await?;
    llm_until_done(&clock, bound, &mut control).await?;
    drop(control);
    wait_for(&*clock, SETTLE).await;
    let counted = h.events_sent().saturating_sub(control_before);
    ensure(
        counted >= 4,
        format!(
            "events_sent counted {counted} events for a response of 3 text deltas and Done: the harness counter is not wired up, and the cancel check would prove nothing"
        ),
    )?;
    h.reset();
    h.script(Response {
        text: vec!["tok".to_owned(); CANCEL_RESPONSE_EVENTS],
        tool_calls: Vec::new(),
        faults: Vec::new(),
    });
    let sent_before = h.events_sent();
    let mut stream = llm_stream(h).await?;
    for _ in 0..2 {
        match llm_event(&clock, bound, &mut stream).await? {
            LlmEvent::TextDelta(_) => {}
            other => {
                return fail(format!(
                    "expected a text delta before the cancel, got {other:?}"
                ));
            }
        }
    }
    // The counter must be live: the two deltas just read have been sent upstream.
    let at_drop = h.events_sent();
    ensure(
        at_drop.saturating_sub(sent_before) >= 2,
        format!(
            "events_sent advanced by {} after two deltas were read: the harness counter is stuck",
            at_drop.saturating_sub(sent_before)
        ),
    )?;
    // The response must still be streaming at the cancel, or the check proves nothing.
    ensure(
        at_drop.saturating_sub(sent_before) < CANCEL_RESPONSE_EVENTS,
        "the response had already finished before the cancel: the check needs a response still streaming",
    )?;
    // Cancel: dropping the stream mid-response must stop generation, not run it to the end.
    drop(stream);
    wait_for(&*clock, CANCEL_PROMPT).await;
    let after = h.events_sent().saturating_sub(at_drop);
    ensure(
        after <= CANCEL_SLACK_EVENTS,
        format!(
            "{after} response events were produced after the cancel, within {CANCEL_PROMPT:?}: generation did not stop"
        ),
    )
}

async fn llm_reconnect<H: LlmHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    h.script(text_response(
        &["one", "two", "three"],
        vec![Fault::Disconnect { after_events: 1 }],
    ));
    let mut stream = llm_stream(h).await?;
    match llm_event(&clock, bound, &mut stream).await? {
        LlmEvent::TextDelta(_) => {}
        other => {
            return fail(format!(
                "expected a text delta before the drop, got {other:?}"
            ));
        }
    }
    match llm_event(&clock, bound, &mut stream).await? {
        LlmEvent::Error(ProviderError::Disconnected { .. }) => {}
        other => {
            return fail(format!(
                "a dropped connection must surface as Disconnected, got {other:?}"
            ));
        }
    }
    match within(&clock, bound, stream.next()).await {
        None => return fail("the stream did not end after the drop"),
        Some(None) => {}
        Some(Some(event)) => return fail(format!("{event:?} after the drop")),
    }
    // Reconnect: a new request works end to end.
    drop(stream);
    h.reset();
    h.script(text_response(&["ok"], Vec::new()));
    let mut stream = llm_stream(h).await?;
    let finish = llm_until_done(&clock, bound, &mut stream).await?;
    ensure(
        finish == FinishReason::Stop,
        format!("the recovered response finished with {finish:?}"),
    )
}

async fn llm_error_mapping<H: LlmHarness>(h: &H) -> Result<()> {
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    for status in [401_u16, 429, 500, 503] {
        h.reset();
        h.script(text_response(&[], vec![Fault::Reject { status }]));
        let provider = h.provider();
        match bounded(&clock, bound, "stream", provider.stream(llm_request())).await? {
            Err(ProviderError::Rejected { status: got, .. }) if got == status => {}
            Err(other) => {
                return fail(format!(
                    "a refusal with status {status} must map to Rejected {{ status: {status} }}, got {other}"
                ));
            }
            Ok(_) => return fail(format!("stream was accepted despite a {status} refusal")),
        }
    }
    h.reset();
    h.script(text_response(
        &["a", "b"],
        vec![Fault::Disconnect { after_events: 1 }],
    ));
    let mut stream = llm_stream(h).await?;
    llm_event(&clock, bound, &mut stream).await?;
    match llm_event(&clock, bound, &mut stream).await? {
        LlmEvent::Error(ProviderError::Disconnected { .. }) => {}
        other => {
            return fail(format!(
                "a dropped connection must map to Disconnected, got {other:?}"
            ));
        }
    }
    h.reset();
    h.script(text_response(
        &["a", "b"],
        vec![Fault::Protocol { after_events: 0 }],
    ));
    let mut stream = llm_stream(h).await?;
    match llm_event(&clock, bound, &mut stream).await? {
        LlmEvent::Error(ProviderError::Protocol { .. }) => Ok(()),
        other => fail(format!(
            "a protocol violation must map to Protocol, got {other:?}"
        )),
    }
}

async fn llm_timeout<H: LlmHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let timeout = h.io_timeout();
    let bound = progress_bound(timeout);
    // A request that takes longer than the deadline to start.
    h.script(text_response(
        &["a"],
        vec![Fault::ConnectStall { duration: STALL }],
    ));
    let provider = h.provider();
    let start = clock.now();
    match bounded(&clock, bound, "stream", provider.stream(llm_request())).await? {
        Err(ProviderError::Core(CoreError::Timeout { .. })) => {
            ensure_timed_out(elapsed_since(&clock, start), timeout)?;
        }
        Err(other) => {
            return fail(format!(
                "a stalled request must yield Core(Timeout), got {other}"
            ));
        }
        Ok(_) => return fail("a stalled request produced a stream"),
    }
    // A slow request inside the deadline must succeed, and its response arrive whole.
    h.reset();
    h.script(text_response(
        &["a"],
        vec![Fault::ConnectStall {
            duration: slow_but_in_time(timeout),
        }],
    ));
    let mut stream = llm_stream(h).await?;
    llm_until_done(&clock, bound, &mut stream).await?;
    // A stream whose first event stalls.
    h.reset();
    h.script(text_response(
        &["a"],
        vec![Fault::Stall {
            before_event: 0,
            duration: STALL,
        }],
    ));
    let mut stream = llm_stream(h).await?;
    let start = clock.now();
    llm_expect_timeout(&clock, bound, timeout, &mut stream, start).await?;
    // A first event that is slow but inside the deadline must be delivered.
    h.reset();
    h.script(text_response(
        &["a"],
        vec![Fault::Stall {
            before_event: 0,
            duration: slow_but_in_time(timeout),
        }],
    ));
    let mut stream = llm_stream(h).await?;
    llm_until_done(&clock, bound, &mut stream).await?;
    // An idle gap after the first event: the deadline applies to every event, not only to the
    // first one.
    h.reset();
    h.script(text_response(
        &["a", "b"],
        vec![Fault::Stall {
            before_event: 1,
            duration: STALL,
        }],
    ));
    let mut stream = llm_stream(h).await?;
    match llm_event(&clock, bound, &mut stream).await? {
        LlmEvent::TextDelta(_) => {}
        other => return fail(format!("expected a text delta first, got {other:?}")),
    }
    let start = clock.now();
    llm_expect_timeout(&clock, bound, timeout, &mut stream, start).await
}

/// Expects the next event to be a `Core(Timeout)` error at the deadline.
async fn llm_expect_timeout(
    clock: &SharedClock,
    bound: Duration,
    timeout: Duration,
    stream: &mut LlmStream,
    start: tokio::time::Instant,
) -> Result<()> {
    match within(clock, bound, stream.next()).await {
        None => fail(format!(
            "no event and no Timeout within {bound:?}: a stalled provider hangs"
        )),
        Some(None) => fail("the stream ended without a Timeout error"),
        Some(Some(LlmEvent::Error(ProviderError::Core(CoreError::Timeout { .. })))) => {
            ensure_timed_out(elapsed_since(clock, start), timeout)
        }
        Some(Some(other)) => fail(format!(
            "a stalled provider must yield Core(Timeout), got {other:?}"
        )),
    }
}

async fn llm_warm<H: LlmHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    let provider = h.provider();
    h.script(text_response(&["a"], Vec::new()));
    let start = clock.now();
    let cold_stream = bounded_call(
        &clock,
        bound,
        "stream before warm",
        provider.stream(llm_request()),
    )
    .await?;
    let cold = elapsed_since(&clock, start);
    drop(cold_stream);
    bounded_call(&clock, bound, "warm", provider.warm()).await?;
    bounded_call(
        &clock,
        bound,
        "warm (second call, must be idempotent)",
        provider.warm(),
    )
    .await?;
    h.script(text_response(&["a"], Vec::new()));
    let start = clock.now();
    let warm_stream = bounded_call(
        &clock,
        bound,
        "stream after warm",
        provider.stream(llm_request()),
    )
    .await?;
    let warm = elapsed_since(&clock, start);
    drop(warm_stream);
    ensure(
        warm <= cold + WARM_TOLERANCE,
        format!("a request after warm() took {warm:?}, a cold request took {cold:?}"),
    )
}

async fn llm_no_task_leak<H: LlmHarness>(h: &H) -> Result<()> {
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    h.reset();
    h.script(text_response(&["a", "b", "c"], Vec::new()));
    let counter = TaskCounter::start_settled(&*clock, LEAK_BOUND).await?;
    let mut stream = llm_stream(h).await?;
    llm_until_done(&clock, bound, &mut stream).await?;
    drop(stream);
    counter.wait_for_baseline(&*clock, LEAK_BOUND).await?;
    // Hang up mid-response. The response is the long one of the cancel check, so an adapter
    // that keeps pulling it after the drop outlives `LEAK_BOUND` and is seen.
    h.reset();
    h.script(Response {
        text: vec!["tok".to_owned(); CANCEL_RESPONSE_EVENTS],
        tool_calls: Vec::new(),
        faults: Vec::new(),
    });
    let counter = TaskCounter::start_settled(&*clock, LEAK_BOUND).await?;
    let mut stream = llm_stream(h).await?;
    llm_event(&clock, bound, &mut stream).await?;
    drop(stream);
    counter.wait_for_baseline(&*clock, LEAK_BOUND).await
}

// ---------------------------------------------------------------------------------------------
// TTS
// ---------------------------------------------------------------------------------------------

const LONG_TEXT: &str = "one two three four five six seven eight nine ten";

fn tts_config() -> TtsConfig {
    TtsConfig {
        sample_rate: SampleRate::Hz16000,
        voice: Arc::from("contract"),
    }
}

async fn connect_tts<H: TtsHarness>(h: &H) -> Result<TtsSession> {
    let clock = h.clock();
    let provider = h.provider();
    bounded_call(
        &clock,
        progress_bound(h.io_timeout()),
        "connect",
        provider.connect(tts_config()),
    )
    .await
}

async fn timed_connect_tts<P: TtsProvider>(
    clock: &SharedClock,
    bound: Duration,
    provider: &P,
) -> Result<(TtsSession, Duration)> {
    let start = clock.now();
    let session = bounded_call(clock, bound, "connect", provider.connect(tts_config())).await?;
    Ok((session, elapsed_since(clock, start)))
}

async fn tts_event(
    clock: &SharedClock,
    bound: Duration,
    events: &mut mpsc::Receiver<TtsEvent>,
) -> Result<TtsEvent> {
    match within(clock, bound, events.recv()).await {
        None => fail("no event within the progress bound: the provider hangs"),
        Some(None) => fail("the event stream closed before the expected event"),
        Some(Some(event)) => Ok(event),
    }
}

/// A short description of a TTS event for failure messages (audio is summarised by its size).
fn describe_tts(event: &TtsEvent) -> String {
    match event {
        TtsEvent::Audio { context, audio } => {
            format!("Audio of {} bytes for {context:?}", audio.len())
        }
        TtsEvent::Word {
            context,
            word,
            start,
        } => format!("Word {word:?} at {start:?} for {context:?}"),
        TtsEvent::Flushed { context } => format!("Flushed {context:?}"),
        TtsEvent::Cancelled { context } => format!("Cancelled {context:?}"),
        TtsEvent::Error(e) => format!("Error({e})"),
    }
}

/// The audio size and words of one context, collected up to its `Flushed`.
#[derive(Debug, Default)]
struct Spoken {
    bytes: usize,
    words: Vec<(String, Duration)>,
}

async fn tts_until_flushed(
    clock: &SharedClock,
    bound: Duration,
    events: &mut mpsc::Receiver<TtsEvent>,
    context: ContextId,
) -> Result<Spoken> {
    let mut spoken = Spoken::default();
    for _ in 0..MAX_EVENTS {
        match tts_event(clock, bound, events).await? {
            TtsEvent::Audio { context: c, audio } if c == context => spoken.bytes += audio.len(),
            TtsEvent::Word {
                context: c,
                word,
                start,
            } if c == context => spoken.words.push((word.to_string(), start)),
            TtsEvent::Flushed { context: c } if c == context => return Ok(spoken),
            TtsEvent::Error(e) => return Err(provider_error("tts", e)),
            other => {
                return fail(format!(
                    "unexpected {} while waiting for Flushed of {context:?}",
                    describe_tts(&other)
                ));
            }
        }
    }
    fail("no Flushed within the event budget")
}

/// Fails if any event arrives within [`QUIET_WINDOW`]; a closed stream is fine.
async fn tts_quiet(
    clock: &SharedClock,
    events: &mut mpsc::Receiver<TtsEvent>,
    what: &str,
) -> Result<()> {
    match within(clock, QUIET_WINDOW, events.recv()).await {
        Some(Some(event)) => fail(format!("{} {what}", describe_tts(&event))),
        _ => Ok(()),
    }
}

fn starts_are_nondecreasing(words: &[(String, Duration)]) -> bool {
    words
        .iter()
        .zip(words.iter().skip(1))
        .all(|(a, b)| a.1 <= b.1)
}

/// The audio must last until the last word starts: words and audio share one timeline, so a
/// truncated audio stream fails here.
fn ensure_covers_words(spoken: &Spoken) -> Result<()> {
    let audio =
        Duration::from_millis(u64::try_from(spoken.bytes / BYTES_PER_MS).unwrap_or(u64::MAX));
    match spoken.words.last() {
        Some((word, start)) => ensure(
            audio >= *start,
            format!("{audio:?} of audio end before {word:?} starts, at {start:?}"),
        ),
        None => Ok(()),
    }
}

async fn tts_audio_and_words<H: TtsHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    let ctx = ContextId(1);
    let mut session = connect_tts(h).await?;
    send(
        &session.input,
        TtsInput::Text {
            context: ctx,
            text: Arc::from("hello brave world"),
        },
    )
    .await?;
    send(&session.input, TtsInput::Flush { context: ctx }).await?;
    let spoken = tts_until_flushed(&clock, bound, &mut session.events, ctx).await?;
    ensure(
        spoken.bytes > 0 && spoken.bytes % 2 == 0,
        format!("{} bytes of audio: expected PCM16 samples", spoken.bytes),
    )?;
    if !h.emits_words() {
        ensure(
            spoken.words.is_empty(),
            format!("{:?}: this provider reports no word times", spoken.words),
        )?;
        return tts_quiet(&clock, &mut session.events, "after Flushed").await;
    }
    let words: Vec<&str> = spoken.words.iter().map(|(w, _)| w.as_str()).collect();
    ensure(
        words == ["hello", "brave", "world"],
        format!("words {words:?}, expected hello, brave, world"),
    )?;
    // The first word need not start at offset zero: providers report alignment after leading
    // silence. Offsets are relative to the first audio sample and, with the coverage check
    // below, must lie inside the audio.
    ensure(
        starts_are_nondecreasing(&spoken.words),
        "word offsets go backwards",
    )?;
    ensure_covers_words(&spoken)?;
    tts_quiet(&clock, &mut session.events, "after Flushed").await
}

async fn tts_flush<H: TtsHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    let ctx = ContextId(1);
    let mut session = connect_tts(h).await?;
    // Two text pieces of one utterance, then the flush: the words continue across the pieces.
    send(
        &session.input,
        TtsInput::Text {
            context: ctx,
            text: Arc::from("one two"),
        },
    )
    .await?;
    send(
        &session.input,
        TtsInput::Text {
            context: ctx,
            text: Arc::from(" three"),
        },
    )
    .await?;
    send(&session.input, TtsInput::Flush { context: ctx }).await?;
    let spoken = tts_until_flushed(&clock, bound, &mut session.events, ctx).await?;
    if !h.emits_words() {
        ensure(
            spoken.words.is_empty() && spoken.bytes > 0,
            format!(
                "{:?} with {} bytes: expected audio and no word times",
                spoken.words, spoken.bytes
            ),
        )?;
        return tts_quiet(&clock, &mut session.events, "after Flushed").await;
    }
    let words: Vec<&str> = spoken.words.iter().map(|(w, _)| w.as_str()).collect();
    ensure(
        words == ["one", "two", "three"],
        format!("words {words:?}, expected one, two, three"),
    )?;
    ensure(
        spoken
            .words
            .iter()
            .zip(spoken.words.iter().skip(1))
            .all(|(a, b)| a.1 < b.1),
        "word offsets must keep increasing across text pieces",
    )?;
    ensure(spoken.bytes > 0, "a flush without audio")?;
    ensure_covers_words(&spoken)?;
    tts_quiet(&clock, &mut session.events, "after Flushed").await
}

async fn tts_cancel<H: TtsHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    let cancelled = ContextId(2);
    let mut session = connect_tts(h).await?;
    send(
        &session.input,
        TtsInput::Text {
            context: cancelled,
            text: Arc::from(LONG_TEXT),
        },
    )
    .await?;
    // Wait until the context is audibly under way, then barge in.
    loop {
        match tts_event(&clock, bound, &mut session.events).await? {
            TtsEvent::Audio { context, .. } if context == cancelled => break,
            TtsEvent::Word { context, .. } if context == cancelled => {}
            other => {
                return fail(format!(
                    "expected audio of the context, got {}",
                    describe_tts(&other)
                ));
            }
        }
    }
    // Output already handed to the caller is not the provider's to take back: give in-flight
    // output a moment to land, then read what is buffered. A synthesizer faster than real time
    // has a burst of events queued here; only what arrives after the cancel is judged.
    wait_for(&*clock, SETTLE).await;
    for _ in 0..MAX_EVENTS {
        match session.events.try_recv() {
            Ok(TtsEvent::Error(e)) => return Err(provider_error("tts", e)),
            Ok(TtsEvent::Cancelled { .. }) => {
                return fail("Cancelled arrived before any cancel was sent");
            }
            Ok(_) => {}
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return fail("the event stream closed before the cancel");
            }
        }
    }
    session.cancel.send(cancelled).await.map_err(|_| {
        Error::Assertion("the cancel channel is closed: cancel is ignored".to_owned())
    })?;
    // The acknowledgement must come promptly, and little of the cancelled context may still
    // arrive before it.
    let deadline = deadline_after(clock.now(), CANCEL_PROMPT);
    let mut late = 0_usize;
    for _ in 0..MAX_EVENTS {
        let remaining = deadline.saturating_duration_since(clock.now());
        let event = match within(&clock, remaining, session.events.recv()).await {
            None => {
                return fail(format!(
                    "no Cancelled within {CANCEL_PROMPT:?} of the barge-in: cancel is not honoured promptly"
                ));
            }
            Some(None) => {
                return fail("the event stream closed before the Cancelled acknowledgement");
            }
            Some(Some(event)) => event,
        };
        match event {
            TtsEvent::Cancelled { context } if context == cancelled => {
                tts_quiet(&clock, &mut session.events, "after Cancelled").await?;
                // The session keeps working after a barge-in.
                let next = ContextId(3);
                send(
                    &session.input,
                    TtsInput::Text {
                        context: next,
                        text: Arc::from("ok"),
                    },
                )
                .await?;
                send(&session.input, TtsInput::Flush { context: next }).await?;
                tts_until_flushed(&clock, bound, &mut session.events, next).await?;
                return Ok(());
            }
            TtsEvent::Audio { context, .. }
            | TtsEvent::Word { context, .. }
            | TtsEvent::Flushed { context }
                if context == cancelled =>
            {
                late += 1;
                ensure(
                    late <= CANCEL_SLACK_EVENTS,
                    format!(
                        "{late} events of the cancelled context arrived after the barge-in: the output did not stop"
                    ),
                )?;
            }
            other => {
                return fail(format!(
                    "unexpected {} around the cancel",
                    describe_tts(&other)
                ));
            }
        }
    }
    fail("no Cancelled acknowledgement within the event budget")
}

async fn tts_reconnect<H: TtsHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    h.inject(Fault::Disconnect { after_events: 1 });
    let mut session = connect_tts(h).await?;
    send(
        &session.input,
        TtsInput::Text {
            context: ContextId(1),
            text: Arc::from("hello world"),
        },
    )
    .await?;
    let mut dropped = false;
    for _ in 0..MAX_EVENTS {
        match tts_event(&clock, bound, &mut session.events).await? {
            TtsEvent::Error(ProviderError::Disconnected { .. }) => {
                dropped = true;
                break;
            }
            TtsEvent::Error(e) => {
                return fail(format!(
                    "a dropped connection must map to Disconnected, got {e}"
                ));
            }
            _ => {}
        }
    }
    ensure(dropped, "the drop never surfaced as an error")?;
    match within(&clock, bound, session.events.recv()).await {
        None => return fail("the stream did not end after the drop"),
        Some(None) => {}
        Some(Some(event)) => return fail(format!("{} after the drop", describe_tts(&event))),
    }
    drop(session);
    h.reset();
    let mut session = connect_tts(h).await?;
    let ctx = ContextId(2);
    send(
        &session.input,
        TtsInput::Text {
            context: ctx,
            text: Arc::from("ok"),
        },
    )
    .await?;
    send(&session.input, TtsInput::Flush { context: ctx }).await?;
    tts_until_flushed(&clock, bound, &mut session.events, ctx)
        .await
        .map(|_| ())
}

async fn tts_error_mapping<H: TtsHarness>(h: &H) -> Result<()> {
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    for status in [401_u16, 429, 500, 503] {
        h.reset();
        h.inject(Fault::Reject { status });
        if !h.faults_at_connect() {
            let mut session = connect_tts(h).await?;
            send(&session.input, tts_text(ContextId(1), "hi")).await?;
            match tts_event(&clock, bound, &mut session.events).await? {
                TtsEvent::Error(ProviderError::Rejected { status: got, .. }) if got == status => {}
                other => {
                    return fail(format!(
                        "a refusal with status {status} must map to Rejected {{ status: {status} }}, got {}",
                        describe_tts(&other)
                    ));
                }
            }
            continue;
        }
        let provider = h.provider();
        match bounded(&clock, bound, "connect", provider.connect(tts_config())).await? {
            Err(ProviderError::Rejected { status: got, .. }) if got == status => {}
            Err(other) => {
                return fail(format!(
                    "a refusal with status {status} must map to Rejected {{ status: {status} }}, got {other}"
                ));
            }
            Ok(_) => return fail(format!("connect was accepted despite a {status} refusal")),
        }
    }
    for (fault, expected) in [
        (Fault::Disconnect { after_events: 0 }, "Disconnected"),
        (Fault::Protocol { after_events: 0 }, "Protocol"),
    ] {
        h.reset();
        h.inject(fault);
        let mut session = connect_tts(h).await?;
        send(
            &session.input,
            TtsInput::Text {
                context: ContextId(1),
                text: Arc::from("hi"),
            },
        )
        .await?;
        let event = tts_event(&clock, bound, &mut session.events).await?;
        let mapped = match expected {
            "Disconnected" => matches!(event, TtsEvent::Error(ProviderError::Disconnected { .. })),
            _ => matches!(event, TtsEvent::Error(ProviderError::Protocol { .. })),
        };
        ensure(
            mapped,
            format!(
                "{fault:?} must map to {expected}, got {}",
                describe_tts(&event)
            ),
        )?;
    }
    Ok(())
}

async fn tts_timeout<H: TtsHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let timeout = h.io_timeout();
    let bound = progress_bound(timeout);
    // A connect that takes longer than the deadline.
    h.inject(Fault::ConnectStall { duration: STALL });
    if h.faults_at_connect() {
        let provider = h.provider();
        let start = clock.now();
        match bounded(&clock, bound, "connect", provider.connect(tts_config())).await? {
            Err(ProviderError::Core(CoreError::Timeout { .. })) => {
                ensure_timed_out(elapsed_since(&clock, start), timeout)?;
            }
            Err(other) => {
                return fail(format!(
                    "a stalled connect must yield Core(Timeout), got {other}"
                ));
            }
            Ok(_) => return fail("a stalled connect produced a session"),
        }
    } else {
        // The stall hits the first request, so the timeout comes as an event after the text.
        let mut session = connect_tts(h).await?;
        let start = clock.now();
        send(&session.input, tts_text(ContextId(1), "hi")).await?;
        tts_expect_timeout(&clock, bound, timeout, &mut session.events, start).await?;
    }
    // A slow connect inside the deadline must succeed.
    h.reset();
    h.inject(Fault::ConnectStall {
        duration: slow_but_in_time(timeout),
    });
    if h.faults_at_connect() {
        drop(connect_tts(h).await?);
    } else {
        let mut session = connect_tts(h).await?;
        let ctx = ContextId(1);
        send(&session.input, tts_text(ctx, "hi")).await?;
        send(&session.input, TtsInput::Flush { context: ctx }).await?;
        tts_until_flushed(&clock, bound, &mut session.events, ctx).await?;
    }
    // A session whose first event stalls.
    h.reset();
    h.inject(Fault::Stall {
        before_event: 0,
        duration: STALL,
    });
    let mut session = connect_tts(h).await?;
    let start = clock.now();
    send(&session.input, tts_text(ContextId(1), "hi")).await?;
    tts_expect_timeout(&clock, bound, timeout, &mut session.events, start).await?;
    // A first event that is slow but inside the deadline must be delivered: the whole
    // utterance plays out.
    h.reset();
    h.inject(Fault::Stall {
        before_event: 0,
        duration: slow_but_in_time(timeout),
    });
    let mut session = connect_tts(h).await?;
    let ctx = ContextId(1);
    send(&session.input, tts_text(ctx, "hi there")).await?;
    send(&session.input, TtsInput::Flush { context: ctx }).await?;
    tts_until_flushed(&clock, bound, &mut session.events, ctx).await?;
    // An idle gap after the first event: the deadline applies to every event, not only to the
    // first one.
    h.reset();
    h.inject(Fault::Stall {
        before_event: 1,
        duration: STALL,
    });
    let mut session = connect_tts(h).await?;
    send(&session.input, tts_text(ctx, "hello brave world")).await?;
    match tts_event(&clock, bound, &mut session.events).await? {
        TtsEvent::Audio { .. } | TtsEvent::Word { .. } => {}
        other => {
            return fail(format!(
                "expected audio or a word first, got {}",
                describe_tts(&other)
            ));
        }
    }
    let start = clock.now();
    tts_expect_timeout(&clock, bound, timeout, &mut session.events, start).await
}

fn tts_text(context: ContextId, text: &str) -> TtsInput {
    TtsInput::Text {
        context,
        text: Arc::from(text),
    }
}

/// Expects the next event to be a `Core(Timeout)` error at the deadline.
async fn tts_expect_timeout(
    clock: &SharedClock,
    bound: Duration,
    timeout: Duration,
    events: &mut mpsc::Receiver<TtsEvent>,
    start: tokio::time::Instant,
) -> Result<()> {
    match within(clock, bound, events.recv()).await {
        None => fail(format!(
            "no event and no Timeout within {bound:?}: a stalled provider hangs"
        )),
        Some(None) => fail("the stream closed without a Timeout error"),
        Some(Some(TtsEvent::Error(ProviderError::Core(CoreError::Timeout { .. })))) => {
            ensure_timed_out(elapsed_since(clock, start), timeout)
        }
        Some(Some(other)) => fail(format!(
            "a stalled provider must yield Core(Timeout), got {}",
            describe_tts(&other)
        )),
    }
}

async fn tts_warm<H: TtsHarness>(h: &H) -> Result<()> {
    h.reset();
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    let provider = h.provider();
    let (cold_session, cold) = timed_connect_tts(&clock, bound, &*provider).await?;
    drop(cold_session);
    bounded_call(&clock, bound, "warm", provider.warm()).await?;
    bounded_call(
        &clock,
        bound,
        "warm (second call, must be idempotent)",
        provider.warm(),
    )
    .await?;
    let (warm_session, warm) = timed_connect_tts(&clock, bound, &*provider).await?;
    drop(warm_session);
    ensure(
        warm <= cold + WARM_TOLERANCE,
        format!("connect after warm() took {warm:?}, a cold connect took {cold:?}"),
    )
}

async fn tts_no_task_leak<H: TtsHarness>(h: &H) -> Result<()> {
    let clock = h.clock();
    let bound = progress_bound(h.io_timeout());
    let ctx = ContextId(1);
    h.reset();
    let counter = TaskCounter::start_settled(&*clock, LEAK_BOUND).await?;
    let mut session = connect_tts(h).await?;
    send(
        &session.input,
        TtsInput::Text {
            context: ctx,
            text: Arc::from("hi there"),
        },
    )
    .await?;
    send(&session.input, TtsInput::Flush { context: ctx }).await?;
    tts_until_flushed(&clock, bound, &mut session.events, ctx).await?;
    drop(session);
    counter.wait_for_baseline(&*clock, LEAK_BOUND).await?;
    // Hang up mid-utterance.
    h.reset();
    let counter = TaskCounter::start_settled(&*clock, LEAK_BOUND).await?;
    let mut session = connect_tts(h).await?;
    send(
        &session.input,
        TtsInput::Text {
            context: ctx,
            text: Arc::from(LONG_TEXT),
        },
    )
    .await?;
    tts_event(&clock, bound, &mut session.events).await?;
    drop(session);
    counter.wait_for_baseline(&*clock, LEAK_BOUND).await
}

// ---------------------------------------------------------------------------------------------
// The mocks implement the harnesses directly.
// ---------------------------------------------------------------------------------------------

fn stream_fault(fault: Fault) -> StreamFault {
    match fault {
        Fault::Reject { status } => StreamFault::ConnectError { status },
        Fault::Disconnect { after_events } => StreamFault::Disconnect { after_events },
        Fault::Protocol { after_events } => StreamFault::ProtocolError { after_events },
        Fault::Stall {
            before_event,
            duration,
        } => StreamFault::Stall {
            before_event,
            duration,
        },
        Fault::ConnectStall { duration } => StreamFault::ConnectStall { duration },
    }
}

fn llm_fault(fault: Fault) -> LlmFault {
    match fault {
        Fault::Reject { status } => LlmFault::ErrorResponse { status },
        Fault::Disconnect { after_events } => LlmFault::Disconnect { after_events },
        Fault::Protocol { after_events } => LlmFault::ProtocolError { after_events },
        Fault::Stall {
            before_event,
            duration,
        } => LlmFault::Stall {
            before_event,
            duration,
        },
        Fault::ConnectStall { duration } => LlmFault::ConnectStall { duration },
    }
}

impl SttHarness for MockStt {
    type P = MockStt;

    fn provider(&self) -> Arc<MockStt> {
        Arc::new(self.clone())
    }

    fn clock(&self) -> SharedClock {
        Arc::clone(&self.clock)
    }

    fn reset(&self) {
        self.clear_script();
    }

    fn script(&self, utterance: Utterance) {
        self.push(SttUtterance {
            interims: utterance.interims,
            finals: utterance.finals,
        });
    }

    fn inject(&self, fault: Fault) {
        self.push_session_faults(vec![stream_fault(fault)]);
    }
}

impl LlmHarness for MockLlm {
    type P = MockLlm;

    fn provider(&self) -> Arc<MockLlm> {
        Arc::new(self.clone())
    }

    fn clock(&self) -> SharedClock {
        Arc::clone(&self.clock)
    }

    fn reset(&self) {
        self.clear_script();
    }

    fn script(&self, response: Response) {
        let mut script = LlmScript::text(response.text);
        for call in response.tool_calls {
            script = script.with_tool_call(&call.id, &call.name, &call.arguments);
        }
        for fault in response.faults {
            script = script.with_fault(llm_fault(fault));
        }
        self.push(script);
    }

    fn events_sent(&self) -> usize {
        MockLlm::events_sent(self)
    }
}

impl TtsHarness for MockTts {
    type P = MockTts;

    fn provider(&self) -> Arc<MockTts> {
        Arc::new(self.clone())
    }

    fn clock(&self) -> SharedClock {
        Arc::clone(&self.clock)
    }

    fn reset(&self) {
        self.clear_script();
    }

    fn inject(&self, fault: Fault) {
        self.push_session_faults(vec![stream_fault(fault)]);
    }
}
