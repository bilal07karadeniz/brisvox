//! `webrtc_echo`: a tiny bot for trying the browser pages without provider keys.
//!
//! It serves the WebRTC signaling endpoint, the built-in demo page and the WebSocket fallback on
//! one port. A call is answered with a one-second tone; when the caller speaks, the bot answers
//! with a two-second tone, and if the caller speaks again while it is still talking the bot is
//! interrupted (barge-in: `clear`), so the page's playback buffer, marks and level meters can all
//! be seen working.
//!
//! ```text
//! cargo run -p brisvox-transport --example webrtc_echo --features webrtc -- [ADDR [PAGE_ORIGIN]]
//! # open http://127.0.0.1:8088/demo (add ?force=ws to skip WebRTC)
//! ```
//!
//! The media socket is bound to loopback, which Chrome does not offer as an ICE candidate, so a
//! browser on the same machine cannot connect over WebRTC and the page falls back to the
//! WebSocket after a few seconds: the situation of a network that only passes HTTP(S). The
//! listener has no credentials; keep it on loopback. `PAGE_ORIGIN` (for example
//! `http://localhost:8000`) lets `examples/web-assistant/web/index.html`, served from another
//! origin, call it (`BRISVOX_WEBRTC_CORS_ORIGIN`).

use std::f32::consts::TAU;
use std::process::ExitCode;
use std::time::Duration;

use brisvox_core::clock::deadline_after;
use brisvox_core::frame::{AudioFrame, SampleRate};
use brisvox_core::{SharedClock, SystemClock, TurnEpoch};
use brisvox_transport::webrtc::{WebRtcConfig, WebRtcServer};
use brisvox_transport::{
    SessionConfig, Transport, TransportCommand, TransportEvent, TransportSession, UtteranceId,
};
use tokio_util::sync::CancellationToken;

/// Mean absolute amplitude (of 32768) above which a caller frame counts as speech.
const SPEECH_LEVEL: i32 = 800;
/// Bot speech older than this is interrupted by caller speech.
const BARGE_IN_AFTER_FRAMES: u32 = 10;

fn level(frame: &AudioFrame) -> i32 {
    let (pairs, _) = frame.data().as_chunks::<2>();
    let sum: i64 = pairs
        .iter()
        .map(|pair| i64::from(i16::from_le_bytes(*pair)).abs())
        .sum();
    i32::try_from(sum / i64::try_from(pairs.len().max(1)).unwrap_or(1)).unwrap_or(i32::MAX)
}

/// What the bot is saying: a tone, one 20 ms frame at a time.
struct Speech {
    /// When the next frame is due (frames are paced against a schedule, not a fixed sleep, so a
    /// coarse timer does not slow the bot down below real time).
    next_at: tokio::time::Instant,
    frame: u32,
    frames: u32,
    freq: f32,
    mark: String,
}

fn tone_frame(
    rate: SampleRate,
    freq: f32,
    index: u32,
    epoch: TurnEpoch,
    clock: &SharedClock,
) -> Option<AudioFrame> {
    let per_frame = (rate.hz() / 50) as usize;
    let mut pcm = Vec::with_capacity(per_frame * 2);
    for n in 0..per_frame {
        let t = (index as usize * per_frame + n) as f32 / rate.hz() as f32;
        let v = (t * freq * TAU).sin() * 0.2 * f32::from(i16::MAX);
        pcm.extend_from_slice(&(v as i16).to_le_bytes());
    }
    AudioFrame::new(
        bytes::Bytes::from(pcm),
        rate,
        clock.now(),
        u64::from(index),
        epoch,
    )
    .ok()
}

async fn serve_call(link: brisvox_transport::CallLink, cancel: CancellationToken) {
    let clock = SystemClock::shared();
    let Ok(mut session) =
        TransportSession::open(clock.clone(), link, SessionConfig::default(), &cancel).await
    else {
        return;
    };
    let mut rate = SampleRate::Hz24000;
    let mut epoch = TurnEpoch::ZERO;
    let mut utterance = 0_u64;
    let mut speech: Option<Speech> = None;
    loop {
        tokio::select! {
            event = session.recv() => match event {
                Some(TransportEvent::Started(info)) => {
                    if let Ok(Some(rates)) = info.sample_rate() {
                        rate = rates.output;
                    }
                    eprintln!("call {} started ({} Hz)", info.call_id, rate.hz());
                    utterance += 1;
                    speech = Some(Speech { next_at: clock.now(), frame: 0, frames: 50, freq: 440.0, mark: format!("greeting-{utterance}") });
                }
                Some(TransportEvent::Audio(frame)) => {
                    if level(&frame) <= SPEECH_LEVEL {
                        continue;
                    }
                    match &speech {
                        Some(talking) if talking.frame >= BARGE_IN_AFTER_FRAMES => {
                            // Barge-in: everything not yet played is dropped, the client is told.
                            eprintln!("barge-in");
                            epoch = TurnEpoch::new(epoch.get() + 1);
                            speech = None;
                            if session.send(TransportCommand::Clear { epoch }).await.is_err() {
                                return;
                            }
                        }
                        Some(_) => {}
                        None => {
                            utterance += 1;
                            speech = Some(Speech { next_at: clock.now(), frame: 0, frames: 100, freq: 660.0, mark: format!("reply-{utterance}") });
                        }
                    }
                }
                Some(TransportEvent::MarkPlayed { id }) => eprintln!("mark played: {id}"),
                Some(TransportEvent::MarkCleared { id }) => eprintln!("mark cleared: {id}"),
                Some(TransportEvent::Stopped { reason }) => {
                    eprintln!("call ended: {reason:?}");
                    return;
                }
                None => return,
                Some(_) => {}
            },
            // The bot speaks in real time: one 20 ms frame per 20 ms.
            () = clock.sleep_until(speech.as_ref().map_or_else(|| clock.now(), |s| s.next_at)), if speech.is_some() => {
                let Some(talking) = speech.as_mut() else { continue };
                if talking.frame < talking.frames {
                    if let Some(frame) = tone_frame(rate, talking.freq, talking.frame, epoch, &clock) {
                        let command = TransportCommand::Audio {
                            epoch,
                            utterance: UtteranceId::new(utterance),
                            frame,
                        };
                        if session.send(command).await.is_err() {
                            return;
                        }
                    }
                    talking.frame += 1;
                    talking.next_at = deadline_after(talking.next_at, Duration::from_millis(20));
                } else {
                    let mark = TransportCommand::Mark { id: talking.mark.clone() };
                    speech = None;
                    if session.send(mark).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8088".to_owned());
    let Ok(http_addr) = addr.parse::<std::net::SocketAddr>() else {
        eprintln!("usage: webrtc_echo [ADDR (127.0.0.1:8088) [PAGE_ORIGIN]]");
        return ExitCode::from(2);
    };
    let config = WebRtcConfig {
        http_addr,
        udp_addr: http_addr,
        cors_origin: std::env::args().nth(2),
        ..WebRtcConfig::default()
    };
    let cancel = CancellationToken::new();
    let mut server = match WebRtcServer::bind(config, SystemClock::shared(), &cancel).await {
        Ok(server) => server,
        Err(error) => {
            eprintln!("cannot start: {error}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "webrtc_echo: open http://{}/demo (WebRTC at /whip, WebSocket fallback at /ws)",
        server.http_addr()
    );
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            call = server.accept(&cancel) => match call {
                Ok(Some(link)) => { tokio::spawn(serve_call(link, cancel.clone())); }
                _ => break,
            },
        }
    }
    cancel.cancel();
    let _ = server.shutdown(Duration::from_secs(5)).await;
    ExitCode::SUCCESS
}
