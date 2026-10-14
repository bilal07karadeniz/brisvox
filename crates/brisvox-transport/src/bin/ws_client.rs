//! `ws_client`: the reference client of the brisvox websocket transport.
//!
//! Streams a mono 16-bit WAV file to a server in real-time 20 ms frames, answers its marks and
//! writes the bot audio it received to a WAV file. See `docs/WEBSOCKET_PROTOCOL.md`.
//!
//! ```text
//! ws_client <URL> <INPUT.wav> <OUTPUT.wav> [--encoding pcm16|mulaw] [--no-realtime]
//!           [--linger-ms N] [--dtmf KEYS] [--meta KEY=VALUE]...
//! ```
//!
//! Exit codes: 0 success, 1 the call failed (a fatal server error, a broken connection, the server
//! closing the call before all input was sent, the maximum duration; the recording is still
//! written), 2 bad usage or an unreadable file.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::Duration;

use brisvox_core::SystemClock;
use brisvox_transport::wav;
use brisvox_transport::websocket::Encoding;
use brisvox_transport::websocket::client::{self, ClientConfig};
use tokio_util::sync::CancellationToken;

const USAGE: &str = "usage: ws_client <URL> <INPUT.wav> <OUTPUT.wav> [--encoding pcm16|mulaw] \
[--no-realtime] [--linger-ms N] [--dtmf KEYS] [--meta KEY=VALUE]...";

/// Parsed command line.
struct Args {
    url: String,
    input: String,
    output: String,
    encoding: Encoding,
    realtime: bool,
    linger: Option<Duration>,
    dtmf: String,
    metadata: BTreeMap<String, String>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut positional = Vec::new();
    let mut parsed = Args {
        url: String::new(),
        input: String::new(),
        output: String::new(),
        encoding: Encoding::Pcm16,
        realtime: true,
        linger: None,
        dtmf: String::new(),
        metadata: BTreeMap::new(),
    };
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--encoding" => {
                parsed.encoding = match args.next().as_deref() {
                    Some("pcm16") => Encoding::Pcm16,
                    Some("mulaw") => Encoding::Mulaw,
                    _ => return Err("--encoding takes pcm16 or mulaw".to_owned()),
                };
            }
            "--no-realtime" => parsed.realtime = false,
            "--linger-ms" => {
                let ms = args
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or("--linger-ms takes a number of milliseconds")?;
                parsed.linger = Some(Duration::from_millis(ms));
            }
            "--dtmf" => parsed.dtmf = args.next().ok_or("--dtmf takes the keys to press")?,
            "--meta" => {
                let pair = args.next().ok_or("--meta takes KEY=VALUE")?;
                let (key, value) = pair.split_once('=').ok_or("--meta takes KEY=VALUE")?;
                parsed.metadata.insert(key.to_owned(), value.to_owned());
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ => positional.push(arg),
        }
    }
    let mut positional = positional.into_iter();
    match (
        positional.next(),
        positional.next(),
        positional.next(),
        positional.next(),
    ) {
        (Some(url), Some(input), Some(output), None) => {
            parsed.url = url;
            parsed.input = input;
            parsed.output = output;
            Ok(parsed)
        }
        _ => Err("expected exactly three arguments".to_owned()),
    }
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("ws_client: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let input = match std::fs::read(&args.input)
        .map_err(|e| e.to_string())
        .and_then(|bytes| wav::parse(&bytes).map_err(|e| e.to_string()))
    {
        Ok(input) => input,
        Err(message) => {
            eprintln!("ws_client: cannot read {}: {message}", args.input);
            return ExitCode::from(2);
        }
    };

    let mut config = ClientConfig::new(args.url);
    config.sample_rate = input.sample_rate;
    config.encoding = args.encoding;
    config.realtime = args.realtime;
    config.dtmf = args.dtmf;
    config.metadata = args.metadata;
    if let Some(linger) = args.linger {
        config.linger = linger;
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ws_client: cannot start the runtime: {error}");
            return ExitCode::from(1);
        }
    };
    let cancel = CancellationToken::new();
    let outcome = runtime.block_on(client::run(
        SystemClock::shared(),
        &config,
        &input.pcm,
        &cancel,
    ));
    let report = match outcome {
        Ok(report) => report,
        Err(error) => {
            eprintln!("ws_client: {error}");
            return ExitCode::from(1);
        }
    };

    let recording = wav::encode(config.sample_rate, &report.audio);
    if let Err(error) = std::fs::write(&args.output, recording) {
        eprintln!("ws_client: cannot write {}: {error}", args.output);
        return ExitCode::from(2);
    }
    println!(
        "call {}: sent {} frames, received {} audio frames ({} bytes), {} marks echoed, {} clears, stop: {}",
        report.call_id,
        report.frames_sent,
        report.audio_frames_received,
        report.audio.len(),
        report.marks_echoed.len(),
        report.clears.len(),
        report.stop_reason.as_deref().unwrap_or("none"),
    );
    for error in &report.errors {
        eprintln!(
            "server {} error {}: {}",
            if error.fatal { "fatal" } else { "reported" },
            error.code,
            error.message
        );
    }
    // The recording is written either way: what was heard before a failure is still evidence.
    if let Some(reason) = report.failure() {
        eprintln!("ws_client: the call failed: {reason}");
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
