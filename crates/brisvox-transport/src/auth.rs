//! Short-lived signed session tokens and static tokens for the browser-facing listeners
//! (`docs/SECURITY.md`).
//!
//! A static, shared, non-expiring token means one leaked page ships the agent (and the provider
//! bill) to everyone. A session token is minted by the application's backend for one visitor,
//! lives for seconds to minutes and opens at most one call.
//!
//! # Format
//! ```text
//! v1.<base64url(payload JSON)>.<base64url(HMAC-SHA256(secret, payload JSON bytes))>
//! ```
//! base64url without padding. The payload is a JSON object:
//!
//! | Field | Type | Meaning |
//! |---|---|---|
//! | `aud` | string | `brisvox-webrtc`, `brisvox-rtvi` or `brisvox-ws`: the listener the token is for |
//! | `exp` | integer | Unix seconds, at most [`MAX_TOKEN_LIFETIME`] (24 h) after the verifier's now; the token is refused from `exp` on |
//! | `nbf` | integer, optional | Unix seconds; refused before it (default 0) |
//! | `sid` | string | Opaque session id (1 to 128 bytes, no control characters); single use |
//! | `agent` | string, optional | Which agent the app intends; reported as call metadata |
//! | `max_s` | integer, optional | Longest call in seconds (1 to 86400) |
//! | `meta` | object, optional | At most 16 entries; scalar values; reported as call metadata |
//!
//! # Verification order
//! 1. The token is at most [`MAX_TOKEN_BYTES`] long ([`AuthError::TooLarge`]: HTTP 413) and has
//!    three parts with the `v1` prefix.
//! 2. The signature is checked with `ring::hmac::verify` (constant time) against the current
//!    secret and then the previous one (rotation), **before the payload is parsed**: a stranger
//!    who cannot sign never reaches the JSON parser, and the failure does not say which part was
//!    wrong. (`BadSignature` is the same answer for a wrong secret, a tampered payload and a
//!    tampered signature.)
//! 3. The payload is parsed and its limits checked, then `aud`, `nbf` and `exp` are checked on
//!    the injected [`Clock`](brisvox_core::Clock)'s wall time, with [`CLOCK_SKEW`] of tolerance on
//!    both sides (the app backend's clock and the server's are not the same clock). An `exp`
//!    further than [`MAX_TOKEN_LIFETIME`] (+ skew) ahead is [`AuthError::LifetimeTooLong`]: a
//!    backend that mints in milliseconds, or with a ten-year TTL, would otherwise park every
//!    `sid` in the single-use set until it is full.
//! 4. [`SessionAuth::consume`] records the `sid` until `exp + skew`. A second token with the same
//!    `sid` is [`AuthError::Replayed`]. The set holds at most `replay_capacity` entries; entries
//!    of expired tokens are dropped first, and when it is still full **new tokens are refused**
//!    ([`AuthError::Busy`], HTTP 503) rather than an unexpired entry evicted (evicting would let
//!    a spent token be used again). Only holders of the secret can add entries, so the set
//!    cannot be filled from outside.
//! 5. [`SessionAuth::refund`] gives a spent `sid` back (a call the server could not start, a
//!    WebRTC session the page hung up before it connected), **at most once per `sid`**: the id
//!    is remembered as refunded until `exp + skew`, a second refund is refused (the token stays
//!    spent), and when the refunded set (bounded like the single-use set) is full a refund is
//!    refused as well.
//!
//! Listeners that need the token checked before they know whether they can take the call
//! ([`Authenticator::peek`]) consume it later ([`Authenticator::consume`]); the others use
//! [`Authenticator::check`]. A call refused for capacity does not burn the token.
//!
//! # Where the token travels
//! - WebRTC: `Authorization: Bearer <token>` on `POST <whip_path>`.
//! - RTVI: `?token=<token>` (what the Pipecat client SDKs send), `Authorization: Bearer`, or the
//!   WebSocket subprotocol `brisvox-token.<token>` ([`SUBPROTOCOL_PREFIX`]; a browser cannot set
//!   headers on a `WebSocket`). A query string ends up in proxy logs: prefer the others.
//! - Generic WebSocket: the same three, with `?token=`.
//!
//! # Rotation
//! Set the new secret as `BRISVOX_SESSION_SECRET` and the old one as
//! `BRISVOX_SESSION_SECRET_PREV` until the old tokens have expired, then drop it.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use brisvox_core::{Clock as _, Secret, SharedClock};
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::Error;

/// Shortest session secret, in bytes. A shorter one is refused at startup.
pub const MIN_SECRET_BYTES: usize = 32;

/// Longest accepted token, in bytes. Larger input is refused before any other work.
pub const MAX_TOKEN_BYTES: usize = 4096;

/// Tolerance for the difference between the clock that minted a token and this server's clock,
/// applied to `nbf` and `exp`. A token is also remembered for this long past its `exp`.
pub const CLOCK_SKEW: Duration = Duration::from_secs(30);

/// Default capacity of the single-use set (entries of unexpired tokens).
pub const DEFAULT_REPLAY_CAPACITY: usize = 100_000;

/// Prefix of the WebSocket subprotocol that carries a token.
pub const SUBPROTOCOL_PREFIX: &str = "brisvox-token.";

/// The plain subprotocol name a browser should offer next to the token entry, so the server can
/// select it instead of echoing the token back.
pub const SUBPROTOCOL_NAME: &str = "brisvox";

/// Longest lifetime a session token may have: `exp` may be at most this far after the verifier's
/// (or, when minting, the minter's) current time. Tokens are meant to live seconds to minutes; the
/// bound keeps the single-use set from being filled by `exp` values in the far future or in
/// milliseconds.
pub const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(86_400);

/// Minimum seconds between two "single-use set full" warnings.
const BUSY_WARNING_INTERVAL_S: u64 = 60;

const VERSION: &str = "v1";
const MAX_SID_BYTES: usize = 128;
const MAX_AGENT_BYTES: usize = 128;
const MAX_META_ENTRIES: usize = 16;
const MAX_META_KEY_BYTES: usize = 64;
const MAX_META_VALUE_BYTES: usize = 256;
const MAX_CALL_SECONDS: u64 = 86_400;

/// Which listener a token is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Audience {
    /// The WebRTC signaling endpoint (`brisvox-webrtc`).
    Webrtc,
    /// The RTVI websocket listener (`brisvox-rtvi`).
    Rtvi,
    /// The generic websocket listener (`brisvox-ws`).
    WebSocket,
}

impl Audience {
    /// The `aud` claim.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Webrtc => "brisvox-webrtc",
            Self::Rtvi => "brisvox-rtvi",
            Self::WebSocket => "brisvox-ws",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [Self::Webrtc, Self::Rtvi, Self::WebSocket]
            .into_iter()
            .find(|audience| audience.as_str() == text)
    }
}

/// Why a token or credential was refused. The text never contains the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// No credential was presented.
    #[error("no token presented")]
    Missing,
    /// The token is longer than [`MAX_TOKEN_BYTES`].
    #[error("token too large")]
    TooLarge,
    /// Not a `v1.<payload>.<signature>` token, or a payload outside the documented limits.
    #[error("malformed token")]
    Malformed,
    /// The signature does not match (wrong secret, tampered payload or tampered signature), or
    /// the credential is not the configured static token.
    #[error("invalid token")]
    BadSignature,
    /// A valid token for another listener.
    #[error("token is for another audience")]
    WrongAudience,
    /// `exp` has passed (beyond the clock skew).
    #[error("token expired")]
    Expired,
    /// `nbf` is in the future (beyond the clock skew).
    #[error("token not yet valid")]
    NotYetValid,
    /// `exp` is further in the future than [`MAX_TOKEN_LIFETIME`] (often a timestamp in
    /// milliseconds, or a far-future TTL).
    #[error("token lifetime too long")]
    LifetimeTooLong,
    /// A token with this `sid` was already used.
    #[error("token already used")]
    Replayed,
    /// The single-use set is full of unexpired tokens: new tokens are refused until some expire.
    #[error("too many live session tokens")]
    Busy,
}

impl AuthError {
    /// The HTTP status for this refusal: `401`, except `413` for [`TooLarge`](Self::TooLarge) and
    /// `503` for [`Busy`](Self::Busy).
    pub const fn http_status(self) -> u16 {
        match self {
            Self::TooLarge => 413,
            Self::Busy => 503,
            _ => 401,
        }
    }
}

/// The secret(s) tokens are signed with: the current one, and optionally the previous one that
/// still verifies during a rotation. Never printed.
#[derive(Clone)]
pub struct SessionSecrets {
    current: Secret<String>,
    previous: Option<Secret<String>>,
}

impl std::fmt::Debug for SessionSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSecrets")
            .field("current", &Secret::<String>::REDACTED)
            .field("previous", &self.previous.is_some())
            .finish()
    }
}

impl SessionSecrets {
    /// Wraps the secrets.
    ///
    /// # Errors
    /// [`Error::Config`] when a secret is shorter than [`MIN_SECRET_BYTES`] bytes.
    pub fn new(current: Secret<String>, previous: Option<Secret<String>>) -> Result<Self, Error> {
        if current.expose().len() < MIN_SECRET_BYTES {
            return Err(Error::Config(format!(
                "the session secret must be at least {MIN_SECRET_BYTES} bytes"
            )));
        }
        if let Some(previous) = &previous
            && previous.expose().len() < MIN_SECRET_BYTES
        {
            return Err(Error::Config(format!(
                "the previous session secret must be at least {MIN_SECRET_BYTES} bytes"
            )));
        }
        Ok(Self { current, previous })
    }
}

/// What a verified token says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionClaims {
    /// The listener the token was for.
    pub audience: Audience,
    /// The opaque session id (single use).
    pub sid: String,
    /// The agent the app intended, if it said.
    pub agent: Option<String>,
    /// Longest call, in seconds, if the app set a cap.
    pub max_s: Option<u64>,
    /// The app's own key/value pairs (scalars rendered as text).
    pub meta: BTreeMap<String, String>,
    /// `nbf`, Unix seconds.
    pub not_before: u64,
    /// `exp`, Unix seconds.
    pub expires: u64,
}

impl SessionClaims {
    /// The call-duration cap, if the token has one.
    pub fn max_duration(&self) -> Option<Duration> {
        self.max_s.map(Duration::from_secs)
    }

    /// Adds the claims to a call's metadata: `session_id`, `session.agent`, `session.max_s` and
    /// `session.meta.<key>`. The values come from the application's backend (it holds the
    /// secret), not from the caller.
    pub fn add_metadata(&self, metadata: &mut BTreeMap<String, String>) {
        metadata.insert("session_id".to_owned(), self.sid.clone());
        if let Some(agent) = &self.agent {
            metadata.insert("session.agent".to_owned(), agent.clone());
        }
        if let Some(max_s) = self.max_s {
            metadata.insert("session.max_s".to_owned(), max_s.to_string());
        }
        for (key, value) in &self.meta {
            metadata.insert(format!("session.meta.{key}"), value.clone());
        }
    }
}

/// The wire payload.
#[derive(Serialize, Deserialize)]
struct Payload {
    aud: String,
    exp: u64,
    #[serde(default)]
    nbf: u64,
    sid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<Map<String, Value>>,
}

/// What [`mint_token`] signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSpec {
    /// The listener the token is for.
    pub audience: Audience,
    /// The session id: unique per token.
    pub sid: String,
    /// `nbf`, Unix seconds (0: valid at once).
    pub not_before: u64,
    /// `exp`, Unix seconds.
    pub expires: u64,
    /// Optional agent name.
    pub agent: Option<String>,
    /// Optional call-duration cap in seconds.
    pub max_s: Option<u64>,
    /// Optional metadata.
    pub meta: BTreeMap<String, String>,
}

/// Mints a session token (the application backend's side; the server only verifies). Rust
/// backends call this; other languages follow `docs/SECURITY.md`. The lifetime check reads the
/// system wall clock; [`mint_token_at`] takes the time explicitly.
///
/// # Errors
/// As [`mint_token_at`].
pub fn mint_token(secret: &Secret<String>, spec: &TokenSpec) -> Result<String, Error> {
    let now = brisvox_core::SystemClock
        .wall()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    mint_token_at(secret, spec, now)
}

/// [`mint_token`] with `now` (Unix seconds) given: `spec.expires` may be at most
/// [`MAX_TOKEN_LIFETIME`] after it.
///
/// # Errors
/// [`Error::Config`] when the secret is shorter than [`MIN_SECRET_BYTES`], `expires` is not after
/// `not_before` or more than [`MAX_TOKEN_LIFETIME`] after `now`, or a field is outside the limits
/// the verifier enforces (so a minted token is always accepted by a server with the same secret).
pub fn mint_token_at(secret: &Secret<String>, spec: &TokenSpec, now: u64) -> Result<String, Error> {
    if spec.expires.saturating_sub(now) > MAX_TOKEN_LIFETIME.as_secs() {
        return Err(Error::Config(format!(
            "session token lifetime exceeds {} seconds (is exp in seconds, not milliseconds?)",
            MAX_TOKEN_LIFETIME.as_secs()
        )));
    }
    if secret.expose().len() < MIN_SECRET_BYTES {
        return Err(Error::Config(format!(
            "the session secret must be at least {MIN_SECRET_BYTES} bytes"
        )));
    }
    let meta = if spec.meta.is_empty() {
        None
    } else {
        Some(
            spec.meta
                .iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect::<Map<String, Value>>(),
        )
    };
    let payload = Payload {
        aud: spec.audience.as_str().to_owned(),
        exp: spec.expires,
        nbf: spec.not_before,
        sid: spec.sid.clone(),
        agent: spec.agent.clone(),
        max_s: spec.max_s,
        meta,
    };
    check_limits(&payload)
        .map_err(|_| Error::Config("session token fields out of range".into()))?;
    let json = serde_json::to_vec(&payload)
        .map_err(|e| Error::Config(format!("session token payload: {e}")))?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.expose().as_bytes());
    let tag = hmac::sign(&key, &json);
    let token = format!(
        "{VERSION}.{}.{}",
        URL_SAFE_NO_PAD.encode(&json),
        URL_SAFE_NO_PAD.encode(tag.as_ref())
    );
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Error::Config(
            "session token would exceed 4096 bytes".into(),
        ));
    }
    Ok(token)
}

/// Field limits shared by the minter and the verifier.
fn check_limits(payload: &Payload) -> Result<(), AuthError> {
    let text_ok = |text: &str, max: usize| {
        !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
    };
    if !text_ok(&payload.sid, MAX_SID_BYTES) {
        return Err(AuthError::Malformed);
    }
    if let Some(agent) = &payload.agent
        && !text_ok(agent, MAX_AGENT_BYTES)
    {
        return Err(AuthError::Malformed);
    }
    if let Some(max_s) = payload.max_s
        && !(1..=MAX_CALL_SECONDS).contains(&max_s)
    {
        return Err(AuthError::Malformed);
    }
    if payload.exp <= payload.nbf {
        return Err(AuthError::Malformed);
    }
    if let Some(meta) = &payload.meta {
        if meta.len() > MAX_META_ENTRIES {
            return Err(AuthError::Malformed);
        }
        for (key, value) in meta {
            if !text_ok(key, MAX_META_KEY_BYTES) {
                return Err(AuthError::Malformed);
            }
            match value {
                Value::String(text) if text.len() <= MAX_META_VALUE_BYTES => {}
                Value::Number(_) | Value::Bool(_) => {}
                _ => return Err(AuthError::Malformed),
            }
        }
    }
    Ok(())
}

/// Single-use set: `(audience, sid)` until the token's expiry plus the skew.
struct ReplayCache {
    capacity: usize,
    live: HashMap<(Audience, String), u64>,
    by_expiry: BinaryHeap<Reverse<(u64, Audience, String)>>,
    /// Session ids whose single use was given back once, with the same retention as `live`. A
    /// second refund of a remembered id is refused, so one token cannot be recycled endlessly.
    /// Holds at most `capacity` entries; when it is full a refund is refused (the token stays
    /// spent), never an entry evicted.
    refunded: HashMap<(Audience, String), u64>,
    refunded_by_expiry: BinaryHeap<Reverse<(u64, Audience, String)>>,
}

impl ReplayCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            live: HashMap::new(),
            by_expiry: BinaryHeap::new(),
            refunded: HashMap::new(),
            refunded_by_expiry: BinaryHeap::new(),
        }
    }

    /// Drops the entries whose retention has passed.
    fn purge(&mut self, now: u64) {
        while let Some(Reverse((until, _, _))) = self.by_expiry.peek() {
            if *until > now {
                break;
            }
            if let Some(Reverse((until, audience, sid))) = self.by_expiry.pop() {
                let key = (audience, sid);
                if self.live.get(&key) == Some(&until) {
                    self.live.remove(&key);
                }
            }
        }
        while let Some(Reverse((until, _, _))) = self.refunded_by_expiry.peek() {
            if *until > now {
                break;
            }
            if let Some(Reverse((until, audience, sid))) = self.refunded_by_expiry.pop() {
                let key = (audience, sid);
                if self.refunded.get(&key) == Some(&until) {
                    self.refunded.remove(&key);
                }
            }
        }
    }

    /// Forgets `sid` so a token with it can be used again, once: `false` (nothing changed) when
    /// the id was refunded before, is not in use, or the refunded set is full. Its entry in
    /// `by_expiry` stays; `purge` ignores an entry whose `live` slot is gone or was renewed.
    fn refund(&mut self, audience: Audience, sid: &str, until: u64, now: u64) -> bool {
        self.purge(now);
        let key = (audience, sid.to_owned());
        if self.refunded.contains_key(&key)
            || self.refunded.len() >= self.capacity
            || !self.live.contains_key(&key)
        {
            return false;
        }
        self.live.remove(&key);
        self.refunded_by_expiry
            .push(Reverse((until, audience, sid.to_owned())));
        self.refunded.insert(key, until);
        true
    }

    fn insert(
        &mut self,
        audience: Audience,
        sid: &str,
        until: u64,
        now: u64,
    ) -> Result<(), AuthError> {
        self.purge(now);
        let key = (audience, sid.to_owned());
        if self.live.contains_key(&key) {
            return Err(AuthError::Replayed);
        }
        if self.live.len() >= self.capacity {
            return Err(AuthError::Busy);
        }
        self.live.insert(key, until);
        self.by_expiry
            .push(Reverse((until, audience, sid.to_owned())));
        Ok(())
    }
}

struct AuthInner {
    keys: Vec<hmac::Key>,
    clock: SharedClock,
    replay: Mutex<ReplayCache>,
    /// Unix second of the last "single-use set full" warning (0: none yet).
    last_busy_warning: AtomicU64,
}

/// Verifies session tokens. Cloning shares the key set and the single-use set.
#[derive(Clone)]
pub struct SessionAuth {
    inner: Arc<AuthInner>,
}

impl std::fmt::Debug for SessionAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAuth")
            .field("keys", &self.inner.keys.len())
            .finish_non_exhaustive()
    }
}

impl SessionAuth {
    /// A verifier for `secrets` on `clock`, remembering at most `replay_capacity` unexpired
    /// session ids (at least 1).
    pub fn new(secrets: &SessionSecrets, clock: SharedClock, replay_capacity: usize) -> Self {
        let mut keys = vec![hmac::Key::new(
            hmac::HMAC_SHA256,
            secrets.current.expose().as_bytes(),
        )];
        if let Some(previous) = &secrets.previous {
            keys.push(hmac::Key::new(
                hmac::HMAC_SHA256,
                previous.expose().as_bytes(),
            ));
        }
        Self {
            inner: Arc::new(AuthInner {
                keys,
                clock,
                replay: Mutex::new(ReplayCache::new(replay_capacity.max(1))),
                last_busy_warning: AtomicU64::new(0),
            }),
        }
    }

    fn now_unix(&self) -> u64 {
        self.inner
            .clock
            .wall()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// Checks `token` for `audience` without using it up. See the module docs for the order.
    ///
    /// # Errors
    /// The [`AuthError`] naming the first check that failed.
    pub fn verify(&self, audience: Audience, token: &str) -> Result<SessionClaims, AuthError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(AuthError::TooLarge);
        }
        let mut parts = token.split('.');
        let (Some(version), Some(payload_b64), Some(sig_b64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(AuthError::Malformed);
        };
        if version != VERSION {
            return Err(AuthError::Malformed);
        }
        let payload = URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| AuthError::Malformed)?;
        let signature = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| AuthError::Malformed)?;
        // The signature first: nothing below runs for a token its sender could not sign.
        if !self
            .inner
            .keys
            .iter()
            .any(|key| hmac::verify(key, &payload, &signature).is_ok())
        {
            return Err(AuthError::BadSignature);
        }
        let payload: Payload =
            serde_json::from_slice(&payload).map_err(|_| AuthError::Malformed)?;
        check_limits(&payload)?;
        let token_audience = Audience::parse(&payload.aud).ok_or(AuthError::WrongAudience)?;
        if token_audience != audience {
            return Err(AuthError::WrongAudience);
        }
        let now = self.now_unix();
        let skew = CLOCK_SKEW.as_secs();
        if payload.nbf > now.saturating_add(skew) {
            return Err(AuthError::NotYetValid);
        }
        if now >= payload.exp.saturating_add(skew) {
            return Err(AuthError::Expired);
        }
        if payload.exp.saturating_sub(now) > MAX_TOKEN_LIFETIME.as_secs().saturating_add(skew) {
            return Err(AuthError::LifetimeTooLong);
        }
        let meta = payload
            .meta
            .unwrap_or_default()
            .into_iter()
            .map(|(key, value)| {
                let text = match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                };
                (key, text)
            })
            .collect();
        Ok(SessionClaims {
            audience,
            sid: payload.sid,
            agent: payload.agent,
            max_s: payload.max_s,
            meta,
            not_before: payload.nbf,
            expires: payload.exp,
        })
    }

    /// Uses the token's `sid` up: a second token with the same `sid` is refused until it would
    /// have expired anyway.
    ///
    /// # Errors
    /// [`AuthError::Replayed`] for a `sid` already used, [`AuthError::Busy`] when the set is full
    /// of unexpired tokens.
    pub fn consume(&self, claims: &SessionClaims) -> Result<(), AuthError> {
        let now = self.now_unix();
        let until = claims.expires.saturating_add(CLOCK_SKEW.as_secs());
        let result = self
            .inner
            .replay
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(claims.audience, &claims.sid, until, now);
        if result == Err(AuthError::Busy) {
            self.warn_busy(now);
        }
        result
    }

    /// Logs that the single-use set is full, at most once a minute (every refused token would
    /// otherwise log).
    fn warn_busy(&self, now: u64) {
        let last = self.inner.last_busy_warning.load(Ordering::Relaxed);
        if last == 0 || now.saturating_sub(last) >= BUSY_WARNING_INTERVAL_S {
            self.inner.last_busy_warning.store(now, Ordering::Relaxed);
            tracing::warn!(
                "the session-token single-use set is full of unexpired tokens: new tokens are \
                 refused with 503 until some expire; check that the backend mints exp in seconds \
                 with a short TTL"
            );
        }
    }

    /// Gives a token's `sid` back after [`consume`](Self::consume) so the client may present the
    /// token once more: the request it was spent on never started a call (the server could not
    /// serve it, or the page hung up a WebRTC session that never connected). Only for a `sid`
    /// this caller consumed.
    ///
    /// A `sid` is refunded at most once until it would have expired anyway (plus the skew): the
    /// second refund returns `false` and the token stays spent, so a token cannot cycle through
    /// handshakes. The refunded ids are bounded by the replay capacity; when that is full a
    /// refund is refused too. Returns whether the token was given back.
    pub fn refund(&self, claims: &SessionClaims) -> bool {
        let now = self.now_unix();
        let until = claims.expires.saturating_add(CLOCK_SKEW.as_secs());
        self.inner
            .replay
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refund(claims.audience, &claims.sid, until, now)
    }

    /// [`verify`](Self::verify) and [`consume`](Self::consume).
    ///
    /// # Errors
    /// As both.
    pub fn verify_once(&self, audience: Audience, token: &str) -> Result<SessionClaims, AuthError> {
        let claims = self.verify(audience, token)?;
        self.consume(&claims)?;
        Ok(claims)
    }

    /// Session ids remembered now (tests and statistics).
    pub fn remembered(&self) -> usize {
        self.inner
            .replay
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live
            .len()
    }
}

/// What a request was granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// The listener needs no credential.
    Open,
    /// The static token matched.
    Static,
    /// A valid session token.
    Session(SessionClaims),
}

impl Grant {
    /// The session claims, if the grant was a session token.
    pub const fn claims(&self) -> Option<&SessionClaims> {
        match self {
            Self::Session(claims) => Some(claims),
            _ => None,
        }
    }

    /// Adds the session's metadata to a call's metadata (nothing for other grants).
    pub fn add_metadata(&self, metadata: &mut BTreeMap<String, String>) {
        if let Some(claims) = self.claims() {
            claims.add_metadata(metadata);
        }
    }
}

/// The credential check of one listener: a static token, session tokens, both, or neither
/// (open).
#[derive(Clone)]
pub struct Authenticator {
    audience: Audience,
    static_token: Option<Arc<Secret<String>>>,
    sessions: Option<SessionAuth>,
}

impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator")
            .field("audience", &self.audience)
            .field("static_token", &self.static_token.is_some())
            .field("sessions", &self.sessions.is_some())
            .finish()
    }
}

impl Authenticator {
    /// A check for `audience` with the given credentials (both `None`: open).
    pub fn new(
        audience: Audience,
        static_token: Option<Secret<String>>,
        sessions: Option<SessionAuth>,
    ) -> Self {
        Self {
            audience,
            static_token: static_token.map(Arc::new),
            sessions,
        }
    }

    /// Builds the check from a configuration, creating the verifier on `clock`.
    pub fn from_config(
        audience: Audience,
        static_token: Option<&Secret<String>>,
        secrets: Option<&SessionSecrets>,
        clock: &SharedClock,
    ) -> Self {
        Self::new(
            audience,
            static_token.cloned(),
            secrets.map(|s| SessionAuth::new(s, Arc::clone(clock), DEFAULT_REPLAY_CAPACITY)),
        )
    }

    /// Whether a credential is required.
    pub const fn required(&self) -> bool {
        self.static_token.is_some() || self.sessions.is_some()
    }

    /// Whether session tokens are accepted.
    pub const fn accepts_sessions(&self) -> bool {
        self.sessions.is_some()
    }

    /// Checks the presented credential without using a session token up.
    ///
    /// # Errors
    /// [`AuthError::Missing`] when a credential is required and none was presented, otherwise as
    /// [`SessionAuth::verify`] ([`AuthError::BadSignature`] for a static token that does not
    /// match).
    pub fn peek(&self, presented: Option<&str>) -> Result<Grant, AuthError> {
        if !self.required() {
            return Ok(Grant::Open);
        }
        let Some(presented) = presented else {
            return Err(AuthError::Missing);
        };
        if presented.len() > MAX_TOKEN_BYTES {
            return Err(AuthError::TooLarge);
        }
        if let Some(token) = &self.static_token
            && constant_time_eq(presented.as_bytes(), token.expose().as_bytes())
        {
            return Ok(Grant::Static);
        }
        match &self.sessions {
            Some(sessions) => sessions
                .verify(self.audience, presented)
                .map(Grant::Session),
            None => Err(AuthError::BadSignature),
        }
    }

    /// Uses a [`peek`](Self::peek)ed grant up (a session token's `sid`; nothing for the others).
    ///
    /// # Errors
    /// As [`SessionAuth::consume`].
    pub fn consume(&self, grant: &Grant) -> Result<(), AuthError> {
        match (grant, &self.sessions) {
            (Grant::Session(claims), Some(sessions)) => sessions.consume(claims),
            _ => Ok(()),
        }
    }

    /// Undoes a [`consume`](Self::consume) of `grant`, at most once per session id (see
    /// [`SessionAuth::refund`]); `false` when nothing was given back.
    pub fn refund(&self, grant: &Grant) -> bool {
        if let (Grant::Session(claims), Some(sessions)) = (grant, &self.sessions) {
            return sessions.refund(claims);
        }
        false
    }

    /// [`peek`](Self::peek) and [`consume`](Self::consume).
    ///
    /// # Errors
    /// As both.
    pub fn check(&self, presented: Option<&str>) -> Result<Grant, AuthError> {
        let grant = self.peek(presented)?;
        self.consume(&grant)?;
        Ok(grant)
    }
}

/// Compares two byte strings in constant time, length included: both are run through HMAC-SHA256
/// under a fixed (public) key so they have equal length, and the tags are compared by
/// `ring::hmac::verify`.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, b"brisvox static token comparison");
    let wanted = hmac::sign(&key, b);
    hmac::verify(&key, a, wanted.as_ref()).is_ok()
}

/// The token of an `Authorization: Bearer <token>` header value.
pub fn bearer(header: &str) -> Option<&str> {
    let (scheme, value) = header.split_once(' ')?;
    let value = value.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !value.is_empty()).then_some(value)
}

/// The token in a `Sec-WebSocket-Protocol` header value (a comma-separated list), from the entry
/// `brisvox-token.<token>`.
pub fn subprotocol_token(header: &str) -> Option<&str> {
    header
        .split(',')
        .map(str::trim)
        .find_map(|entry| entry.strip_prefix(SUBPROTOCOL_PREFIX))
        .filter(|token| !token.is_empty())
}

/// The subprotocol the server answers with when the client offered `header`: the plain
/// [`SUBPROTOCOL_NAME`] if it is in the list, else none. The `brisvox-token.<token>` entry is
/// never echoed back: it carries the credential, and a response (a log line, a proxy) would
/// repeat it. A client that offers only the token entry gets no `Sec-WebSocket-Protocol` in the
/// answer (a browser then refuses the connection by its own rules, so pages offer both entries).
pub fn selected_subprotocol(header: &str) -> Option<String> {
    header
        .split(',')
        .map(str::trim)
        .any(|entry| entry == SUBPROTOCOL_NAME)
        .then(|| SUBPROTOCOL_NAME.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use brisvox_core::SimClock;

    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef-current";
    const OTHER: &str = "ffffffffffffffffffffffffffffffff-other!!!";

    fn secret(text: &str) -> Secret<String> {
        Secret::new(text.to_owned())
    }

    /// A clock whose wall time starts at 2_000_000_000.
    fn clock() -> SharedClock {
        Arc::new(SimClock::with_wall_origin(
            UNIX_EPOCH + Duration::from_secs(2_000_000_000),
        ))
    }

    fn now(clock: &SharedClock) -> u64 {
        clock.wall().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    fn spec(clock: &SharedClock, sid: &str) -> TokenSpec {
        let now = now(clock);
        TokenSpec {
            audience: Audience::Webrtc,
            sid: sid.to_owned(),
            not_before: now - 1,
            expires: now + 60,
            agent: None,
            max_s: None,
            meta: BTreeMap::new(),
        }
    }

    fn auth(clock: &SharedClock) -> SessionAuth {
        let secrets = SessionSecrets::new(secret(SECRET), None).unwrap();
        SessionAuth::new(&secrets, Arc::clone(clock), 8)
    }

    fn mint(clock: &SharedClock, sid: &str) -> String {
        mint_token_at(&secret(SECRET), &spec(clock, sid), now(clock)).unwrap()
    }

    /// Re-signs a hand-written payload with `secret`.
    fn forge(secret_text: &str, payload: &[u8]) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret_text.as_bytes());
        let tag = hmac::sign(&key, payload);
        format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(tag.as_ref())
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_minted_token_verifies_and_carries_its_claims() {
        let clock = clock();
        let mut spec = spec(&clock, "visitor-42");
        spec.agent = Some("support".to_owned());
        spec.max_s = Some(600);
        spec.meta.insert("plan".to_owned(), "pro".to_owned());
        let token = mint_token_at(&secret(SECRET), &spec, now(&clock)).unwrap();
        let claims = auth(&clock).verify(Audience::Webrtc, &token).unwrap();
        assert_eq!(claims.sid, "visitor-42");
        assert_eq!(claims.agent.as_deref(), Some("support"));
        assert_eq!(claims.max_duration(), Some(Duration::from_secs(600)));
        let mut metadata = BTreeMap::new();
        claims.add_metadata(&mut metadata);
        assert_eq!(metadata["session_id"], "visitor-42");
        assert_eq!(metadata["session.agent"], "support");
        assert_eq!(metadata["session.max_s"], "600");
        assert_eq!(metadata["session.meta.plan"], "pro");
    }

    #[tokio::test(start_paused = true)]
    async fn expired_and_not_yet_valid_tokens_are_refused_with_a_skew_allowance() {
        let clock = clock();
        let auth = auth(&clock);
        let base = now(&clock);
        let mut late = spec(&clock, "late");
        late.not_before = base + 120;
        late.expires = base + 180;
        let late = mint_token_at(&secret(SECRET), &late, now(&clock)).unwrap();
        assert_eq!(
            auth.verify(Audience::Webrtc, &late),
            Err(AuthError::NotYetValid)
        );
        // Within the skew of nbf: accepted.
        let mut near = spec(&clock, "near");
        near.not_before = base + 20;
        near.expires = base + 90;
        let near = mint_token_at(&secret(SECRET), &near, now(&clock)).unwrap();
        assert!(auth.verify(Audience::Webrtc, &near).is_ok());

        let token = mint(&clock, "expiring"); // expires at base + 60
        assert!(auth.verify(Audience::Webrtc, &token).is_ok());
        tokio::time::advance(Duration::from_secs(60 + 29)).await;
        assert!(
            auth.verify(Audience::Webrtc, &token).is_ok(),
            "within the skew after exp"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            auth.verify(Audience::Webrtc, &token),
            Err(AuthError::Expired)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_wrong_audience_secret_payload_or_signature_is_refused() {
        let clock = clock();
        let auth = auth(&clock);
        let token = mint(&clock, "a");
        assert_eq!(
            auth.verify(Audience::Rtvi, &token),
            Err(AuthError::WrongAudience)
        );
        assert_eq!(
            auth.verify(Audience::WebSocket, &token),
            Err(AuthError::WrongAudience)
        );

        let foreign = mint_token_at(&secret(OTHER), &spec(&clock, "b"), now(&clock)).unwrap();
        assert_eq!(
            auth.verify(Audience::Webrtc, &foreign),
            Err(AuthError::BadSignature)
        );

        // Tampered payload: a different sid under the original signature.
        let parts: Vec<&str> = token.split('.').collect();
        let mut json = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let text = String::from_utf8(json.clone())
            .unwrap()
            .replace("\"a\"", "\"z\"");
        json = text.into_bytes();
        let tampered = format!("v1.{}.{}", URL_SAFE_NO_PAD.encode(&json), parts[2]);
        assert_eq!(
            auth.verify(Audience::Webrtc, &tampered),
            Err(AuthError::BadSignature)
        );

        // Tampered signature: one byte flipped.
        let mut sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        sig[0] ^= 1;
        let tampered = format!("v1.{}.{}", parts[1], URL_SAFE_NO_PAD.encode(&sig));
        assert_eq!(
            auth.verify(Audience::Webrtc, &tampered),
            Err(AuthError::BadSignature)
        );
        // A truncated signature.
        let tampered = format!("v1.{}.{}", parts[1], URL_SAFE_NO_PAD.encode(&sig[..16]));
        assert_eq!(
            auth.verify(Audience::Webrtc, &tampered),
            Err(AuthError::BadSignature)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_signature_is_checked_before_the_payload_is_parsed() {
        let clock = clock();
        let auth = auth(&clock);
        // Not JSON, wrongly signed: the answer is the signature failure, not a parse failure.
        let key = hmac::Key::new(hmac::HMAC_SHA256, OTHER.as_bytes());
        let junk = b"this is not json";
        let tag = hmac::sign(&key, junk);
        let token = format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(junk),
            URL_SAFE_NO_PAD.encode(tag.as_ref())
        );
        assert_eq!(
            auth.verify(Audience::Webrtc, &token),
            Err(AuthError::BadSignature)
        );
        // Not JSON but signed by the right secret: now the parse failure shows.
        assert_eq!(
            auth.verify(Audience::Webrtc, &forge(SECRET, junk)),
            Err(AuthError::Malformed)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_is_single_use_and_the_set_refuses_rather_than_evicts() {
        let clock = clock();
        let auth = auth(&clock); // capacity 8
        let token = mint(&clock, "once");
        assert!(auth.verify_once(Audience::Webrtc, &token).is_ok());
        assert_eq!(
            auth.verify_once(Audience::Webrtc, &token),
            Err(AuthError::Replayed)
        );
        // peek does not use it up
        let fresh = mint(&clock, "peeked");
        assert!(auth.verify(Audience::Webrtc, &fresh).is_ok());
        assert!(auth.verify(Audience::Webrtc, &fresh).is_ok());
        assert!(auth.verify_once(Audience::Webrtc, &fresh).is_ok());
        for n in 0..6 {
            let token = mint(&clock, &format!("fill-{n}"));
            assert!(auth.verify_once(Audience::Webrtc, &token).is_ok(), "{n}");
        }
        assert_eq!(auth.remembered(), 8);
        let overflow = mint(&clock, "overflow");
        assert_eq!(
            auth.verify_once(Audience::Webrtc, &overflow),
            Err(AuthError::Busy)
        );
        // The spent tokens stay spent while the set is full.
        assert_eq!(
            auth.verify_once(Audience::Webrtc, &token),
            Err(AuthError::Replayed)
        );
        // Once they have expired (exp + skew), their slots come back.
        tokio::time::advance(Duration::from_secs(60 + 31)).await;
        let later = mint(&clock, "later");
        assert!(auth.verify_once(Audience::Webrtc, &later).is_ok());
        assert_eq!(auth.remembered(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_is_refunded_at_most_once_per_sid() {
        let clock = clock();
        let auth = auth(&clock);
        let token = mint(&clock, "cycle");
        let claims = auth.verify_once(Audience::Webrtc, &token).unwrap();
        // Nothing to give back for a sid that is not in use.
        let unused = auth
            .verify(Audience::Webrtc, &mint(&clock, "unused"))
            .unwrap();
        assert!(!auth.refund(&unused));
        assert!(auth.refund(&claims), "the first refund is granted");
        let claims = auth.verify_once(Audience::Webrtc, &token).unwrap();
        assert!(!auth.refund(&claims), "the second refund is refused");
        assert_eq!(
            auth.verify_once(Audience::Webrtc, &token),
            Err(AuthError::Replayed),
            "the token stays spent"
        );
        assert!(!auth.refund(&claims), "and stays refused");
    }

    #[tokio::test(start_paused = true)]
    async fn refunds_are_bounded_and_refused_when_the_refunded_set_is_full() {
        let clock = clock();
        let auth = auth(&clock); // capacity 8
        for n in 0..8 {
            let token = mint(&clock, &format!("r-{n}"));
            let claims = auth.verify_once(Audience::Webrtc, &token).unwrap();
            assert!(auth.refund(&claims), "{n}");
        }
        let token = mint(&clock, "ninth");
        let claims = auth.verify_once(Audience::Webrtc, &token).unwrap();
        assert!(
            !auth.refund(&claims),
            "a full refunded set refuses, never evicts"
        );
        assert_eq!(
            auth.verify_once(Audience::Webrtc, &token),
            Err(AuthError::Replayed)
        );
        // Entries leave the refunded set with the tokens' expiry (exp + skew).
        tokio::time::advance(Duration::from_secs(60 + 31)).await;
        let token = mint(&clock, "later");
        let claims = auth.verify_once(Audience::Webrtc, &token).unwrap();
        assert!(auth.refund(&claims));
    }

    #[tokio::test(start_paused = true)]
    async fn the_authenticator_refund_shares_the_once_rule_with_every_caller() {
        let clock = clock();
        let session_auth = auth(&clock);
        let token = mint(&clock, "relay");
        let claims = session_auth.verify_once(Audience::Webrtc, &token).unwrap();
        let authenticator = Authenticator::new(Audience::Webrtc, None, Some(session_auth.clone()));
        assert!(
            authenticator.refund(&Grant::Session(claims)),
            "e.g. a relay-unavailable 503"
        );
        let claims = session_auth.verify_once(Audience::Webrtc, &token).unwrap();
        // A later hang-up refund of the same sid is refused: both paths count.
        assert!(!authenticator.refund(&Grant::Session(claims)));
        assert!(!authenticator.refund(&Grant::Static));
        assert!(!authenticator.refund(&Grant::Open));
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_lifetime_is_capped_when_verifying_and_when_minting() {
        let clock = clock();
        let auth = auth(&clock);
        let now = now(&clock);
        let day = MAX_TOKEN_LIFETIME.as_secs();
        // Exactly 24 h (and 24 h + skew, for a fast backend clock) is accepted.
        for (sid, ttl) in [("day", day), ("day-skew", day + CLOCK_SKEW.as_secs())] {
            let mut spec = spec(&clock, sid);
            spec.expires = now + ttl;
            let token = forge_spec(&spec);
            assert!(auth.verify(Audience::Webrtc, &token).is_ok(), "{sid}");
        }
        // A millisecond timestamp, a ten-year TTL and u64::MAX are refused and park nothing.
        for (sid, exp) in [
            ("millis", now * 1000),
            ("ten-years", now + 10 * 365 * day),
            ("max", u64::MAX),
            ("a-bit-over", now + day + CLOCK_SKEW.as_secs() + 1),
        ] {
            let mut spec = spec(&clock, sid);
            spec.expires = exp;
            let token = forge_spec(&spec);
            assert_eq!(
                auth.verify_once(Audience::Webrtc, &token),
                Err(AuthError::LifetimeTooLong),
                "{sid}"
            );
        }
        assert_eq!(auth.remembered(), 0);
        assert_eq!(AuthError::LifetimeTooLong.http_status(), 401);
        // The minter refuses the same values, so a minted token is always accepted.
        let mut spec = spec(&clock, "mint");
        spec.expires = now + day;
        assert!(mint_token_at(&secret(SECRET), &spec, now).is_ok());
        spec.expires = now + day + 1;
        assert!(matches!(
            mint_token_at(&secret(SECRET), &spec, now),
            Err(Error::Config(_))
        ));
        spec.expires = now * 1000;
        assert!(mint_token_at(&secret(SECRET), &spec, now).is_err());
    }

    /// Signs a spec's payload without the minter's lifetime check.
    fn forge_spec(spec: &TokenSpec) -> String {
        let json = format!(
            r#"{{"aud":"{}","exp":{},"nbf":{},"sid":"{}"}}"#,
            spec.audience.as_str(),
            spec.expires,
            spec.not_before,
            spec.sid
        );
        forge(SECRET, json.as_bytes())
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_single_use_set_warns_at_most_once_a_minute() {
        let clock = clock();
        let auth = auth(&clock); // capacity 8
        for n in 0..8 {
            let token = mint(&clock, &format!("fill-{n}"));
            assert!(auth.verify_once(Audience::Webrtc, &token).is_ok());
        }
        let busy = |sid: &str| auth.verify_once(Audience::Webrtc, &mint(&clock, sid));
        assert_eq!(busy("o1"), Err(AuthError::Busy));
        let first = auth.inner.last_busy_warning.load(Ordering::Relaxed);
        assert_ne!(first, 0);
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(busy("o2"), Err(AuthError::Busy));
        assert_eq!(auth.inner.last_busy_warning.load(Ordering::Relaxed), first);
        tokio::time::advance(Duration::from_secs(20)).await;
        assert_eq!(busy("o3"), Err(AuthError::Busy));
        assert_eq!(auth.inner.last_busy_warning.load(Ordering::Relaxed), first);
        // A minute after the first warning (the tokens are still live: 60 s lifetime + skew).
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(busy("o4"), Err(AuthError::Busy));
        assert!(auth.inner.last_busy_warning.load(Ordering::Relaxed) > first);
    }

    #[tokio::test(start_paused = true)]
    async fn the_previous_secret_still_verifies_during_a_rotation() {
        let clock = clock();
        let old = mint_token_at(&secret(OTHER), &spec(&clock, "old"), now(&clock)).unwrap();
        let rotated = SessionSecrets::new(secret(SECRET), Some(secret(OTHER))).unwrap();
        let auth = SessionAuth::new(&rotated, Arc::clone(&clock), 8);
        assert!(auth.verify(Audience::Webrtc, &old).is_ok());
        assert!(auth.verify(Audience::Webrtc, &mint(&clock, "new")).is_ok());
        let dropped = SessionSecrets::new(secret(SECRET), None).unwrap();
        let auth = SessionAuth::new(&dropped, Arc::clone(&clock), 8);
        assert_eq!(
            auth.verify(Audience::Webrtc, &old),
            Err(AuthError::BadSignature)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn oversize_and_malformed_tokens_are_refused_without_panicking() {
        let clock = clock();
        let auth = auth(&clock);
        let huge = format!("v1.{}.x", "a".repeat(MAX_TOKEN_BYTES));
        assert_eq!(
            auth.verify(Audience::Webrtc, &huge),
            Err(AuthError::TooLarge)
        );
        for bad in [
            "",
            ".",
            "..",
            "v1",
            "v1..",
            "v1.a.b",
            "v2.YQ.YQ",
            "v1.YQ.YQ.YQ",
            "v1.!!!.???",
            "v1.YQ==.YQ==",
            "v1.\u{1F600}.\u{1F600}",
            "bearer token",
        ] {
            assert!(auth.verify(Audience::Webrtc, bad).is_err(), "{bad:?}");
        }
        // Signed but outside the limits.
        let now = now(&clock);
        for payload in [
            format!(r#"{{"aud":"brisvox-webrtc","exp":{},"sid":""}}"#, now + 60),
            format!(
                r#"{{"aud":"brisvox-webrtc","exp":{},"sid":"a\u0000b"}}"#,
                now + 60
            ),
            format!(
                r#"{{"aud":"brisvox-webrtc","exp":{},"sid":"a","max_s":0}}"#,
                now + 60
            ),
            format!(
                r#"{{"aud":"brisvox-webrtc","exp":{},"sid":"a","meta":{{"k":[1]}}}}"#,
                now + 60
            ),
            format!(
                r#"{{"aud":"brisvox-webrtc","exp":{},"sid":"a","nbf":{}}}"#,
                now + 60,
                now + 99
            ),
            r#"{"aud":"brisvox-webrtc","sid":"a"}"#.to_owned(),
            r#"[1,2,3]"#.to_owned(),
            "null".to_owned(),
        ] {
            assert_eq!(
                auth.verify(Audience::Webrtc, &forge(SECRET, payload.as_bytes())),
                Err(AuthError::Malformed),
                "{payload}"
            );
        }
        assert_eq!(
            auth.verify(
                Audience::Webrtc,
                &forge(SECRET, br#"{"aud":"nope","exp":4000000000,"sid":"a"}"#)
            ),
            Err(AuthError::WrongAudience)
        );
    }

    /// A deterministic xorshift stream, so the "fuzz" runs are reproducible.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[tokio::test(start_paused = true)]
    async fn random_and_mutated_tokens_never_panic_and_never_verify() {
        let clock = clock();
        let auth = auth(&clock);
        let valid = mint(&clock, "fuzz");
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let alphabet = b"abcXYZ019-_.=+/ \t\"{}[]:,\\\xc3\xa9";
        for round in 0..3_000 {
            let len = usize::try_from(rng.next() % 200).unwrap();
            let random: Vec<u8> = (0..len)
                .map(|_| alphabet[usize::try_from(rng.next()).unwrap() % alphabet.len()])
                .collect();
            let text = String::from_utf8_lossy(&random).into_owned();
            assert!(
                auth.verify(Audience::Webrtc, &text).is_err(),
                "random {round}"
            );
            // Structured garbage: valid framing, random parts.
            let framed = format!(
                "v1.{}.{}",
                URL_SAFE_NO_PAD.encode(&random),
                URL_SAFE_NO_PAD.encode(&random)
            );
            assert!(
                auth.verify(Audience::Webrtc, &framed).is_err(),
                "framed {round}"
            );
            // A valid token with one character replaced.
            let mut bytes = valid.clone().into_bytes();
            let at = usize::try_from(rng.next()).unwrap() % bytes.len();
            let replacement = alphabet[usize::try_from(rng.next()).unwrap() % alphabet.len()];
            if bytes[at] == replacement {
                continue;
            }
            bytes[at] = replacement;
            if let Ok(mutated) = String::from_utf8(bytes) {
                assert!(
                    auth.verify(Audience::Webrtc, &mutated).is_err(),
                    "mutated {round}"
                );
            }
        }
        assert!(auth.verify(Audience::Webrtc, &valid).is_ok());
    }

    /// Tokens minted by the Python and TypeScript helpers (`sdk/python/brisvox_agent/session_token.py`,
    /// `sdk/typescript/src/session-token.ts`) at the simulated clock's origin, 2026-01-01T00:00:00Z.
    const GOLDEN_WEBRTC: &str = "v1.eyJhdWQiOiJicmlzdm94LXdlYnJ0YyIsImV4cCI6MTc2NzIyNjIwMCwic2lkIjoiZ29sZGVuLTEiLCJhZ2VudCI6InN1cHBvcnQiLCJtYXhfcyI6MzAwLCJtZXRhIjp7InBsYW4iOiJwcm8ifX0.zHMxezQrKQHSJbz6ffciHtW_RCV8WWYojeuQ9oa3p-A";
    const GOLDEN_RTVI: &str = "v1.eyJhdWQiOiJicmlzdm94LXJ0dmkiLCJleHAiOjE3NjcyMjYyMDAsInNpZCI6ImdvbGRlbi0yIn0.kg-l-Emp5-CxMv7XqkIXi6RRHjtx0s1UzPiDxuVRzRU";

    #[tokio::test(start_paused = true)]
    async fn tokens_minted_by_the_sdk_helpers_verify() {
        let clock: SharedClock = SimClock::shared();
        let secrets =
            SessionSecrets::new(secret("0123456789abcdef0123456789abcdef"), None).unwrap();
        let auth = SessionAuth::new(&secrets, Arc::clone(&clock), 8);
        let claims = auth.verify(Audience::Webrtc, GOLDEN_WEBRTC).unwrap();
        assert_eq!(claims.sid, "golden-1");
        assert_eq!(claims.agent.as_deref(), Some("support"));
        assert_eq!(claims.max_s, Some(300));
        assert_eq!(claims.meta["plan"], "pro");
        assert_eq!(claims.not_before, 0);
        assert_eq!(claims.expires, 1_767_225_600 + 600);
        let rtvi = auth.verify(Audience::Rtvi, GOLDEN_RTVI).unwrap();
        assert_eq!(rtvi.sid, "golden-2");
        assert_eq!(
            auth.verify(Audience::Webrtc, GOLDEN_RTVI),
            Err(AuthError::WrongAudience)
        );
        // And they expire like any other.
        tokio::time::advance(Duration::from_secs(700)).await;
        assert_eq!(
            auth.verify(Audience::Webrtc, GOLDEN_WEBRTC),
            Err(AuthError::Expired)
        );
    }

    #[test]
    fn short_secrets_are_refused() {
        assert!(SessionSecrets::new(secret("too short"), None).is_err());
        assert!(SessionSecrets::new(secret(SECRET), Some(secret("short"))).is_err());
        assert!(
            mint_token(
                &secret("short"),
                &TokenSpec {
                    audience: Audience::Rtvi,
                    sid: "a".into(),
                    not_before: 0,
                    expires: 10,
                    agent: None,
                    max_s: None,
                    meta: BTreeMap::new(),
                }
            )
            .is_err()
        );
        let secrets = SessionSecrets::new(secret(SECRET), None).unwrap();
        assert!(!format!("{secrets:?}").contains(SECRET));
    }

    #[tokio::test(start_paused = true)]
    async fn the_authenticator_combines_static_and_session_credentials() {
        let clock = clock();
        let secrets = SessionSecrets::new(secret(SECRET), None).unwrap();
        let both = Authenticator::from_config(
            Audience::Webrtc,
            Some(&secret("dev-token")),
            Some(&secrets),
            &clock,
        );
        assert!(both.required());
        assert_eq!(both.check(None), Err(AuthError::Missing));
        assert_eq!(both.check(Some("dev-token")), Ok(Grant::Static));
        assert_eq!(
            both.check(Some("dev-token")),
            Ok(Grant::Static),
            "static is reusable"
        );
        assert!(both.check(Some("dev-tokeN")).is_err());
        let token = mint(&clock, "x");
        let grant = both.peek(Some(&token)).unwrap();
        assert_eq!(grant.claims().unwrap().sid, "x");
        assert!(both.consume(&grant).is_ok());
        assert_eq!(both.consume(&grant), Err(AuthError::Replayed));
        let huge = "a".repeat(MAX_TOKEN_BYTES + 1);
        assert_eq!(both.check(Some(&huge)), Err(AuthError::TooLarge));

        let open = Authenticator::new(Audience::Webrtc, None, None);
        assert!(!open.required());
        assert_eq!(open.check(None), Ok(Grant::Open));
        assert_eq!(AuthError::TooLarge.http_status(), 413);
        assert_eq!(AuthError::Replayed.http_status(), 401);
        assert_eq!(AuthError::Busy.http_status(), 503);
    }

    #[test]
    fn header_helpers_extract_tokens() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
        assert_eq!(bearer("Bearer "), None);
        assert_eq!(
            subprotocol_token("brisvox, brisvox-token.v1.a.b"),
            Some("v1.a.b")
        );
        assert_eq!(subprotocol_token("brisvox-token."), None);
        assert_eq!(subprotocol_token("other"), None);
        assert_eq!(
            selected_subprotocol("brisvox-token.v1.a.b, brisvox").as_deref(),
            Some("brisvox")
        );
        assert_eq!(
            selected_subprotocol("brisvox-token.v1.a.b"),
            None,
            "the credential is never echoed back"
        );
        assert_eq!(selected_subprotocol("chat"), None);
    }
}
