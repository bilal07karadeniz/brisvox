//! The session task: pumps the engine's audio and control channels into the socket and the
//! socket's messages into the engine.

use std::sync::Arc;

use brisvox_core::SharedClock;
use bytes::{Bytes, BytesMut};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

use super::config::GeminiLiveConfig;
use super::wire::{self, Decoded, Mapper, ToolAnswer};
use super::{PROVIDER, Ws, timed, ws_error};
use crate::error::Error;
use crate::s2s::{S2sControl, S2sEvent};

/// What the task needs besides its channels.
pub(super) struct Params {
    pub(super) cfg: Arc<GeminiLiveConfig>,
    pub(super) clock: SharedClock,
    /// The engine decides the caller's turn ([`S2sTurnDetection::Manual`](crate::s2s::S2sTurnDetection::Manual)).
    pub(super) manual: bool,
    /// Rate of the caller audio, for the audio MIME type.
    pub(super) input_rate: brisvox_core::frame::SampleRate,
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
    mapper: Mapper,
    /// The fixed start of every audio message.
    head: Bytes,
    /// Reused buffer of audio messages.
    scratch: BytesMut,
    /// Manual mode: an `activityStart` was sent and no `activityEnd` yet.
    in_activity: bool,
    /// The server answers by itself (after `activityEnd` or a tool response): a following
    /// [`S2sControl::CreateResponse`] must not ask again.
    owes_response: bool,
    /// Tool answers waiting for the other calls of the same server message.
    batch: Vec<ToolAnswer>,
}

/// Runs until the engine drops its senders, the socket closes or something fails. The last thing
/// sent to the engine on a failure is an [`S2sEvent::Error`].
pub(super) async fn run(
    params: Params,
    ws: Ws,
    mut audio: mpsc::Receiver<Bytes>,
    mut control: mpsc::Receiver<S2sControl>,
    events: mpsc::Sender<S2sEvent>,
    mapper: Mapper,
) {
    let (sink, mut stream) = ws.split();
    let head = wire::audio_head(params.input_rate);
    let mut session = Session {
        params,
        sink,
        events,
        mapper,
        head,
        scratch: BytesMut::new(),
        in_activity: false,
        owes_response: false,
        batch: Vec::new(),
    };
    let end = session.pump(&mut audio, &mut control, &mut stream).await;
    match end {
        End::Closed => {
            // Best effort, bounded: a polite close.
            let _ = timed(
                &*session.params.clock,
                session.params.cfg.io_timeout,
                "gemini-live close",
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
        // Liveness: only inbound frames count (see the OpenAI adapter for the reasoning).
        let mut wake_at = brisvox_core::clock::deadline_after(self.params.clock.now(), keepalive);
        let mut probing = false;
        loop {
            let clock = Arc::clone(&self.params.clock);
            tokio::select! {
                biased;
                command = control.recv() => {
                    let Some(command) = command else { return End::Closed };
                    if let Err(error) = self.on_control(command, audio).await {
                        return End::Failed(error);
                    }
                }
                message = stream.next() => {
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
                            what: "gemini-live liveness (no pong)",
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

    /// Sends one audio chunk as a `realtimeInput.audio`; in manual mode the first chunk of an
    /// utterance is preceded by `activityStart`.
    async fn write_audio(&mut self, chunk: &[u8]) -> Result<(), Error> {
        if self.params.manual && !self.in_activity {
            self.in_activity = true;
            self.write(Message::text(wire::ACTIVITY_START)).await?;
        }
        let bytes = wire::append_audio(&self.head, chunk, &mut self.scratch)?;
        let frame = Utf8Bytes::try_from(bytes).map_err(|_| Error::Protocol {
            provider: PROVIDER,
            detail: "an audio frame is not valid UTF-8".to_owned(),
        })?;
        self.write(Message::Text(frame)).await
    }

    /// Sends the audio chunks that are queued right now (and only those).
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
            "gemini-live write",
            self.sink.send(message),
        )
        .await?
        .map_err(|e| ws_error(&e))
    }

    /// Hands one event to the engine, waiting at most `io_timeout`.
    async fn emit(&self, event: S2sEvent) -> Result<(), Error> {
        let sent = timed(
            &*self.params.clock,
            self.params.cfg.io_timeout,
            "gemini-live event hand-over",
            self.events.send(event),
        )
        .await?;
        // A dropped receiver means the engine is done with the session.
        sent.map_err(|_| Error::Core(brisvox_core::Error::ChannelClosed("s2s events")))
    }

    /// The tool answers collected so far go out as one `toolResponse`.
    async fn flush_batch(&mut self) -> Result<(), Error> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let answers = std::mem::take(&mut self.batch);
        self.write(Message::text(wire::tool_response(&answers)))
            .await?;
        // The server continues by itself after a tool response.
        self.owes_response = true;
        Ok(())
    }

    async fn on_control(
        &mut self,
        command: S2sControl,
        audio: &mut mpsc::Receiver<Bytes>,
    ) -> Result<(), Error> {
        match command {
            S2sControl::Commit => {
                // Audio the engine queued before the commit belongs to this utterance.
                self.flush_audio(audio).await?;
                if self.params.manual && self.in_activity {
                    self.in_activity = false;
                    self.write(Message::text(wire::ACTIVITY_END)).await?;
                    // The server answers by itself once the activity ended.
                    self.owes_response = true;
                }
            }
            S2sControl::ClearInput => {
                // Gemini cannot drop audio it already received; nothing to send.
                tracing::debug!(
                    provider = PROVIDER,
                    "ClearInput has no Gemini Live equivalent"
                );
            }
            S2sControl::CreateResponse => {
                self.flush_batch().await?;
                if self.owes_response {
                    // The server answers by itself (activityEnd or a tool response).
                    self.owes_response = false;
                } else {
                    self.write(Message::text(wire::TURN_COMPLETE)).await?;
                }
            }
            S2sControl::CancelResponse => {
                // No cancel message exists: the caller's next `activityStart` (or speech)
                // interrupts the server; locally the rest of the response is dropped.
                for event in self.mapper.cancel_local() {
                    self.emit(event).await?;
                }
            }
            S2sControl::Truncate { .. } => {
                // The server has no way to cut its history to what the caller heard; the engine's
                // local mirror holds the truth and re-seeds the next session (docs/S2S.md).
                tracing::debug!(
                    provider = PROVIDER,
                    "Truncate has no Gemini Live equivalent"
                );
            }
            S2sControl::ToolResult { call_id, output } => {
                match self.mapper.answer(&call_id, &output) {
                    Some(answer) => self.batch.push(answer),
                    None => {
                        // The call was cancelled or interrupted: the server dropped it.
                        tracing::debug!(
                            provider = PROVIDER,
                            "tool result of a closed call dropped"
                        );
                    }
                }
                if self.mapper.outstanding() == 0 {
                    self.flush_batch().await?;
                }
            }
            S2sControl::Note { text } => {
                self.write(Message::text(wire::note_message(&text))).await?;
            }
        }
        Ok(())
    }

    async fn on_message(
        &mut self,
        message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Result<(), Error> {
        match message {
            // The service sends its JSON as text or as binary frames.
            Some(Ok(Message::Text(text))) => self.on_payload(text.as_bytes()).await,
            Some(Ok(Message::Binary(bytes))) => self.on_payload(&bytes).await,
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

    async fn on_payload(&mut self, payload: &[u8]) -> Result<(), Error> {
        match self.mapper.decode(payload)? {
            Decoded::SetupComplete => Ok(()),
            Decoded::Events(events) => {
                for event in events {
                    if matches!(
                        event,
                        S2sEvent::ResponseStarted { .. } | S2sEvent::ResponseDone { .. }
                    ) {
                        // The server did answer: the next CreateResponse is a new request.
                        self.owes_response = false;
                    }
                    self.emit(event).await?;
                }
                Ok(())
            }
            Decoded::Error { code, message } => {
                self.emit(S2sEvent::Warning { code, message }).await
            }
        }
    }
}
