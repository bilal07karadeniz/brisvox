//! Scripted mock LLM with latency profiles and fault injection.
//!
//! Timing: `stream()` itself takes the connect latency (also before a rejection); the first
//! event follows after the first-token latency, every further event after an inter-token
//! sample. Samplers are per mock, so a `Scripted` inter-token profile indexes events across all
//! streams in call order. Fault indices past the end of a response are no-ops.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_providers::llm::LLM_EVENT_CAPACITY;
use brisvox_providers::{
    Error, FinishReason, LlmEvent, LlmProvider, LlmRequest, LlmStream, Result, Usage,
};
use tokio::sync::mpsc;

use crate::latency::{LatencyProfile, LatencySampler};

const NAME: &str = "mock-llm";

/// A tool call the mock streams as argument deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedToolCall {
    /// Call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Complete JSON arguments.
    pub arguments: String,
}

/// Something going wrong with one response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmFault {
    /// The request is refused before streaming (`stream` returns `Error::Rejected`).
    ErrorResponse {
        /// Status, e.g. 429 or 500.
        status: u16,
    },
    /// `stream` takes `duration` longer. With an adapter deadline shorter than that, `stream`
    /// fails with `Error::Core(Timeout)` at the deadline.
    ConnectStall {
        /// Extra delay of the `stream` call.
        duration: Duration,
    },
    /// After `after_events` events the connection drops: `Error::Disconnected`, no `Done`.
    Disconnect {
        /// Events delivered before the drop.
        after_events: usize,
    },
    /// The stream pauses for `duration` before event number `before_event`.
    Stall {
        /// Index of the event that is delayed.
        before_event: usize,
        /// Extra delay.
        duration: Duration,
    },
    /// After `after_events` events the provider sends something unparsable:
    /// `Error::Protocol`, no `Done`.
    ProtocolError {
        /// Events delivered before the violation.
        after_events: usize,
    },
    /// Tool-call arguments are cut to their first half (invalid JSON for any object of two or
    /// more characters; shorter arguments are left as they are).
    MalformedToolArguments,
    /// Deltas of different tool calls interleave (round-robin) instead of call after call.
    InterleavedToolDeltas,
}

/// One scripted response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmScript {
    /// Text deltas, in order.
    pub text: Vec<String>,
    /// Tool calls, streamed after the text.
    pub tool_calls: Vec<ScriptedToolCall>,
    /// Argument fragment size in bytes (at least 1).
    pub argument_chunk: usize,
    /// Faults applied to this response.
    pub faults: Vec<LlmFault>,
}

impl LlmScript {
    /// A plain text answer streamed as the given deltas.
    pub fn text<I, S>(deltas: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            text: deltas.into_iter().map(Into::into).collect(),
            tool_calls: Vec::new(),
            argument_chunk: 8,
            faults: Vec::new(),
        }
    }

    /// Adds a tool call.
    #[must_use]
    pub fn with_tool_call(mut self, id: &str, name: &str, arguments: &str) -> Self {
        self.tool_calls.push(ScriptedToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        });
        self
    }

    /// Adds a fault.
    #[must_use]
    pub fn with_fault(mut self, fault: LlmFault) -> Self {
        self.faults.push(fault);
        self
    }

    /// Extra delay of the `stream` call, summed over the connect stalls of this response.
    fn connect_stall(&self) -> Duration {
        self.faults
            .iter()
            .filter_map(|f| match f {
                LlmFault::ConnectStall { duration } => Some(*duration),
                _ => None,
            })
            .sum()
    }

    fn reject_status(&self) -> Option<u16> {
        self.faults.iter().find_map(|f| match f {
            LlmFault::ErrorResponse { status } => Some(*status),
            _ => None,
        })
    }

    fn has(&self, fault: &LlmFault) -> bool {
        self.faults.contains(fault)
    }

    /// The events of a successful stream, before timing and connection faults.
    fn events(&self, usage: Usage) -> Vec<LlmEvent> {
        let mut events: Vec<LlmEvent> = self
            .text
            .iter()
            .map(|t| LlmEvent::TextDelta(Arc::from(t.as_str())))
            .collect();
        let chunk = self.argument_chunk.max(1);
        let malformed = self.has(&LlmFault::MalformedToolArguments);
        let mut per_call: Vec<VecDeque<LlmEvent>> = Vec::new();
        for (index, call) in self.tool_calls.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let mut arguments = call.arguments.clone();
            if malformed {
                // Drop the closing half: never valid JSON for a non-empty object.
                let keep = arguments.len() / 2;
                arguments = arguments.chars().take(keep).collect();
            }
            let pieces = split_chars(&arguments, chunk);
            let mut deltas = VecDeque::with_capacity(pieces.len().max(1));
            for (i, piece) in pieces.into_iter().enumerate() {
                deltas.push_back(LlmEvent::ToolCallDelta {
                    index,
                    id: (i == 0).then(|| Arc::from(call.id.as_str())),
                    name: (i == 0).then(|| Arc::from(call.name.as_str())),
                    arguments: Arc::from(piece.as_str()),
                });
            }
            if deltas.is_empty() {
                deltas.push_back(LlmEvent::ToolCallDelta {
                    index,
                    id: Some(Arc::from(call.id.as_str())),
                    name: Some(Arc::from(call.name.as_str())),
                    arguments: Arc::from(""),
                });
            }
            per_call.push(deltas);
        }
        if self.has(&LlmFault::InterleavedToolDeltas) {
            while per_call.iter().any(|d| !d.is_empty()) {
                for deltas in &mut per_call {
                    if let Some(event) = deltas.pop_front() {
                        events.push(event);
                    }
                }
            }
        } else {
            events.extend(per_call.into_iter().flatten());
        }
        let finish = if self.tool_calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        };
        events.push(LlmEvent::Done {
            finish,
            usage: Some(usage),
            provider_data: None,
        });
        events
    }
}

/// Splits `s` into pieces of at most `chunk` bytes on char boundaries.
fn split_chars(s: &str, chunk: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    for c in s.chars() {
        if !current.is_empty() && current.len() + c.len_utf8() > chunk {
            pieces.push(std::mem::take(&mut current));
        }
        current.push(c);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Chooses the response from the request itself (see [`MockLlm::set_responder`]).
pub type LlmResponder = Arc<dyn Fn(&LlmRequest) -> Option<LlmScript> + Send + Sync>;

struct State {
    scripts: VecDeque<LlmScript>,
    fallback: LlmScript,
    responder: Option<LlmResponder>,
    connect: LatencySampler,
    first_token: LatencySampler,
    inter_token: LatencySampler,
    /// Adapter-side deadline for `stream` and for each event; `None` waits as long as the
    /// latency and faults say.
    timeout: Option<Duration>,
    requests: Vec<LlmRequest>,
}

/// A mock [`LlmProvider`]: each `stream` call plays the next queued [`LlmScript`] (or the
/// fallback), timed by the configured latency profiles on the injected clock.
#[derive(Clone)]
pub struct MockLlm {
    pub(crate) clock: SharedClock,
    state: Arc<Mutex<State>>,
    sent: Arc<AtomicUsize>,
}

impl MockLlm {
    /// A mock with fixed zero latency and an empty fallback answer.
    pub fn new(clock: SharedClock) -> Self {
        Self {
            clock,
            state: Arc::new(Mutex::new(State {
                scripts: VecDeque::new(),
                fallback: LlmScript::text(Vec::<String>::new()),
                responder: None,
                connect: LatencySampler::new(LatencyProfile::ZERO, 0),
                first_token: LatencySampler::new(LatencyProfile::ZERO, 0),
                inter_token: LatencySampler::new(LatencyProfile::ZERO, 0),
                timeout: None,
                requests: Vec::new(),
            })),
            sent: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Time to first token (from the `stream` call) and between events.
    #[must_use]
    pub fn with_latency(
        self,
        first_token: LatencyProfile,
        inter_token: LatencyProfile,
        seed: u64,
    ) -> Self {
        {
            let mut state = self.lock();
            state.first_token = LatencySampler::new(first_token, seed);
            state.inter_token = LatencySampler::new(inter_token, seed.wrapping_add(1));
        }
        self
    }

    /// Time `stream()` itself takes (connection + response headers), also before a rejection.
    #[must_use]
    pub fn with_connect_latency(self, connect: LatencyProfile, seed: u64) -> Self {
        self.lock().connect = LatencySampler::new(connect, seed);
        self
    }

    /// Adapter-side deadline: a `stream` call slower than `timeout` fails with
    /// `Error::Core(Timeout)`, and an event that would arrive later than `timeout` after the
    /// previous one ends the stream with the same error.
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.lock().timeout = Some(timeout);
        self
    }

    /// Queues a response for the next `stream` call.
    pub fn push(&self, script: LlmScript) {
        self.lock().scripts.push_back(script);
    }

    /// Answers each request from the request itself, ahead of the queue: `responder` returns the
    /// response for a request, or `None` to fall through to the queue and the fallback. A
    /// stateless responder keeps a call's answers right when requests are not one per turn (a
    /// speculative request that is discarded would otherwise consume the next turn's queued
    /// answer).
    pub fn set_responder(&self, responder: LlmResponder) {
        self.lock().responder = Some(responder);
    }

    /// Response used when the queue is empty.
    pub fn set_fallback(&self, script: LlmScript) {
        self.lock().fallback = script;
    }

    /// Drops queued responses and restores the empty fallback.
    pub fn clear_script(&self) {
        let mut state = self.lock();
        state.scripts.clear();
        state.responder = None;
        state.fallback = LlmScript::text(Vec::<String>::new());
    }

    /// Requests received so far, in order.
    pub fn requests(&self) -> Vec<LlmRequest> {
        self.lock().requests.clone()
    }

    /// Response events (text deltas, tool-call fragments and `Done`) handed to streams so far,
    /// across all requests. Counts what was sent into the stream, not what the consumer read.
    pub fn events_sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for MockLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockLlm")
            .field("requests", &self.lock().requests.len())
            .finish_non_exhaustive()
    }
}

impl LlmProvider for MockLlm {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream> {
        let usage = estimate_usage(&request);
        let (script, connect, timeout) = {
            let mut state = self.lock();
            let chosen = state.responder.clone().and_then(|r| r(&request));
            state.requests.push(request);
            let script = match chosen.or_else(|| state.scripts.pop_front()) {
                Some(script) => script,
                None => state.fallback.clone(),
            };
            let connect = state.connect.sample();
            (script, connect, state.timeout)
        };
        let connect = connect.saturating_add(script.connect_stall());
        if let Some(timeout) = timeout
            && connect > timeout
        {
            brisvox_core::clock::wait_for(&*self.clock, timeout).await;
            return Err(Error::Core(brisvox_core::Error::Timeout {
                what: "llm connect",
                after: timeout,
            }));
        }
        brisvox_core::clock::wait_for(&*self.clock, connect).await;
        if let Some(status) = script.reject_status() {
            return Err(Error::Rejected {
                provider: NAME,
                status,
            });
        }
        let events = script.events(usage);
        let timing: Vec<Duration> = {
            let mut state = self.lock();
            let first = state.first_token.sample();
            std::iter::once(first)
                .chain((1..events.len()).map(|_| state.inter_token.sample()))
                .collect()
        };
        let (tx, rx) = mpsc::channel(LLM_EVENT_CAPACITY);
        // Ends when the response is delivered or the consumer drops the stream.
        tokio::spawn(play(
            self.clock.clone(),
            script.faults,
            events,
            timing,
            timeout,
            tx,
            Arc::clone(&self.sent),
        ));
        Ok(LlmStream::new(rx))
    }
}

/// Rough token counts (4 characters per token), so usage-dependent code has numbers to use.
fn estimate_usage(request: &LlmRequest) -> Usage {
    let chars: usize = request
        .messages
        .iter()
        .map(|m| m.content.chars().count())
        .sum();
    Usage {
        input_tokens: u32::try_from(chars / 4).unwrap_or(u32::MAX),
        cached_input_tokens: 0,
        output_tokens: 0,
    }
}

/// Delivers `events` with their timing and connection faults. Ends early when the consumer
/// drops the stream.
async fn play(
    clock: SharedClock,
    faults: Vec<LlmFault>,
    events: Vec<LlmEvent>,
    timing: Vec<Duration>,
    timeout: Option<Duration>,
    tx: mpsc::Sender<LlmEvent>,
    sent: Arc<AtomicUsize>,
) {
    let output_tokens = u32::try_from(events.len().saturating_sub(1)).unwrap_or(u32::MAX);
    for (i, event) in events.into_iter().enumerate() {
        let mut wait = timing.get(i).copied().unwrap_or_default();
        for fault in &faults {
            if let LlmFault::Stall {
                before_event,
                duration,
            } = fault
                && *before_event == i
            {
                wait += *duration;
            }
        }
        // Past the adapter's deadline the wait ends at the deadline with a Timeout error.
        let limit = timeout.map_or(wait, |t| wait.min(t));
        tokio::select! {
            () = brisvox_core::clock::wait_for(&*clock, limit) => {}
            () = tx.closed() => return,
        }
        if let Some(timeout) = timeout
            && wait > timeout
        {
            let _ = tx
                .send(LlmEvent::Error(Error::Core(brisvox_core::Error::Timeout {
                    what: "llm event",
                    after: timeout,
                })))
                .await;
            return;
        }
        let cut = faults.iter().find_map(|f| match f {
            LlmFault::Disconnect { after_events } if *after_events == i => {
                Some(Error::Disconnected { provider: NAME })
            }
            LlmFault::ProtocolError { after_events } if *after_events == i => {
                Some(Error::Protocol {
                    provider: NAME,
                    detail: "mock: unparsable event".to_owned(),
                })
            }
            _ => None,
        });
        if let Some(error) = cut {
            let _ = tx.send(LlmEvent::Error(error)).await;
            return;
        }
        let event = match event {
            LlmEvent::Done {
                finish,
                usage: Some(usage),
                provider_data,
            } => LlmEvent::Done {
                finish,
                usage: Some(Usage {
                    output_tokens,
                    ..usage
                }),
                provider_data,
            },
            other => other,
        };
        if tx.send(event).await.is_err() {
            return;
        }
        sent.fetch_add(1, Ordering::SeqCst);
    }
}
