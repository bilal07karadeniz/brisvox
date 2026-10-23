//! The session core shared by the local-speech STT adapters ([`crate::sherpa_onnx`],
//! [`crate::openai_realtime_stt`]): one task per session that owns the socket, the audio buffer
//! and the timers, while the adapter supplies only the wire protocol through [`Wire`].
//!
//! # What the core does (so the adapters do not)
//! - **Audio.** The engine's PCM16 frames are resampled to the rate the server wants (only when
//!   the rates differ; a no-op otherwise, the caller's `Bytes` pass through uncopied), kept in a
//!   bounded [`Ring`] and written to the socket from there. The ring is what a reconnect replays
//!   and what is held back while the protocol shuts the audio gate.
//! - **Finalize.** Each [`SttInput::Finalize`] becomes a queued request at its position in the
//!   audio. It is handed to the protocol ([`Wire::finalize`]) once all audio before it has been
//!   written, one at a time, in order. Every request is answered exactly once: by the protocol,
//!   or, when `finalize_timeout` (counted from the request, across reconnects) runs out, by the
//!   protocol's own timeout answer ([`Wire::finalize_timed_out`]), so the turn machine never
//!   waits for a server that will not answer.
//! - **Liveness.** After audio was written, the server must show a sign of life (any frame,
//!   a pong included) within `io_timeout`; half-way a WebSocket ping is sent so that a server
//!   that is silent by design (no per-chunk traffic) proves it is alive. Without a sign the
//!   connection is replaced like any lost one, and when no reconnect is left the session ends
//!   with `Core(Timeout)`. After `keepalive_interval` without audio a ping goes out and the
//!   wait is suspended until audio resumes.
//! - **Reconnect.** An unexpected drop opens a new connection (at most `max_reconnects`
//!   attempts per proof of health, exponential backoff on the injected clock), calls
//!   [`Wire::opened`] on it and replays the ring. Audio and finalize requests keep being
//!   accepted while reconnecting.
//! - **Rotation.** A protocol may declare the connection spent ([`Actions::rotate`]; sherpa-onnx
//!   after `Done`): the core opens a replacement at once, the same way.
//! - **Delivery.** Overflow policy of the events channel: an interim that finds it full is
//!   dropped (the next supersedes it); finals and errors wait, bounded by `io_timeout`, and a
//!   caller that stalls longer is treated as gone.
//! - **Hang-up.** Dropping the input (or the events receiver) ends the session: events close at
//!   once, then the socket is closed and drained for at most `close_timeout`. Nothing produced
//!   after the hang-up reaches the caller.

mod conn;
mod ring;
mod session;

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SharedClock;
use brisvox_core::frame::SampleRate;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

#[cfg(feature = "provider-openai-realtime-stt")]
pub(crate) use conn::percent_encode;
pub(crate) use conn::{Target, Ws, check_base_url, is_token, open, timed, warm};

use crate::error::{Error, Result};
use crate::stt::{STT_CHANNEL_CAPACITY, SttEvent, SttSession};

/// Timing and sizing settings of a session, copied out of an adapter's public config.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// Provider name in errors and logs.
    pub(crate) provider: &'static str,
    /// Label of the connect deadline's timeout error.
    pub(crate) connect_label: &'static str,
    /// Deadline of every network wait.
    pub(crate) io_timeout: Duration,
    /// Ping after this long without audio.
    pub(crate) keepalive_interval: Duration,
    /// How long a `Finalize` may stay unanswered.
    pub(crate) finalize_timeout: Duration,
    /// How long the close handshake may take on hang-up.
    pub(crate) close_timeout: Duration,
    /// Reconnect attempts per proof of health.
    pub(crate) max_reconnects: u32,
    /// Wait before the first reconnect attempt; doubles per attempt.
    pub(crate) reconnect_backoff: Duration,
    /// Audio kept for replay (and held back while the gate is shut).
    pub(crate) replay_window: Duration,
    /// Largest server message accepted.
    pub(crate) max_message_bytes: usize,
    /// Rate of the PCM16 audio [`Wire::audio`] receives.
    pub(crate) wire_rate: SampleRate,
}

impl Settings {
    /// Bytes of PCM16 audio per second at the wire rate.
    pub(crate) fn wire_bytes_per_second(&self) -> usize {
        usize::try_from(self.wire_rate.hz())
            .unwrap_or(0)
            .saturating_mul(2)
    }

    /// Bytes of wire audio in `span`.
    pub(crate) fn wire_bytes_in(&self, span: Duration) -> usize {
        let bytes = span.as_secs_f64() * self.wire_bytes_per_second() as f64;
        if bytes.is_finite() && bytes > 0.0 {
            bytes as usize
        } else {
            0
        }
    }
}

/// How much of the sent audio a protocol step settles.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trim {
    /// Keep everything.
    #[default]
    None,
    /// Everything up to the position of the `Finalize` being answered is settled.
    ToMark,
    /// All but the last `Duration` of sent audio is settled (a server-side endpoint).
    #[cfg_attr(not(feature = "provider-sherpa-onnx"), allow(dead_code))]
    KeepLast(Duration),
}

/// What a [`Wire`] method asks the core to do. The core owns one and reuses its buffers.
#[derive(Debug, Default)]
pub(crate) struct Actions {
    /// Messages to write, in order, before any more audio.
    pub(crate) send: Vec<Message>,
    /// Events for the caller, in order.
    pub(crate) events: Vec<SttEvent>,
    /// The oldest outstanding `Finalize` has been answered (the event is in `events`).
    pub(crate) answered: bool,
    /// Audio the answer settles.
    pub(crate) trim: Trim,
    /// Open (`true`) or shut (`false`) the audio gate: while shut, audio accumulates in the
    /// ring and is not written.
    pub(crate) gate: Option<bool>,
    /// The connection is spent: replace it now.
    pub(crate) rotate: bool,
    /// A transcript message proved the server works (resets the reconnect budget).
    pub(crate) healthy: bool,
}

impl Actions {
    /// Clears the flags and the lists, keeping the allocations.
    pub(crate) fn reset(&mut self) {
        self.send.clear();
        self.events.clear();
        self.answered = false;
        self.trim = Trim::None;
        self.gate = None;
        self.rotate = false;
        self.healthy = false;
    }

    /// Queues an interim transcript.
    pub(crate) fn interim(&mut self, text: &str) {
        self.events.push(SttEvent::Transcript {
            text: Arc::from(text),
            is_final: false,
            speech_final: false,
            from_finalize: false,
        });
    }

    /// Queues a final segment the server ended by itself (its own endpointing).
    pub(crate) fn endpoint_final(&mut self, text: &str) {
        self.events.push(SttEvent::Transcript {
            text: Arc::from(text),
            is_final: true,
            speech_final: true,
            from_finalize: false,
        });
    }

    /// Queues the answer to the oldest `Finalize`.
    pub(crate) fn answer(&mut self, text: &str) {
        self.events.push(finalize_answer(text));
        self.answered = true;
    }
}

/// The transcript that answers a `Finalize` (`text` may be empty).
pub(crate) fn finalize_answer(text: &str) -> SttEvent {
    SttEvent::Transcript {
        text: Arc::from(text),
        is_final: true,
        speech_final: true,
        from_finalize: true,
    }
}

/// What a close frame (or the end of the stream) means to the protocol.
#[derive(Debug)]
pub(crate) enum Closed {
    /// A drop worth a reconnect.
    Lost,
    /// A failure a retry would repeat.
    Fatal(Error),
}

/// The default meaning of a close: `1008` (policy: bad credentials or audio) is final, anything
/// else is a drop.
pub(crate) fn default_closed(provider: &'static str, code: Option<CloseCode>) -> Closed {
    if code == Some(CloseCode::Policy) {
        Closed::Fatal(Error::Rejected {
            provider,
            status: u16::from(CloseCode::Policy),
        })
    } else {
        Closed::Lost
    }
}

/// The wire protocol of one adapter. All methods are synchronous and cheap: the core does the I/O.
pub(crate) trait Wire: Send + 'static {
    /// A connection was opened (the first or a replacement): reset per-connection state and
    /// queue the setup messages. They go out before any audio, which is replayed afterwards.
    fn opened(&mut self, act: &mut Actions);

    /// Encodes one chunk of PCM16 LE mono audio at the wire rate into the message that carries
    /// it.
    ///
    /// # Errors
    /// [`Error::Protocol`] when the chunk cannot be encoded.
    fn audio(&mut self, pcm: &[u8]) -> Result<Message>;

    /// The oldest outstanding `Finalize` is due: all audio before it has been written. Answer it
    /// now (`act.answer`) when there is nothing to flush, or send what asks the server to flush
    /// and answer later from [`Wire::text`] / [`Wire::closed`].
    fn finalize(&mut self, act: &mut Actions);

    /// One text message from the server.
    ///
    /// # Errors
    /// [`Error::Protocol`] for a message that violates the protocol (the session ends).
    fn text(&mut self, text: &str, act: &mut Actions) -> Result<()>;

    /// The server closed the connection (`code`), or the stream ended (`None`). A protocol whose
    /// server closes after its answer produces the answer here.
    fn closed(&mut self, code: Option<CloseCode>, act: &mut Actions) -> Closed;

    /// The outstanding `Finalize` is out of time: answer it (`act.answer`) with what is known
    /// and forget it, so that a late answer from the server is recognised and dropped.
    fn finalize_timed_out(&mut self, act: &mut Actions);

    /// Messages to send when the caller hangs up (before the close handshake).
    fn goodbye(&mut self) -> Vec<Message> {
        Vec::new()
    }

    /// The server closes the connection itself after the goodbye: on hang-up the session waits
    /// for that close instead of starting the handshake (a server that is still answering when a
    /// client's close arrives reports an error).
    fn server_closes_after_goodbye(&self) -> bool {
        false
    }
}

/// Opens the first connection of a session and starts its task.
///
/// # Errors
/// The connect errors of [`open`]; `Protocol` if the resampler cannot be built.
pub(crate) async fn start<W: Wire>(
    settings: Arc<Settings>,
    clock: SharedClock,
    target: Target,
    wire: W,
    input_rate: SampleRate,
) -> Result<SttSession> {
    let resampler =
        session::Resample::new(input_rate, settings.wire_rate, settings.provider).await?;
    let ws = open(&settings, &*clock, &target).await?;
    let (input_tx, input_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(STT_CHANNEL_CAPACITY);
    let params = session::Params {
        settings,
        clock,
        target,
        wire,
        resampler,
    };
    // Ends when the caller drops the input sender or the events receiver (see `session`).
    tokio::spawn(session::run(params, ws, input_rx, event_tx));
    Ok(SttSession {
        input: input_tx,
        events: event_rx,
    })
}
