//! The session task: pumps the engine's audio and control channels into the socket and the
//! socket's events into the engine.

use std::collections::VecDeque;
use std::sync::Arc;

use brisvox_core::SharedClock;
use bytes::{Bytes, BytesMut};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

use super::config::OpenAiRealtimeConfig;
use super::wire::{self, Decoded, Decoder, Sent};
use super::{PROVIDER, Ws, timed, ws_error};
use crate::error::Error;
use crate::s2s::{S2sControl, S2sEvent};

/// Client events whose errors are remembered to tell expected races from real problems.
const RACY_CAP: usize = 32;

/// What the task needs besides its channels.
pub(super) struct Params {
    pub(super) cfg: Arc<OpenAiRealtimeConfig>,
    pub(super) clock: SharedClock,
}

/// How the session ended.
enum End {
    /// The engine closed its side.
    Closed,
    /// Something went wrong; the engine is told.
    Failed(Error),
}

struct Session {
    params: Params,
    sink: SplitSink<Ws, Message>,
    events: mpsc::Sender<S2sEvent>,
    decoder: Decoder,
    next_event: u64,
    /// `event_id`s of client events whose errors are expected races.
    racy: VecDeque<String>,
    /// Reused buffer of audio-append messages.
    scratch: BytesMut,
}

/// Runs until the engine drops its senders, the socket closes or something fails. The last thing
/// sent to the engine on a failure is an [`S2sEvent::Error`].
pub(super) async fn run(
    params: Params,
    ws: Ws,
    mut audio: mpsc::Receiver<Bytes>,
    mut control: mpsc::Receiver<S2sControl>,
    events: mpsc::Sender<S2sEvent>,
) {
    let (sink, mut stream) = ws.split();
    let mut session = Session {
        params,
        sink,
        events,
        decoder: Decoder::default(),
        next_event: 0,
        racy: VecDeque::new(),
        scratch: BytesMut::new(),
    };
    let end = session.pump(&mut audio, &mut control, &mut stream).await;
    match end {
        End::Closed => {
            // Best effort, bounded: a polite close.
            let _ = timed(
                &*session.params.clock,
                session.params.cfg.io_timeout,
                "openai-realtime close",
                session.sink.send(Message::Close(None)),
            )
            .await;
        }
        End::Failed(error) => {
            tracing::debug!(provider = PROVIDER, %error, "session ended with an error");
            // The engine may be gone or stuck: never wait for it here.
            let _ = session.events.try_send(S2sEvent::Error(error));
        }
    }
}

impl Session {
    async fn pump(
        &mut self,
        audio: &mut mpsc::Receiver<Bytes>,
        control: &mut mpsc::Receiver<S2sControl>,
        stream: &mut SplitStream<Ws>,
    ) -> End {
        let keepalive = self.params.cfg.keepalive_interval;
        let io_timeout = self.params.cfg.io_timeout;
        // Liveness: outbound traffic proves nothing (it lands in the kernel buffer of a dead
        // peer), so only inbound frames count. After `keepalive` of inbound silence a `Ping` is
        // sent and its `Pong` (or any frame) must arrive within `io_timeout`.
        let mut wake_at = brisvox_core::clock::deadline_after(self.params.clock.now(), keepalive);
        let mut probing = false;
        loop {
            let clock = Arc::clone(&self.params.clock);
            tokio::select! {
                biased;
                command = control.recv() => {
                    let Some(command) = command else { return End::Closed };
                    // Control is served before audio, but audio the engine queued before a
                    // commit belongs in the buffer that is committed.
                    if matches!(command, S2sControl::Commit)
                        && let Err(error) = self.flush_audio(audio).await
                    {
                        return End::Failed(error);
                    }
                    if let Err(error) = self.send_control(&command).await {
                        return End::Failed(error);
                    }
                }
                message = stream.next() => {
                    // Any inbound frame (text, Pong, even an error) is a sign of life.
                    probing = false;
                    wake_at = brisvox_core::clock::deadline_after(clock.now(), keepalive);
                    if let Err(error) = self.on_message(message).await {
                        return End::Failed(error);
                    }
                }
                chunk = audio.recv() => {
                    let Some(chunk) = chunk else { return End::Closed };
                    if let Err(error) = self.write_audio(&chunk).await {
                        return End::Failed(error);
                    }
                }
                () = clock.sleep_until(wake_at) => {
                    if probing {
                        return End::Failed(Error::Core(brisvox_core::Error::Timeout {
                            what: "openai-realtime liveness (no pong)",
                            after: io_timeout,
                        }));
                    }
                    if let Err(error) = self.write(Message::Ping(Bytes::new())).await {
                        return End::Failed(error);
                    }
                    probing = true;
                    wake_at = brisvox_core::clock::deadline_after(clock.now(), io_timeout);
                }
            }
        }
    }

    /// Sends one audio chunk as an `input_audio_buffer.append`.
    async fn write_audio(&mut self, chunk: &[u8]) -> Result<(), Error> {
        let bytes = wire::append_audio(chunk, &mut self.scratch)?;
        let frame = Utf8Bytes::try_from(bytes).map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "an audio frame is not valid UTF-8".to_owned(),
        })?;
        self.write(Message::Text(frame)).await
    }

    /// Sends the audio chunks that are queued right now (and only those: the bound is the queue
    /// length at the start).
    async fn flush_audio(&mut self, audio: &mut mpsc::Receiver<Bytes>) -> Result<(), Error> {
        for _ in 0..audio.len() {
            let Ok(chunk) = audio.try_recv() else { break };
            self.write_audio(&chunk).await?;
        }
        Ok(())
    }

    async fn write(&mut self, message: Message) -> Result<(), Error> {
        timed(
            &*self.params.clock,
            self.params.cfg.io_timeout,
            "openai-realtime write",
            self.sink.send(message),
        )
        .await?
        .map_err(|e| ws_error(&e))
    }

    async fn send_control(&mut self, command: &S2sControl) -> Result<(), Error> {
        self.next_event = self.next_event.saturating_add(1);
        let id = format!("c{}", self.next_event);
        let (text, sent) = wire::control_event(command, &id);
        if sent == Sent::Racy {
            if self.racy.len() >= RACY_CAP {
                self.racy.pop_front();
            }
            self.racy.push_back(id);
        }
        self.write(Message::text(text)).await
    }

    /// Hands one event to the engine, waiting at most `io_timeout`.
    async fn emit(&self, event: S2sEvent) -> Result<(), Error> {
        let sent = timed(
            &*self.params.clock,
            self.params.cfg.io_timeout,
            "openai-realtime event hand-over",
            self.events.send(event),
        )
        .await?;
        // A dropped receiver means the engine is done with the session.
        sent.map_err(|_| Error::Core(brisvox_core::Error::ChannelClosed("s2s events")))
    }

    async fn on_message(
        &mut self,
        message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Result<(), Error> {
        match message {
            Some(Ok(Message::Text(text))) => self.on_text(text.as_str()).await,
            Some(Ok(Message::Close(frame))) => Err(match frame {
                Some(frame) => Error::Rejected {
                    provider: PROVIDER,
                    status: u16::from(frame.code),
                },
                None => Error::Disconnected { provider: PROVIDER },
            }),
            Some(Ok(_)) => Ok(()),
            Some(Err(e)) => Err(ws_error(&e)),
            None => Err(Error::Disconnected { provider: PROVIDER }),
        }
    }

    async fn on_text(&mut self, text: &str) -> Result<(), Error> {
        match self.decoder.decode(text)? {
            Decoded::SessionCreated | Decoded::SessionUpdated => Ok(()),
            Decoded::One(event) => self.emit(event).await,
            Decoded::Events(events) => {
                for event in events {
                    self.emit(event).await?;
                }
                Ok(())
            }
            Decoded::Error {
                event_id,
                code,
                message,
            } => {
                let racy = event_id
                    .as_deref()
                    .is_some_and(|id| self.racy.iter().any(|known| known == id));
                if racy {
                    tracing::debug!(provider = PROVIDER, %code, "expected race on cancel/truncate");
                    return Ok(());
                }
                self.emit(S2sEvent::Warning { code, message }).await
            }
        }
    }
}
