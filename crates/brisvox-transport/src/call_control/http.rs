//! The HTTP client shared by the Twilio and Telnyx bindings: the shared rustls TLS stack, a deadline on
//! the injected clock around every request (send, headers and the bounded body), no proxy, no
//! redirects (credentials never follow one), plain `http://` for loopback only, and the small
//! form encoding Twilio needs (reqwest's `form` feature is not enabled).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use brisvox_core::SharedClock;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};

use super::error::{Error, Result};

/// Bytes of a response body kept (REST answers are small; this bounds a hostile one).
const MAX_BODY: usize = 64 * 1024;
/// Characters of a provider error text kept in an [`Error`].
const MAX_DETAIL: usize = 200;

static TLS: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

/// The process-wide TLS client config: rustls with the `ring` provider and the compiled-in
/// webpki roots, the same stack as `brisvox_providers::net::tls_config`. Duplicated here
/// instead of depending on the providers crate, which is a sibling of this one in the crate
/// graph.
fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    if let Some(config) = TLS.get() {
        return Ok(Arc::clone(config));
    }
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Config(format!("TLS setup failed: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::clone(TLS.get_or_init(|| Arc::new(config))))
}

/// Refuses a base URL that would send credentials in cleartext to a remote host: `https://`, or
/// `http://` on a loopback host (tests, a local mock).
pub(crate) fn check_base_url(base_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|_| Error::Config("base_url is not a valid absolute URL".to_owned()))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = url.host_str().unwrap_or_default();
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let loopback = host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            if loopback {
                Ok(())
            } else {
                Err(Error::Config(
                    "base_url is plain http:// on a non-loopback host: credentials would be sent \
                     unencrypted (use https://)"
                        .to_owned(),
                ))
            }
        }
        _ => Err(Error::Config(
            "base_url must be an http:// or https:// URL".to_owned(),
        )),
    }
}

/// A sensitive `Authorization` header value.
pub(crate) fn auth_header(value: &str) -> Result<HeaderValue> {
    let mut header = HeaderValue::from_str(value)
        .map_err(|_| Error::Config("the credentials are not valid in an HTTP header".to_owned()))?;
    header.set_sensitive(true);
    Ok(header)
}

/// The body of a request.
pub(crate) enum Body {
    /// No body.
    None,
    /// `application/x-www-form-urlencoded`.
    Form(String),
    /// `application/json`.
    Json(Vec<u8>),
}

/// A complete response (any status).
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

/// One provider's HTTP client.
pub(crate) struct Http {
    provider: &'static str,
    client: reqwest::Client,
    base: String,
    auth: HeaderValue,
    clock: SharedClock,
    timeout: Duration,
}

impl Http {
    /// Builds the client. `timeout` bounds each request end to end, `connect_timeout` its
    /// connection setup.
    pub(crate) fn new(
        provider: &'static str,
        base_url: &str,
        auth: HeaderValue,
        clock: SharedClock,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self> {
        check_base_url(base_url)?;
        if timeout.is_zero() || connect_timeout.is_zero() {
            return Err(Error::Config("timeouts must not be zero".to_owned()));
        }
        let mut tls = (*tls_config()?).clone();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("brisvox/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|e| Error::Config(format!("cannot build the HTTP client: {e}")))?;
        Ok(Self {
            provider,
            client,
            base: base_url.trim_end_matches('/').to_owned(),
            auth,
            clock,
            timeout,
        })
    }

    /// Sends `method path` (path relative to the base URL, starting with `/`) and reads the
    /// whole answer within the timeout. Returns any HTTP status as a [`Reply`]; only transport
    /// failures are errors.
    pub(crate) async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Body,
        what: &'static str,
    ) -> Result<Reply> {
        let url = format!("{}{path}", self.base);
        let mut builder = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, self.auth.clone())
            .header(reqwest::header::ACCEPT, "application/json");
        builder = match body {
            Body::None => builder,
            Body::Form(text) => builder
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(text),
            Body::Json(bytes) => builder.header(CONTENT_TYPE, "application/json").body(bytes),
        };
        let provider = self.provider;
        let exchange = async move {
            let mut response = builder
                .send()
                .await
                .map_err(|e| transport_error(provider, e))?;
            let status = response.status().as_u16();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| transport_error(provider, e))?
            {
                let room = MAX_BODY.saturating_sub(body.len());
                body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
                if body.len() >= MAX_BODY {
                    break;
                }
            }
            Ok::<Reply, Error>(Reply { status, body })
        };
        tokio::select! {
            biased;
            reply = exchange => reply,
            () = self.clock.sleep(self.timeout) => Err(Error::Timeout {
                provider,
                what,
                after: self.timeout,
            }),
        }
    }
}

fn transport_error(provider: &'static str, error: reqwest::Error) -> Error {
    // The URL is left out: it names the account and the call. reqwest's `Display` appends
    // " for url (...)" unless the URL is removed first. Credentials are in headers, never in text.
    let unreachable = error.is_connect() || error.is_builder();
    let detail = sanitize(&error.without_url().to_string(), MAX_DETAIL);
    if unreachable {
        Error::Unreachable { provider, detail }
    } else {
        Error::Transport { provider, detail }
    }
}

/// `text` without control characters and at most `max` characters.
pub(crate) fn sanitize(text: &str, max: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// `application/x-www-form-urlencoded` of `pairs` (a key may repeat: Twilio takes repeated
/// parameters as a list).
pub(crate) fn form_encode(pairs: &[(&str, String)]) -> String {
    let mut out = String::new();
    for (index, (key, value)) in pairs.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        percent_encode_into(&mut out, key);
        out.push('=');
        percent_encode_into(&mut out, value);
    }
    out
}

fn percent_encode_into(out: &mut String, text: &str) {
    let hex = |nibble: u8| {
        char::from_digit(u32::from(nibble), 16)
            .unwrap_or('0')
            .to_ascii_uppercase()
    };
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex(byte >> 4));
            out.push(hex(byte & 0x0f));
        }
    }
}

/// Decodes `application/x-www-form-urlencoded` into pairs (invalid escapes are kept verbatim).
pub fn form_decode(text: &str) -> Vec<(String, String)> {
    text.split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes
                    .get(index.saturating_add(1)..index.saturating_add(3))
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok());
                if let Some(decoded) = hex {
                    out.push(decoded);
                    index = index.saturating_add(2);
                } else {
                    out.push(b'%');
                }
            }
            other => out.push(other),
        }
        index = index.saturating_add(1);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_must_be_https_or_loopback() {
        assert!(check_base_url("https://api.twilio.com/2010-04-01").is_ok());
        assert!(check_base_url("http://127.0.0.1:8080").is_ok());
        assert!(check_base_url("http://localhost:1").is_ok());
        assert!(check_base_url("http://[::1]:1").is_ok());
        assert!(check_base_url("http://api.twilio.com").is_err());
        assert!(check_base_url("ftp://x").is_err());
        assert!(check_base_url("nonsense").is_err());
    }

    #[test]
    fn form_encoding_round_trips() {
        let pairs = [
            ("Twiml", "<Response a=\"b\">é & ü+</Response>".to_owned()),
            ("To", "+905551112233".to_owned()),
            ("To", "x y".to_owned()),
        ];
        let encoded = form_encode(&pairs);
        assert!(encoded.starts_with("Twiml=%3CResponse%20a%3D%22b%22%3E%C3%A9%20%26"));
        assert!(!encoded.contains(' '));
        let decoded = form_decode(&encoded);
        assert_eq!(decoded.len(), 3);
        assert_eq!(
            decoded.first().map(|p| p.1.as_str()),
            Some("<Response a=\"b\">é & ü+</Response>")
        );
        assert_eq!(decoded.get(1).map(|p| p.1.as_str()), Some("+905551112233"));
        assert_eq!(
            form_decode("a=%zz&b=1+2").first().map(|p| p.1.as_str()),
            Some("%zz")
        );
        assert_eq!(
            form_decode("a=%zz&b=1+2").get(1).map(|p| p.1.as_str()),
            Some("1 2")
        );
    }

    #[test]
    fn sanitize_strips_control_characters_and_bounds() {
        assert_eq!(sanitize("a\r\nb\u{7}c", 10), "a  b c");
        assert_eq!(sanitize("abcdef", 3), "abc");
    }
}
