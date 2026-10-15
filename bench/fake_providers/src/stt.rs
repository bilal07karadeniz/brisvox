//! Fake Deepgram live STT (`wss://.../v1/listen`).
//!
//! Wire behaviour, from docs/providers/deepgram.md and what Pipecat's Deepgram service sends:
//!
//! - binary frames are linear16 audio; control messages are JSON texts (`KeepAlive` ignored,
//!   `Finalize`, `CloseStream`);
//! - `Results` messages have Deepgram's shape (channel/alternatives/words, request metadata), so
//!   a message weighs what a real one weighs (0.6 to 1.5 KB);
//! - interim results every `stt_interim` while the caller speaks (growing word prefixes of the
//!   fixture's next utterance), a final `stt_final` after the end of speech the server detects
//!   (energy on 32 ms windows, silence of the `endpointing` query parameter: `is_final` and
//!   `speech_final`), or `stt_final` after a `Finalize` (`is_final` and `from_finalize`, without
//!   `speech_final`); a `Finalize` with nothing pending is answered at once with an empty
//!   `from_finalize` result, as Deepgram does;
//! - `CloseStream` flushes what is pending, sends `Metadata` and closes.
//!
//! Simplified: the transcript is the fixture's text, not recognised from the audio; no
//! `UtteranceEnd`, `SpeechStarted`, keyterm or language handling; only linear16.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::script::{self, Utterance};
use crate::server::{Shared, Ws};

/// Silence on a connection that ends it (Deepgram closes idle sockets after 10 s without data;
/// a load test client always sends audio or `KeepAlive`).
const IDLE: Duration = Duration::from_secs(60);
/// A send to a client that does not read.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Window of the energy detector (the 32 ms window of the Silero VAD at 16 kHz).
const WINDOW_MS: u64 = 32;

/// The query parameters that matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    /// Sample rate of the audio (`sample_rate`).
    pub sample_rate: u32,
    /// Silence that ends an utterance (`endpointing=<ms>`); `None`: `endpointing=false`.
    pub endpointing_ms: Option<u64>,
    /// `interim_results=true`.
    pub interim_results: bool,
}

impl Params {
    /// Reads the query string; Deepgram's defaults for what is missing (16 kHz is assumed when
    /// `sample_rate` is absent, endpointing 10 ms, no interim results).
    pub fn from_query(query: &str) -> Self {
        let mut params = Self {
            sample_rate: 16_000,
            endpointing_ms: Some(10),
            interim_results: false,
        };
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "sample_rate" => {
                    if let Ok(rate) = value.parse::<u32>() {
                        params.sample_rate = rate.clamp(8_000, 48_000);
                    }
                }
                "endpointing" => {
                    params.endpointing_ms = match value {
                        "false" | "0" => None,
                        v => v.parse::<u64>().ok().or(Some(10)),
                    };
                }
                "interim_results" => params.interim_results = value == "true",
                _ => {}
            }
        }
        params
    }
}

struct Speech {
    index: usize,
    start_samples: u64,
    interims: Vec<String>,
    sent_interims: usize,
    next_interim: Option<Instant>,
    final_due: Option<Instant>,
    speech_final: bool,
    from_finalize: bool,
}

/// The per-connection state machine. Pure: time comes in as arguments, messages go out as texts.
pub struct Engine {
    cfg: Arc<Config>,
    script: Arc<Vec<Utterance>>,
    params: Params,
    request_id: String,
    window_samples: usize,
    acc_squares: f64,
    acc_samples: usize,
    odd_byte: Option<u8>,
    audio_samples: u64,
    quiet_windows: usize,
    next_utterance: usize,
    speech: Option<Speech>,
}

impl Engine {
    /// A fresh connection.
    pub fn new(
        cfg: Arc<Config>,
        script: Arc<Vec<Utterance>>,
        params: Params,
        request_id: String,
    ) -> Self {
        let window_samples = usize::try_from(u64::from(params.sample_rate) * WINDOW_MS / 1000)
            .unwrap_or(512)
            .max(1);
        Self {
            cfg,
            script,
            params,
            request_id,
            window_samples,
            acc_squares: 0.0,
            acc_samples: 0,
            odd_byte: None,
            audio_samples: 0,
            quiet_windows: 0,
            next_utterance: 0,
            speech: None,
        }
    }

    /// Consecutive quiet windows that end an utterance (`None`: endpointing is off).
    fn quiet_needed(&self) -> Option<usize> {
        self.params.endpointing_ms.map(|ms| {
            usize::try_from(ms.div_ceil(WINDOW_MS))
                .unwrap_or(usize::MAX)
                .max(1)
        })
    }

    /// Audio from the client (linear16, little endian, mono).
    pub fn audio(&mut self, now: Instant, bytes: &[u8]) {
        let mut iter = bytes.iter().copied();
        if let Some(low) = self.odd_byte.take() {
            match iter.next() {
                Some(high) => self.sample(now, i16::from_le_bytes([low, high])),
                None => {
                    self.odd_byte = Some(low);
                    return;
                }
            }
        }
        while let Some(low) = iter.next() {
            let Some(high) = iter.next() else {
                self.odd_byte = Some(low);
                break;
            };
            self.sample(now, i16::from_le_bytes([low, high]));
        }
    }

    fn sample(&mut self, now: Instant, s: i16) {
        let v = f64::from(s);
        self.acc_squares += v * v;
        self.acc_samples += 1;
        self.audio_samples = self.audio_samples.saturating_add(1);
        if self.acc_samples >= self.window_samples {
            // The window length is a few hundred samples: exact in f64.
            #[allow(clippy::cast_precision_loss)]
            let rms = (self.acc_squares / self.acc_samples as f64).sqrt();
            self.acc_squares = 0.0;
            self.acc_samples = 0;
            self.window(now, rms >= self.cfg.vad_rms);
        }
    }

    fn window(&mut self, now: Instant, loud: bool) {
        let needed = self.quiet_needed();
        let final_after = self.cfg.stt_final;
        let interim_every = self.cfg.stt_interim;
        match &mut self.speech {
            None => {
                if loud && let Some(utt) = self.script.get(self.next_utterance) {
                    {
                        let interims = if self.params.interim_results {
                            script::interims_of(&utt.text)
                        } else {
                            Vec::new()
                        };
                        let next_interim = (!interims.is_empty()).then(|| now + interim_every);
                        let window = self.window_samples as u64;
                        self.speech = Some(Speech {
                            index: self.next_utterance,
                            // The loud window just completed started one window ago.
                            start_samples: self.audio_samples.saturating_sub(window),
                            interims,
                            sent_interims: 0,
                            next_interim,
                            final_due: None,
                            speech_final: false,
                            from_finalize: false,
                        });
                        self.quiet_windows = 0;
                    }
                }
            }
            Some(speech) if speech.final_due.is_none() => {
                if loud {
                    self.quiet_windows = 0;
                } else {
                    self.quiet_windows += 1;
                    if needed.is_some_and(|n| self.quiet_windows >= n) {
                        speech.final_due = Some(now + final_after);
                        speech.speech_final = true;
                        speech.next_interim = None;
                    }
                }
            }
            Some(_) => {}
        }
    }

    /// `{"type":"Finalize"}`.
    pub fn finalize(&mut self, now: Instant, out: &mut Vec<String>) {
        match &mut self.speech {
            Some(speech) => {
                speech.from_finalize = true;
                speech.next_interim = None;
                if speech.final_due.is_none() {
                    speech.final_due = Some(now + self.cfg.stt_final);
                }
            }
            None => {
                let (start, duration) = self.empty_span();
                out.push(self.results("", false, false, true, start, duration, &[]));
            }
        }
    }

    /// `{"type":"CloseStream"}`: whatever is pending is final now, then the metadata.
    pub fn close_stream(&mut self, out: &mut Vec<String>) {
        if let Some(mut speech) = self.speech.take() {
            speech.final_due = Some(Instant::now());
            speech.speech_final = true;
            out.push(self.final_of(&speech));
            self.next_utterance = speech.index + 1;
        }
        out.push(
            json!({
                "type": "Metadata",
                "transaction_key": "deprecated",
                "request_id": self.request_id,
                "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                "created": "2026-01-01T00:00:00.000Z",
                "duration": self.seconds(self.audio_samples),
                "channels": 1,
            })
            .to_string(),
        );
    }

    /// When the next interim or final is due.
    pub fn next_deadline(&self) -> Option<Instant> {
        let speech = self.speech.as_ref()?;
        match (speech.next_interim, speech.final_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Emits what is due at `now`.
    pub fn tick(&mut self, now: Instant, out: &mut Vec<String>) {
        loop {
            let Some(speech) = self.speech.as_mut() else {
                return;
            };
            let Some(at) = speech.next_interim.filter(|at| *at <= now) else {
                break;
            };
            let text = speech.interims.get(speech.sent_interims).cloned();
            speech.sent_interims += 1;
            speech.next_interim =
                (speech.sent_interims < speech.interims.len()).then(|| at + self.cfg.stt_interim);
            let start_samples = speech.start_samples;
            let Some(text) = text else { continue };
            let heard = self.audio_samples.saturating_sub(start_samples);
            let (start, duration) = (self.seconds(start_samples), self.seconds(heard));
            let words = self.words(&text, start, duration);
            out.push(self.results(&text, false, false, false, start, duration, &words));
        }
        let due = self
            .speech
            .as_ref()
            .and_then(|s| s.final_due)
            .is_some_and(|d| d <= now);
        if due && let Some(speech) = self.speech.take() {
            out.push(self.final_of(&speech));
            self.next_utterance = speech.index + 1;
            self.quiet_windows = 0;
        }
    }

    fn final_of(&self, speech: &Speech) -> String {
        let utt = self.script.get(speech.index);
        let text = utt.map_or("", |u| u.text.as_str());
        let seconds = utt.map_or(0.0, |u| u.seconds);
        let start = self.seconds(speech.start_samples);
        let words = self.words(text, start, seconds);
        self.results(
            text,
            true,
            speech.speech_final && !speech.from_finalize,
            speech.from_finalize,
            start,
            seconds,
            &words,
        )
    }

    fn seconds(&self, samples: u64) -> f64 {
        // Audio positions stay far below 2^53 samples.
        #[allow(clippy::cast_precision_loss)]
        let s = samples as f64 / f64::from(self.params.sample_rate);
        s
    }

    fn empty_span(&self) -> (f64, f64) {
        (self.seconds(self.audio_samples), 0.0)
    }

    /// Word entries spread evenly over `[start, start + duration]`.
    fn words(&self, text: &str, start: f64, duration: f64) -> Vec<Value> {
        let all: Vec<&str> = text.split_whitespace().collect();
        if all.is_empty() {
            return Vec::new();
        }
        // A handful of words: exact in f64.
        #[allow(clippy::cast_precision_loss)]
        let step = duration / all.len() as f64;
        all.iter()
            .enumerate()
            .map(|(i, w)| {
                #[allow(clippy::cast_precision_loss)]
                let from = start + step * i as f64;
                let plain: String = w
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '\'')
                    .collect();
                json!({
                    "word": plain.to_lowercase(),
                    "start": round3(from),
                    "end": round3(from + step * 0.9),
                    "confidence": 0.98 - 0.01 * f64::from(u8::try_from(i % 5).unwrap_or(0)),
                    "punctuated_word": w,
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn results(
        &self,
        transcript: &str,
        is_final: bool,
        speech_final: bool,
        from_finalize: bool,
        start: f64,
        duration: f64,
        words: &[Value],
    ) -> String {
        json!({
            "type": "Results",
            "channel_index": [0, 1],
            "duration": round3(duration),
            "start": round3(start),
            "is_final": is_final,
            "speech_final": speech_final,
            "channel": {
                "alternatives": [{
                    "transcript": transcript,
                    "confidence": if transcript.is_empty() { 0.0 } else { 0.986 },
                    "words": words,
                }]
            },
            "metadata": {
                "request_id": self.request_id,
                "model_info": {
                    "name": "general-nova-3",
                    "version": "2025-09-05.14486",
                    "arch": "nova-3",
                },
                "model_uuid": "c0d1a568-ce81-4fea-97e7-bd45cb1fdf3c",
            },
            "from_finalize": from_finalize,
        })
        .to_string()
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[derive(Deserialize)]
struct Control {
    #[serde(rename = "type", default)]
    kind: String,
}

/// Serves one STT websocket until it ends.
pub async fn run(mut ws: Ws, params: Params, shared: Arc<Shared>) {
    let _open = shared.metrics.stt_streams.enter();
    let request_id = shared.next_request_id();
    let mut engine = Engine::new(
        Arc::clone(&shared.config),
        Arc::clone(&shared.script),
        params,
        request_id,
    );
    let mut out: Vec<String> = Vec::new();
    let mut last_rx = Instant::now();
    let mut idle_at = last_rx + IDLE;
    let mut closing = false;
    // One timer for the whole call, moved only when the next deadline changes: re-arming a timer
    // per audio frame (50 per second per call) would cost more than the frame.
    let sleep = sleep_until(idle_at);
    tokio::pin!(sleep);
    loop {
        let wake = engine.next_deadline().map_or(idle_at, |d| d.min(idle_at));
        if sleep.deadline() != wake {
            sleep.as_mut().reset(wake);
        }
        tokio::select! {
            msg = ws.next() => {
                last_rx = Instant::now();
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        shared.metrics.stt_audio_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        engine.audio(last_rx, &bytes);
                    }
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Control>(&text).map(|c| c.kind).as_deref() {
                            Ok("Finalize") => engine.finalize(last_rx, &mut out),
                            Ok("CloseStream") => {
                                engine.close_stream(&mut out);
                                closing = true;
                            }
                            _ => {}
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
                engine.tick(now, &mut out);
            }
        }
        if engine.next_deadline().is_some() {
            engine.tick(Instant::now(), &mut out);
        }
        for text in out.drain(..) {
            shared.metrics.stt_results.fetch_add(1, Ordering::Relaxed);
            match timeout(SEND_TIMEOUT, ws.send(Message::Text(text.into()))).await {
                Ok(Ok(())) => {}
                _ => return,
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

    const FIXTURE: &str = r#"{"sample_rate":16000,"segments":[
        {"start":0,"end":32000,"lang":"en","text":"hello there my friend"},
        {"start":0,"end":16000,"lang":"en","text":"second one"}]}"#;

    fn engine(query: &str) -> Engine {
        let script = Arc::new(script::parse(FIXTURE).unwrap());
        Engine::new(
            Arc::new(Config::default()),
            script,
            Params::from_query(query),
            "req".to_owned(),
        )
    }

    /// 20 ms of PCM16 at 16 kHz: a loud square wave or silence.
    fn frame(loud: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        for i in 0..320_i32 {
            let v: i16 = if loud {
                if (i / 10) % 2 == 0 { 3000 } else { -3000 }
            } else {
                0
            };
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes
    }

    fn texts(out: &[String]) -> Vec<Value> {
        out.iter()
            .map(|t| serde_json::from_str(t).unwrap())
            .collect()
    }

    fn feed(
        e: &mut Engine,
        t0: Instant,
        from_ms: u64,
        ms: u64,
        loud: bool,
        _out: &mut Vec<String>,
    ) {
        for k in 0..ms / 20 {
            e.audio(t0 + Duration::from_millis(from_ms + k * 20), &frame(loud));
        }
    }

    #[test]
    fn the_query_is_read() {
        let p = Params::from_query(
            "model=nova-3&sample_rate=16000&endpointing=300&interim_results=true",
        );
        assert_eq!(
            (p.sample_rate, p.endpointing_ms, p.interim_results),
            (16_000, Some(300), true)
        );
        assert_eq!(Params::from_query("endpointing=false").endpointing_ms, None);
        assert_eq!(Params::from_query("").endpointing_ms, Some(10));
    }

    #[test]
    fn speech_then_silence_gives_interims_and_an_endpoint_final() {
        let mut e = engine("endpointing=300&interim_results=true&sample_rate=16000");
        let (t0, mut out) = (Instant::now(), Vec::new());
        feed(&mut e, t0, 0, 400, true, &mut out);
        assert!(e.next_deadline().is_some());
        e.tick(t0 + Duration::from_millis(250), &mut out);
        let interims = texts(&out);
        assert_eq!(interims.len(), 2);
        assert_eq!(
            interims[1]["channel"]["alternatives"][0]["transcript"],
            "hello there"
        );
        assert_eq!(interims[0]["is_final"], false);
        out.clear();
        // 320 ms of silence: ten windows (320 ms) end the utterance, the final follows 150 ms later.
        feed(&mut e, t0, 400, 340, false, &mut out);
        let due = e.next_deadline().unwrap();
        e.tick(due - Duration::from_millis(1), &mut out);
        assert!(out.is_empty());
        e.tick(due, &mut out);
        let finals = texts(&out);
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0]["is_final"], true);
        assert_eq!(finals[0]["speech_final"], true);
        assert_eq!(finals[0]["from_finalize"], false);
        assert_eq!(
            finals[0]["channel"]["alternatives"][0]["transcript"],
            "hello there my friend"
        );
        assert_eq!(
            finals[0]["channel"]["alternatives"][0]["words"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        // The second utterance follows.
        out.clear();
        feed(&mut e, t0, 800, 200, true, &mut out);
        feed(&mut e, t0, 1000, 340, false, &mut out);
        let due = e.next_deadline().unwrap();
        e.tick(due, &mut out);
        assert_eq!(
            texts(&out)[0]["channel"]["alternatives"][0]["transcript"],
            "second one"
        );
    }

    #[test]
    fn finalize_answers_after_the_final_latency_and_marks_it() {
        let mut e = engine("endpointing=300&interim_results=true");
        let (t0, mut out) = (Instant::now(), Vec::new());
        feed(&mut e, t0, 0, 400, true, &mut out);
        feed(&mut e, t0, 400, 100, false, &mut out);
        e.finalize(t0 + Duration::from_millis(500), &mut out);
        assert!(out.is_empty());
        e.tick(t0 + Duration::from_millis(650), &mut out);
        let r = &texts(&out)[0];
        assert_eq!(
            (
                r["is_final"].as_bool(),
                r["from_finalize"].as_bool(),
                r["speech_final"].as_bool()
            ),
            (Some(true), Some(true), Some(false))
        );
        assert_eq!(
            r["channel"]["alternatives"][0]["transcript"],
            "hello there my friend"
        );
        // Nothing pending any more: a second Finalize gets an empty answer at once.
        out.clear();
        e.finalize(t0 + Duration::from_millis(700), &mut out);
        let r = &texts(&out)[0];
        assert_eq!(r["from_finalize"], true);
        assert_eq!(r["channel"]["alternatives"][0]["transcript"], "");
    }

    #[test]
    fn endpointing_false_waits_for_finalize_and_close_stream_flushes() {
        let mut e = engine("endpointing=false");
        let (t0, mut out) = (Instant::now(), Vec::new());
        feed(&mut e, t0, 0, 200, true, &mut out);
        feed(&mut e, t0, 200, 1000, false, &mut out);
        assert!(e.next_deadline().is_none());
        e.close_stream(&mut out);
        let all = texts(&out);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["is_final"], true);
        assert_eq!(all[1]["type"], "Metadata");
    }

    #[test]
    fn a_sample_split_across_frames_is_not_lost() {
        let mut e = engine("");
        let t0 = Instant::now();
        e.audio(t0, &[0x10]);
        e.audio(t0, &[0x00, 0x20]);
        assert_eq!(e.audio_samples, 1);
        assert_eq!(e.odd_byte, Some(0x20));
    }
}
