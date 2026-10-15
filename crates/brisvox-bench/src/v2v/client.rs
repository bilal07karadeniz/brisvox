//! The benchmark client: one call over the websocket protocol (`docs/WEBSOCKET_PROTOCOL.md`).
//!
//! It streams a fixture in real time and records when the bot's audio arrives, so
//! [`measure`](super::measure::measure) can compute voice-to-voice latency per turn.
//!
//! # Timing model
//! - The stream origin `begin` is the clock reading right after the server's `started`.
//! - Sample `s` of the fixture is "spoken" at `begin + s / rate`. A frame is sent when its last
//!   sample has been spoken (a microphone delivers a frame once it is complete), so no sample
//!   leaves earlier than it was uttered. Ground-truth end of speech is `begin + end_sample /
//!   rate`; a frame that ends exactly there is sent exactly then.
//! - Frame deadlines are absolute (`begin + k * 20 ms`), not "20 ms after the previous send",
//!   so a late send does not shift the following ones. How late sends were is reported
//!   ([`CallOutcome::max_send_lateness`]) so a loaded client machine shows up.
//! - After the fixture the client keeps sending digital silence on the same 20 ms grid until the
//!   last turn is answered or [`ClientConfig::response_timeout`] has passed: a phone does not go
//!   quiet on the wire, and a server that counts silence from incoming frames (VAD, smart turn,
//!   STT endpointing) needs it to end the last turn.
//! - A bot frame is stamped with the clock the moment the reader task gets it from the socket,
//!   before anything else is done with it. The reader is a task of its own, so a blocked write
//!   does not delay the stamps.
//! - TCP_NODELAY is on: Nagle would hold back small frames.
//!
//! # Tasks, bounds and cancellation
//! One reader task per call, a child of a token that the call owns: dropping or cancelling the
//! call cancels it. It hands stamped messages to the call over a bounded channel
//! ([`INBOUND_CAPACITY`] messages, about 80 s of 20 ms audio); when the call falls behind the
//! reader waits, which delays later stamps but never loses a message. Every network await
//! (connect, `started`, each write, the goodbye) has a timeout on the injected clock; the wait for
//! `started` has one deadline overall, so pings or unknown messages cannot stretch it.
//!
//! The client behaves like a phone for marks and clears: audio plays back to back, a `mark` is
//! echoed when everything before it has played, a `clear` forgets the unplayed audio and the
//! pending marks. Past [`MAX_PENDING_MARKS`] unechoed marks the oldest is given up (the server
//! then reports it as cut).
//!
//! Only `pcm16` is supported: the audible/silent decision needs linear samples.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::time::Duration;

use brisvox_core::clock::{deadline_after, wait_until};
use brisvox_core::{Clock, Secret, SharedClock};
use brisvox_sim::fixture::Fixture;
use brisvox_transport::websocket::protocol::{
    ClientMessage, Encoding, ProtocolError, ServerMessage, Start,
};
use bytes::Bytes;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};
use tokio_util::sync::CancellationToken;

use super::measure::{
    DEFAULT_OVERLAP_GUARD, DEFAULT_SILENCE_PEAK, MeasureConfig, Tracker, Turn, TurnResult,
    is_audible, whole_micros,
};
use crate::BenchError;

/// Messages the reader may hold for the call (overflow policy: the reader waits).
pub const INBOUND_CAPACITY: usize = 4096;

/// Marks the client remembers before it gives up the oldest.
pub const MAX_PENDING_MARKS: usize = 256;

/// Server `error` messages a call keeps verbatim; later ones are only counted (a server that
/// floods errors must not grow the report without bound).
pub const MAX_SERVER_ERRORS: usize = 64;

/// Settings of one call.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Server URL, `ws://host:port/path` or `wss://` (TLS: the shared rustls config, no ALPN).
    /// A secret because live URLs may carry a token in the query: it never appears in `Debug`,
    /// errors or reports.
    pub url: Secret<String>,
    /// Duration of one audio frame sent to the server.
    pub frame: Duration,
    /// Peak amplitude below which a bot frame is silence ([`DEFAULT_SILENCE_PEAK`]).
    pub silence_peak: u16,
    /// See [`MeasureConfig::overlap_guard`].
    pub overlap_guard: Duration,
    /// Longest wait for the TCP connection and the websocket upgrade.
    pub connect_timeout: Duration,
    /// Longest wait for the server's answer to `start`, for any write, and for the goodbye.
    pub io_timeout: Duration,
    /// How long after the end of the fixture the last turn may still be answered. Meanwhile the
    /// client keeps sending digital silence at the frame cadence, as an open phone line does, so
    /// a server that ends a turn on received silence can end the last one. The call ends earlier
    /// as soon as audible bot audio has arrived after the last turn's end of speech.
    pub response_timeout: Duration,
    /// A fixture frame sent more than this after its deadline counts in
    /// [`CallOutcome::late_frames`] (the capacity benchmark judges a generator by how many
    /// frames were late, not by the single latest one).
    pub late_threshold: Duration,
    /// Free-form metadata for `start`.
    pub metadata: BTreeMap<String, String>,
    /// Counted up while a call is open (from the server's `started` until the call ends), for a
    /// load generator that shows how many calls the server really holds.
    pub live: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl ClientConfig {
    /// A configuration for `url` with 20 ms frames, 5 s connect and I/O timeouts, a 10 s
    /// response timeout and the default silence threshold and overlap guard.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: Secret::new(url.into()),
            frame: Duration::from_millis(20),
            silence_peak: DEFAULT_SILENCE_PEAK,
            overlap_guard: DEFAULT_OVERLAP_GUARD,
            connect_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(10),
            late_threshold: Duration::from_millis(100),
            metadata: BTreeMap::new(),
            live: None,
        }
    }
}

/// What one call measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutcome {
    /// The server's id for the call.
    pub call_id: String,
    /// One result per fixture turn (also when the call failed: the turns it did not get to are
    /// `NoResponse`; the caller must check [`failure`](Self::failure) first).
    pub turns: Vec<TurnResult>,
    /// Audio frames sent: the fixture's, then the trailing silence sent while waiting for the
    /// last reply.
    pub frames_sent: u64,
    /// Binary frames received.
    pub audio_frames_received: u64,
    /// Received frames that were not silent.
    pub audible_frames: u64,
    /// `clear` messages received (barge-ins the server performed).
    pub clears: usize,
    /// The `error` messages the server sent, as `code: message`: the first
    /// [`MAX_SERVER_ERRORS`], then one line counting the rest.
    pub server_errors: Vec<String>,
    /// The largest amount by which a frame went out after its deadline.
    pub max_send_lateness: Duration,
    /// Fixture frames sent (the paced frames the lateness is measured on, not the trailing
    /// silence).
    pub fixture_frames_sent: u64,
    /// Of those, the frames that went out more than [`ClientConfig::late_threshold`] late.
    pub late_frames: u64,
    /// Why the measurement is not valid: the connection broke, the server ended or failed the
    /// call before all input was sent, a write timed out, the run was cancelled.
    pub failure: Option<String>,
}

/// A timeout of [`within`] elapsed.
struct Elapsed;

/// Runs `future`, giving up after `after` on `clock` (the future is dropped when the deadline
/// wins).
async fn within<F: Future>(
    clock: &dyn Clock,
    after: Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    tokio::select! {
        biased;
        output = future => Ok(output),
        () = clock.sleep(after) => Err(Elapsed),
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<Socket, Message>;
type Source = SplitStream<Socket>;

/// What the reader task hands to the call. Every item carries the arrival time.
enum Inbound {
    Audio {
        at: Instant,
        data: Bytes,
    },
    Mark {
        at: Instant,
        id: String,
    },
    Clear {
        at: Instant,
    },
    Error {
        code: String,
        message: String,
        fatal: bool,
    },
    Stop {
        reason: String,
    },
    /// The connection is over; the reader has ended. `clean` means a close frame.
    Ended {
        clean: bool,
        detail: String,
    },
}

/// Reads the socket until it ends or `cancel` fires, stamping each message on arrival.
async fn read_loop(
    clock: SharedClock,
    mut source: Source,
    inbound: mpsc::Sender<Inbound>,
    cancel: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            next = source.next() => next,
        };
        let at = clock.now();
        let (item, last) = match next {
            None => (
                Some(Inbound::Ended {
                    clean: false,
                    detail: "the connection ended without a close frame".to_owned(),
                }),
                true,
            ),
            Some(Err(error)) => (
                Some(Inbound::Ended {
                    clean: false,
                    detail: error.to_string(),
                }),
                true,
            ),
            Some(Ok(Message::Close(_))) => (
                Some(Inbound::Ended {
                    clean: true,
                    detail: "closed by the server".to_owned(),
                }),
                true,
            ),
            Some(Ok(Message::Binary(data))) => (Some(Inbound::Audio { at, data }), false),
            Some(Ok(Message::Text(text))) => (
                match ServerMessage::parse(text.as_str()) {
                    Ok(ServerMessage::Mark { id }) => Some(Inbound::Mark { at, id }),
                    Ok(ServerMessage::Clear { .. }) => Some(Inbound::Clear { at }),
                    Ok(ServerMessage::Stop { reason }) => Some(Inbound::Stop { reason }),
                    Ok(ServerMessage::Error {
                        code,
                        message,
                        fatal,
                    }) => Some(Inbound::Error {
                        code,
                        message,
                        fatal,
                    }),
                    Ok(ServerMessage::Started { .. }) | Err(_) => None,
                },
                false,
            ),
            Some(Ok(_)) => (None, false),
        };
        if let Some(item) = item {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                sent = inbound.send(item) => if sent.is_err() { return },
            }
        }
        if last {
            return;
        }
    }
}

/// Everything the call learns from the server's messages.
struct Log {
    begin: Instant,
    rate: u64,
    silence_peak: u16,
    last_eos: Duration,
    /// The per-turn measurement, fed with each audible arrival as it comes (fixed size: an
    /// arrival list would grow with whatever the server sends).
    tracker: Tracker,
    audio_frames: u64,
    audible_frames: u64,
    clears: usize,
    errors: Vec<String>,
    /// Errors past [`MAX_SERVER_ERRORS`], counted only.
    errors_dropped: usize,
    /// Audible audio has arrived after the last turn's end of speech.
    answered_last: bool,
    /// Where the bot's audio would be playing until.
    playout_end: Instant,
    pending_marks: VecDeque<(Instant, String)>,
    stop_reason: Option<String>,
    fatal_error: Option<String>,
    /// The connection ended: `(clean, detail)`.
    ended: Option<(bool, String)>,
}

impl Log {
    /// Applies one message. Returns whether the call is over.
    fn apply(&mut self, item: Inbound) -> bool {
        match item {
            Inbound::Audio { at, data } => {
                self.audio_frames = self.audio_frames.saturating_add(1);
                let samples = u64::try_from(data.len() / 2).unwrap_or(u64::MAX);
                let micros = samples.saturating_mul(1_000_000) / self.rate.max(1);
                self.playout_end =
                    deadline_after(self.playout_end.max(at), Duration::from_micros(micros));
                if is_audible(&data, self.silence_peak) {
                    self.audible_frames = self.audible_frames.saturating_add(1);
                    let offset = at.saturating_duration_since(self.begin);
                    self.tracker.observe(offset);
                    if offset > self.last_eos {
                        self.answered_last = true;
                    }
                }
                false
            }
            Inbound::Mark { at, id } => {
                if self.pending_marks.len() >= MAX_PENDING_MARKS {
                    self.pending_marks.pop_front();
                }
                self.pending_marks.push_back((self.playout_end.max(at), id));
                false
            }
            Inbound::Clear { at } => {
                self.clears = self.clears.saturating_add(1);
                self.playout_end = at;
                self.pending_marks.clear();
                false
            }
            Inbound::Error {
                code,
                message,
                fatal,
            } => {
                let text = format!("{code}: {message}");
                if fatal {
                    self.fatal_error = Some(text.clone());
                }
                if self.errors.len() < MAX_SERVER_ERRORS {
                    self.errors.push(text);
                } else {
                    self.errors_dropped = self.errors_dropped.saturating_add(1);
                }
                fatal
            }
            Inbound::Stop { reason } => {
                self.stop_reason = Some(reason);
                true
            }
            Inbound::Ended { clean, detail } => {
                self.ended = Some((clean, detail));
                true
            }
        }
    }
}

/// An error text with every trace of the URL removed.
fn scrub(config: &ClientConfig, text: impl std::fmt::Display) -> String {
    let text = text.to_string();
    let url = config.url.expose();
    if url.is_empty() {
        text
    } else {
        text.replace(url.as_str(), "<url>")
    }
}

fn call_error(config: &ClientConfig, what: &str, error: impl std::fmt::Display) -> BenchError {
    BenchError::Call(format!("{what}: {}", scrub(config, error)))
}

/// Counts one open call in [`ClientConfig::live`] until dropped.
struct LiveGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl LiveGuard {
    fn enter(live: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        live.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::sync::Arc::clone(live))
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Streams `fixture` to the server at `config.url` in real time and measures voice-to-voice
/// latency for each of its turns (timing model: the module docs).
///
/// A call that starts but does not end properly is returned as an outcome with
/// [`CallOutcome::failure`] set; its turn results must not be used.
///
/// # Errors
/// [`BenchError::Config`] for a fixture without speech or an unusable configuration;
/// [`BenchError::Call`] if the connection cannot be made, the server refuses `start`, or the
/// handshake times out or is cancelled.
#[allow(clippy::too_many_lines)] // one linear call loop reads better than split-up state
pub async fn run_call(
    clock: SharedClock,
    config: &ClientConfig,
    fixture: &Fixture,
    cancel: &CancellationToken,
) -> Result<CallOutcome, BenchError> {
    let rate = u64::from(fixture.sample_rate.hz());
    let frame_samples = (rate.saturating_mul(u64::try_from(config.frame.as_micros()).unwrap_or(0))
        / 1_000_000)
        .max(1);
    let Some(last_segment) = fixture.segments.last() else {
        return Err(BenchError::Config(format!(
            "fixture {} has no speech segments",
            fixture.name
        )));
    };
    let turns: Vec<Turn> = fixture
        .segments
        .iter()
        .enumerate()
        .map(|(index, segment)| Turn {
            index,
            lang: segment.lang.clone(),
            speech_start: fixture.time_of(segment.start),
            eos: fixture.time_of(segment.end),
        })
        .collect();
    let last_eos = fixture.time_of(last_segment.end);
    let cancelled = || BenchError::Call("cancelled".to_owned());

    // Connect and open the call.
    let url = config.url.expose().as_str();
    let connector = if url.starts_with("wss://") {
        let tls = brisvox_providers::net::tls_config()
            .map_err(|e| BenchError::Call(format!("tls: {e}")))?;
        Some(Connector::Rustls(tls))
    } else {
        None
    };
    let connect = connect_async_tls_with_config(url, None, true, connector);
    let (mut ws, _response) = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(cancelled()),
        connected = within(&*clock, config.connect_timeout, connect) => match connected {
            Err(Elapsed) => return Err(BenchError::Call("connect timed out".to_owned())),
            Ok(Err(error)) => return Err(call_error(config, "connect", error)),
            Ok(Ok(connected)) => connected,
        },
    };
    let mut start = Start::new(fixture.sample_rate.hz(), Encoding::Pcm16);
    start.metadata.clone_from(&config.metadata);
    match within(
        &*clock,
        config.io_timeout,
        ws.send(Message::text(ClientMessage::Start(start).to_json())),
    )
    .await
    {
        Err(Elapsed) => return Err(BenchError::Call("sending start timed out".to_owned())),
        Ok(Err(error)) => return Err(call_error(config, "sending start", error)),
        Ok(Ok(())) => {}
    }
    // One deadline for the whole wait: pings, pongs and unknown messages do not extend it.
    let started_deadline = deadline_after(clock.now(), config.io_timeout);
    let call_id = loop {
        let message = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(cancelled()),
            () = wait_until(&*clock, started_deadline) => {
                return Err(BenchError::Call("no answer to start".to_owned()));
            }
            message = ws.next() => message,
        };
        match message {
            Some(Ok(Message::Text(text))) => match ServerMessage::parse(text.as_str()) {
                Ok(ServerMessage::Started {
                    call_id,
                    sample_rate,
                    encoding,
                    ..
                }) => {
                    if sample_rate != fixture.sample_rate.hz() || encoding != Encoding::Pcm16 {
                        return Err(BenchError::Call(
                            "the server changed the audio format in started".to_owned(),
                        ));
                    }
                    break call_id;
                }
                Ok(ServerMessage::Error { code, message, .. }) => {
                    return Err(BenchError::Call(format!(
                        "the server refused the call: {code}: {message}"
                    )));
                }
                Ok(_) | Err(ProtocolError::UnknownType(_)) => {}
                Err(error) => return Err(call_error(config, "protocol", error)),
            },
            Some(Ok(Message::Close(_))) | None => {
                return Err(BenchError::Call(
                    "closed before the call started".to_owned(),
                ));
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(call_error(config, "before started", error)),
        }
    };

    // The call is open: stream, and read on a task of its own.
    let _live = config.live.as_ref().map(LiveGuard::enter);
    let begin = clock.now();
    let (mut sink, source): (Sink, Source) = ws.split();
    let reader_cancel = cancel.child_token();
    let _reader_guard = reader_cancel.clone().drop_guard();
    let (inbound_tx, mut inbound) = mpsc::channel::<Inbound>(INBOUND_CAPACITY);
    let reader = tokio::spawn(read_loop(
        clock.clone(),
        source,
        inbound_tx,
        reader_cancel.clone(),
    ));

    let total_samples = u64::try_from(fixture.pcm.len() / 2).unwrap_or(u64::MAX);
    let frames = total_samples.div_ceil(frame_samples);
    let tail_deadline = deadline_after(begin, fixture.duration() + config.response_timeout);
    let hard_deadline = deadline_after(tail_deadline, config.io_timeout);
    let mut log = Log {
        begin,
        rate,
        silence_peak: config.silence_peak,
        last_eos,
        tracker: Tracker::new(
            turns,
            MeasureConfig {
                overlap_guard: config.overlap_guard,
            },
        ),
        audio_frames: 0,
        audible_frames: 0,
        clears: 0,
        errors: Vec::new(),
        errors_dropped: 0,
        answered_last: false,
        playout_end: begin,
        pending_marks: VecDeque::new(),
        stop_reason: None,
        fatal_error: None,
        ended: None,
    };
    // After the fixture the line stays open: digital silence at the same frame cadence, as a
    // phone keeps sending, so a server whose end of turn counts incoming silence still ends the
    // last turn.
    let silence = Bytes::from(vec![
        0u8;
        usize::try_from(frame_samples.saturating_mul(2))
            .unwrap_or(0)
    ]);
    let mut failure: Option<String> = None;
    let mut sent = 0u64;
    let mut padded = 0u64;
    let mut max_late = Duration::ZERO;
    let mut late_frames = 0u64;
    loop {
        let in_fixture = sent < frames;
        if !in_fixture && log.answered_last {
            break;
        }
        let end_sample = if in_fixture {
            sent.saturating_add(1)
                .saturating_mul(frame_samples)
                .min(total_samples)
        } else {
            padded
                .saturating_add(1)
                .saturating_mul(frame_samples)
                .saturating_add(total_samples)
        };
        let frame_due = deadline_after(begin, fixture.time_of(end_sample));
        let sending = in_fixture || frame_due <= tail_deadline;
        let mark_due = log.pending_marks.front().map(|(due, _)| *due);
        let have_mark = mark_due.is_some();
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                failure = Some("cancelled".to_owned());
                break;
            }
            () = wait_until(&*clock, hard_deadline) => {
                failure = Some("the call did not finish in time".to_owned());
                break;
            }
            item = inbound.recv() => {
                match item {
                    Some(item) => if log.apply(item) { break; },
                    None => {
                        log.ended.get_or_insert((false, "the reader stopped".to_owned()));
                        break;
                    }
                }
            }
            () = wait_until(&*clock, mark_due.unwrap_or(hard_deadline)), if have_mark => {
                if let Some((_, id)) = log.pending_marks.pop_front() {
                    let echo = Message::text(ClientMessage::Mark { id }.to_json());
                    match within(&*clock, config.io_timeout, sink.send(echo)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            failure = Some(format!("a write failed: {}", scrub(config, error)));
                            break;
                        }
                        Err(Elapsed) => {
                            failure = Some("a write timed out".to_owned());
                            break;
                        }
                    }
                }
            }
            () = wait_until(&*clock, tail_deadline), if !in_fixture => break,
            () = wait_until(&*clock, frame_due), if sending => {
                let chunk = if in_fixture {
                    // Only the fixture's frames carry ends of speech, so only their lateness is
                    // reported.
                    let late = clock.now().saturating_duration_since(frame_due);
                    max_late = max_late.max(late);
                    if late > config.late_threshold {
                        late_frames += 1;
                    }
                    let from = usize::try_from(sent.saturating_mul(frame_samples).saturating_mul(2)).unwrap_or(usize::MAX);
                    let to = usize::try_from(end_sample.saturating_mul(2)).unwrap_or(usize::MAX);
                    if from <= to && to <= fixture.pcm.len() {
                        fixture.pcm.slice(from..to)
                    } else {
                        Bytes::new()
                    }
                } else {
                    silence.clone()
                };
                match within(&*clock, config.io_timeout, sink.send(Message::Binary(chunk))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        failure = Some(format!("a write failed: {}", scrub(config, error)));
                        break;
                    }
                    Err(Elapsed) => {
                        failure = Some("a write timed out".to_owned());
                        break;
                    }
                }
                if in_fixture {
                    sent += 1;
                } else {
                    padded += 1;
                }
            }
        }
    }

    // Failure analysis, then a polite goodbye.
    if failure.is_none() {
        if let Some(error) = &log.fatal_error {
            failure = Some(format!("server error {error}"));
        } else if let Some((false, detail)) = &log.ended {
            failure = Some(format!("the connection broke: {}", scrub(config, detail)));
        } else if sent < frames && (log.stop_reason.is_some() || log.ended.is_some()) {
            failure = Some(format!(
                "the server ended the call before all input was sent (stop: {})",
                log.stop_reason.as_deref().unwrap_or("none")
            ));
        }
    }
    if log.ended.is_none() {
        if log.stop_reason.is_none() {
            let _ = within(
                &*clock,
                config.io_timeout,
                sink.send(Message::text(ClientMessage::Stop.to_json())),
            )
            .await;
        }
        let drain = async {
            while let Some(item) = inbound.recv().await {
                if matches!(item, Inbound::Ended { .. }) {
                    break;
                }
            }
        };
        if within(&*clock, config.io_timeout, drain).await.is_err() {
            let _ = within(&*clock, config.io_timeout, sink.close()).await;
        }
    }
    reader_cancel.cancel();
    let _ = reader.await;

    let results = log.tracker.results();
    if log.errors_dropped > 0 {
        log.errors
            .push(format!("... and {} more errors", log.errors_dropped));
    }
    Ok(CallOutcome {
        call_id,
        turns: results,
        frames_sent: sent.saturating_add(padded),
        audio_frames_received: log.audio_frames,
        audible_frames: log.audible_frames,
        clears: log.clears,
        server_errors: log.errors,
        max_send_lateness: whole_micros(max_late),
        fixture_frames_sent: sent,
        late_frames,
        failure,
    })
}

#[cfg(test)]
mod tests;
