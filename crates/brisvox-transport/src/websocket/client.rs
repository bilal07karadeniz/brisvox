//! The reference websocket client: streams PCM16 audio to a server in real-time frames,
//! answers marks and records the bot audio it would have played.
//!
//! The client models a phone's speaker. Audio the server sends plays back to back from the moment
//! it arrives (or from the end of what is already playing); a `mark` is echoed when everything
//! before it has finished playing; a `clear` throws away what has not played yet, both from the
//! recording and from the pending marks. The recording is the concatenation of what played, with
//! no silence inserted between bursts.
//!
//! The run ends when the server stops the call, closes the connection, or has been silent for
//! [`ClientConfig::linger`] after the input finished (pending marks are still echoed first). The
//! `ws_client` binary wraps [`run`]. A run whose call did not end properly (the connection broke,
//! the server closed it without `stop` before all input was sent, a fatal server error, the
//! maximum duration) still returns its [`ClientReport`]; [`ClientReport::failure`] says why it is
//! not a success.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use brisvox_audio::Codec;
use brisvox_core::SharedClock;
use brisvox_core::clock::{deadline_after, wait_until};
use bytes::Bytes;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::protocol::{ClientMessage, Encoding, ProtocolError, ServerMessage, Start};
use super::timeout;
use crate::error::{Error, Result};

/// Settings of one client run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// Server URL, `ws://host:port/path` (or `wss://`).
    pub url: String,
    /// Sample rate of the input audio and of the call.
    pub sample_rate: u32,
    /// Encoding of the call's audio frames.
    pub encoding: Encoding,
    /// Duration of one audio frame sent to the server.
    pub frame: Duration,
    /// Send frames at real-time speed (one per `frame`). Off: as fast as the server takes them.
    pub realtime: bool,
    /// After the input has been sent, how long the server may stay silent before the client hangs
    /// up.
    pub linger: Duration,
    /// Longest wait for the connection and the websocket upgrade.
    pub connect_timeout: Duration,
    /// Longest wait for any single read of the server's answer to `start`, and for any write.
    pub io_timeout: Duration,
    /// Upper bound on the whole call, from `start` on.
    pub max_duration: Duration,
    /// Free-form metadata sent in `start`.
    pub metadata: BTreeMap<String, String>,
    /// DTMF keys sent right after the call started.
    pub dtmf: String,
}

impl ClientConfig {
    /// A configuration for `url` with 16 kHz PCM16, 20 ms real-time frames and a 2 s linger.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            sample_rate: 16_000,
            encoding: Encoding::Pcm16,
            frame: Duration::from_millis(20),
            realtime: true,
            linger: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(5),
            max_duration: Duration::from_secs(600),
            metadata: BTreeMap::new(),
            dtmf: String::new(),
        }
    }
}

/// An `error` message the server sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerError {
    /// The error code.
    pub code: String,
    /// The description.
    pub message: String,
    /// Whether the server closed the connection because of it.
    pub fatal: bool,
}

/// What a client run saw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientReport {
    /// The server's id for the call.
    pub call_id: String,
    /// PCM16 LE mono audio the client played (decoded if the call used mulaw), at the call's rate.
    pub audio: Vec<u8>,
    /// Audio frames sent.
    pub frames_sent: u64,
    /// Binary frames received.
    pub audio_frames_received: u64,
    /// Marks received, in order.
    pub marks_received: Vec<String>,
    /// Marks echoed back (the rest were cut by a clear or the call ended first).
    pub marks_echoed: Vec<String>,
    /// Epochs of the clears received.
    pub clears: Vec<u32>,
    /// DTMF keys sent.
    pub dtmf_sent: usize,
    /// The `reason` of the server's `stop`, if it sent one.
    pub stop_reason: Option<String>,
    /// Every `error` the server sent.
    pub errors: Vec<ServerError>,
    /// Whether the connection was already closed by the server when the run ended.
    pub server_closed: bool,
    /// Whether every input frame was sent.
    pub input_complete: bool,
    /// Why the connection broke, if it broke (a read or write error, or the end of the stream
    /// without a close frame) before the server stopped the call.
    pub transport_error: Option<String>,
    /// Whether the run ended because [`ClientConfig::max_duration`] passed.
    pub timed_out: bool,
}

impl ClientReport {
    /// Why this run is not a success, or `None` if the call went as intended: the server did not
    /// report a fatal error, the connection did not break, the maximum duration was not hit, and
    /// the server did not close the call (without a `stop`) before all input was sent. A `stop`
    /// from the server, even while input was still being sent (the bot hung up), is not a failure.
    pub fn failure(&self) -> Option<String> {
        if let Some(error) = self.errors.iter().find(|e| e.fatal) {
            return Some(format!("server error {}: {}", error.code, error.message));
        }
        if let Some(error) = &self.transport_error {
            return Some(format!("the connection broke: {error}"));
        }
        if self.timed_out {
            return Some("the call exceeded the maximum duration".to_owned());
        }
        if self.server_closed && self.stop_reason.is_none() && !self.input_complete {
            return Some("the server closed the call before all input was sent".to_owned());
        }
        None
    }

    /// Notes that the connection is gone: `error` is the first cause kept.
    fn lose_connection(&mut self, error: impl std::fmt::Display) {
        self.server_closed = true;
        if self.transport_error.is_none() {
            self.transport_error = Some(error.to_string());
        }
    }
}

/// Bot audio the client holds and when it plays.
struct Playout {
    rate: u32,
    /// When everything received so far has finished playing.
    end: Instant,
}

impl Playout {
    /// Registers `pcm_bytes` of audio received at `now`; returns nothing, the audio plays from
    /// `max(end, now)`.
    fn add(&mut self, now: Instant, pcm_bytes: usize) {
        let samples = u64::try_from(pcm_bytes / 2).unwrap_or(u64::MAX);
        let micros = samples.saturating_mul(1_000_000) / u64::from(self.rate.max(1));
        self.end = self.end.max(now) + Duration::from_micros(micros);
    }

    /// PCM bytes still unplayed at `now`.
    fn unplayed_bytes(&self, now: Instant) -> usize {
        let left = self.end.saturating_duration_since(now).as_micros();
        let samples = left.saturating_mul(u128::from(self.rate)) / 1_000_000;
        usize::try_from(samples.saturating_mul(2)).unwrap_or(usize::MAX)
    }

    /// When a mark received now is due.
    fn mark_due(&self, now: Instant) -> Instant {
        self.end.max(now)
    }
}

fn ws_error(error: impl std::fmt::Display) -> Error {
    Error::WebSocket(error.to_string())
}

fn timed_out(what: &'static str, after: Duration) -> Error {
    brisvox_core::Error::Timeout { what, after }.into()
}

/// Streams `input` (mono PCM16 LE at `config.sample_rate`) to the server and records what it
/// plays back.
///
/// # Errors
/// [`Error::WebSocket`] if the connection cannot be made or breaks before the call started,
/// [`Error::Remote`] if the server refuses `start`, [`Error::Protocol`] for a server that
/// breaks the protocol before the call started, [`Error::Core`] for a timeout or cancellation.
/// Problems after the call started are reported in [`ClientReport`], not as errors.
#[allow(clippy::too_many_lines)] // one linear session loop reads better than split-up state
pub async fn run(
    clock: SharedClock,
    config: &ClientConfig,
    input: &[u8],
    cancel: &CancellationToken,
) -> Result<ClientReport> {
    let connect = tokio_tungstenite::connect_async(config.url.as_str());
    let (mut ws, _response) = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(brisvox_core::Error::Cancelled.into()),
        connected = timeout(&*clock, config.connect_timeout, connect) => connected
            .map_err(|_| timed_out("websocket connect", config.connect_timeout))?
            .map_err(ws_error)?,
    };

    let mut start = Start::new(config.sample_rate, config.encoding);
    start.metadata.clone_from(&config.metadata);
    send(
        &*clock,
        &mut ws,
        Message::text(ClientMessage::Start(start).to_json()),
        config,
    )
    .await?;

    let mut report = ClientReport::default();
    // The server answers `start` with `started` (or a fatal error).
    loop {
        let message = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(brisvox_core::Error::Cancelled.into()),
            message = timeout(&*clock, config.io_timeout, ws.next()) => message
                .map_err(|_| timed_out("websocket start answer", config.io_timeout))?,
        };
        match message {
            Some(Ok(Message::Text(text))) => match ServerMessage::parse(text.as_str()) {
                Ok(ServerMessage::Started {
                    call_id,
                    sample_rate,
                    encoding,
                    ..
                }) => {
                    if sample_rate != config.sample_rate || encoding != config.encoding {
                        return Err(Error::Protocol(
                            "the server changed the audio format in started".to_owned(),
                        ));
                    }
                    report.call_id = call_id;
                    break;
                }
                Ok(ServerMessage::Error { code, message, .. }) => {
                    return Err(Error::Remote { code, message });
                }
                Ok(_) | Err(ProtocolError::UnknownType(_)) => {}
                Err(error) => return Err(Error::Protocol(error.to_string())),
            },
            Some(Ok(Message::Close(_))) | None => {
                return Err(Error::WebSocket(
                    "closed before the call started".to_owned(),
                ));
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(ws_error(error)),
        }
    }

    for digit in config.dtmf.chars() {
        let Ok(digit) = brisvox_core::frame::DtmfDigit::try_from(digit) else {
            continue;
        };
        let message = ClientMessage::Dtmf { digit }.to_json();
        send(&*clock, &mut ws, Message::text(message), config).await?;
        report.dtmf_sent += 1;
    }

    // Input, as one buffer sliced into frames.
    let samples_per_frame = usize::try_from(
        (u128::from(config.sample_rate) * config.frame.as_micros() / 1_000_000).max(1),
    )
    .unwrap_or(1);
    let frame_bytes = samples_per_frame.saturating_mul(2);
    let input = Bytes::copy_from_slice(input.get(..input.len() & !1).unwrap_or_default());
    let mut offset = 0usize;

    let begin = clock.now();
    let hard_stop = deadline_after(begin, config.max_duration);
    let mut next_frame = begin;
    let mut input_done_at: Option<Instant> = input.is_empty().then_some(begin);
    report.input_complete = input.is_empty();
    let mut call = Call {
        config,
        report,
        playout: Playout {
            rate: config.sample_rate,
            end: begin,
        },
        pending: VecDeque::new(),
        last_rx: begin,
    };
    // From here on the two halves are used side by side: a write that cannot complete (the
    // server is not reading because it is blocked writing to us) must not stop the reads.
    let (mut sink, mut source) = ws.split();
    // After every sent frame the loop looks at the socket once before it sends the next one. The
    // `select!` is biased, and in non-realtime mode the next frame is always due, so without this
    // the receive side would starve until the whole input was out: a server that is already
    // producing audio would fill the client's receive buffer and close the call as `slow_client`.
    let mut read_turn = false;

    loop {
        let input_left = offset < input.len();
        let sending = input_left && !read_turn;
        let mark_due = call.pending.front().map_or(hard_stop, |(due, _)| *due);
        let linger_due = input_done_at.map_or(hard_stop, |done| {
            deadline_after(done.max(call.last_rx), config.linger)
        });
        let have_marks = !call.pending.is_empty();
        let lingering = input_done_at.is_some();
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = wait_until(&*clock, hard_stop) => {
                call.report.timed_out = true;
                break;
            }
            () = wait_until(&*clock, mark_due), if have_marks => {
                if let Some((_, id)) = call.pending.pop_front() {
                    let echo = Message::text(ClientMessage::Mark { id: id.clone() }.to_json());
                    match send_reading(&*clock, &mut sink, &mut source, &mut call, echo).await {
                        Sent::Done => call.report.marks_echoed.push(id),
                        Sent::CallEnded => break,
                        Sent::Failed(error) => {
                            call.report.lose_connection(error);
                            break;
                        }
                    }
                }
            }
            () = wait_until(&*clock, next_frame), if sending => {
                read_turn = true;
                let end = offset.saturating_add(frame_bytes).min(input.len());
                let chunk = input.slice(offset..end);
                offset = end;
                let payload = match config.encoding {
                    Encoding::Pcm16 => chunk,
                    Encoding::Mulaw => {
                        let mut out = vec![0u8; Codec::Ulaw.encoded_len(chunk.len())];
                        if let Err(error) = Codec::Ulaw.encode(&chunk, &mut out) {
                            call.report.lose_connection(format!("cannot encode audio: {error}"));
                            break;
                        }
                        Bytes::from(out)
                    }
                };
                match send_reading(&*clock, &mut sink, &mut source, &mut call, Message::Binary(payload)).await {
                    Sent::Done => {}
                    Sent::CallEnded => break,
                    Sent::Failed(error) => {
                        call.report.lose_connection(error);
                        break;
                    }
                }
                call.report.frames_sent += 1;
                next_frame = if config.realtime {
                    next_frame + config.frame
                } else {
                    clock.now()
                };
                if offset >= input.len() {
                    call.report.input_complete = true;
                    input_done_at = Some(clock.now());
                    call.last_rx = clock.now();
                }
            }
            // Pending marks are echoed first: their audio is still playing, and an echo cut off by
            // the linger would be lost. They are bounded by the audio they follow and `hard_stop`.
            () = wait_until(&*clock, linger_due), if lingering && !input_left && !have_marks => break,
            message = source.next() => {
                read_turn = false;
                if matches!(call.on_server(message, clock.now()), Inbound::End) {
                    break;
                }
            }
            // The read turn found nothing on the socket (it is polled before this branch): the
            // next iteration sends again.
            () = std::future::ready(()), if read_turn => read_turn = false,
        }
    }

    let mut report = call.report;
    finish(&*clock, &mut sink, &mut source, config, &mut report).await;
    Ok(report)
}

/// The two halves of the client's websocket.
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Sink = SplitSink<Socket, Message>;
type Source = SplitStream<Socket>;

/// What the client knows about the call while it runs: the one place where messages from the
/// server are applied, whether they arrive between two sends or while a send is blocked.
struct Call<'a> {
    config: &'a ClientConfig,
    report: ClientReport,
    playout: Playout,
    /// Marks received and not yet echoed, with the time their audio has finished playing.
    pending: VecDeque<(Instant, String)>,
    /// When the server last sent audio or a mark or a clear.
    last_rx: Instant,
}

/// What a message from the server means for the run.
enum Inbound {
    Continue,
    /// The call is over (the server stopped it, closed the connection, reported a fatal error or
    /// the connection broke): leave the main loop.
    End,
}

impl Call<'_> {
    /// Applies one item of the server's stream, received at `now`.
    fn on_server(
        &mut self,
        message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
        now: Instant,
    ) -> Inbound {
        let report = &mut self.report;
        match message {
            None => {
                report.lose_connection("the connection ended without a close frame");
                Inbound::End
            }
            Some(Err(error)) => {
                report.lose_connection(error);
                Inbound::End
            }
            Some(Ok(Message::Close(_))) => {
                report.server_closed = true;
                Inbound::End
            }
            Some(Ok(Message::Binary(data))) => {
                let pcm = match self.config.encoding {
                    Encoding::Pcm16 => data.to_vec(),
                    Encoding::Mulaw => {
                        let mut out = vec![0u8; Codec::Ulaw.decoded_len(data.len())];
                        if let Err(error) = Codec::Ulaw.decode(&data, &mut out) {
                            report.errors.push(ServerError {
                                code: "bad_audio".to_owned(),
                                message: format!("undecodable audio frame: {error}"),
                                fatal: false,
                            });
                            return Inbound::Continue;
                        }
                        out
                    }
                };
                self.playout.add(now, pcm.len());
                report.audio.extend_from_slice(&pcm);
                report.audio_frames_received += 1;
                self.last_rx = now;
                Inbound::Continue
            }
            Some(Ok(Message::Text(text))) => match ServerMessage::parse(text.as_str()) {
                Ok(ServerMessage::Mark { id }) => {
                    self.pending
                        .push_back((self.playout.mark_due(now), id.clone()));
                    report.marks_received.push(id);
                    self.last_rx = now;
                    Inbound::Continue
                }
                Ok(ServerMessage::Clear { epoch }) => {
                    let cut = self.playout.unplayed_bytes(now).min(report.audio.len());
                    report.audio.truncate(report.audio.len() - cut);
                    self.playout.end = now;
                    self.pending.clear();
                    report.clears.push(epoch);
                    self.last_rx = now;
                    Inbound::Continue
                }
                Ok(ServerMessage::Stop { reason }) => {
                    report.stop_reason = Some(reason);
                    Inbound::End
                }
                Ok(ServerMessage::Error {
                    code,
                    message,
                    fatal,
                }) => {
                    report.errors.push(ServerError {
                        code,
                        message,
                        fatal,
                    });
                    if fatal {
                        Inbound::End
                    } else {
                        Inbound::Continue
                    }
                }
                Ok(ServerMessage::Started { .. }) | Err(ProtocolError::UnknownType(_)) => {
                    Inbound::Continue
                }
                Err(error) => {
                    report.errors.push(ServerError {
                        code: error.code().to_owned(),
                        message: error.to_string(),
                        fatal: false,
                    });
                    Inbound::Continue
                }
            },
            Some(Ok(_)) => Inbound::Continue,
        }
    }
}

/// How a send ended.
enum Sent {
    Done,
    /// While the send was blocked the server ended the call (see [`Inbound::End`]); the report
    /// says how.
    CallEnded,
    Failed(Error),
}

/// Sends one message, bounded by the client's write timeout, and keeps reading the server's
/// stream while the write cannot complete. A server that is blocked writing to us does not read
/// our input (its reader is not stuck, but TCP's window closes when we do not read either), so a
/// client that only waited on its write would deadlock with it until the timeout.
async fn send_reading(
    clock: &dyn brisvox_core::Clock,
    sink: &mut Sink,
    source: &mut Source,
    call: &mut Call<'_>,
    message: Message,
) -> Sent {
    let limit = call.config.io_timeout;
    let write = timeout(clock, limit, sink.send(message));
    tokio::pin!(write);
    loop {
        tokio::select! {
            biased;
            written = &mut write => {
                return match written {
                    Ok(Ok(())) => Sent::Done,
                    Ok(Err(error)) => Sent::Failed(ws_error(error)),
                    Err(_) => Sent::Failed(timed_out("websocket write", limit)),
                };
            }
            message = source.next() => {
                if matches!(call.on_server(message, clock.now()), Inbound::End) {
                    return Sent::CallEnded;
                }
            }
        }
    }
}

/// Hangs up politely: `stop` (unless the server already stopped the call), then a bounded wait
/// for the server to say `stop` and close its side. The client closes first only if the server
/// does not answer in time: a close frame sent before the server's `stop` would keep the server
/// from sending it.
async fn finish(
    clock: &dyn brisvox_core::Clock,
    sink: &mut Sink,
    source: &mut Source,
    config: &ClientConfig,
    report: &mut ClientReport,
) {
    let wind_down = async {
        if !report.server_closed && report.stop_reason.is_none() {
            let _ = sink
                .send(Message::text(ClientMessage::Stop.to_json()))
                .await;
        }
        // Read what the server still sends (its stop, its close) until the connection ends.
        while let Some(Ok(message)) = source.next().await {
            if let Message::Text(text) = message {
                match ServerMessage::parse(text.as_str()) {
                    Ok(ServerMessage::Stop { reason }) if report.stop_reason.is_none() => {
                        report.stop_reason = Some(reason);
                    }
                    Ok(ServerMessage::Error {
                        code,
                        message,
                        fatal,
                    }) => report.errors.push(ServerError {
                        code,
                        message,
                        fatal,
                    }),
                    _ => {}
                }
            }
        }
    };
    if timeout(clock, config.io_timeout, wind_down).await.is_err() {
        let _ = timeout(clock, config.io_timeout, sink.close()).await;
    }
}

/// Sends one message, bounded by the client's write timeout.
async fn send(
    clock: &dyn brisvox_core::Clock,
    ws: &mut Socket,
    message: Message,
    config: &ClientConfig,
) -> Result<()> {
    timeout(clock, config.io_timeout, ws.send(message))
        .await
        .map_err(|_| timed_out("websocket write", config.io_timeout))?
        .map_err(ws_error)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // real instants are fine in a pure model test
mod tests {
    use super::*;

    #[test]
    fn playout_tracks_what_has_not_played_yet() {
        let start = Instant::now();
        let mut playout = Playout {
            rate: 16_000,
            end: start,
        };
        // One second of audio arrives at once: all of it is still to play.
        playout.add(start, 32_000);
        assert_eq!(playout.unplayed_bytes(start), 32_000);
        assert_eq!(playout.mark_due(start), start + Duration::from_secs(1));
        // A quarter second later, three quarters are left.
        let later = start + Duration::from_millis(250);
        assert_eq!(playout.unplayed_bytes(later), 24_000);
        // More audio queues behind it.
        playout.add(later, 16_000);
        assert_eq!(
            playout.mark_due(later),
            start + Duration::from_millis(1_500)
        );
        // Everything has played: nothing is left, and new audio starts when it arrives.
        let idle = start + Duration::from_secs(5);
        assert_eq!(playout.unplayed_bytes(idle), 0);
        playout.add(idle, 3_200);
        assert_eq!(playout.mark_due(idle), idle + Duration::from_millis(100));
    }
}
