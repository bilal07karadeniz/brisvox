//! `selftest`: a load generator for the fakes themselves, so a capacity run can show the fakes
//! are not the bottleneck. It simulates calls the way an engine uses the providers: per call one
//! STT websocket fed in real time (3 s of loud audio, 1.5 s of silence, twice), one TTS websocket
//! (`Speak` + `Flush` after every final) and, on a plain URL, one chat request per turn.
//! It reports how late the fakes are against their own promises (final 150 ms after the endpoint,
//! first audio 150 ms, first token 350 ms), at the concurrency asked for.
//!
//! This is a measurement tool: it runs outside the engine, reads the wall clock and prints.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

/// Command line of `selftest`.
pub struct Options {
    /// `ws://host:port` or `wss://host:port` of a running server.
    pub url: String,
    /// CA certificate PEM for `wss://`.
    pub ca: Option<String>,
    /// Concurrent calls.
    pub calls: usize,
    /// Seconds of load.
    pub seconds: u64,
}

impl Options {
    /// Parses `--url`, `--ca`, `--calls`, `--seconds`.
    ///
    /// # Errors
    /// A message for an unknown flag or a bad value.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut o = Self {
            url: "ws://127.0.0.1:8080".to_owned(),
            ca: None,
            calls: 100,
            seconds: 20,
        };
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--url" => o.url = value,
                "--ca" => o.ca = Some(value),
                "--calls" => {
                    o.calls = value
                        .parse()
                        .map_err(|_| "--calls: bad number".to_owned())?
                }
                "--seconds" => {
                    o.seconds = value
                        .parse()
                        .map_err(|_| "--seconds: bad number".to_owned())?
                }
                other => return Err(format!("unknown selftest flag {other:?}")),
            }
        }
        Ok(o)
    }
}

#[derive(Default)]
struct Stats {
    final_late_ms: Mutex<Vec<u32>>,
    tts_first_ms: Mutex<Vec<u32>>,
    tts_flushed_ms: Mutex<Vec<u32>>,
    llm_first_ms: Mutex<Vec<u32>>,
    calls: AtomicU64,
    turns: AtomicU64,
    errors: AtomicU64,
}

async fn push(v: &Mutex<Vec<u32>>, ms: u128) {
    v.lock().await.push(u32::try_from(ms).unwrap_or(u32::MAX));
}

fn frame(loud: bool) -> Bytes {
    // 20 ms at 16 kHz
    let mut bytes = Vec::with_capacity(640);
    for i in 0..320_i32 {
        let v: i16 = if loud && (i / 10) % 2 == 0 {
            3000
        } else if loud {
            -3000
        } else {
            0
        };
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Bytes::from(bytes)
}

type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn dial(url: &str, connector: &Option<Connector>) -> Result<Client, String> {
    let fut = connect_async_tls_with_config(url, None, true, connector.clone());
    match timeout(Duration::from_secs(15), fut).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("connect timed out".to_owned()),
    }
}

/// A plain-HTTP chat request; returns the time to the first byte of the body.
async fn chat(authority: &str, user: &str) -> Result<Duration, String> {
    let body = serde_json::json!({
        "model": "fake-model", "stream": true, "stream_options": {"include_usage": true},
        "messages": [{"role": "user", "content": user}],
    })
    .to_string();
    let mut tcp = TcpStream::connect(authority)
        .await
        .map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let start = Instant::now();
    tcp.write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut first: Option<Duration> = None;
    let mut seen = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        let n = timeout(Duration::from_secs(15), tcp.read(&mut buf))
            .await
            .map_err(|_| "chat read timed out".to_owned())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let chunk = buf.get(..n).unwrap_or(&[]);
        seen.extend_from_slice(chunk);
        if first.is_none() && seen.windows(5).any(|w| w == b"data:") {
            first = Some(start.elapsed());
        }
        if seen.windows(6).any(|w| w == b"[DONE]") {
            break;
        }
    }
    first.ok_or_else(|| "no stream".to_owned())
}

async fn tts_turn(tts: &mut Client, stats: &Stats) -> Result<(), String> {
    let t0 = Instant::now();
    let speak = r#"{"type":"Speak","text":"Okay, got it."}"#;
    tts.send(Message::Text(speak.into()))
        .await
        .map_err(|e| e.to_string())?;
    tts.send(Message::Text(r#"{"type":"Flush"}"#.into()))
        .await
        .map_err(|e| e.to_string())?;
    let mut first = false;
    loop {
        let message = timeout(Duration::from_secs(10), tts.next())
            .await
            .map_err(|_| "tts timed out".to_owned())?
            .ok_or_else(|| "tts closed".to_owned())?
            .map_err(|e| e.to_string())?;
        match message {
            Message::Binary(_) if !first => {
                first = true;
                push(&stats.tts_first_ms, t0.elapsed().as_millis()).await;
            }
            Message::Text(t) if t.contains("Flushed") => {
                push(&stats.tts_flushed_ms, t0.elapsed().as_millis()).await;
                return Ok(());
            }
            _ => {}
        }
    }
}

async fn call(
    url: String,
    authority: Option<String>,
    connector: Option<Connector>,
    stats: Arc<Stats>,
    stop: Instant,
) {
    let tts_url =
        format!("{url}/v1/speak?model=aura-2-thalia-en&encoding=linear16&sample_rate=24000");
    let stt_url = format!(
        "{url}/v1/listen?model=nova-3&encoding=linear16&sample_rate=16000&endpointing=300&interim_results=true"
    );
    let (loud, quiet) = (frame(true), frame(false));
    while Instant::now() < stop {
        let (stt, tts) = tokio::join!(dial(&stt_url, &connector), dial(&tts_url, &connector));
        let (Ok(stt), Ok(mut tts)) = (stt, tts) else {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        stats.calls.fetch_add(1, Ordering::Relaxed);
        let (mut tx, mut rx) = stt.split();
        // Absolute 20 ms schedule: a late tick is followed by an early one, so the audio keeps its
        // real-time rate even where the timer is coarse (Windows).
        let mut next_frame = tokio::time::Instant::now();
        // Per turn: 0.5 s silence, 3 s speech, 1.5 s silence; two turns, then the call ends.
        let mut frame_no: u64 = 0;
        let mut speech_end: Option<Instant> = None;
        let mut turns_done = 0;
        let total_frames = 2 * 250;
        'call: while frame_no < total_frames && Instant::now() < stop {
            tokio::select! {
                () = tokio::time::sleep_until(next_frame) => {
                    next_frame += Duration::from_millis(20);
                    let in_turn = frame_no % 250;
                    let is_loud = (25..175).contains(&in_turn);
                    if in_turn == 175 {
                        speech_end = Some(Instant::now());
                    }
                    let f = if is_loud { loud.clone() } else { quiet.clone() };
                    if tx.send(Message::Binary(f)).await.is_err() {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        break 'call;
                    }
                    frame_no += 1;
                }
                message = rx.next() => {
                    let Some(Ok(Message::Text(text))) = message else {
                        if message.is_none() || matches!(message, Some(Err(_))) { stats.errors.fetch_add(1, Ordering::Relaxed); break 'call; }
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                    let is_final = value.get("is_final").and_then(Value::as_bool) == Some(true);
                    let has_text = value.pointer("/channel/alternatives/0/transcript").and_then(Value::as_str).is_some_and(|t| !t.is_empty());
                    if is_final && has_text {
                        if let Some(end) = speech_end.take() {
                            push(&stats.final_late_ms, end.elapsed().as_millis()).await;
                        }
                        let user = value.pointer("/channel/alternatives/0/transcript").and_then(Value::as_str).unwrap_or("").to_owned();
                        if let Some(authority) = &authority {
                            match chat(authority, &user).await {
                                Ok(d) => push(&stats.llm_first_ms, d.as_millis()).await,
                                Err(_) => { stats.errors.fetch_add(1, Ordering::Relaxed); }
                            }
                        }
                        if tts_turn(&mut tts, &stats).await.is_err() {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            break 'call;
                        }
                        stats.turns.fetch_add(1, Ordering::Relaxed);
                        turns_done += 1;
                    }
                }
            }
        }
        let _ = turns_done;
        let _ = tx
            .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
            .await;
        let _ = tts.send(Message::Text(r#"{"type":"Close"}"#.into())).await;
    }
}

fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    // Sample counts are far below 2^52.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted
        .get(rank.saturating_sub(1).min(sorted.len() - 1))
        .copied()
        .unwrap_or(0)
}

async fn report(name: &str, v: &Mutex<Vec<u32>>, promise_ms: u32) {
    let mut v = v.lock().await.clone();
    v.sort_unstable();
    println!(
        "{name:<28} n={:<6} p50={:>5} ms  p99={:>5} ms  max={:>5} ms   (promise {promise_ms} ms)",
        v.len(),
        percentile(&v, 50.0),
        percentile(&v, 99.0),
        v.last().copied().unwrap_or(0)
    );
}

/// Runs the load.
///
/// # Errors
/// A message for an unreadable CA file or a bad URL.
pub async fn run(options: Options) -> Result<(), String> {
    let connector = match &options.ca {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(&pem) {
                roots
                    .add(cert.map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            }
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
            Some(Connector::Rustls(Arc::new(config)))
        }
        None => None,
    };
    let authority = options.url.strip_prefix("ws://").map(str::to_owned);
    let stats = Arc::new(Stats::default());
    let stop = Instant::now() + Duration::from_secs(options.seconds);
    println!(
        "selftest: {} calls for {} s against {}",
        options.calls, options.seconds, options.url
    );
    let mut tasks = Vec::new();
    for i in 0..options.calls {
        let (url, authority, connector, stats) = (
            options.url.clone(),
            authority.clone(),
            connector.clone(),
            Arc::clone(&stats),
        );
        tasks.push(tokio::spawn(async move {
            // Spread the starts over 5 s like calls arriving.
            tokio::time::sleep(Duration::from_millis(
                u64::try_from(i % 500).unwrap_or(0) * 10,
            ))
            .await;
            call(url, authority, connector, stats, stop).await;
        }));
    }
    for task in tasks {
        let _ = task.await;
    }
    println!(
        "calls started {}  turns {}  errors {}",
        stats.calls.load(Ordering::Relaxed),
        stats.turns.load(Ordering::Relaxed),
        stats.errors.load(Ordering::Relaxed)
    );
    report("STT final after end of speech", &stats.final_late_ms, 470).await;
    report("TTS first audio", &stats.tts_first_ms, 150).await;
    report("TTS Flushed", &stats.tts_flushed_ms, 550).await;
    report("LLM first byte", &stats.llm_first_ms, 350).await;
    Ok(())
}
