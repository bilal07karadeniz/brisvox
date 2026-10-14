//! `rtvi_server`: a small RTVI demo bot for the Pipecat JS client (`docs/RTVI.md`).
//!
//! It needs no provider keys. A `@pipecat-ai/client-js` app with the websocket transport
//! connects to `ws://127.0.0.1:8765/rtvi`, and the bot
//!
//! - answers the RTVI handshake (`client-ready` to `bot-ready`),
//! - detects speech on the caller's audio with a plain energy threshold, sends
//!   `user-started-speaking` / `user-stopped-speaking`,
//! - answers every utterance with a short two-tone beep (24 kHz, the JS player's default rate)
//!   and a `bot-output` text with the length of what it heard,
//! - answers `client-message` `ping` with a `server-response`, and `send-text` with `bot-output`.
//!
//! ```text
//! cargo run -p brisvox-transport --example rtvi_server -- [ADDR] [PATH]
//! ```

use std::f32::consts::TAU;
use std::process::ExitCode;
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{SystemClock, TurnEpoch};
use brisvox_transport::rtvi::{
    BotOutput, RtviConfig, RtviEvent, RtviRequest, RtviSender, RtviServer, SpokenStatus,
};
use brisvox_transport::websocket::ServerConfig;
use brisvox_transport::{
    SessionConfig, TransportCommand, TransportEvent, TransportSession, UtteranceId,
};
use tokio_util::sync::CancellationToken;

/// Mean absolute amplitude (of 32768) above which a frame counts as speech.
const SPEECH_LEVEL: i32 = 800;
/// Silence that ends an utterance.
const HANGOVER: Duration = Duration::from_millis(600);
/// The beep: two tones of this length each.
const TONE: Duration = Duration::from_millis(200);

fn level(frame: &AudioFrame) -> i32 {
    let samples: Vec<i32> = frame
        .data()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i32::from(i16::from_le_bytes(*pair)).abs())
        .collect();
    let count = i32::try_from(samples.len().max(1)).unwrap_or(i32::MAX);
    samples.iter().sum::<i32>() / count
}

/// 20 ms frames of a two-tone beep at 24 kHz.
fn beep(clock: &brisvox_core::SharedClock, epoch: TurnEpoch) -> Vec<AudioFrame> {
    let rate = 24_000_f32;
    let per_tone = (rate * TONE.as_secs_f32()) as usize;
    let mut pcm = Vec::new();
    for freq in [660_f32, 880_f32] {
        for n in 0..per_tone {
            let v = (n as f32 / rate * freq * TAU).sin() * 0.25 * f32::from(i16::MAX);
            pcm.extend_from_slice(&(v as i16).to_le_bytes());
        }
    }
    pcm.chunks(960)
        .enumerate()
        .filter_map(|(seq, chunk)| {
            AudioFrame::new(
                bytes::Bytes::copy_from_slice(chunk),
                SampleRate::Hz24000,
                clock.now(),
                seq as u64,
                epoch,
            )
            .ok()
        })
        .collect()
}

async fn say(sender: &RtviSender, text: &str) {
    let _ = sender
        .send(RtviEvent::BotOutput(BotOutput {
            text: text.to_owned(),
            aggregated_by: "sentence".to_owned(),
            will_be_spoken: Some(false),
            spoken_status: Some(SpokenStatus::Completed),
        }))
        .await;
}

async fn serve_call(call: brisvox_transport::rtvi::RtviCall, cancel: CancellationToken) {
    let clock = SystemClock::shared();
    let brisvox_transport::rtvi::RtviCall {
        link,
        sender,
        mut inbox,
    } = call;
    let Ok(mut session) =
        TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel).await
    else {
        return;
    };
    let mut speaking_since: Option<tokio::time::Instant> = None;
    let mut last_speech: Option<tokio::time::Instant> = None;
    let mut utterance = 0_u64;
    let mut inbox_open = true;
    loop {
        tokio::select! {
            event = session.recv() => match event {
                Some(TransportEvent::Audio(frame)) => {
                    let now = frame.timestamp();
                    if level(&frame) > SPEECH_LEVEL {
                        if speaking_since.is_none() {
                            speaking_since = Some(now);
                            let _ = sender.send(RtviEvent::UserStartedSpeaking).await;
                        }
                        last_speech = Some(now);
                    } else if let (Some(start), Some(last)) = (speaking_since, last_speech)
                        && now.saturating_duration_since(last) >= HANGOVER
                    {
                        speaking_since = None;
                        let heard = last.saturating_duration_since(start);
                        let _ = sender.send(RtviEvent::UserStoppedSpeaking).await;
                        say(&sender, &format!("I heard you for {} ms.", heard.as_millis())).await;
                        utterance += 1;
                        for frame in beep(&clock, TurnEpoch::ZERO) {
                            let command = TransportCommand::Audio {
                                epoch: TurnEpoch::ZERO,
                                utterance: UtteranceId::new(utterance),
                                frame,
                            };
                            if session.send(command).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                Some(TransportEvent::Stopped { .. }) | None => return,
                Some(_) => {}
            },
            request = inbox.recv(), if inbox_open => match request {
                Some(RtviRequest::ClientMessage { id, t, d }) if t == "ping" => {
                    let _ = sender.respond(&id, &t, d).await;
                }
                Some(RtviRequest::ClientMessage { id, t, .. }) => {
                    let _ = sender.respond_error(&id, &format!("unknown message {t}")).await;
                }
                Some(RtviRequest::SendText { content, .. }) => {
                    say(&sender, &format!("You typed: {content}")).await;
                }
                Some(_) => {}
                None => inbox_open = false,
            },
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:8765".to_owned());
    let path = args.next().unwrap_or_else(|| "/rtvi".to_owned());
    let Ok(bind_addr) = addr.parse() else {
        eprintln!("usage: rtvi_server [ADDR (127.0.0.1:8765)] [PATH (/rtvi)]");
        return ExitCode::from(2);
    };
    let server = ServerConfig {
        bind_addr,
        ..ServerConfig::default()
    };
    let rtvi = RtviConfig {
        path: path.clone(),
        ..RtviConfig::default()
    };
    let cancel = CancellationToken::new();
    let mut server = match RtviServer::bind(server, rtvi, SystemClock::shared(), &cancel).await {
        Ok(server) => server,
        Err(error) => {
            eprintln!("cannot start: {error}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "rtvi_server listening on ws://{}{path}",
        server.local_addr()
    );
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            call = server.accept_call(&cancel) => match call {
                Ok(Some(call)) => { tokio::spawn(serve_call(call, cancel.clone())); }
                _ => break,
            },
        }
    }
    cancel.cancel();
    let _ = server.shutdown(Duration::from_secs(5)).await;
    ExitCode::SUCCESS
}
