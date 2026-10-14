//! Redaction of log lines (conventions in `docs/TRACING.md`).
//!
//! The first line of defence is the type system: credentials live in
//! `brisvox_core::Secret`, whose `Debug`/`Display` print `[REDACTED]`. This is the second one,
//! for the credentials that reach a log line some other way (a provider's error text echoing a
//! request, a URL with `?key=...`, a header dump, a value pasted into a `format!`): the log writer
//! scrubs every line before it is written.
//!
//! A [`Redactor`] removes
//! - the exact values of the process's secrets (environment variables whose names say `KEY`,
//!   `TOKEN`, `SECRET`, `PASSWORD`, `CREDENTIAL`, ...; [`Redactor::from_vars`]), wherever they
//!   appear, in any encoding that keeps them contiguous;
//! - the value after a credential-looking name (`api_key=...`, `"token":"..."`,
//!   `Authorization: Bearer ...`, `x-api-key: ...`, `password=...`);
//! - `Bearer` and `Basic` credentials, `?key=`/`&sig=`-style query values, and the password in
//!   `scheme://user:password@host` URLs;
//! - SDES-SRTP key material: the base64 after `inline:` in an SDP `a=crypto` line
//!   (RFC 4568), wherever it shows up, so an SDP dumped into a log never carries a media key.
//!
//! It over-redacts rather than under-redacts: a log line that happens to say `token: 5` loses the
//! `5`. With [`Redactor::with_pii`] (the opt-in `BRISVOX_LOG_REDACT_PII`, `docs/PRIVACY.md`) the
//! personal identifiers of the call recording's filter (emails, IBANs, ID, card and phone
//! numbers) are scrubbed too. [`RedactingMakeWriter`] applies a redactor to every line the tracing subscriber writes.
//! The scrubbing costs one scan per log line (logs are per event, never per audio frame).

use std::borrow::Cow;
use std::io;
use std::sync::Arc;

use tracing_subscriber::fmt::MakeWriter;

use crate::privacy::{PiiKinds, redact_pii_with};

/// What replaces a removed value.
pub const REDACTED: &str = "[REDACTED]";

/// Secrets shorter than this are not scrubbed by value (too many false hits: a value of `1` or
/// `on` is not a credential).
const MIN_SECRET_LEN: usize = 8;

/// Environment variable name parts that mark a value as a secret.
const SECRET_NAME_PARTS: [&str; 7] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH_",
];

/// Names after which a value is a credential, wherever the name ends (`deepgram_api_key`,
/// `X-Api-Key`, `client_secret`). Matched case-insensitively; the name must be followed by an
/// optional closing quote, optional spaces and `=` or `:`.
const KEY_NAMES: [&str; 11] = [
    "api_key",
    "api-key",
    "apikey",
    "secret",
    "token",
    "password",
    "passwd",
    "authorization",
    "private_key",
    "credential",
    "credentials",
];

/// Query parameters whose value is a credential when written `?name=` or `&name=`.
const QUERY_NAMES: [&str; 8] = [
    "key",
    "sig",
    "signature",
    "auth",
    "access_token",
    "code",
    "sas",
    "jwt",
];

/// Removes credentials from text (see the [module docs](self)).
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Exact secret values, longest first so a secret that contains another goes whole.
    secrets: Vec<String>,
    /// Also scrub these kinds of personal identifier; `None` (the default) leaves them.
    pii: Option<PiiKinds>,
}

fn is_value_end(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '&' | ',' | ';' | '}' | ']' | ')' | '\\' | '<' | '>'
        )
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '+' | '/' | '=')
}

impl Redactor {
    /// A redactor that knows no secret values; the name- and shape-based rules still apply.
    pub fn new() -> Self {
        Self::default()
    }

    /// Also scrubs the personal identifiers of `kinds` (emails, IBANs, national IDs, card and
    /// phone numbers; [`crate::privacy::redact_pii_with`]) from every line. Off by default: a
    /// number of seven or more digits is replaced even when it is a counter or a duration.
    #[must_use]
    pub const fn with_pii(mut self, kinds: PiiKinds) -> Self {
        self.pii = Some(kinds);
        self
    }

    /// The kinds of personal identifier this redactor scrubs; `None`: none.
    pub const fn pii_kinds(&self) -> Option<PiiKinds> {
        self.pii
    }

    /// Also scrubs `value` wherever it appears. Values under 8 bytes are ignored.
    #[must_use]
    pub fn with_secret(mut self, value: &str) -> Self {
        if value.len() >= MIN_SECRET_LEN && !self.secrets.iter().any(|s| s == value) {
            self.secrets.push(value.to_owned());
            self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
        self
    }

    /// A redactor for the values of the variables in `vars` whose names mark them as secrets.
    pub fn from_vars<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        vars.into_iter()
            .fold(Self::new(), |redactor, (name, value)| {
                let name = name.as_ref().to_ascii_uppercase();
                if SECRET_NAME_PARTS.iter().any(|part| name.contains(part)) {
                    redactor.with_secret(value.as_ref().trim())
                } else {
                    redactor
                }
            })
    }

    /// [`Self::from_vars`] over the process environment.
    pub fn from_process_env() -> Self {
        Self::from_vars(
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))),
        )
    }

    /// `text` with every credential replaced by [`REDACTED`]; borrowed when there is none.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let credentials = self.redact_credentials(text);
        let Some(kinds) = self.pii else {
            return credentials;
        };
        match redact_pii_with(&credentials, kinds) {
            Cow::Borrowed(_) => credentials,
            Cow::Owned(owned) => Cow::Owned(owned),
        }
    }

    fn redact_credentials<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut text = Cow::Borrowed(text);
        for secret in &self.secrets {
            if text.contains(secret.as_str()) {
                text = Cow::Owned(text.replace(secret.as_str(), REDACTED));
            }
        }
        let ranges = credential_ranges(&text);
        if ranges.is_empty() {
            return text;
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (start, end) in ranges {
            if let Some(kept) = text.get(at..start) {
                out.push_str(kept);
            }
            out.push_str(REDACTED);
            at = end;
        }
        if let Some(rest) = text.get(at..) {
            out.push_str(rest);
        }
        Cow::Owned(out)
    }
}

/// The byte ranges of `text` that hold credentials, sorted and not overlapping.
fn credential_ranges(text: &str) -> Vec<(usize, usize)> {
    let lower = text.to_ascii_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // The ASCII-lowercased copy has the same byte offsets as `text`.
    bearer_and_basic(text, &lower, &mut ranges);
    for name in KEY_NAMES {
        named_values(text, &lower, name, &mut ranges);
    }
    query_values(text, &lower, &mut ranges);
    url_passwords(text, &mut ranges);
    sdes_keys(text, &lower, &mut ranges);
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The end of the run of characters satisfying `keep` that starts at `from`.
fn run_end(text: &str, from: usize, keep: impl Fn(char) -> bool) -> usize {
    text.get(from..).map_or(from, |rest| {
        rest.char_indices()
            .find(|(_, c)| !keep(*c))
            .map_or(text.len(), |(i, _)| from + i)
    })
}

/// `Bearer <token>` and `Basic <base64>` (at least 8 characters of token).
fn bearer_and_basic(text: &str, lower: &str, ranges: &mut Vec<(usize, usize)>) {
    for scheme in ["bearer ", "basic "] {
        for (at, _) in lower.match_indices(scheme) {
            let start = at + scheme.len();
            let end = run_end(text, start, is_token_char);
            if end - start >= MIN_SECRET_LEN {
                ranges.push((start, end));
            }
        }
    }
}

/// Skips ANSI colour sequences (`ESC [ ... letter`: the text log format puts them around field
/// names and the `=`), quote marks, backslashes and spaces, from `at`.
fn skip_decoration(text: &str, mut at: usize) -> usize {
    const ESC: char = '\u{1b}';
    loop {
        let Some(rest) = text.get(at..) else {
            return at;
        };
        if let Some(params) = rest.strip_prefix(ESC).and_then(|r| r.strip_prefix('[')) {
            // `params` follows `ESC [`: parameter bytes, then one final letter.
            let end = params
                .char_indices()
                .find(|(_, c)| c.is_ascii_alphabetic())
                .map_or(params.len(), |(i, c)| i + c.len_utf8());
            at = text.len() - params.len() + end;
        } else if rest.starts_with(['"', '\'', '\\', ' ']) {
            at += 1;
        } else {
            return at;
        }
    }
}

/// The value after `name` + decoration + (`=` | `:`) + decoration.
fn value_after(text: &str, name_end: usize) -> Option<(usize, usize)> {
    let mut at = skip_decoration(text, name_end);
    if !matches!(text.get(at..)?.chars().next()?, '=' | ':') {
        return None;
    }
    at = skip_decoration(text, at + 1);
    let end = run_end(text, at, |c| !is_value_end(c));
    (end > at).then_some((at, end))
}

fn named_values(text: &str, lower: &str, name: &str, ranges: &mut Vec<(usize, usize)>) {
    for (at, _) in lower.match_indices(name) {
        let name_end = at + name.len();
        // A longer word (`tokens`, `secrets_dir`) is not the name.
        let next = text.get(name_end..).and_then(|rest| rest.chars().next());
        if next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let Some((start, mut end)) = value_after(text, name_end) else {
            continue;
        };
        let value = text.get(start..end).unwrap_or_default();
        if value.starts_with("[REDACTED") {
            continue;
        }
        // `Authorization: Bearer abc`: the scheme is no credential, the token after it is.
        // The same goes for any other scheme of an authorization header (`Token <key>`, the one
        // Deepgram uses, `ApiKey <key>`, ...): the word after the header name is a scheme.
        let scheme_word = value.chars().all(|c| c.is_ascii_alphabetic());
        if value.eq_ignore_ascii_case("bearer")
            || value.eq_ignore_ascii_case("basic")
            || (name.ends_with("authorization") && scheme_word)
        {
            let token = run_end(text, end, |c| c == ' ');
            let token_end = run_end(text, token, is_token_char);
            if token_end > token {
                end = token_end;
            }
        }
        ranges.push((start, end));
    }
}

fn query_values(text: &str, lower: &str, ranges: &mut Vec<(usize, usize)>) {
    for name in QUERY_NAMES {
        for lead in ['?', '&'] {
            let needle = format!("{lead}{name}=");
            for (at, _) in lower.match_indices(needle.as_str()) {
                let start = at + needle.len();
                let end = run_end(text, start, |c| !is_value_end(c));
                if end > start
                    && !text
                        .get(start..end)
                        .is_some_and(|v| v.starts_with("[REDACTED"))
                {
                    ranges.push((start, end));
                }
            }
        }
    }
}

/// SDES key material: the base64 `key||salt` after `inline:` in `a=crypto:1 AES_CM_128_HMAC_SHA1_80
/// inline:<base64>|2^31`. The key ends at the `|` of a lifetime, at a space (session parameters),
/// at `;` (a second key) or at a quote or line end; the suite and tag stay readable. Any run of
/// four or more base64 characters counts, so a truncated or mangled key is removed as well.
fn sdes_keys(text: &str, lower: &str, ranges: &mut Vec<(usize, usize)>) {
    for (at, _) in lower.match_indices("inline:") {
        let start = at + "inline:".len();
        let end = run_end(text, start, is_token_char);
        if end - start >= 4
            && !text
                .get(start..end)
                .is_some_and(|v| v.starts_with("[REDACTED"))
        {
            ranges.push((start, end));
        }
    }
}

/// `scheme://user:password@host`: the password.
fn url_passwords(text: &str, ranges: &mut Vec<(usize, usize)>) {
    for (at, _) in text.match_indices("://") {
        let start = at + 3;
        let end = run_end(text, start, |c| {
            !(c.is_whitespace() || matches!(c, '/' | '"' | '\''))
        });
        let Some(authority) = text.get(start..end) else {
            continue;
        };
        let Some(at_sign) = authority.rfind('@') else {
            continue;
        };
        let Some(colon) = authority
            .get(..at_sign)
            .and_then(|userinfo| userinfo.find(':'))
        else {
            continue;
        };
        let (from, to) = (start + colon + 1, start + at_sign);
        if to > from {
            ranges.push((from, to));
        }
    }
}

/// A [`MakeWriter`] that scrubs each event's text with a [`Redactor`] before it reaches the
/// inner writer.
#[derive(Debug, Clone)]
pub struct RedactingMakeWriter<M> {
    inner: M,
    redactor: Arc<Redactor>,
}

impl<M> RedactingMakeWriter<M> {
    /// Wraps `inner`.
    pub const fn new(inner: M, redactor: Arc<Redactor>) -> Self {
        Self { inner, redactor }
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer(), Arc::clone(&self.redactor))
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer_for(meta), Arc::clone(&self.redactor))
    }
}

/// The writer of [`RedactingMakeWriter`]: buffers what it is given (one event, normally a single
/// `write_all`) and writes it scrubbed when flushed or dropped. Buffering the whole event means
/// a secret split over two `write` calls is still found.
#[derive(Debug)]
pub struct RedactingWriter<W: io::Write> {
    inner: W,
    redactor: Arc<Redactor>,
    buf: Vec<u8>,
}

impl<W: io::Write> RedactingWriter<W> {
    fn new(inner: W, redactor: Arc<Redactor>) -> Self {
        Self {
            inner,
            redactor,
            buf: Vec::new(),
        }
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let buf = std::mem::take(&mut self.buf);
        let text = String::from_utf8_lossy(&buf);
        self.inner.write_all(self.redactor.redact(&text).as_bytes())
    }
}

impl<W: io::Write> io::Write for RedactingWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit()?;
        self.inner.flush()
    }
}

impl<W: io::Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        let _ = self.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrub(text: &str) -> String {
        Redactor::new().redact(text).into_owned()
    }

    #[test]
    fn clean_text_is_untouched_and_not_copied() {
        let text = "turn finished epoch=3 max_tokens=256 tokens: 12 monkey=1";
        assert!(matches!(Redactor::new().redact(text), Cow::Borrowed(_)));
    }

    #[test]
    fn named_values_in_text_json_and_headers() {
        assert_eq!(
            scrub("api_key=sk-abcdef123456 rest"),
            "api_key=[REDACTED] rest"
        );
        assert_eq!(
            scrub(r#"{"token":"abc.def.ghi","user":"bob"}"#),
            r#"{"token":"[REDACTED]","user":"bob"}"#
        );
        assert_eq!(
            scrub(r#"msg="{\"password\":\"hunter2hunter2\"}""#),
            r#"msg="{\"password\":\"[REDACTED]\"}""#
        );
        assert_eq!(
            scrub("x-api-key: 0123456789abcdef"),
            "x-api-key: [REDACTED]"
        );
        assert_eq!(
            scrub("DEEPGRAM_API_KEY=dg_secret_value"),
            "DEEPGRAM_API_KEY=[REDACTED]"
        );
        assert_eq!(scrub("client_secret: s3cr3t!"), "client_secret: [REDACTED]");
    }

    #[test]
    fn authorization_headers_lose_the_scheme_token() {
        assert_eq!(
            scrub("Authorization: Bearer abcdefghijklmnop"),
            "Authorization: Bearer [REDACTED]".replace("Bearer [REDACTED]", "[REDACTED]")
        );
        assert_eq!(
            scrub("sent header bearer abcdefghijklmnop to host"),
            "sent header bearer [REDACTED] to host"
        );
        assert_eq!(scrub("Basic dXNlcjpwYXNzd29yZA=="), "Basic [REDACTED]");
        // Other schemes: Deepgram's `Token <key>`, `ApiKey <key>`, the proxy header, JSON.
        assert_eq!(
            scrub("Authorization: Token 0123456789abcdef"),
            "Authorization: [REDACTED]"
        );
        assert_eq!(
            scrub("proxy-authorization: ApiKey 0123456789abcdef; next"),
            "proxy-authorization: [REDACTED]; next"
        );
        assert_eq!(
            scrub(r#"{"authorization":"Token 0123456789abcdef","user":"bob"}"#),
            r#"{"authorization":"[REDACTED]","user":"bob"}"#
        );
        // Prose is not a credential.
        assert_eq!(
            scrub("a basic idea; bearer bonds"),
            "a basic idea; bearer bonds"
        );
    }

    #[test]
    fn sdes_key_material_in_sdp_text_is_removed() {
        let key = "8P+/ENeqX2Lq5iuOk4diJQO+nCdvwRdtgN1B4COi";
        assert_eq!(
            scrub(&format!(
                "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{key}|2^31\r\na=ptime:20"
            )),
            "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:[REDACTED]|2^31\r\na=ptime:20"
        );
        // Several lines, upper case, a JSON string, session parameters after the key.
        let sdp = format!(
            "a=crypto:1 AES_CM_128_HMAC_SHA1_80 INLINE:{key}\na=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:{key} UNENCRYPTED_SRTP"
        );
        let out = scrub(&sdp);
        assert!(!out.contains(key), "{out}");
        assert!(out.contains("AES_CM_128_HMAC_SHA1_32"), "{out}");
        assert_eq!(
            scrub(&format!(r#"{{"sdp":"a=crypto:1 X inline:{key}"}}"#)),
            r#"{"sdp":"a=crypto:1 X inline:[REDACTED]"}"#
        );
        // A mangled key still goes; a line that only mentions the word does not lose anything.
        assert_eq!(scrub("inline:AAAA|2^20"), "inline:[REDACTED]|2^20");
        assert_eq!(scrub("an inline: comment"), "an inline: comment");
        assert_eq!(scrub("inline:[REDACTED]"), "inline:[REDACTED]");
    }

    #[test]
    fn urls_lose_query_secrets_and_passwords() {
        assert_eq!(
            scrub(
                "GET https://generativelanguage.googleapis.com/v1/models?key=AIzaSyABCDEF&alt=sse failed"
            ),
            "GET https://generativelanguage.googleapis.com/v1/models?key=[REDACTED]&alt=sse failed"
        );
        assert_eq!(
            scrub("connect redis://user:p4ssw0rd@db.internal:6379/0"),
            "connect redis://user:[REDACTED]@db.internal:6379/0"
        );
        assert_eq!(scrub("https://example.com/a@b"), "https://example.com/a@b");
    }

    #[test]
    fn known_secret_values_go_wherever_they_are() {
        let redactor = Redactor::from_vars([
            ("OPENAI_API_KEY", "sk-live-0123456789"),
            ("BRISVOX_LOG_FORMAT", "json-json-json"),
            ("SHORT_TOKEN", "abc"),
        ]);
        let out = redactor.redact("failed: 401 for sk-live-0123456789 (json-json-json, abc)");
        assert_eq!(out, "failed: 401 for [REDACTED] (json-json-json, abc)");
    }

    #[test]
    fn a_quoted_field_value_is_found_after_a_message_that_says_token() {
        assert_eq!(
            scrub(r#"field named token token="dg_0123456789abcdef0123""#),
            r#"field named token token="[REDACTED]""#
        );
    }

    #[test]
    fn ansi_coloured_field_names_do_not_hide_the_value() {
        // What the text log format writes for `token = "..."` on a colour terminal.
        let line = "\u{1b}[3mtoken\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"dg_0123456789abcdef0123\" tail";
        assert_eq!(
            scrub(line),
            "\u{1b}[3mtoken\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"[REDACTED]\" tail"
        );
    }

    #[test]
    fn already_redacted_values_are_left_alone() {
        assert_eq!(scrub("key token: [REDACTED] x"), "key token: [REDACTED] x");
    }

    #[test]
    fn writer_scrubs_a_secret_split_over_two_writes() {
        use io::Write as _;
        let sink = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        #[derive(Clone)]
        struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
        impl io::Write for Sink {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let redactor = Arc::new(Redactor::new().with_secret("sk-live-0123456789"));
        let mut writer = RedactingWriter::new(Sink(sink.clone()), redactor);
        writer.write_all(b"using sk-live-").unwrap();
        writer.write_all(b"0123456789 now\n").unwrap();
        drop(writer);
        let written = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        assert_eq!(written, "using [REDACTED] now\n");
    }
}
