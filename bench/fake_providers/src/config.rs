//! Command line and the latencies of the fakes.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// How the fake LLM cuts its reply into streamed tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSplit {
    /// A word (with its leading space) or a punctuation mark per token, like a real tokenizer.
    Words,
    /// The whole reply in one token (what the in-process mocks do).
    Single,
}

/// Everything the fakes need to know.
#[derive(Debug, Clone)]
pub struct Config {
    /// TLS listener (`wss://` and `https://`, ALPN `h2` and `http/1.1`); `None` disables it.
    pub tls_addr: Option<SocketAddr>,
    /// Plain listener (`ws://`, `http://`, `/metrics`, `/healthz`, `/ca.pem`); `None` disables it.
    pub plain_addr: Option<SocketAddr>,
    /// Directory of the fixture JSON sidecars.
    pub fixtures_dir: PathBuf,
    /// Fixture whose utterances the fake STT "hears" and the fake LLM answers.
    pub fixture: String,
    /// Directory the CA and the server certificate are kept in (reused when present).
    pub ca_dir: Option<PathBuf>,
    /// Names and addresses of the server certificate.
    pub sans: Vec<String>,
    /// Maximum open TCP connections (further ones are closed at accept).
    pub max_connections: usize,
    /// HTTP/2 `SETTINGS_MAX_CONCURRENT_STREAMS` advertised on the TLS port (real API gateways
    /// advertise 100 to 256); a client multiplexing more requests than this queues them.
    pub h2_max_streams: u32,
    /// STT: spacing of interim transcripts after the first loud audio.
    pub stt_interim: Duration,
    /// STT: final transcript this long after the end of speech it detects, or after a `Finalize`.
    pub stt_final: Duration,
    /// STT: RMS (PCM16 units) at or above which a 32 ms window is speech.
    pub vad_rms: f64,
    /// LLM: first token this long after the request.
    pub llm_first_token: Duration,
    /// LLM: spacing of the following tokens.
    pub llm_inter_token: Duration,
    /// LLM: token cutting.
    pub llm_tokens: TokenSplit,
    /// LLM: answer to an English utterance (and to one it cannot match).
    pub reply_en: String,
    /// LLM: answer to a Turkish utterance.
    pub reply_tr: String,
    /// TTS: first audio this long after text arrives at an idle connection.
    pub tts_first_audio: Duration,
    /// TTS: audio per binary frame.
    pub tts_chunk: Duration,
    /// TTS: spacing of the binary frames (a chunk of 40 ms every 20 ms is 2x real time).
    pub tts_interval: Duration,
    /// TTS: audio per character of text, in milliseconds.
    pub tts_ms_per_char: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tls_addr: Some(SocketAddr::from(([0, 0, 0, 0], 8443))),
            plain_addr: Some(SocketAddr::from(([0, 0, 0, 0], 8080))),
            fixtures_dir: PathBuf::from("fixtures"),
            fixture: "en_two_utterances".to_owned(),
            ca_dir: None,
            sans: ["localhost", "127.0.0.1", "::1", "fakes", "fake-providers"]
                .map(str::to_owned)
                .to_vec(),
            max_connections: 20_000,
            h2_max_streams: 100,
            stt_interim: Duration::from_millis(100),
            stt_final: Duration::from_millis(150),
            vad_rms: 500.0,
            llm_first_token: Duration::from_millis(350),
            llm_inter_token: Duration::from_millis(30),
            llm_tokens: TokenSplit::Words,
            reply_en: "Okay, got it.".to_owned(),
            reply_tr: "Peki, anlad\u{131}m.".to_owned(),
            tts_first_audio: Duration::from_millis(150),
            tts_chunk: Duration::from_millis(40),
            tts_interval: Duration::from_millis(20),
            tts_ms_per_char: 60,
        }
    }
}

/// Usage text (printed on `--help` and on a bad flag).
pub const USAGE: &str = "usage: brisvox-fake-providers [flags]
  --tls-addr ADDR        TLS listener, wss:// and https:// (default 0.0.0.0:8443; 'off' disables)
  --plain-addr ADDR      plain listener, ws:// http:// /metrics /healthz /ca.pem (default
                         0.0.0.0:8080, or 0.0.0.0:$PORT when PORT is set; 'off' disables)
  --fixtures-dir DIR     fixture JSON sidecars (default fixtures)
  --fixture NAME         the script of every call (default en_two_utterances)
  --ca-dir DIR           keep the CA and server certificate here and reuse them (default: new at start)
  --san NAME             certificate name or IP, repeatable (default localhost 127.0.0.1 ::1 fakes
                         fake-providers); the list is replaced by the first --san
  --max-connections N    open TCP connections (default 20000)
  --h2-max-streams N     HTTP/2 concurrent streams per connection (default 100)
  --stt-interim-ms MS    spacing of interim transcripts (default 100)
  --stt-final-ms MS      final transcript this long after the end of speech / a Finalize (default 150)
  --vad-rms RMS          speech threshold in PCM16 RMS units (default 500)
  --llm-first-token-ms MS  (default 350)
  --llm-inter-token-ms MS  (default 30)
  --llm-tokens words|single  (default words)
  --reply-en TEXT / --reply-tr TEXT  (defaults 'Okay, got it.' / 'Peki, anladim.')
  --tts-first-audio-ms MS  (default 150)
  --tts-chunk-ms MS      audio per binary frame (default 40)
  --tts-interval-ms MS   spacing of binary frames (default 20)
  --tts-ms-per-char MS   audio per character of text (default 60)
";

fn parse_addr(value: &str) -> Result<Option<SocketAddr>, String> {
    if value == "off" {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|e| format!("bad address {value:?}: {e}"))
}

fn parse_ms(flag: &str, value: &str) -> Result<Duration, String> {
    value
        .parse::<u64>()
        .map(Duration::from_millis)
        .map_err(|_| format!("{flag} wants a number of milliseconds, got {value:?}"))
}

fn parse_num<T: std::str::FromStr>(flag: &str, value: &str) -> Result<T, String> {
    value
        .parse::<T>()
        .map_err(|_| format!("{flag}: bad number {value:?}"))
}

impl Config {
    /// Parses `args` (without the program name) over the defaults; `port_env` is `$PORT`.
    ///
    /// # Errors
    /// A message for an unknown flag or a bad value; `Ok(None)` for `--help`.
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        port_env: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let mut config = Self::default();
        if let Some(port) = port_env.filter(|p| !p.is_empty()) {
            let port: u16 = parse_num("PORT", port)?;
            config.plain_addr = Some(SocketAddr::from(([0, 0, 0, 0], port)));
        }
        let mut sans_set = false;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            if flag == "--help" || flag == "-h" {
                return Ok(None);
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--tls-addr" => config.tls_addr = parse_addr(&value)?,
                "--plain-addr" => config.plain_addr = parse_addr(&value)?,
                "--fixtures-dir" => config.fixtures_dir = PathBuf::from(value),
                "--fixture" => config.fixture = value,
                "--ca-dir" => config.ca_dir = Some(PathBuf::from(value)),
                "--san" => {
                    if !sans_set {
                        config.sans.clear();
                        sans_set = true;
                    }
                    config.sans.push(value);
                }
                "--max-connections" => config.max_connections = parse_num(&flag, &value)?,
                "--h2-max-streams" => config.h2_max_streams = parse_num(&flag, &value)?,
                "--stt-interim-ms" => config.stt_interim = parse_ms(&flag, &value)?,
                "--stt-final-ms" => config.stt_final = parse_ms(&flag, &value)?,
                "--vad-rms" => config.vad_rms = parse_num(&flag, &value)?,
                "--llm-first-token-ms" => config.llm_first_token = parse_ms(&flag, &value)?,
                "--llm-inter-token-ms" => config.llm_inter_token = parse_ms(&flag, &value)?,
                "--llm-tokens" => {
                    config.llm_tokens = match value.as_str() {
                        "words" => TokenSplit::Words,
                        "single" => TokenSplit::Single,
                        other => {
                            return Err(format!("--llm-tokens: words or single, got {other:?}"));
                        }
                    };
                }
                "--reply-en" => config.reply_en = value,
                "--reply-tr" => config.reply_tr = value,
                "--tts-first-audio-ms" => config.tts_first_audio = parse_ms(&flag, &value)?,
                "--tts-chunk-ms" => config.tts_chunk = parse_ms(&flag, &value)?,
                "--tts-interval-ms" => config.tts_interval = parse_ms(&flag, &value)?,
                "--tts-ms-per-char" => config.tts_ms_per_char = parse_num(&flag, &value)?,
                other => return Err(format!("unknown flag {other:?}")),
            }
        }
        if config.tts_chunk.is_zero() || config.tts_interval.is_zero() {
            return Err("--tts-chunk-ms and --tts-interval-ms must be above zero".to_owned());
        }
        if config.max_connections == 0 {
            return Err("--max-connections must be above zero".to_owned());
        }
        Ok(Some(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], port: Option<&str>) -> Result<Option<Config>, String> {
        Config::parse(args.iter().map(|s| (*s).to_owned()), port)
    }

    #[test]
    fn defaults_and_overrides() {
        let c = parse(&[], None).unwrap().unwrap();
        assert_eq!(c.llm_first_token, Duration::from_millis(350));
        assert_eq!(c.plain_addr.unwrap().port(), 8080);
        let c = parse(
            &["--llm-first-token-ms", "10", "--tls-addr", "off"],
            Some("9000"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.llm_first_token, Duration::from_millis(10));
        assert!(c.tls_addr.is_none());
        assert_eq!(c.plain_addr.unwrap().port(), 9000);
    }

    #[test]
    fn first_san_replaces_the_default_list() {
        let c = parse(&["--san", "a", "--san", "b"], None).unwrap().unwrap();
        assert_eq!(c.sans, ["a", "b"]);
    }

    #[test]
    fn bad_input_is_an_error() {
        assert!(parse(&["--nope", "1"], None).is_err());
        assert!(parse(&["--stt-final-ms", "x"], None).is_err());
        assert!(parse(&["--stt-final-ms"], None).is_err());
        assert!(parse(&["--tts-chunk-ms", "0"], None).is_err());
        assert!(parse(&["--help"], None).unwrap().is_none());
    }
}
