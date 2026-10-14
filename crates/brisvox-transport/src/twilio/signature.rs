//! `X-Twilio-Signature` validation for the WebSocket upgrade (`docs/providers/twilio.md`,
//! "Request validation").
//!
//! Twilio signs the request URL with HMAC-SHA1 keyed by the account's primary Auth Token and sends
//! the base64 of the digest in the `X-Twilio-Signature` header. A WebSocket upgrade is a GET
//! without parameters and `<Stream url>` cannot carry a query string, so the signed string is
//! just the URL as written in the TwiML.
//!
//! Which exact URL form Twilio signs for a `wss://` upgrade is not documented (scheme `wss` or
//! `https`, port `:443` or not, trailing `/` or not; the security page only suggests trying a
//! trailing slash). [`candidate_urls`] therefore derives every plausible form from the configured
//! public URL and [`verify`] accepts a signature that matches any of them. Each comparison is
//! constant-time (`ring::hmac::verify`); the candidates are public information, so trying several
//! does not leak the token.
//!
//! The HMAC comes from `ring` (already in the workspace through rustls). SHA-1 is acceptable here
//! because HMAC-SHA1 is the algorithm Twilio mandates and is not affected by SHA-1 collisions.

use base64::Engine as _;
use ring::hmac;

use super::protocol::BASE64;

/// The signature Twilio would send for `url`: base64 of HMAC-SHA1(`auth_token`, `url`).
pub fn sign(auth_token: &str, url: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, auth_token.as_bytes());
    BASE64.encode(hmac::sign(&key, url.as_bytes()).as_ref())
}

/// Whether `header` (the value of `X-Twilio-Signature`) is the signature of any of `urls`.
/// Constant-time per URL; a header that is not valid base64 never matches.
pub fn verify(auth_token: &str, urls: &[String], header: &str) -> bool {
    let Ok(tag) = BASE64.decode(header.trim().as_bytes()) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, auth_token.as_bytes());
    let mut matched = false;
    for url in urls {
        // No early exit: every candidate costs the same whatever the header holds.
        matched |= hmac::verify(&key, url.as_bytes(), &tag).is_ok();
    }
    matched
}

/// The URL split into scheme, authority (without user info) and path.
fn split(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    // Twilio always drops a username and password from the signed URL.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if scheme.is_empty() || authority.is_empty() {
        return None;
    }
    Some((scheme, authority, path))
}

/// The scheme that is the other name for the same endpoint (`wss` and `https` share port 443,
/// `ws` and `http` port 80).
fn counterpart(scheme: &str) -> Option<&'static str> {
    match scheme {
        "wss" => Some("https"),
        "https" => Some("wss"),
        "ws" => Some("http"),
        "http" => Some("ws"),
        _ => None,
    }
}

/// The default port of a scheme.
fn default_port(scheme: &str) -> Option<&'static str> {
    match scheme {
        "wss" | "https" => Some("443"),
        "ws" | "http" => Some("80"),
        _ => None,
    }
}

/// Every form of `public_url` Twilio might have signed: the URL as given, with the other scheme
/// name, with the default port added or removed, and with a trailing `/` toggled. Deduplicated,
/// in that order of likelihood (the URL as given first). Empty if `public_url` is not a URL.
pub fn candidate_urls(public_url: &str) -> Vec<String> {
    let Some((scheme, authority, path)) = split(public_url) else {
        return Vec::new();
    };
    let mut schemes = vec![scheme];
    schemes.extend(counterpart(scheme));
    let mut authorities = vec![authority.to_owned()];
    if let Some(port) = default_port(scheme) {
        let suffix = format!(":{port}");
        match authority.strip_suffix(&suffix) {
            Some(bare) => authorities.push(bare.to_owned()),
            None if !authority.contains(':') || authority.ends_with(']') => {
                authorities.push(format!("{authority}{suffix}"));
            }
            None => {}
        }
    }
    let mut paths = vec![path.to_owned()];
    if let Some(trimmed) = path.strip_suffix('/') {
        paths.push(trimmed.to_owned());
    } else {
        paths.push(format!("{path}/"));
    }
    let mut out: Vec<String> = Vec::new();
    for path in &paths {
        for authority in &authorities {
            for scheme in &schemes {
                let url = format!("{scheme}://{authority}{path}");
                if !out.contains(&url) {
                    out.push(url);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The RFC 2202 test cases for HMAC-SHA1, through the same primitive `sign` uses.
    #[test]
    fn hmac_sha1_matches_the_rfc_2202_vectors() {
        let cases: [(&[u8], &[u8], &str); 5] = [
            (
                &[0x0b; 20],
                b"Hi There",
                "b617318655057264e28bc0b6fb378c8ef146be00",
            ),
            (
                b"Jefe",
                b"what do ya want for nothing?",
                "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79",
            ),
            (
                &[0xaa; 20],
                &[0xdd; 50],
                "125d7342b9ac11cd91a39af48aa17b4f63f175d3",
            ),
            (
                &[0xaa; 80],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "aa4ae5e15272d00e95705637ce8a3b55ed402112",
            ),
            (
                &[0xaa; 80],
                b"Test Using Larger Than Block-Size Key and Larger Than One Block-Size Data",
                "e8e99d0f45237d786d6bbaa7965c7808bbff1a91",
            ),
        ];
        for (key, data, expected) in cases {
            let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
            assert_eq!(hex(hmac::sign(&key, data).as_ref()), expected);
        }
    }

    #[test]
    fn sign_is_the_base64_of_the_digest() {
        // RFC 2202 case 2: effcdf6a... base64 is "7/zfauXrL6LSdBbV8YTfnCWafHk=".
        assert_eq!(
            sign("Jefe", "what do ya want for nothing?"),
            "7/zfauXrL6LSdBbV8YTfnCWafHk="
        );
    }

    /// The worked example of Twilio's security documentation for POST webhooks (URL, then the
    /// sorted parameter names and values, HMAC-SHA1 keyed by the token). A WebSocket upgrade
    /// signs the URL alone, but this pins the primitive and the encoding to Twilio's published
    /// output. (The example is quoted from the security page; the page fetched for
    /// `docs/providers/twilio.md` only describes it.)
    #[test]
    fn twilios_published_example_signature_is_reproduced() {
        let signed = concat!(
            "https://mycompany.com/myapp.php?foo=1&bar=2",
            "CallSid",
            "CA1234567890ABCDE",
            "Caller",
            "+14158675310",
            "Digits",
            "1234",
            "From",
            "+14158675310",
            "To",
            "+18005551212",
        );
        assert_eq!(sign("12345", signed), "GvWf1cFY/Q7PnoempGyD5oXAezc=");
    }

    #[test]
    fn a_matching_signature_verifies_and_anything_else_does_not() {
        let url = "wss://agent.example.com/twilio/stream";
        let good = sign("token", url);
        assert!(verify("token", &[url.to_owned()], &good));
        assert!(!verify("other", &[url.to_owned()], &good));
        assert!(!verify(
            "token",
            &["wss://agent.example.com/other".to_owned()],
            &good
        ));
        assert!(!verify("token", &[url.to_owned()], ""));
        assert!(!verify("token", &[url.to_owned()], "!!not base64!!"));
        assert!(!verify("token", &[], &good));
        // A prefix of the right tag is a different tag.
        assert!(!verify(
            "token",
            &[url.to_owned()],
            good.get(..good.len() - 4).unwrap_or("")
        ));
    }

    #[test]
    fn candidates_cover_scheme_port_and_trailing_slash() {
        let urls = candidate_urls("wss://agent.example.com/twilio/stream");
        assert_eq!(urls[0], "wss://agent.example.com/twilio/stream");
        for expected in [
            "https://agent.example.com/twilio/stream",
            "wss://agent.example.com:443/twilio/stream",
            "https://agent.example.com:443/twilio/stream",
            "wss://agent.example.com/twilio/stream/",
            "https://agent.example.com/twilio/stream/",
            "wss://agent.example.com:443/twilio/stream/",
            "https://agent.example.com:443/twilio/stream/",
        ] {
            assert!(urls.iter().any(|u| u == expected), "missing {expected}");
        }
        assert_eq!(urls.len(), 8);
        let mut sorted = urls.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), urls.len(), "duplicates in {urls:?}");
    }

    #[test]
    fn candidates_handle_ports_trailing_slashes_and_user_info() {
        // A non-default port stays as it is.
        let urls = candidate_urls("wss://agent.example.com:8443/twilio/stream/");
        assert!(urls.contains(&"wss://agent.example.com:8443/twilio/stream".to_owned()));
        assert!(urls.contains(&"https://agent.example.com:8443/twilio/stream/".to_owned()));
        assert!(!urls.iter().any(|u| u.contains(":443/")));
        // An explicit default port is also tried without it.
        let urls = candidate_urls("wss://agent.example.com:443/s");
        assert!(urls.contains(&"wss://agent.example.com/s".to_owned()));
        // User info never reaches the signed string.
        let urls = candidate_urls("wss://user:pw@agent.example.com/s");
        assert!(urls.iter().all(|u| !u.contains('@')), "{urls:?}");
        // Plain ws/http.
        let urls = candidate_urls("ws://127.0.0.1:9000/s");
        assert!(urls.contains(&"http://127.0.0.1:9000/s".to_owned()));
        // Not URLs.
        assert!(candidate_urls("agent.example.com/s").is_empty());
        assert!(candidate_urls("wss:///s").is_empty());
        assert!(candidate_urls("").is_empty());
    }

    #[test]
    fn a_signature_of_any_candidate_form_is_accepted() {
        let configured = "wss://agent.example.com/twilio/stream";
        let urls = candidate_urls(configured);
        for form in &urls {
            let header = sign("tok", form);
            assert!(verify("tok", &urls, &header), "form {form}");
        }
        // But not a different host or path.
        assert!(!verify(
            "tok",
            &urls,
            &sign("tok", "wss://evil.example.com/twilio/stream")
        ));
        assert!(!verify(
            "tok",
            &urls,
            &sign("tok", "wss://agent.example.com/twilio/other")
        ));
    }
}
