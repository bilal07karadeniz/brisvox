//! Fake Deepgram Aura streaming TTS (`wss://.../v1/speak`, linear16).
//!
//! Wire behaviour, from docs/providers/deepgram-aura.md and Pipecat's `DeepgramTTSService`:
//!
//! - `Metadata` right after the upgrade; client messages `Speak {text}`, `Flush`, `Clear`,
//!   `Close`; server messages: binary linear16 frames, `Flushed {sequence_id}`,
//!   `Cleared {sequence_id}`;
//! - the first audio of text that reaches an idle connection comes `tts_first_audio` later; the
//!   audio of the text then follows in `tts_chunk` frames spaced `tts_interval` apart (40 ms every
//!   20 ms is twice real time, like the in-process mock TTS); text that arrives while audio is
//!   still scheduled continues it; the audio of a text is `tts_ms_per_char` per character, a
//!   square wave of +-1000 (audible to the energy VAD of a client, never silence);
//! - `Flush` is answered with `Flushed` after the audio of all text before it;
//! - `Clear` drops the audio not yet sent (an ideal server) and is answered with `Cleared` at once.
//!
//! Simplified: no flush rate limit and no `Warning`; text is never rejected; the voice in the
//! `model` query parameter is not checked; only linear16 (other encodings are treated as 16-bit).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::server::{Shared, Ws};

/// A client that sends nothing for this long is gone.
const IDLE: Duration = Duration::from_secs(120);
/// A send to a client that does not read.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Most frames sent per wake-up (bounds the work between two reads).
const BATCH: usize = 64;

/// The query parameters that matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    /// Output sample rate (`sample_rate`).
    pub sample_rate: u32,
    /// Model string (`model`), echoed in the metadata.
    pub model: String,
}

impl Params {
    /// Reads the query string (24 kHz and `aura-2-thalia-en` when missing).
    pub fn from_query(query: &str) -> Self {
        let mut params = Self {
            sample_rate: 24_000,
            model: "aura-2-thalia-en".to_owned(),
        };
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "sample_rate" => {
                    if let Ok(rate) = value.parse::<u32>() {
                        params.sample_rate = rate.clamp(8_000, 48_000);
                    }
                }
                "model" if !value.is_empty() && value.len() <= 64 => {
                    value.clone_into(&mut params.model)
                }
                _ => {}
            }
        }
        params
    }
}

#[derive(Debug)]
enum Item {
    Chunk,
    Flushed(u64),
    Cleared(u64),
}

/// The per-connection schedule. Pure: time comes in as arguments.
pub struct Schedule {
    cfg: Arc<Config>,
    /// Instant the schedule is free again (the end of the audio queued so far).
    cursor: Instant,
    queue: VecDeque<(Instant, Item)>,
    flushes: u64,
    clears: u64,
}

/// What `Schedule::due` hands to the writer.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// One binary audio frame.
    Audio,
    /// `Flushed` text.
    Text(String),
}

impl Schedule {
    /// An idle connection at `now`.
    pub fn new(cfg: Arc<Config>, now: Instant) -> Self {
        Self {
            cfg,
            cursor: now,
            queue: VecDeque::new(),
            flushes: 0,
            clears: 0,
        }
    }

    /// Frames that `text` makes.
    pub fn frames_for(&self, text: &str) -> u64 {
        let chars = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        let audio_ms = chars.saturating_mul(self.cfg.tts_ms_per_char);
        let chunk_ms = u64::try_from(self.cfg.tts_chunk.as_millis())
            .unwrap_or(40)
            .max(1);
        audio_ms.div_ceil(chunk_ms).max(1)
    }

    /// `Speak {text}` received at `now`.
    pub fn speak(&mut self, now: Instant, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let mut at = if self.cursor <= now {
            now + self.cfg.tts_first_audio
        } else {
            self.cursor
        };
        for _ in 0..self.frames_for(text) {
            self.queue.push_back((at, Item::Chunk));
            at += self.cfg.tts_interval;
        }
        self.cursor = at;
    }

    /// `Flush` received at `now`.
    pub fn flush(&mut self, now: Instant) {
        self.flushes += 1;
        let at = self.cursor.max(now);
        self.queue.push_back((at, Item::Flushed(self.flushes - 1)));
        self.cursor = at;
    }

    /// `Clear` received at `now`: unsent audio and flushes are dropped, `Cleared` is due now.
    pub fn clear(&mut self, now: Instant) {
        self.queue.clear();
        self.queue.push_back((now, Item::Cleared(self.clears)));
        self.clears += 1;
        self.cursor = now;
    }

    /// The time of the next frame.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.queue.front().map(|(at, _)| *at)
    }

    /// Everything due at `now`, at most `limit` items.
    pub fn due(&mut self, now: Instant, limit: usize, out: &mut Vec<Out>) {
        while out.len() < limit {
            match self.queue.front() {
                Some((at, _)) if *at <= now => {}
                _ => break,
            }
            if let Some((_, item)) = self.queue.pop_front() {
                out.push(match item {
                    Item::Chunk => Out::Audio,
                    Item::Flushed(id) => {
                        Out::Text(json!({"type": "Flushed", "sequence_id": id}).to_string())
                    }
                    Item::Cleared(id) => {
                        Out::Text(json!({"type": "Cleared", "sequence_id": id}).to_string())
                    }
                });
            }
        }
    }
}

/// One frame of `chunk` of a +-1000 square wave (period 40 samples, like the mock TTS).
pub fn tone_frame(sample_rate: u32, chunk: Duration) -> Bytes {
    let samples = u64::from(sample_rate) * u64::try_from(chunk.as_millis()).unwrap_or(40) / 1000;
    let mut bytes = Vec::with_capacity(usize::try_from(samples).unwrap_or(0) * 2);
    for i in 0..samples {
        let v: i16 = if (i / 20) % 2 == 0 { 1000 } else { -1000 };
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Bytes::from(bytes)
}

#[derive(Deserialize)]
struct Control {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
}

/// Serves one TTS websocket until it ends.
pub async fn run(mut ws: Ws, params: Params, shared: Arc<Shared>) {
    let _open = shared.metrics.tts_streams.enter();
    let cfg = Arc::clone(&shared.config);
    let frame = tone_frame(params.sample_rate, cfg.tts_chunk);
    let mut schedule = Schedule::new(Arc::clone(&cfg), Instant::now());
    let metadata = json!({
        "type": "Metadata",
        "request_id": shared.next_request_id(),
        "model_name": params.model,
        "model_version": "2025-04-07.0",
        "model_uuid": "5b3d0a2e-9b0e-4c0b-8c5e-7d4f3a1c2b90",
    })
    .to_string();
    if !matches!(
        timeout(SEND_TIMEOUT, ws.send(Message::Text(metadata.into()))).await,
        Ok(Ok(()))
    ) {
        return;
    }
    let mut last_rx = Instant::now();
    let mut idle_at = last_rx + IDLE;
    let mut due: Vec<Out> = Vec::with_capacity(BATCH);
    let sleep = sleep_until(idle_at);
    tokio::pin!(sleep);
    loop {
        let wake = schedule.next_deadline().map_or(idle_at, |d| d.min(idle_at));
        if sleep.deadline() != wake {
            sleep.as_mut().reset(wake);
        }
        let mut closing = false;
        tokio::select! {
            msg = ws.next() => {
                last_rx = Instant::now();
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(c) = serde_json::from_str::<Control>(&text) {
                            match c.kind.as_str() {
                                "Speak" => schedule.speak(last_rx, &c.text),
                                "Flush" => schedule.flush(last_rx),
                                "Clear" => schedule.clear(last_rx),
                                "Close" => closing = true,
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
            () = &mut sleep => {
                let now = Instant::now();
                if now >= idle_at {
                    if now >= last_rx + IDLE {
                        break;
                    }
                    idle_at = last_rx + IDLE;
                }
            }
        }
        // Everything due, in batches so that a long backlog cannot starve the reads.
        loop {
            due.clear();
            schedule.due(Instant::now(), BATCH, &mut due);
            if due.is_empty() {
                break;
            }
            for item in due.drain(..) {
                let message = match item {
                    Out::Audio => {
                        shared
                            .metrics
                            .tts_audio_bytes
                            .fetch_add(frame.len() as u64, Ordering::Relaxed);
                        Message::Binary(frame.clone())
                    }
                    Out::Text(text) => Message::Text(text.into()),
                };
                if !matches!(timeout(SEND_TIMEOUT, ws.feed(message)).await, Ok(Ok(()))) {
                    return;
                }
            }
            if !matches!(timeout(SEND_TIMEOUT, ws.flush()).await, Ok(Ok(()))) {
                return;
            }
        }
        if closing {
            break;
        }
    }
    let _ = timeout(Duration::from_secs(2), ws.close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule() -> (Schedule, Instant) {
        let t0 = Instant::now();
        (Schedule::new(Arc::new(Config::default()), t0), t0)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn query_defaults() {
        let p = Params::from_query("model=aura-2-ozge-en&encoding=linear16&sample_rate=16000");
        assert_eq!(
            (p.sample_rate, p.model.as_str()),
            (16_000, "aura-2-ozge-en")
        );
        assert_eq!(Params::from_query("").sample_rate, 24_000);
    }

    #[test]
    fn audio_starts_150ms_after_the_text_and_is_60ms_per_character() {
        let (mut s, t0) = schedule();
        s.speak(t0, "Okay, got it.");
        // 13 characters * 60 ms = 780 ms = 20 frames of 40 ms.
        assert_eq!(s.queue.len(), 20);
        assert_eq!(s.next_deadline(), Some(t0 + ms(150)));
        let mut out = Vec::new();
        s.due(t0 + ms(149), 64, &mut out);
        assert!(out.is_empty());
        s.due(t0 + ms(190), 64, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(
            s.queue.back().map(|(at, _)| *at),
            Some(t0 + ms(150 + 19 * 20))
        );
    }

    #[test]
    fn flush_follows_the_audio_and_text_in_flight_continues_it() {
        let (mut s, t0) = schedule();
        s.speak(t0, "Okay, got it.");
        s.speak(t0 + ms(100), "More.");
        s.flush(t0 + ms(110));
        let mut out = Vec::new();
        s.due(t0 + ms(5_000), 1_000, &mut out);
        let flushed = out
            .iter()
            .position(|o| matches!(o, Out::Text(t) if t.contains("Flushed")));
        assert_eq!(flushed, Some(out.len() - 1));
        // 780 ms + 300 ms of audio = 27 frames.
        assert_eq!(out.len(), 20 + 8 + 1);
    }

    #[test]
    fn flush_without_text_is_answered_at_once() {
        let (mut s, t0) = schedule();
        s.flush(t0);
        let mut out = Vec::new();
        s.due(t0, 8, &mut out);
        assert_eq!(
            out,
            [Out::Text(
                r#"{"sequence_id":0,"type":"Flushed"}"#.to_owned()
            )]
        );
    }

    #[test]
    fn clear_drops_the_unsent_audio_and_answers_cleared() {
        let (mut s, t0) = schedule();
        s.speak(t0, "Okay, got it.");
        s.flush(t0);
        let mut out = Vec::new();
        s.due(t0 + ms(200), 64, &mut out);
        let sent = out.len();
        s.clear(t0 + ms(200));
        s.due(t0 + ms(200), 64, &mut out);
        assert_eq!(out.len(), sent + 1);
        assert!(matches!(out.last(), Some(Out::Text(t)) if t.contains("Cleared")));
        s.due(t0 + ms(10_000), 64, &mut out);
        assert_eq!(out.len(), sent + 1);
        // New text after the clear starts like on an idle connection.
        s.speak(t0 + ms(300), "Next.");
        assert_eq!(s.next_deadline(), Some(t0 + ms(450)));
    }

    #[test]
    fn the_tone_frame_is_a_square_wave_of_the_right_length() {
        let f = tone_frame(24_000, ms(40));
        assert_eq!(f.len(), 24_000 * 40 / 1000 * 2);
        assert_eq!(i16::from_le_bytes([f[0], f[1]]), 1000);
        assert_eq!(i16::from_le_bytes([f[40], f[41]]), -1000);
    }
}
