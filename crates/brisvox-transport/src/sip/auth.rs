//! Digest authentication of inbound INVITEs (RFC 3261 section 22, RFC 2617 without `qop`).
//!
//! The server challenges an INVITE that carries no (or a stale) `Authorization` with a fresh
//! nonce; the retry is checked against the configured users. A nonce is valid once and for a
//! short time, so a captured `Authorization` header cannot be replayed. The password hash comes
//! from rsipstack's `verify_digest`.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::Duration;

use brisvox_core::Secret;
use brisvox_core::clock::deadline_after;
use rsipstack::dialog::authenticate::verify_digest;
use rsipstack::sip::typed::Authorization;
use rsipstack::sip::{Header, Request, WwwAuthenticate};
use tokio::time::Instant;

use crate::error::{Error, Result};

/// Most nonces remembered at once; the oldest is forgotten first (its holder gets a new
/// challenge).
const MAX_NONCES: usize = 4096;

/// Credentials accepted from inbound callers.
#[derive(Debug, Clone)]
pub struct InboundAuth {
    /// The realm named in the challenge.
    pub realm: String,
    /// `(user name, password)` pairs; the digest `username` must match one of them.
    pub users: Vec<(String, Secret<String>)>,
    /// How long a challenge nonce may be answered.
    pub nonce_ttl: Duration,
}

impl InboundAuth {
    /// Checks the settings.
    ///
    /// # Errors
    /// [`Error::Config`] for an empty realm, no users, an empty user name or password, or a zero
    /// nonce lifetime.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(what.to_owned()));
        if self.realm.is_empty() || self.realm.contains('"') {
            return bad("sip inbound_auth realm must be non-empty and without quotes");
        }
        if self.users.is_empty() {
            return bad("sip inbound_auth needs at least one user");
        }
        if self
            .users
            .iter()
            .any(|(user, password)| user.is_empty() || password.expose().is_empty())
        {
            return bad("sip inbound_auth users and passwords must not be empty");
        }
        if self.nonce_ttl.is_zero() {
            return bad("sip inbound_auth nonce_ttl must be positive");
        }
        Ok(())
    }
}

/// What to do with an INVITE after the credential check.
pub(crate) enum Verdict {
    /// The request is authenticated.
    Allowed,
    /// Answer 401 with this `WWW-Authenticate` header.
    Challenge(Header),
    /// Answer 403: the credentials were wrong.
    Forbidden,
}

/// The nonces issued and not yet used.
#[derive(Debug, Default)]
pub(crate) struct Nonces {
    issued: VecDeque<(String, Instant)>,
}

impl Nonces {
    fn issue(&mut self, now: Instant, ttl: Duration) -> Option<String> {
        use ring::rand::SecureRandom as _;
        let mut bytes = [0u8; 16];
        ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
        let mut nonce = String::with_capacity(32);
        for byte in bytes {
            let _ = write!(nonce, "{byte:02x}");
        }
        while self.issued.front().is_some_and(|(_, due)| now >= *due) {
            self.issued.pop_front();
        }
        if self.issued.len() >= MAX_NONCES {
            self.issued.pop_front();
        }
        self.issued
            .push_back((nonce.clone(), deadline_after(now, ttl)));
        Some(nonce)
    }

    /// Removes `nonce` and tells whether it was issued and is still valid.
    fn consume(&mut self, nonce: &str, now: Instant) -> bool {
        let Some(position) = self.issued.iter().position(|(n, _)| n == nonce) else {
            return false;
        };
        self.issued
            .remove(position)
            .is_some_and(|(_, due)| now < due)
    }
}

fn challenge(nonces: &mut Nonces, auth: &InboundAuth, now: Instant) -> Verdict {
    match nonces.issue(now, auth.nonce_ttl) {
        Some(nonce) => Verdict::Challenge(
            WwwAuthenticate::new(format!(
                "Digest realm=\"{}\", nonce=\"{nonce}\", algorithm=MD5",
                auth.realm
            ))
            .into(),
        ),
        // No randomness: refuse rather than issue a guessable nonce.
        None => Verdict::Forbidden,
    }
}

/// Checks the `Authorization` of `request` against `auth`.
pub(super) fn check(
    auth: &InboundAuth,
    nonces: &mut Nonces,
    request: &Request,
    now: Instant,
) -> Verdict {
    let Some(header) = request.headers.iter().find_map(|h| match h {
        Header::Authorization(a) => Some(a.value().to_owned()),
        _ => None,
    }) else {
        return challenge(nonces, auth, now);
    };
    let Ok(parsed) = Authorization::parse(&header) else {
        return challenge(nonces, auth, now);
    };
    if parsed.realm != auth.realm || !nonces.consume(&parsed.nonce, now) {
        // Unknown, used or expired nonce: start over.
        return challenge(nonces, auth, now);
    }
    let known = auth.users.iter().find(|(user, _)| *user == parsed.username);
    match known {
        Some((_, password))
            if verify_digest(&parsed, password.expose(), &request.method, &header) =>
        {
            Verdict::Allowed
        }
        _ => Verdict::Forbidden,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // synthetic instants for a pure model test
mod tests {
    use rsipstack::dialog::authenticate::compute_digest;
    use rsipstack::sip::headers::auth::Algorithm;
    use rsipstack::sip::{Method, SipMessage};

    use super::*;

    fn auth() -> InboundAuth {
        InboundAuth {
            realm: "brisvox.test".to_owned(),
            users: vec![("alice".to_owned(), Secret::new("s3cret".to_owned()))],
            nonce_ttl: Duration::from_secs(30),
        }
    }

    fn invite(authorization: Option<String>) -> Request {
        let extra = authorization
            .map(|a| format!("Authorization: {a}\r\n"))
            .unwrap_or_default();
        let text = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bKx\r\n\
             Max-Forwards: 70\r\nFrom: <sip:alice@example.com>;tag=a\r\nTo: <sip:bob@example.com>\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\nContact: <sip:alice@127.0.0.1:5070>\r\n{extra}Content-Length: 0\r\n\r\n"
        );
        match SipMessage::try_from(text.as_str()).unwrap() {
            SipMessage::Request(request) => request,
            SipMessage::Response(_) => panic!("not a request"),
        }
    }

    fn nonce_of(verdict: &Verdict) -> String {
        let Verdict::Challenge(Header::WwwAuthenticate(header)) = verdict else {
            panic!("expected a challenge");
        };
        let value = header.value();
        let start = value.find("nonce=\"").unwrap() + 7;
        let end = value[start..].find('"').unwrap() + start;
        value[start..end].to_owned()
    }

    fn answer(nonce: &str, user: &str, password: &str) -> String {
        let response = compute_digest(
            user,
            password,
            "brisvox.test",
            nonce,
            &Method::Invite,
            "sip:bob@example.com",
            Algorithm::Md5,
            None,
        );
        format!(
            "Digest username=\"{user}\", realm=\"brisvox.test\", nonce=\"{nonce}\", \
             uri=\"sip:bob@example.com\", response=\"{response}\", algorithm=MD5"
        )
    }

    /// Known answers that do not come from rsipstack: the credentials and nonce of the RFC 2617
    /// section 3.5 example (Mufasa / "Circle Of Life" / testrealm@host.com), hashed by an
    /// independent MD5 implementation (Python `hashlib`) following RFC 2617 section 3.2.2.1,
    /// for the method and URI of a SIP INVITE. The same script reproduces the RFC's own GET
    /// answer `6629fae49393a05397450978507c4ef1`, which `compute_digest` cannot be asked for
    /// (SIP has no GET method).
    #[test]
    fn the_digest_matches_known_answers_computed_independently() {
        const NONCE: &str = "dcd98b7102dd2f0e8b11d0f600bfb0c093";
        let without_qop = compute_digest(
            "Mufasa",
            "Circle Of Life",
            "testrealm@host.com",
            NONCE,
            &Method::Invite,
            "sip:agent@example.com",
            Algorithm::Md5,
            None,
        );
        assert_eq!(without_qop, "4fbfe6ab510a7b6d6f5ca2b8904230cc");
        let with_qop = compute_digest(
            "Mufasa",
            "Circle Of Life",
            "testrealm@host.com",
            NONCE,
            &Method::Invite,
            "sip:agent@example.com",
            Algorithm::Md5,
            Some(&rsipstack::sip::headers::auth::AuthQop::Auth {
                cnonce: "0a4f113b".to_owned(),
                nc: 1,
            }),
        );
        assert_eq!(with_qop, "2b4a237d57834dd41b7177639548e2a3");
    }

    #[test]
    fn an_invite_without_credentials_is_challenged() {
        let mut nonces = Nonces::default();
        let verdict = check(&auth(), &mut nonces, &invite(None), Instant::now());
        assert!(matches!(verdict, Verdict::Challenge(_)));
        let nonce = nonce_of(&verdict);
        assert_eq!(nonce.len(), 32);
    }

    #[test]
    fn the_right_password_passes_once_and_the_nonce_cannot_be_replayed() {
        let now = Instant::now();
        let mut nonces = Nonces::default();
        let nonce = nonce_of(&check(&auth(), &mut nonces, &invite(None), now));
        let request = invite(Some(answer(&nonce, "alice", "s3cret")));
        assert!(matches!(
            check(&auth(), &mut nonces, &request, now),
            Verdict::Allowed
        ));
        // The same header again: the nonce is spent, a new challenge comes back.
        assert!(matches!(
            check(&auth(), &mut nonces, &request, now),
            Verdict::Challenge(_)
        ));
    }

    #[test]
    fn a_wrong_password_or_unknown_user_is_forbidden() {
        let now = Instant::now();
        let mut nonces = Nonces::default();
        for (user, password) in [("alice", "wrong"), ("mallory", "s3cret")] {
            let nonce = nonce_of(&check(&auth(), &mut nonces, &invite(None), now));
            let request = invite(Some(answer(&nonce, user, password)));
            assert!(matches!(
                check(&auth(), &mut nonces, &request, now),
                Verdict::Forbidden
            ));
        }
    }

    #[test]
    fn an_expired_or_foreign_nonce_gets_a_new_challenge() {
        let now = Instant::now();
        let mut nonces = Nonces::default();
        let nonce = nonce_of(&check(&auth(), &mut nonces, &invite(None), now));
        let late = now + Duration::from_secs(31);
        let request = invite(Some(answer(&nonce, "alice", "s3cret")));
        assert!(matches!(
            check(&auth(), &mut nonces, &request, late),
            Verdict::Challenge(_)
        ));
        let forged = invite(Some(answer(
            "00112233445566778899aabbccddeeff",
            "alice",
            "s3cret",
        )));
        assert!(matches!(
            check(&auth(), &mut nonces, &forged, now),
            Verdict::Challenge(_)
        ));
        let garbage = invite(Some("Digest nonsense".to_owned()));
        assert!(matches!(
            check(&auth(), &mut nonces, &garbage, now),
            Verdict::Challenge(_)
        ));
    }

    #[test]
    fn the_nonce_book_is_bounded() {
        let now = Instant::now();
        let mut nonces = Nonces::default();
        for _ in 0..MAX_NONCES + 10 {
            nonces.issue(now, Duration::from_secs(30)).unwrap();
        }
        assert_eq!(nonces.issued.len(), MAX_NONCES);
    }

    #[test]
    fn invalid_settings_are_refused_and_passwords_stay_out_of_debug() {
        assert!(auth().validate().is_ok());
        let mut bad = auth();
        bad.users.clear();
        assert!(bad.validate().is_err());
        let mut bad = auth();
        bad.realm = String::new();
        assert!(bad.validate().is_err());
        let mut bad = auth();
        bad.users = vec![("alice".to_owned(), Secret::new(String::new()))];
        assert!(bad.validate().is_err());
        assert!(!format!("{:?}", auth()).contains("s3cret"));
    }
}
