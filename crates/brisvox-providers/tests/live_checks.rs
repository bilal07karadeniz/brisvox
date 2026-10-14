//! Live provider checks with real keys: a
//! Turkish TTS -> STT round trip, prosody samples for one-word first chunks, and the
//! speculation hit rate of a real STT. Gated on `BRISVOX_LIVE_TESTS=1` and the keys; without
//! them every test skips cleanly. Numbers go to stderr as `LIVE|...` lines; audio goes to
//! `bench/results/live-audio/`; the speculation report to `bench/results/`.
//!
//! The sockets are real, so these tests use the real system clock and wall-clock pacing (the
//! `disallowed_methods` lint guards engine logic, not test pacing).
#![cfg(all(
    feature = "provider-deepgram",
    feature = "provider-cartesia",
    feature = "provider-elevenlabs",
    feature = "provider-deepgram-aura"
))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::print_stderr,
    clippy::needless_range_loop,
    clippy::chunks_exact_to_as_chunks,
    clippy::collapsible_if,
    missing_docs
)]

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use brisvox_audio::StreamResampler;
use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SharedClock, SystemClock};
use brisvox_providers::cartesia::{CartesiaConfig, CartesiaTts};
use brisvox_providers::deepgram::{DeepgramConfig, DeepgramStt};
use brisvox_providers::deepgram_aura::{DeepgramAuraConfig, DeepgramAuraTts};
use brisvox_providers::elevenlabs::{ElevenLabsConfig, ElevenLabsTts};
use brisvox_providers::{
    ContextId, SttConfig, SttEvent, SttInput, SttProvider, TtsConfig, TtsEvent, TtsInput,
    TtsProvider, TtsSession,
};
use brisvox_sim::fixture::{Fixture, fixtures_dir};
use bytes::Bytes;
use tokio::time::Instant;

const DATE: &str = "2026-10-13";
const TTS_RATE: u32 = 24_000;

fn clock() -> SharedClock {
    SystemClock::shared()
}

fn live() -> bool {
    if env("BRISVOX_LIVE_TESTS").as_deref() != Some("1") {
        eprintln!("skipped: set BRISVOX_LIVE_TESTS=1 (and the provider keys)");
        return false;
    }
    true
}

/// A non-empty variable of the process environment. Keys come only from the environment, like
/// every other live test (load `.env` into the shell first; the test never reads the file).
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn audio_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/results/live-audio");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn results_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/results")
}

// ---------------------------------------------------------------------------------------------
// Audio helpers
// ---------------------------------------------------------------------------------------------

fn pcm_from_bytes(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// Resamples a whole clip with the repo's resampler.
fn resample(pcm: &[i16], from: SampleRate, to: SampleRate) -> Vec<i16> {
    let mut r = StreamResampler::new(from, to).unwrap();
    let mut out = Vec::new();
    let chunk = 480_usize;
    for part in pcm.chunks(chunk) {
        let mut buf = vec![0_i16; r.max_output_len(part.len())];
        let n = r.process(part, &mut buf).unwrap();
        out.extend_from_slice(&buf[..n]);
    }
    let mut tail = vec![0_i16; r.max_flush_len()];
    let n = r.flush(&mut tail).unwrap();
    out.extend_from_slice(&tail[..n]);
    out
}

fn write_wav(path: &std::path::Path, rate: u32, pcm: &[i16]) {
    let data_len = (pcm.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + pcm.len() * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16_u32.to_le_bytes());
    v.extend_from_slice(&1_u16.to_le_bytes());
    v.extend_from_slice(&1_u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes());
    v.extend_from_slice(&2_u16.to_le_bytes());
    v.extend_from_slice(&16_u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        v.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, v).unwrap();
}

/// Internal silences of a clip: runs of 10 ms frames whose peak is below `-44 dBFS`, between the
/// first and the last non-silent frame, at least `min_ms` long. Returns `(start_ms, len_ms)`.
fn internal_gaps(pcm: &[i16], rate: u32, min_ms: u32) -> Vec<(u32, u32)> {
    let frame = (rate / 100) as usize;
    let loud: Vec<bool> = pcm
        .chunks(frame)
        .map(|f| f.iter().any(|s| s.unsigned_abs() > 200))
        .collect();
    let (Some(first), Some(last)) = (loud.iter().position(|l| *l), loud.iter().rposition(|l| *l))
    else {
        return Vec::new();
    };
    let mut gaps = Vec::new();
    let mut run_start = None;
    for i in first..=last {
        match (loud[i], run_start) {
            (false, None) => run_start = Some(i),
            (true, Some(s)) => {
                let len = (i - s) as u32 * 10;
                if len >= min_ms {
                    gaps.push((s as u32 * 10 - first as u32 * 10, len));
                }
                run_start = None;
            }
            _ => {}
        }
    }
    gaps
}

/// Real-time playout model: audio is played back to back from the first chunk on; a chunk that
/// arrives after the queued audio ran out is an underrun of the pacer (stall = the wait).
/// Returns `(underruns of at least 20 ms, total stall in ms)`.
fn playout_underruns(arrivals: &[(Duration, usize)], rate: u32) -> (u32, u32) {
    let mut runs_out: Option<Duration> = None;
    let (mut count, mut stall) = (0_u32, Duration::ZERO);
    for (t, bytes) in arrivals {
        let dur = Duration::from_secs_f64(*bytes as f64 / 2.0 / f64::from(rate));
        let start = match runs_out {
            Some(end) if *t > end => {
                let wait = *t - end;
                if wait >= Duration::from_millis(20) {
                    count += 1;
                    stall += wait;
                }
                *t
            }
            Some(end) => end,
            None => *t,
        };
        runs_out = Some(start + dur);
    }
    (count, stall.as_millis() as u32)
}

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

/// Lowercases, Turkish-aware when `tr` (`İ` -> `i`, `I` -> `ı`), drops apostrophes, turns every
/// other non-alphanumeric character into a space.
fn normalize(text: &str, tr: bool) -> Vec<String> {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\'' | '\u{2019}' => {}
            'İ' if tr => out.push('i'),
            'I' if tr => out.push('ı'),
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().map(str::to_owned).collect()
}

/// Word error rate by Levenshtein distance over words: `(S + D + I) / N`.
fn wer(reference: &str, hypothesis: &str, tr: bool) -> f64 {
    let r = normalize(reference, tr);
    let h = normalize(hypothesis, tr);
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, hw) in h.iter().enumerate() {
            let sub = prev[j] + usize::from(rw != hw);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[h.len()] as f64 / r.len() as f64
}

// ---------------------------------------------------------------------------------------------
// TTS helpers
// ---------------------------------------------------------------------------------------------

struct Synth {
    pcm: Vec<i16>,
    first_audio: Duration,
    words: Vec<(String, Duration)>,
    arrivals: Vec<(Duration, usize)>,
}

impl Synth {
    fn secs(&self) -> f64 {
        self.pcm.len() as f64 / f64::from(TTS_RATE)
    }
}

fn ctx_text(context: u64, text: &str) -> TtsInput {
    TtsInput::Text {
        context: ContextId(context),
        text: Arc::from(text),
    }
}

/// Sends `chunks` (a delay in ms before each, then the text) into one context, flushes and
/// collects the audio until `Flushed`.
async fn synth(session: &mut TtsSession, context: u64, chunks: &[(u64, &str)]) -> Synth {
    let input = session.input.clone();
    let owned: Vec<(u64, String)> = chunks.iter().map(|(d, t)| (*d, (*t).to_owned())).collect();
    let started = Instant::now();
    let sender = async move {
        for (delay, text) in owned {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            input.send(ctx_text(context, &text)).await.unwrap();
        }
        input
            .send(TtsInput::Flush {
                context: ContextId(context),
            })
            .await
            .unwrap();
    };
    let receiver = async {
        let mut bytes = Vec::new();
        let mut first_audio = None;
        let mut words = Vec::new();
        let mut arrivals = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(25), session.events.recv())
                .await
                .expect("live: no TTS event within 25 s")
                .expect("live: TTS stream ended");
            match event {
                TtsEvent::Audio { audio, .. } => {
                    let t = started.elapsed();
                    first_audio.get_or_insert(t);
                    arrivals.push((t, audio.len()));
                    bytes.extend_from_slice(&audio);
                }
                TtsEvent::Word { word, start, .. } => words.push((word.to_string(), start)),
                TtsEvent::Flushed { .. } => break,
                TtsEvent::Cancelled { .. } => {}
                TtsEvent::Error(e) => panic!("live: TTS error {e}"),
            }
        }
        Synth {
            pcm: pcm_from_bytes(&bytes),
            first_audio: first_audio.expect("live: no audio"),
            words,
            arrivals,
        }
    };
    let ((), out) = tokio::join!(sender, receiver);
    out
}

fn tts_config(voice: &str) -> TtsConfig {
    TtsConfig {
        sample_rate: SampleRate::Hz24000,
        voice: Arc::from(voice),
    }
}

/// One TTS under test: its name, language and an opened session.
struct Engine {
    provider: &'static str,
    lang: &'static str,
    session: TtsSession,
}

/// Connects the TTS providers whose keys are set, for `lang` (`"tr"` skips Aura: no Turkish
/// voice, docs/providers/deepgram-aura.md).
async fn engines(lang: &'static str) -> Vec<Engine> {
    let mut out = Vec::new();
    if let (Some(key), Some(voice)) = (env("CARTESIA_API_KEY"), env("CARTESIA_VOICE_ID")) {
        let mut cfg = CartesiaConfig::new(Secret::new(key), clock());
        cfg.language = Some(lang.to_owned());
        let p = CartesiaTts::new(cfg).unwrap();
        p.warm().await.unwrap();
        let session = p.connect(tts_config(&voice)).await.unwrap();
        out.push(Engine {
            provider: "cartesia",
            lang,
            session,
        });
    }
    if let (Some(key), Some(voice)) = (env("ELEVENLABS_API_KEY"), env("ELEVENLABS_VOICE_ID")) {
        let mut cfg = ElevenLabsConfig::new(Secret::new(key), clock());
        if let Some(model) = env("ELEVENLABS_MODEL") {
            cfg.model_id = model;
        }
        cfg.language_code = Some(lang.to_owned());
        cfg.warm_target = Some((Arc::from(voice.as_str()), SampleRate::Hz24000));
        let p = ElevenLabsTts::new(cfg).unwrap();
        p.warm().await.unwrap();
        let session = p.connect(tts_config(&voice)).await.unwrap();
        out.push(Engine {
            provider: "elevenlabs",
            lang,
            session,
        });
    }
    if lang == "en" {
        if let Some(key) = env("DEEPGRAM_API_KEY") {
            let cfg = DeepgramAuraConfig::new(Secret::new(key), clock());
            let p = DeepgramAuraTts::new(cfg).unwrap();
            let session = p
                .connect(tts_config(brisvox_providers::deepgram_aura::DEFAULT_MODEL))
                .await
                .unwrap();
            out.push(Engine {
                provider: "deepgram-aura",
                lang,
                session,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// STT helper
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Ev {
    t: Duration,
    text: String,
    is_final: bool,
    speech_final: bool,
    from_finalize: bool,
}

/// Streams `pcm` (16 kHz) to Deepgram in real time in 20 ms chunks followed by `tail` of
/// silence, sends `Finalize` at each of `finalize_at` (audio time) and returns every transcript
/// event with its arrival time (since the first chunk).
async fn run_stt(
    key: &str,
    lang: &str,
    tune: impl FnOnce(&mut DeepgramConfig),
    pcm: &[i16],
    finalize_at: &[Duration],
    tail: Duration,
) -> Vec<Ev> {
    let mut cfg = DeepgramConfig::new(Secret::new(key.to_owned()));
    cfg.language = Some(lang.to_owned());
    tune(&mut cfg);
    let stt = DeepgramStt::new(cfg, clock());
    stt.warm().await.expect("warm");
    let session = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: Some(Arc::from(lang)),
        })
        .await
        .expect("connect");
    let t0 = Instant::now();
    let mut events = session.events;
    let collector = tokio::spawn(async move {
        let mut out = Vec::new();
        while let Some(event) = events.recv().await {
            match event {
                SttEvent::Transcript {
                    text,
                    is_final,
                    speech_final,
                    from_finalize,
                } => out.push(Ev {
                    t: t0.elapsed(),
                    text: text.to_string(),
                    is_final,
                    speech_final,
                    from_finalize,
                }),
                SttEvent::Error(e) => panic!("live: STT error {e}"),
            }
        }
        out
    });
    let mut samples = pcm.to_vec();
    samples.resize(pcm.len() + (tail.as_secs_f64() * 16_000.0) as usize, 0);
    let mut pending: Vec<Duration> = finalize_at.to_vec();
    pending.sort();
    let mut pending = pending.into_iter().peekable();
    for (k, chunk) in samples.chunks(320).enumerate() {
        let at = Duration::from_millis(20 * k as u64);
        tokio::time::sleep_until(t0 + at).await;
        while pending.peek().is_some_and(|f| *f <= at) {
            pending.next();
            session.input.send(SttInput::Finalize).await.unwrap();
        }
        let mut b = Vec::with_capacity(chunk.len() * 2);
        for s in chunk {
            b.extend_from_slice(&s.to_le_bytes());
        }
        session
            .input
            .send(SttInput::Audio(Bytes::from(b)))
            .await
            .unwrap();
    }
    session.input.send(SttInput::Finalize).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    drop(session.input);
    tokio::time::timeout(Duration::from_secs(8), collector)
        .await
        .expect("live: STT did not close")
        .unwrap()
}

fn joined_finals(events: &[Ev]) -> String {
    events
        .iter()
        .filter(|e| e.is_final)
        .map(|e| e.text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------------------------
// 1. Turkish round trip
// ---------------------------------------------------------------------------------------------

/// (text sent to the TTS, spoken form the STT is scored against)
const TR_SENTENCES: [(&str, &str); 3] = [
    (
        "Yarın saat üçte randevum var mı?",
        "Yarın saat üçte randevum var mı?",
    ),
    (
        "Çağrınızı şimdi İstanbul şubesine aktarıyorum, lütfen hatta kalın.",
        "Çağrınızı şimdi İstanbul şubesine aktarıyorum, lütfen hatta kalın.",
    ),
    (
        "Toplam tutar 250 lira, ödemeyi öğleden sonra 14.30'da yapabilirsiniz.",
        "Toplam tutar iki yüz elli lira, ödemeyi öğleden sonra on dört otuzda yapabilirsiniz.",
    ),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_turkish_round_trip() {
    if !live() {
        return;
    }
    let Some(dg_key) = env("DEEPGRAM_API_KEY") else {
        eprintln!("skipped: DEEPGRAM_API_KEY not set");
        return;
    };
    let mut report = String::new();
    // Deepgram on the Turkish FLEURS fixture (the STT-only baseline).
    let fixture = Fixture::load(&fixtures_dir(), "tr_two_utterances").unwrap();
    let pcm = pcm_from_bytes(&fixture.pcm);
    let events = run_stt(
        &dg_key,
        "tr",
        |_| {},
        &pcm,
        &[],
        Duration::from_millis(1500),
    )
    .await;
    let reference: String = fixture
        .segments
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let hyp = joined_finals(&events);
    let fx_wer = wer(&reference, &hyp, true);
    eprintln!("LIVE|stt-fixture|deepgram nova-3 tr|tr_two_utterances|WER {fx_wer:.3}|hyp: {hyp}");
    let _ = writeln!(report, "fixture tr_two_utterances WER {fx_wer:.3}");
    assert!(fx_wer < 0.35, "Deepgram TR fixture WER {fx_wer}");

    let dir = audio_dir();
    let mut engines = engines("tr").await;
    assert!(!engines.is_empty(), "no TTS key set");
    let mut ctx = 100_u64;
    for engine in &mut engines {
        let mut total = (0.0_f64, 0.0_f64);
        // The three sentences as written, plus the third through the repo's TTS text normaliser.
        let mut sentences: Vec<(String, String)> = TR_SENTENCES
            .iter()
            .map(|(t, s)| ((*t).to_owned(), (*s).to_owned()))
            .collect();
        sentences.push((
            brisvox_agent::normalize(TR_SENTENCES[2].0, brisvox_agent::Language::Tr).into_owned(),
            TR_SENTENCES[2].1.to_owned(),
        ));
        for (i, (text, spoken)) in sentences.iter().enumerate() {
            let (text, spoken) = (text.as_str(), spoken.as_str());
            ctx += 1;
            let s = synth(&mut engine.session, ctx, &[(0, text)]).await;
            let secs = s.secs();
            // Word timestamps: monotonic, inside the audio, Turkish letters intact.
            let monotonic = s.words.windows(2).all(|w| w[0].1 <= w[1].1);
            let last = s.words.last().map(|w| w.1.as_secs_f64());
            let joined: String = s
                .words
                .iter()
                .map(|w| w.0.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let missing: Vec<char> = "çğıöşüÇĞİÖŞÜ"
                .chars()
                .filter(|c| text.contains(*c) && !joined.contains(*c))
                .collect();
            eprintln!(
                "LIVE|tts|{}|tr|s{}|first audio {:?}|{secs:.2} s|{} words|monotonic {monotonic}|last word {last:?}|missing chars {missing:?}|words: {joined}",
                engine.provider,
                i + 1,
                s.first_audio,
                s.words.len()
            );
            assert!(!s.pcm.is_empty());
            if engine.provider != "deepgram-aura" {
                assert!(
                    !s.words.is_empty(),
                    "{} gave no word timestamps",
                    engine.provider
                );
                assert!(monotonic, "{} word times not monotonic", engine.provider);
                assert!(
                    last.unwrap_or(0.0) <= secs + 0.5,
                    "{} words run past the audio",
                    engine.provider
                );
                assert!(
                    missing.is_empty(),
                    "{} lost Turkish letters {missing:?}",
                    engine.provider
                );
            }
            // Resample to the STT rate with the repo's resampler, save, transcribe.
            let pcm16 = resample(&s.pcm, SampleRate::Hz24000, SampleRate::Hz16000);
            write_wav(
                &dir.join(format!("tr-roundtrip-{}-s{}.wav", engine.provider, i + 1)),
                16_000,
                &pcm16,
            );
            let events = run_stt(
                &dg_key,
                "tr",
                |c| {
                    c.smart_format = false;
                    c.punctuate = true;
                },
                &pcm16,
                &[Duration::from_secs_f64(secs + 0.1)],
                Duration::from_millis(1200),
            )
            .await;
            let hyp = joined_finals(&events);
            let w = wer(spoken, &hyp, true);
            total.0 += w;
            total.1 += 1.0;
            eprintln!(
                "LIVE|wer|{}|tr|s{}|WER {w:.3}|ref: {spoken}|hyp: {hyp}",
                engine.provider,
                i + 1
            );
            let _ = writeln!(report, "{} s{} WER {w:.3}", engine.provider, i + 1);
            assert!(w < 0.35, "{} sentence {} WER {w}", engine.provider, i + 1);
        }
        eprintln!(
            "LIVE|wer-mean|{}|tr|{:.3}",
            engine.provider,
            total.0 / total.1
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 2. Prosody samples: one-word first chunk vs one chunk
// ---------------------------------------------------------------------------------------------

const PROSODY: [(&str, &str, &str); 2] = [
    ("en", "Okay,", " I can book that for you tomorrow at three."),
    ("tr", "Tamam,", " yarın saat üçe randevunuzu alıyorum."),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_prosody_samples() {
    if !live() {
        return;
    }
    let dir = audio_dir();
    let mut ctx = 200_u64;
    for (lang, first, rest) in PROSODY {
        let lang: &'static str = if lang == "en" { "en" } else { "tr" };
        let mut set = engines(lang).await;
        for engine in &mut set {
            // The segmenter with one first word sends "Okay," when the first clause closes and
            // the rest when the next boundary closes (a token-stream delay later); the engine
            // flushes at the end of the response. Two runs of each variant (the first warms the
            // context machinery) so the gap numbers are not one-off.
            let whole = format!("{first}{rest}");
            for run in 0..2 {
                ctx += 1;
                let split = synth(&mut engine.session, ctx, &[(0, first), (150, rest)]).await;
                ctx += 1;
                let single = synth(&mut engine.session, ctx, &[(0, whole.as_str())]).await;
                let (su, ss) = playout_underruns(&split.arrivals, TTS_RATE);
                let (nu, ns) = playout_underruns(&single.arrivals, TTS_RATE);
                let sg = internal_gaps(&split.pcm, TTS_RATE, 80);
                let ng = internal_gaps(&single.pcm, TTS_RATE, 80);
                eprintln!(
                    "LIVE|prosody|{}|{}|run{run}|split: first audio {:?}, {:.2} s, gaps>=80ms {sg:?}, playout underruns {su} ({ss} ms)|single: first audio {:?}, {:.2} s, gaps>=80ms {ng:?}, playout underruns {nu} ({ns} ms)",
                    engine.provider,
                    engine.lang,
                    split.first_audio,
                    split.secs(),
                    single.first_audio,
                    single.secs(),
                );
                if run == 1 {
                    for (name, s) in [("split", &split), ("single", &single)] {
                        let pcm16 = resample(&s.pcm, SampleRate::Hz24000, SampleRate::Hz16000);
                        write_wav(
                            &dir.join(format!(
                                "prosody-{}-{}-{name}.wav",
                                engine.provider, engine.lang
                            )),
                            16_000,
                            &pcm16,
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Speculation hit rate with real Deepgram interims
// ---------------------------------------------------------------------------------------------

/// VAD stop after the segment end (brisvox's fixed VAD stop parameter, 0.1 s).
const VAD_STOP: Duration = Duration::from_millis(100);

#[derive(Debug, Default, Clone)]
struct Tally {
    turns: u32,
    /// The final arrived before the VAD stop (no candidate needed).
    final_before_stop: u32,
    /// Candidate empty at the stop (no request would be made).
    empty: u32,
    hits: u32,
    misses: u32,
    /// Optimistic candidate (finals + trailing interim) equals the final.
    hits_optimistic: u32,
    /// Last interim before the final equals the final / exists at all.
    interim_equal: u32,
    interim_present: u32,
    gaps_ms: Vec<u64>,
    saved_ms_on_hit: Vec<u64>,
    finalize_rtt_ms: Vec<u64>,
    detail: Vec<String>,
}

fn pct(n: u32, d: u32) -> String {
    if d == 0 {
        "n/a".to_owned()
    } else {
        format!("{:.0}% ({n}/{d})", 100.0 * f64::from(n) / f64::from(d))
    }
}

fn median(v: &[u64]) -> String {
    if v.is_empty() {
        return "n/a".to_owned();
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    format!("{} (max {})", s[s.len() / 2], s[s.len() - 1])
}

/// Replays the events the way the engine's turn driver reads them (docs: `partial_text`): a
/// final group of `is_final` segments, the latest interim, and a candidate taken at each VAD
/// stop (the group, else the interim).
fn analyse(events: &[Ev], stops: &[Duration], tr: bool, label: &str, tally: &mut Tally) {
    // Timeline of (time, kind): events and VAD stops, in time order.
    enum K<'a> {
        Ev(&'a Ev),
        Stop(usize),
    }
    let mut timeline: Vec<(Duration, K<'_>)> = events.iter().map(|e| (e.t, K::Ev(e))).collect();
    timeline.extend(stops.iter().enumerate().map(|(i, s)| (*s, K::Stop(i))));
    timeline.sort_by_key(|(t, _)| *t);
    let mut group = String::new();
    let mut interim = String::new();
    let mut last_interim: Option<(Duration, String)> = None;
    // Candidate recorded at a stop, resolved by the next utterance final.
    let mut cand: Option<(usize, Duration, String, String)> = None; // (stop idx, stop time, engine candidate, optimistic)
    let mut finals_seen_stops = 0;
    for (t, k) in timeline {
        match k {
            K::Stop(i) => {
                let engine_cand = if group.trim().is_empty() {
                    interim.clone()
                } else {
                    group.clone()
                };
                let optimistic = if group.trim().is_empty() {
                    interim.clone()
                } else {
                    format!("{} {}", group.trim(), interim.trim())
                };
                if engine_cand.trim().is_empty() {
                    tally.empty += 1;
                    tally.turns += 1;
                    cand = Some((i, t, String::new(), String::new()));
                } else {
                    cand = Some((i, t, engine_cand, optimistic));
                }
                finals_seen_stops += 1;
            }
            K::Ev(e) => {
                if !e.is_final {
                    interim = e.text.clone();
                    last_interim = Some((e.t, e.text.clone()));
                    continue;
                }
                if !e.text.trim().is_empty() {
                    if !group.is_empty() {
                        group.push(' ');
                    }
                    group.push_str(e.text.trim());
                }
                if !e.speech_final {
                    interim.clear();
                    continue;
                }
                // Utterance final.
                let final_text = std::mem::take(&mut group);
                interim.clear();
                if final_text.trim().is_empty() {
                    last_interim = None;
                    continue;
                }
                let norm_final = normalize(&final_text, tr);
                if let Some((lt, li)) = last_interim.take() {
                    tally.interim_present += 1;
                    if normalize(&li, tr) == norm_final {
                        tally.interim_equal += 1;
                    }
                    tally
                        .gaps_ms
                        .push((t.saturating_sub(lt)).as_millis() as u64);
                }
                match cand.take() {
                    Some((i, stop_t, c, opt)) => {
                        if !c.is_empty() {
                            tally.turns += 1;
                            tally
                                .finalize_rtt_ms
                                .push(t.saturating_sub(stop_t).as_millis() as u64);
                            if normalize(&c, tr) == norm_final {
                                tally.hits += 1;
                                tally
                                    .saved_ms_on_hit
                                    .push(t.saturating_sub(stop_t).as_millis() as u64);
                            } else {
                                tally.misses += 1;
                                tally.detail.push(format!(
                                    "{label} stop{i}: candidate `{c}` vs final `{final_text}`"
                                ));
                            }
                            if normalize(&opt, tr) == norm_final {
                                tally.hits_optimistic += 1;
                            }
                        } else {
                            tally
                                .finalize_rtt_ms
                                .push(t.saturating_sub(stop_t).as_millis() as u64);
                        }
                    }
                    None => {
                        tally.turns += 1;
                        tally.final_before_stop += 1;
                    }
                }
            }
        }
    }
    let _ = finals_seen_stops;
}

fn stops_of(fx: &Fixture) -> Vec<Duration> {
    fx.segments
        .iter()
        .map(|s| fx.time_of(s.end) + VAD_STOP)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_speculation_hit_rate_deepgram() {
    if !live() {
        return;
    }
    let Some(key) = env("DEEPGRAM_API_KEY") else {
        eprintln!("skipped: DEEPGRAM_API_KEY not set");
        return;
    };
    // (fixture, language sent to Deepgram, finalize at the VAD stop like the engine)
    let plan: [(&str, &str, bool); 9] = [
        ("en_two_utterances", "en", true),
        ("tr_two_utterances", "tr", true),
        ("en_tr_mixed", "tr", true),
        ("en_two_utterances", "en", true),
        ("tr_two_utterances", "tr", true),
        ("en_tr_mixed", "en", true),
        ("en_two_utterances", "en", false),
        ("tr_two_utterances", "tr", false),
        ("en_tr_mixed", "en", false),
    ];
    let mut by_group: std::collections::BTreeMap<String, Tally> = std::collections::BTreeMap::new();
    let mut wers = Vec::new();
    let mut total_audio = Duration::ZERO;
    let limit = env("BRISVOX_LIVE_SPEC_RUNS")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(plan.len());
    for (name, lang, finalize) in plan.into_iter().take(limit) {
        let fx = Fixture::load(&fixtures_dir(), name).unwrap();
        let pcm = pcm_from_bytes(&fx.pcm);
        let stops = stops_of(&fx);
        let finalize_at: Vec<Duration> = if finalize { stops.clone() } else { Vec::new() };
        let events = run_stt(
            &key,
            lang,
            |_| {},
            &pcm,
            &finalize_at,
            Duration::from_millis(1200),
        )
        .await;
        total_audio += fx.duration() + Duration::from_millis(1200);
        let tr = lang == "tr";
        let mode = if finalize {
            "finalize-at-stop"
        } else {
            "natural-endpointing"
        };
        let label = format!("{name}/{lang}/{mode}");
        let group = format!("{} ({mode})", if tr { "TR" } else { "EN" });
        let tally = by_group.entry(group).or_default();
        // The mixed fixture is scored per its sentence language by `lang`; the other-language
        // sentence is recognised badly but its hit rate is still a property of the interims.
        analyse(&events, &stops, tr, &label, tally);
        if name != "en_tr_mixed" && finalize {
            let reference: String = fx
                .segments
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let hyp = joined_finals(&events);
            let w = wer(&reference, &hyp, tr);
            wers.push((label.clone(), w));
            eprintln!("LIVE|stt-wer|{label}|WER {w:.3}|hyp: {hyp}");
        }
        for e in &events {
            eprintln!(
                "LIVE|ev|{label}|{:>5} ms|final {} speech_final {} from_finalize {}|{}",
                e.t.as_millis(),
                u8::from(e.is_final),
                u8::from(e.speech_final),
                u8::from(e.from_finalize),
                e.text
            );
        }
        eprintln!(
            "LIVE|stops|{label}|{:?}",
            stops.iter().map(|s| s.as_millis()).collect::<Vec<_>>()
        );
        eprintln!(
            "LIVE|spec-events|{label}|{} events|{} interims|{} finals",
            events.len(),
            events.iter().filter(|e| !e.is_final).count(),
            events.iter().filter(|e| e.is_final).count()
        );
        let from_fin = events.iter().filter(|e| e.from_finalize).count();
        eprintln!("LIVE|spec-from-finalize|{label}|{from_fin}");
    }

    let mut md = String::new();
    let _ = writeln!(md, "# Speculation hit rate with real Deepgram\n");
    let _ = writeln!(
        md,
        "Measured {DATE} with Deepgram nova-3 (streaming, adapter defaults: smart_format on, punctuate off, endpointing 300 ms, interim results on), fixtures streamed in real time in 20 ms chunks plus 1.2 s of trailing silence; {:.0} s of audio in total. \
         `finalize-at-stop` sends `Finalize` at the VAD stop (segment end + {} ms), as the engine does; `natural-endpointing` leaves the final to Deepgram's 300 ms endpointing.\n",
        total_audio.as_secs_f64(),
        VAD_STOP.as_millis()
    );
    let _ = writeln!(
        md,
        "Candidate rule = the engine's `TurnDriver::partial_text` at the VAD stop (`on_partial`): the `is_final` segments heard so far, else the latest interim. A **hit** means the normalized candidate equals the normalized utterance final (case, punctuation, Turkish-aware i/I). `optimistic` would also append the trailing interim to the finals. The last-interim columns compare the last interim before the final with the final.\n"
    );
    let _ = writeln!(
        md,
        "| group | turns | final before stop | empty candidate | hit | miss (wasted request) | optimistic hit | last interim == final | median interim->final ms | median stop->final ms (all) |\n|---|---|---|---|---|---|---|---|---|---|"
    );
    for (g, t) in &by_group {
        let cand = t.hits + t.misses;
        let _ = writeln!(
            md,
            "| {g} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            t.turns,
            t.final_before_stop,
            t.empty,
            pct(t.hits, cand),
            pct(t.misses, cand),
            pct(t.hits_optimistic, cand),
            pct(t.interim_equal, t.interim_present),
            median(&t.gaps_ms),
            median(&t.finalize_rtt_ms),
        );
    }
    let _ = writeln!(
        md,
        "\nSTT-only WER on the fixtures (finalize-at-stop runs):\n"
    );
    for (l, w) in &wers {
        let _ = writeln!(md, "- {l}: WER {w:.3}");
    }
    let _ = writeln!(md, "\nMisses:\n");
    for t in by_group.values() {
        for d in &t.detail {
            let _ = writeln!(md, "- {d}");
        }
    }
    std::fs::write(
        results_dir().join(format!("speculation-live-deepgram-{DATE}.md")),
        &md,
    )
    .unwrap();
    eprintln!("{md}");
    assert!(by_group.values().map(|t| t.turns).sum::<u32>() > 0);
}
