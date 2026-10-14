//! Value types of agent definition files that check themselves while the file is read, so a bad
//! value is reported at its own line and column (the TOML reader attaches the value's position
//! to every error a value raises).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use serde::de::{Deserialize, Deserializer, Error as _};

/// A duration in whole milliseconds within `MIN..=MAX` (the bounds are part of the type, so the
/// file reader rejects a value outside them at its position).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis<const MIN: u64, const MAX: u64>(u64);

impl<const MIN: u64, const MAX: u64> Millis<MIN, MAX> {
    /// `ms` when it is within the bounds.
    pub const fn new(ms: u64) -> Option<Self> {
        if ms >= MIN && ms <= MAX {
            Some(Self(ms))
        } else {
            None
        }
    }

    /// The value in milliseconds.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The value as a [`Duration`].
    pub const fn duration(self) -> Duration {
        Duration::from_millis(self.0)
    }
}

impl<'de, const MIN: u64, const MAX: u64> Deserialize<'de> for Millis<MIN, MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let ms = u64::deserialize(deserializer)?;
        Self::new(ms).ok_or_else(|| {
            D::Error::custom(format!(
                "{ms} ms is out of range: expected {MIN}..={MAX} ms"
            ))
        })
    }
}

/// A whole number within `MIN..=MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Count<const MIN: u32, const MAX: u32>(u32);

impl<const MIN: u32, const MAX: u32> Count<MIN, MAX> {
    /// `n` when it is within the bounds.
    pub const fn new(n: u32) -> Option<Self> {
        if n >= MIN && n <= MAX {
            Some(Self(n))
        } else {
            None
        }
    }

    /// The value.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The value as a `usize` (lossless on every supported target; saturates otherwise).
    pub fn as_usize(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

impl<'de, const MIN: u32, const MAX: u32> Deserialize<'de> for Count<MIN, MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let n = u64::deserialize(deserializer)?;
        u32::try_from(n)
            .ok()
            .and_then(Self::new)
            .ok_or_else(|| D::Error::custom(format!("{n} is out of range: expected {MIN}..={MAX}")))
    }
}

/// A real number within `LO / 1000 ..= HI / 1000` (the bounds are in thousandths because const
/// generics cannot be floats). Integers are accepted (`1` reads as `1.0`).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Real<const LO: u32, const HI: u32>(f32);

impl<const LO: u32, const HI: u32> Real<LO, HI> {
    /// The lower bound.
    fn lo() -> f64 {
        f64::from(LO) / 1000.0
    }

    /// The upper bound.
    fn hi() -> f64 {
        f64::from(HI) / 1000.0
    }

    /// `v` when it is finite and within the bounds.
    pub fn new(v: f64) -> Option<Self> {
        // The bounds are small (thousandths of a u32): the `as` narrowing keeps them exact
        // enough for a configuration value.
        #[allow(clippy::cast_possible_truncation)]
        let narrowed = v as f32;
        (v.is_finite() && v >= Self::lo() && v <= Self::hi()).then_some(Self(narrowed))
    }

    /// The value.
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl<'de, const LO: u32, const HI: u32> Deserialize<'de> for Real<LO, HI> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = f64::deserialize(deserializer)?;
        Self::new(v).ok_or_else(|| {
            D::Error::custom(format!(
                "{v} is out of range: expected {}..={}",
                Self::lo(),
                Self::hi()
            ))
        })
    }
}

/// Reads a string and checks it with `check`, which describes what is wrong.
fn checked_string<'de, D: Deserializer<'de>>(
    deserializer: D,
    check: impl FnOnce(&str) -> Result<(), String>,
) -> Result<String, D::Error> {
    let s = String::deserialize(deserializer)?;
    check(&s).map_err(D::Error::custom)?;
    Ok(s)
}

/// A BCP-47 style language tag such as `en`, `tr`, `en-US` or `tr-TR`: a primary subtag of 2-3
/// ASCII letters, then `-` separated subtags of 1-8 ASCII letters or digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LanguageTag(String);

impl LanguageTag {
    /// Checks the shape of `tag`.
    ///
    /// # Errors
    /// A description of what is wrong.
    pub fn parse(tag: &str) -> Result<Self, String> {
        let mut parts = tag.split('-');
        let primary = parts.next().unwrap_or("");
        let primary_ok =
            (2..=3).contains(&primary.len()) && primary.bytes().all(|b| b.is_ascii_alphabetic());
        let rest_ok = parts
            .all(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()));
        if primary_ok && rest_ok {
            Ok(Self(tag.to_owned()))
        } else {
            Err(format!(
                "`{tag}` is not a language tag (expected e.g. \"en\", \"tr\" or \"tr-TR\")"
            ))
        }
    }

    /// The tag as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The primary subtag in lower case (`tr` for `tr-TR`).
    pub fn primary(&self) -> String {
        self.0.split('-').next().unwrap_or("").to_ascii_lowercase()
    }
}

impl fmt::Display for LanguageTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LanguageTag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(D::Error::custom)
    }
}

/// Characters of a key-like value shown in messages (enough to recognise a vendor prefix such
/// as `sk-p`, far too few to use).
const SHOWN_KEY_CHARS: usize = 4;

/// Whether `s`, given where an environment variable name belongs, looks like a pasted key
/// rather than a name: longer than 32 bytes, contains `-`, or mixes lower-case letters and
/// digits. Conventional names (`DEEPGRAM_API_KEY`) are short, upper-case and never match.
pub fn looks_like_key(s: &str) -> bool {
    let lower = s.bytes().any(|b| b.is_ascii_lowercase());
    let digit = s.bytes().any(|b| b.is_ascii_digit());
    s.len() > 32 || s.contains('-') || (lower && digit)
}

/// `s` as it may appear in a message about an environment variable name: in full (escaped)
/// when it looks like a name, else only its first few characters and its length plus a hint,
/// so a key pasted into `api_key_env` never reaches a log.
pub fn shown_env_name(s: &str) -> String {
    if looks_like_key(s) {
        let prefix: String = s.chars().take(SHOWN_KEY_CHARS).collect();
        format!(
            "`{}…` ({} characters; this looks like a key, not a variable name: put the key in              an environment variable and name that variable here)",
            prefix.escape_debug(),
            s.chars().count()
        )
    } else {
        format!("`{}`", s.escape_debug())
    }
}

/// The name of an environment variable that holds a secret: `[A-Za-z_][A-Za-z0-9_]*`, at most
/// 128 bytes. The file names the variable; the value is read at load time and never stored in
/// the file.
///
/// `Debug` and [`Self::shown`] mask a name that [looks like a key](looks_like_key).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct EnvVarName(String);

impl EnvVarName {
    /// The variable name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name for a message: in backquotes, masked when it looks like a key
    /// ([`shown_env_name`]).
    pub fn shown(&self) -> String {
        shown_env_name(&self.0)
    }
}

impl fmt::Debug for EnvVarName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if looks_like_key(&self.0) {
            write!(f, "EnvVarName({})", self.shown())
        } else {
            f.debug_tuple("EnvVarName").field(&self.0).finish()
        }
    }
}

impl<'de> Deserialize<'de> for EnvVarName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        checked_string(deserializer, |s| {
            let mut bytes = s.bytes();
            let first_ok = bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
            if first_ok && s.len() <= 128 && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                Ok(())
            } else {
                Err(format!(
                    "{} is not an environment variable name (expected letters, digits and '_',                      not starting with a digit, e.g. \"DEEPGRAM_API_KEY\")",
                    shown_env_name(s)
                ))
            }
        })
        .map(Self)
    }
}

/// A name of an agent or a tool: 1-64 ASCII letters, digits, `_` or `-` (the tool name rule the
/// LLM providers share).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ident(Arc<str>);

impl Ident {
    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name as a shared string.
    pub fn shared(&self) -> Arc<str> {
        Arc::clone(&self.0)
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What is wrong with `s` as an [`Ident`], or `None` when it is a valid one.
pub fn ident_problem(s: &str) -> Option<String> {
    let ok = (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    (!ok).then(|| {
        format!(
            "`{}` is not a valid name (expected 1-64 letters, digits, '_' or '-')",
            s.escape_debug()
        )
    })
}

impl<'de> Deserialize<'de> for Ident {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        checked_string(deserializer, |s| ident_problem(s).map_or(Ok(()), Err))
            .map(|s| Self(s.into()))
    }
}

/// A provider model, voice or tier id: 1-256 bytes without whitespace or control characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Token(String);

impl Token {
    /// The id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Token {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        checked_string(deserializer, |s| {
            if (1..=256).contains(&s.len())
                && !s.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                Ok(())
            } else {
                Err(format!(
                    "`{}` is not a valid id (expected 1-256 characters without spaces)",
                    s.escape_debug()
                ))
            }
        })
        .map(Self)
    }
}

/// What a URL setting connects to: the secure scheme it takes, and the plain scheme it allows
/// only to a loopback host (a local test server). The provider adapters apply the same rule
/// when they are built; checking it while the file is read reports a wrong value at its
/// position instead of failing later without one.
trait UrlKind {
    /// The secure scheme (`wss` or `https`).
    const SECURE: &'static str;
    /// The plain scheme, accepted only for a loopback host (`ws` or `http`).
    const PLAIN: &'static str;
    /// What the setting is, for messages ("WebSocket", "HTTP").
    const WHAT: &'static str;
}

/// Longest accepted URL, in bytes.
const MAX_URL_BYTES: usize = 2048;

/// The authority of the URL part after `scheme://`: everything up to the path, query or
/// fragment (user info, host and port).
fn url_authority(rest: &str) -> &str {
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

/// The host of the URL part after `scheme://`: the authority without user info and port,
/// IPv6 brackets removed. Empty when there is none.
pub(super) fn url_host(rest: &str) -> &str {
    let authority = url_authority(rest);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    }
}

/// Checks `s` as a URL of kind `K`. The message never repeats the URL (it may carry
/// credentials); the error position points at it.
fn check_url<K: UrlKind>(s: &str) -> Result<(), String> {
    let (secure, plain, what) = (K::SECURE, K::PLAIN, K::WHAT);
    let expected =
        format!("expected a {what} URL: {secure}://host..., or {plain}:// to a loopback host");
    if s.len() > MAX_URL_BYTES {
        return Err(format!(
            "the URL is longer than {MAX_URL_BYTES} bytes ({expected})"
        ));
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "the URL contains spaces or control characters ({expected})"
        ));
    }
    let Some((scheme, rest)) = s.split_once("://") else {
        return Err(format!("not a URL ({expected})"));
    };
    let host = url_host(rest);
    if host.is_empty() {
        return Err(format!("the URL has no host ({expected})"));
    }
    if scheme == secure {
        Ok(())
    } else if scheme == plain {
        if brisvox_providers::loopback::is_loopback_host(host) {
            Ok(())
        } else {
            Err(format!(
                "plain {plain}:// is only allowed to a loopback host (localhost, 127.0.0.1,                  ::1): the API key would travel unencrypted; use {secure}://"
            ))
        }
    } else {
        let shown = if scheme.len() <= 16 && scheme.bytes().all(|b| b.is_ascii_alphanumeric()) {
            format!("`{scheme}://`")
        } else {
            "this scheme".to_owned()
        };
        Err(format!(
            "{shown} is the wrong scheme for this setting ({expected})"
        ))
    }
}

/// [`UrlKind`] of [`WsUrl`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WebSocket {}

impl UrlKind for WebSocket {
    const SECURE: &'static str = "wss";
    const PLAIN: &'static str = "ws";
    const WHAT: &'static str = "WebSocket";
}

/// [`UrlKind`] of [`HttpUrl`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Http {}

impl UrlKind for Http {
    const SECURE: &'static str = "https";
    const PLAIN: &'static str = "http";
    const WHAT: &'static str = "HTTP";
}

/// The address of a speech server run by the operator (`tts.openai_speech.base_url`):
/// `https://`, or `http://` to ANY host (a GPU box on the LAN, a container name). Plain HTTP is
/// fine here because no key travels with it; a definition that also names an API key is checked
/// by the definition checks (the key would be sent unencrypted to a non-loopback host).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalHttpUrl(String);

impl LocalHttpUrl {
    /// The URL.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the URL is `http://` to a host that is not loopback.
    pub fn is_remote_plain_http(&self) -> bool {
        self.0
            .strip_prefix("http://")
            .is_some_and(|rest| !brisvox_providers::loopback::is_loopback_host(url_host(rest)))
    }
}

/// Checks `s` as the address of a local speech server.
fn check_local_url(s: &str) -> Result<(), String> {
    let expected = "expected an HTTP URL: http://host:port/v1 or https://host/v1";
    if s.len() > MAX_URL_BYTES {
        return Err(format!(
            "the URL is longer than {MAX_URL_BYTES} bytes ({expected})"
        ));
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "the URL contains spaces or control characters ({expected})"
        ));
    }
    let Some((scheme, rest)) = s.split_once("://") else {
        return Err(format!("not a URL ({expected})"));
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!("the scheme must be http or https ({expected})"));
    }
    if url_authority(rest).contains('@') {
        // reqwest turns user info into a Basic Authorization header: sent in the clear to a
        // remote host over http://. Same rule as `LocalWsUrl`.
        return Err(format!(
            "the URL must not carry user info (user:password@): credentials do not belong in              the URL ({expected})"
        ));
    }
    if url_host(rest).is_empty() {
        return Err(format!("the URL has no host ({expected})"));
    }
    Ok(())
}

impl<'de> Deserialize<'de> for LocalHttpUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        checked_string(deserializer, check_local_url).map(Self)
    }
}

/// A WebSocket endpoint override (Deepgram and AssemblyAI STT `base_url`, every TTS `url`):
/// `wss://`, or `ws://` to a loopback host (a local test server), at most 2048 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WsUrl(String);

impl WsUrl {
    /// The URL.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WsUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        checked_string(deserializer, check_url::<WebSocket>).map(Self)
    }
}

/// The address of a self-hosted speech server that needs no key (`sherpa_onnx` and
/// `openai_realtime_stt` `base_url`): `ws://host[:port]` or `wss://host[:port]`, any host (a GPU
/// box on the LAN, a container), no path, user info or query, at most 2048 bytes. Plain `ws://`
/// is fine here because no secret travels; an adapter with an API key applies the stricter
/// loopback rule itself when it is built.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalWsUrl(String);

impl LocalWsUrl {
    /// The URL.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for LocalWsUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        checked_string(deserializer, |s| {
            let expected = "expected ws://host[:port] or wss://host[:port]";
            if s.len() > MAX_URL_BYTES {
                return Err(format!(
                    "the URL is longer than {MAX_URL_BYTES} bytes ({expected})"
                ));
            }
            if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!(
                    "the URL contains spaces or control characters ({expected})"
                ));
            }
            let Some((scheme, rest)) = s.split_once("://") else {
                return Err(format!("not a URL ({expected})"));
            };
            if !matches!(scheme, "ws" | "wss") {
                return Err(format!("the scheme must be ws or wss ({expected})"));
            }
            if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
                return Err(format!(
                    "the URL must be scheme and host[:port] only, without a path, query or \
                     user info ({expected})"
                ));
            }
            Ok(())
        })
        .map(Self)
    }
}

/// An HTTP endpoint override (Gladia STT and every LLM `base_url`): `https://`, or `http://`
/// to a loopback host (a local test server), at most 2048 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HttpUrl(String);

impl HttpUrl {
    /// The URL.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for HttpUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        checked_string(deserializer, check_url::<Http>).map(Self)
    }
}

/// A sample rate the engine supports (8000, 16000, 22050, 24000, 44100 or 48000 Hz).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rate(pub SampleRate);

impl<'de> Deserialize<'de> for Rate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hz = u64::deserialize(deserializer)?;
        u32::try_from(hz)
            .ok()
            .and_then(|hz| SampleRate::try_from(hz).ok())
            .map(Self)
            .ok_or_else(|| {
                D::Error::custom(format!(
                    "{hz} Hz is not a supported sample rate (expected 8000, 16000, 22050, \
                     24000, 44100 or 48000)"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tags() {
        for ok in ["en", "tr", "tr-TR", "en-US", "es-419", "zh-Hant-TW"] {
            assert!(LanguageTag::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "e",
            "english",
            "tr_TR",
            "tr-",
            "-tr",
            "t1",
            "tr-TOOLONGSUB",
        ] {
            assert!(LanguageTag::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(LanguageTag::parse("TR-tr").unwrap().primary(), "tr");
    }

    #[test]
    fn local_speech_urls() {
        for ok in [
            "http://localhost:8880/v1",
            "http://192.168.1.20:8880/v1",
            "http://kokoro:8880/v1",
            "https://tts.example.net/v1",
        ] {
            assert!(check_local_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "localhost:8880",
            "ftp://h/v1",
            "http:///v1",
            "http://a b/v1",
        ] {
            assert!(check_local_url(bad).is_err(), "{bad}");
        }
        // User info would become a Basic Authorization header sent in the clear.
        for userinfo in [
            "http://user:pw@gpu.lan:8880/v1",
            "http://user@gpu.lan/v1",
            "https://user:pw@tts.example.net/v1",
            "http://@gpu.lan/v1",
        ] {
            let err = check_local_url(userinfo).unwrap_err();
            assert!(err.contains("user info"), "{userinfo}: {err}");
            assert!(
                !err.contains("pw"),
                "the message must not echo the password"
            );
        }
        // An `@` after the authority (path, query) is not user info.
        assert!(check_local_url("http://gpu.lan/v1/a@b").is_ok());
        assert!(check_local_url("http://gpu.lan/v1?x=a@b").is_ok());
        let remote = LocalHttpUrl("http://192.168.1.20:8880/v1".to_owned());
        let loopback = LocalHttpUrl("http://localhost:8880/v1".to_owned());
        let secure = LocalHttpUrl("https://tts.example.net/v1".to_owned());
        assert!(remote.is_remote_plain_http());
        assert!(!loopback.is_remote_plain_http());
        assert!(!secure.is_remote_plain_http());
    }

    #[test]
    fn bounds_are_inclusive() {
        assert!(Millis::<5, 10>::new(5).is_some());
        assert!(Millis::<5, 10>::new(10).is_some());
        assert!(Millis::<5, 10>::new(11).is_none());
        assert!(Count::<1, 3>::new(0).is_none());
        assert!(Real::<700, 1200>::new(0.7).is_some());
        assert!(Real::<700, 1200>::new(1.21).is_none());
        assert!(Real::<0, 1000>::new(f64::NAN).is_none());
    }
}
