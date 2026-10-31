//! The word error rate hook of the noise suppression evaluation (feature `denoise`;
//! `brisvox-bench denoise transcribe`).
//!
//! The mixes of [`crate::denoise_eval`] (speech fixtures with licensed noise at several signal-to-noise
//! ratios, wideband and through the telephone path) are sent to a speech-to-text server, once as
//! they are (what the provider hears with `denoise_stt = false`) and once enhanced by each
//! backend (`denoise_stt = true`). The server is a sherpa-onnx streaming server (the adapter the
//! engine uses: `brisvox_providers::sherpa_onnx`), so the word error rate is the one the
//! local recognizer gives. One JSON line per transcript goes to the output file, with the
//! reference text, the hypothesis and the error counts, so a script can recompute the rate with
//! another normalisation; the markdown summary groups the rate by fixture, path, SNR and backend.
//!
//! The audio can also be written out (`dump_dir`) to run another recognizer over the same mixes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Clock, SystemClock};
use brisvox_providers::sherpa_onnx::{SherpaOnnxConfig, SherpaOnnxStt};
use brisvox_providers::{SttConfig, SttEvent, SttInput, SttProvider};
use bytes::Bytes;
use serde::Serialize;

use crate::BenchError;
use crate::denoise_eval::{NoiseClip, Path16, Speech, Variant, enhance, mix, write_wav};

/// What to run.
pub struct TranscribeConfig {
    /// `ws://host:port` of the sherpa-onnx streaming server.
    pub url: String,
    /// Backends; [`Variant::None`] is the audio as it is.
    pub variants: Vec<Variant>,
    /// Paths.
    pub paths: Vec<Path16>,
    /// Signal-to-noise ratios.
    pub snrs: Vec<f32>,
    /// Speech.
    pub speech: Vec<Speech>,
    /// Noise.
    pub noise: Vec<NoiseClip>,
    /// Also run the clean speech.
    pub clean: bool,
    /// Where to write the mixed audio, if anywhere.
    pub dump_dir: Option<PathBuf>,
    /// Do not contact a server (only dump the audio).
    pub dump_only: bool,
    /// Language handed to the server.
    pub language: Option<String>,
    /// Longest wait for the final result of one clip.
    pub timeout: Duration,
}

/// One transcript.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// Fixture.
    pub fixture: String,
    /// Noise recording, or `none`.
    pub noise: String,
    /// Signal-to-noise ratio in dB (`null` for clean).
    pub snr_db: Option<f32>,
    /// `wideband` or `telephone`.
    pub path: &'static str,
    /// `none` (the audio as it is) or the backend.
    pub variant: &'static str,
    /// The reference text (the utterances joined).
    pub reference: String,
    /// What the server recognised.
    pub hypothesis: String,
    /// Words in the reference after normalisation.
    pub ref_words: usize,
    /// Substitutions, deletions and insertions.
    pub errors: usize,
    /// The audio file written for this clip.
    pub audio_file: Option<String>,
}

/// Lowercased words of `text` (letters and digits; Turkish case rules when `turkish`).
pub fn words(text: &str, turkish: bool) -> Vec<String> {
    let mut cleaned = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_alphanumeric() {
            if turkish && c == 'I' {
                cleaned.push('\u{131}');
            } else if turkish && c == '\u{130}' {
                cleaned.push('i');
            } else {
                cleaned.extend(c.to_lowercase());
            }
        } else {
            cleaned.push(' ');
        }
    }
    cleaned.split_whitespace().map(str::to_owned).collect()
}

/// Word-level edit distance (substitutions + deletions + insertions).
pub fn edit_distance(reference: &[String], hypothesis: &[String]) -> usize {
    let mut previous: Vec<usize> = (0..=hypothesis.len()).collect();
    for (i, r) in reference.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, h) in hypothesis.iter().enumerate() {
            let substitute = previous.get(j).copied().unwrap_or(0) + usize::from(r != h);
            let delete = previous.get(j + 1).copied().unwrap_or(0) + 1;
            let insert = current.get(j).copied().unwrap_or(0) + 1;
            current.push(substitute.min(delete).min(insert));
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(reference.len())
}

fn pcm_bytes(pcm: &[i16]) -> Bytes {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    Bytes::from(out)
}

/// Streams `pcm` (16 kHz) to the server and returns the final text.
async fn transcribe(
    provider: &SherpaOnnxStt,
    pcm: &[i16],
    language: Option<&str>,
    timeout: Duration,
) -> Result<String, BenchError> {
    let call = |e: brisvox_providers::Error| BenchError::Call(format!("stt: {e}"));
    let mut session = provider
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: language.map(Arc::from),
        })
        .await
        .map_err(call)?;
    for chunk in pcm.chunks(320) {
        session
            .input
            .send(SttInput::Audio(pcm_bytes(chunk)))
            .await
            .map_err(|_| BenchError::Call("stt: session closed while sending".into()))?;
    }
    session
        .input
        .send(SttInput::Finalize)
        .await
        .map_err(|_| BenchError::Call("stt: session closed before finalize".into()))?;
    let mut parts: Vec<String> = Vec::new();
    let clock = SystemClock;
    let started = clock.now();
    loop {
        let left = timeout.saturating_sub(clock.now().saturating_duration_since(started));
        let Ok(event) = tokio::time::timeout(left, session.events.recv()).await else {
            break;
        };
        match event {
            Some(SttEvent::Transcript {
                text,
                is_final,
                speech_final,
                from_finalize,
                ..
            }) => {
                if (is_final || from_finalize) && !text.trim().is_empty() {
                    parts.push(text.trim().to_owned());
                }
                if from_finalize && speech_final {
                    break;
                }
            }
            Some(SttEvent::Error(e)) => return Err(call(e)),
            None => break,
        }
    }
    Ok(parts.join(" "))
}

fn file_name(parts: &[&str]) -> String {
    let mut name = parts.join("__");
    name.retain(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    name.push_str(".wav");
    name
}

/// Runs every mix through every variant and the server.
///
/// # Errors
/// [`BenchError`] when the server cannot be reached or a clip cannot be processed.
pub async fn run(config: &TranscribeConfig) -> Result<Vec<Row>, BenchError> {
    let mut sherpa = SherpaOnnxConfig::new();
    sherpa.base_url.clone_from(&config.url);
    sherpa.io_timeout = Duration::from_secs(10);
    sherpa.finalize_timeout = Duration::from_secs(10);
    let provider = SherpaOnnxStt::new(sherpa, SystemClock::shared());
    if let Some(dir) = &config.dump_dir {
        std::fs::create_dir_all(dir)?;
    }
    // (noise clip or none, snr or none)
    let mut conditions: Vec<(Option<&NoiseClip>, Option<f32>)> = Vec::new();
    if config.clean {
        conditions.push((None, None));
    }
    for snr in &config.snrs {
        for clip in &config.noise {
            conditions.push((Some(clip), Some(*snr)));
        }
    }
    let mut rows = Vec::new();
    for (si, speech) in config.speech.iter().enumerate() {
        let turkish = speech.name.starts_with("tr_");
        let reference = speech.texts.join(" ");
        let ref_words = words(&reference, turkish);
        for (ci, (clip, snr)) in conditions.iter().enumerate() {
            let mixed = mix(
                speech,
                clip.map(|c| (c, (si * 3 + ci) * 12_345)),
                snr.unwrap_or(0.0),
            );
            for path in &config.paths {
                let audio = path.apply(&mixed)?;
                for variant in &config.variants {
                    let heard = match variant.build(16_000)? {
                        None => audio.clone(),
                        Some(mut d) => enhance(d.as_mut(), &audio)?,
                    };
                    let noise_name = clip.map_or("none", |c| c.name.as_str());
                    let snr_label = snr.map_or_else(|| "clean".to_owned(), |s| format!("snr{s}"));
                    let audio_file = match &config.dump_dir {
                        Some(dir) => {
                            let name = file_name(&[
                                &speech.name,
                                noise_name,
                                &snr_label,
                                path.name(),
                                variant.name(),
                            ]);
                            write_wav(&dir.join(&name), 16_000, &heard)?;
                            Some(name)
                        }
                        None => None,
                    };
                    let hypothesis = if config.dump_only {
                        String::new()
                    } else {
                        transcribe(
                            &provider,
                            &heard,
                            config.language.as_deref(),
                            config.timeout,
                        )
                        .await?
                    };
                    let errors = edit_distance(&ref_words, &words(&hypothesis, turkish));
                    rows.push(Row {
                        fixture: speech.name.clone(),
                        noise: noise_name.to_owned(),
                        snr_db: *snr,
                        path: path.name(),
                        variant: variant.name(),
                        reference: reference.clone(),
                        hypothesis,
                        ref_words: ref_words.len(),
                        errors,
                        audio_file,
                    });
                }
            }
        }
        eprintln!("transcribe: {} done ({} rows)", speech.name, rows.len());
    }
    Ok(rows)
}

/// The rows as JSON lines.
///
/// # Errors
/// [`BenchError::Io`] if serialisation fails.
pub fn jsonl(rows: &[Row]) -> Result<String, BenchError> {
    let mut out = String::new();
    for row in rows {
        let line = serde_json::to_string(row).map_err(|e| BenchError::Config(e.to_string()))?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

/// (fixture, path, SNR label) to variant to (errors, reference words).
type Table = BTreeMap<(String, &'static str, String), BTreeMap<&'static str, (usize, usize)>>;

/// Word error rate by fixture, path, SNR and backend.
pub fn markdown(rows: &[Row], environment: &[String]) -> String {
    let mut out = String::from("# Noise suppression: word error rate\n\n");
    for line in environment {
        let _ = writeln!(out, "- {line}");
    }
    out.push_str(
        "\nThe clips are sent to the server as the audio of one utterance stream (20 ms chunks, then a \
         finalize). `none` is the audio as it is (what the provider hears with `denoise_stt = false`); \
         the other columns are the audio enhanced by the backend first (`denoise_stt = true`). WER = \
         (substitutions + deletions + insertions) / reference words, lowercased words only, summed over \
         the noise recordings of the row.\n\n",
    );
    // (fixture, path, snr label) -> variant -> (errors, words)
    let mut table: Table = BTreeMap::new();
    let mut variants: Vec<&str> = Vec::new();
    for r in rows {
        if !variants.contains(&r.variant) {
            variants.push(r.variant);
        }
        let label = r
            .snr_db
            .map_or_else(|| "clean".to_owned(), |s| format!("{s} dB"));
        let cell = table
            .entry((r.fixture.clone(), r.path, label))
            .or_default()
            .entry(r.variant)
            .or_default();
        cell.0 += r.errors;
        cell.1 += r.ref_words;
    }
    let _ = write!(out, "| fixture | path | SNR |");
    for v in &variants {
        let _ = write!(out, " WER {v} |");
    }
    out.push_str("\n|---|---|---|");
    for _ in &variants {
        out.push_str("---|");
    }
    out.push('\n');
    for ((fixture, path, label), by_variant) in &table {
        let _ = write!(out, "| {fixture} | {path} | {label} |");
        for v in &variants {
            match by_variant.get(v) {
                Some((errors, words)) if *words > 0 => {
                    #[allow(clippy::cast_precision_loss)]
                    let wer = 100.0 * *errors as f64 / *words as f64;
                    let _ = write!(out, " {wer:.1} % |");
                }
                _ => out.push_str(" n/a |"),
            }
        }
        out.push('\n');
    }
    out
}

///
/// # Errors
/// [`BenchError`] for a missing file.
pub fn dump_dir_exists(dir: &Path) -> bool {
    dir.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        words(s, false)
    }

    #[test]
    fn the_edit_distance_counts_substitutions_deletions_and_insertions() {
        assert_eq!(edit_distance(&w("a b c"), &w("a b c")), 0);
        assert_eq!(edit_distance(&w("a b c"), &w("a x c")), 1);
        assert_eq!(edit_distance(&w("a b c"), &w("a c")), 1);
        assert_eq!(edit_distance(&w("a c"), &w("a b c")), 1);
        assert_eq!(edit_distance(&w("a b c"), &w("")), 3);
        assert_eq!(edit_distance(&w(""), &w("a b")), 2);
    }

    /// A server that speaks the sherpa-onnx protocol: float32 audio in, `Done` ends the stream,
    /// one result with the number of samples it heard, then the close.
    async fn mock_server() -> String {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    let mut heard = 0usize;
                    while let Some(Ok(message)) = ws.next().await {
                        match message {
                            Message::Binary(b) => heard += b.len() / 4,
                            Message::Text(t) if t.as_str() == "Done" => {
                                let result = format!(
                                    "{{\"text\": \"hello world {heard}\", \"segment\": 0}}"
                                );
                                let _ = ws.send(Message::text(result)).await;
                                let _ = ws.close(None).await;
                                return;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        url
    }

    #[tokio::test]
    async fn a_clip_is_streamed_and_the_final_text_comes_back() {
        let url = mock_server().await;
        let mut config = SherpaOnnxConfig::new();
        config.base_url = url;
        let provider = SherpaOnnxStt::new(config, SystemClock::shared());
        let pcm = vec![0i16; 16_000];
        let text = transcribe(&provider, &pcm, None, Duration::from_secs(10))
            .await
            .unwrap();
        // One second of audio plus the adapter's silence padding reached the server.
        assert!(text.starts_with("hello world "), "{text}");
        let heard: usize = text.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(heard >= 16_000, "{heard} samples heard");
    }

    #[test]
    fn words_drop_case_and_punctuation() {
        assert_eq!(
            w("The U.N. also, hopes."),
            ["the", "u", "n", "also", "hopes"]
        );
        assert_eq!(words("IŞIK İyi", true), ["ışık", "iyi"]);
    }
}
