//! Per-call stream URLs.
//!
//! The `X-Twilio-Signature` of a Media Streams upgrade covers the URL only (a WebSocket upgrade is a
//! GET without parameters, and `<Stream url>` cannot carry a query string), so with one fixed URL
//! the signature is one constant: whoever captured a single signed upgrade (a proxy or access log)
//! can replay it for ever. The fix is a URL that is different for every call: the TwiML names
//! `wss://host/twilio/stream/<segment>`, and Twilio signs that full URL as it does any other.
//!
//! ```text
//! segment = <callSid> "." <exp> "." <mac>
//! mac     = base64url_nopad( HMAC-SHA256( secret, "brisvox-twilio-stream-v1|" callSid "|" exp ) )
//! ```
//!
//! `callSid` is the call's SID (`CA...`, ASCII letters and digits), `exp` the Unix time in seconds
//! after which the URL is dead. An upgrade is accepted only if
//! - the segment parses and the MAC verifies (constant time, `ring::hmac::verify`);
//! - `exp` is in the future by the injected clock and at most [`MAX_TTL`] ahead (an operator's
//!   mistake cannot make the used-set remember a token for years);
//! - the segment has not been used before (a bounded set, entries dropped when they expire);
//! - the `start` message that follows carries the same `callSid` (checked by the connection:
//!   a URL minted for one call cannot open another).
//!
//! The TwiML is written by the server only for streams it restarts itself (DTMF, a failed warm
//! transfer: [`StreamUrlSigner`]). The first stream of an inbound call is named by the operator's
//! own webhook, which mints the segment with the same few lines of HMAC (`docs/providers/twilio.md`,
//! "Per-call stream URLs" has Python and Node versions). A deployment that cannot do that sets
//! [`TwilioConfig::allow_static_url`](super::TwilioConfig::allow_static_url) (the legacy mode: the
//! fixed URL, replayable).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use brisvox_core::{Secret, SharedClock};
use ring::hmac;

/// Shortest secret accepted, in bytes.
pub const MIN_SECRET_BYTES: usize = 32;

/// Furthest in the future an `exp` may lie.
pub const MAX_TTL: Duration = Duration::from_secs(3600);

/// Default lifetime of a segment the server mints itself.
pub const DEFAULT_TTL: Duration = Duration::from_secs(120);

/// Most unexpired tokens remembered at once; a full set refuses new upgrades (fail closed).
pub const MAX_USED: usize = 65_536;

/// Longest accepted segment, in bytes.
const MAX_SEGMENT_BYTES: usize = 160;

/// The domain label that is part of every MAC, so the key cannot be confused with another use.
const LABEL: &str = "brisvox-twilio-stream-v1";

/// Why a segment was refused. Only logged; the peer sees a plain 403.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// The segment does not have the `<callSid>.<exp>.<mac>` shape.
    Malformed,
    /// The MAC does not verify.
    BadMac,
    /// `exp` is in the past.
    Expired,
    /// `exp` is further away than [`MAX_TTL`].
    TooFar,
    /// The segment was used before.
    Reused,
    /// The used-set is full.
    Full,
}

/// A secret that does not meet the rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretTooShort;

impl std::fmt::Display for SecretTooShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the stream secret must be at least {MIN_SECRET_BYTES} bytes"
        )
    }
}

impl std::error::Error for SecretTooShort {}

fn key_of(secret: &Secret<String>) -> Result<hmac::Key, SecretTooShort> {
    if secret.expose().len() < MIN_SECRET_BYTES {
        return Err(SecretTooShort);
    }
    Ok(hmac::Key::new(
        hmac::HMAC_SHA256,
        secret.expose().as_bytes(),
    ))
}

fn message(call_sid: &str, exp: u64) -> String {
    format!("{LABEL}|{call_sid}|{exp}")
}

fn now_secs(clock: &SharedClock) -> u64 {
    clock
        .wall()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Whether `sid` can be a segment's call SID: 1 to 64 ASCII letters and digits.
fn sid_ok(sid: &str) -> bool {
    !sid.is_empty() && sid.len() <= 64 && sid.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Mints segments (the TwiML side). Cheap to clone behind an `Arc`.
pub struct StreamUrlSigner {
    key: hmac::Key,
    clock: SharedClock,
    ttl: Duration,
    /// The `exp` of the segment minted last: the next one is at least one second later, so two
    /// segments for the same call minted within one second (a restart that follows another) never
    /// share a MAC, and the used-set cannot refuse the second as already used.
    last_exp: AtomicU64,
}

impl std::fmt::Debug for StreamUrlSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamUrlSigner")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl StreamUrlSigner {
    /// A signer for `secret`, minting segments valid for `ttl` (at most [`MAX_TTL`]).
    ///
    /// # Errors
    /// [`SecretTooShort`] for a secret under [`MIN_SECRET_BYTES`] bytes.
    pub fn new(
        secret: &Secret<String>,
        clock: SharedClock,
        ttl: Duration,
    ) -> Result<Self, SecretTooShort> {
        Ok(Self {
            key: key_of(secret)?,
            clock,
            ttl: ttl.min(MAX_TTL),
            last_exp: AtomicU64::new(0),
        })
    }

    /// The segment for `call_sid`, valid from now for the signer's `ttl`. `None` for a SID that is
    /// not ASCII letters and digits (it could not be told apart from the separators). Every
    /// segment the signer mints has its own `exp` (the seconds count up when several are minted
    /// within one second; at most [`MAX_TTL`] ahead), so two segments for one call are different
    /// tokens.
    pub fn segment(&self, call_sid: &str) -> Option<String> {
        if !sid_ok(call_sid) {
            return None;
        }
        let exp = self.next_exp();
        segment_for(&self.key, call_sid, exp)
    }

    /// `max(now + ttl, last + 1)`, capped at `now + MAX_TTL`, recorded as the last one.
    fn next_exp(&self) -> u64 {
        let now = now_secs(&self.clock);
        let wanted = now.saturating_add(self.ttl.as_secs());
        let cap = now.saturating_add(MAX_TTL.as_secs());
        let mut last = self.last_exp.load(Ordering::Relaxed);
        loop {
            let exp = wanted.max(last.saturating_add(1)).min(cap);
            match self.last_exp.compare_exchange_weak(
                last,
                exp,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return exp,
                Err(actual) => last = actual,
            }
        }
    }

    /// `base` (a stream URL without a trailing `/`) followed by the segment for `call_sid`; `None`
    /// as [`segment`](Self::segment).
    pub fn url(&self, base: &str, call_sid: &str) -> Option<String> {
        let segment = self.segment(call_sid)?;
        Some(format!("{}/{segment}", base.trim_end_matches('/')))
    }
}

fn segment_for(key: &hmac::Key, call_sid: &str, exp: u64) -> Option<String> {
    if !sid_ok(call_sid) {
        return None;
    }
    let tag = hmac::sign(key, message(call_sid, exp).as_bytes());
    Some(format!(
        "{call_sid}.{exp}.{}",
        URL_SAFE_NO_PAD.encode(tag.as_ref())
    ))
}

/// Verifies segments and remembers the ones used (the listener side).
pub struct StreamTokenBook {
    key: hmac::Key,
    clock: SharedClock,
    /// MAC bytes of the segments used so far -> their `exp`.
    used: Mutex<HashMap<Vec<u8>, u64>>,
}

impl std::fmt::Debug for StreamTokenBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamTokenBook").finish_non_exhaustive()
    }
}

/// A segment that passed [`StreamTokenBook::check`] but is not consumed yet.
#[derive(Debug)]
pub struct Checked {
    /// The call SID the segment was minted for.
    pub call_sid: String,
    mac: Vec<u8>,
    exp: u64,
}

impl StreamTokenBook {
    /// A book for `secret`.
    ///
    /// # Errors
    /// [`SecretTooShort`] for a secret under [`MIN_SECRET_BYTES`] bytes.
    pub fn new(secret: &Secret<String>, clock: SharedClock) -> Result<Self, SecretTooShort> {
        Ok(Self {
            key: key_of(secret)?,
            clock,
            used: Mutex::new(HashMap::new()),
        })
    }

    /// Parses and verifies `segment` without using it up: shape, MAC, expiry, a segment not used
    /// before. The HMAC is checked before anything else is decided, so a wrong segment of any
    /// kind takes the same work.
    ///
    /// # Errors
    /// The [`Reject`] reason.
    pub fn check(&self, segment: &str) -> Result<Checked, Reject> {
        if segment.len() > MAX_SEGMENT_BYTES {
            return Err(Reject::Malformed);
        }
        let mut parts = segment.split('.');
        let (Some(sid), Some(exp_text), Some(mac_text), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Reject::Malformed);
        };
        let exp: u64 = exp_text.parse().map_err(|_| Reject::Malformed)?;
        let mac = URL_SAFE_NO_PAD
            .decode(mac_text.as_bytes())
            .map_err(|_| Reject::Malformed)?;
        if !sid_ok(sid) {
            return Err(Reject::Malformed);
        }
        if hmac::verify(&self.key, message(sid, exp).as_bytes(), &mac).is_err() {
            return Err(Reject::BadMac);
        }
        let now = now_secs(&self.clock);
        if exp <= now {
            return Err(Reject::Expired);
        }
        if exp - now > MAX_TTL.as_secs() {
            return Err(Reject::TooFar);
        }
        let used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        if used.contains_key(&mac) {
            return Err(Reject::Reused);
        }
        Ok(Checked {
            call_sid: sid.to_owned(),
            mac,
            exp,
        })
    }

    /// Uses `checked` up: no upgrade with the same segment is accepted afterwards.
    ///
    /// # Errors
    /// [`Reject::Reused`] when another upgrade used it first, [`Reject::Full`] when the set holds
    /// [`MAX_USED`] unexpired entries.
    pub fn consume(&self, checked: Checked) -> Result<String, Reject> {
        let now = now_secs(&self.clock);
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        if used.len() >= MAX_USED / 2 {
            used.retain(|_, exp| *exp > now);
        }
        if used.contains_key(&checked.mac) {
            return Err(Reject::Reused);
        }
        if used.len() >= MAX_USED {
            return Err(Reject::Full);
        }
        used.insert(checked.mac, checked.exp);
        Ok(checked.call_sid)
    }

    /// Entries in the used-set (expired ones included until the next prune).
    pub fn used_len(&self) -> usize {
        self.used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use brisvox_core::clock::SimClock;

    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef-secret";

    fn secret() -> Secret<String> {
        Secret::new(SECRET.to_owned())
    }

    fn rig() -> (SharedClock, StreamUrlSigner, StreamTokenBook) {
        let clock: SharedClock = Arc::new(SimClock::new());
        let signer = StreamUrlSigner::new(&secret(), clock.clone(), DEFAULT_TTL).unwrap();
        let book = StreamTokenBook::new(&secret(), clock.clone()).unwrap();
        (clock, signer, book)
    }

    fn hand_key() -> hmac::Key {
        hmac::Key::new(hmac::HMAC_SHA256, SECRET.as_bytes())
    }

    #[tokio::test(start_paused = true)]
    async fn a_minted_segment_verifies_once() {
        let (_clock, signer, book) = rig();
        let segment = signer.segment("CA0123456789abcdef").unwrap();
        let checked = book.check(&segment).unwrap();
        assert_eq!(checked.call_sid, "CA0123456789abcdef");
        // Checking does not use it up; consuming does.
        assert!(book.check(&segment).is_ok());
        assert_eq!(book.consume(checked).unwrap(), "CA0123456789abcdef");
        assert_eq!(book.check(&segment).unwrap_err(), Reject::Reused);
        assert_eq!(book.used_len(), 1);
    }

    #[test]
    fn the_format_is_what_the_docs_give_other_implementations() {
        // Computed with Python's hmac, hashlib and base64.urlsafe_b64encode (the snippet in
        // docs/providers/twilio.md).
        let segment = segment_for(&hand_key(), "CAabc123", 1_900_000_000).unwrap();
        assert_eq!(
            segment,
            "CAabc123.1900000000.ixBbTZftQyn5JYCeFPFULx9O_UM1uxv0OZ_JhN6GIm0"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn two_segments_for_one_call_in_one_second_are_both_usable() {
        // The clock does not move: both are minted in the same second.
        let (clock, signer, book) = rig();
        let first = signer.segment("CA1").unwrap();
        let second = signer.segment("CA1").unwrap();
        let third = signer.segment("CA1").unwrap();
        assert_ne!(first, second);
        assert_ne!(second, third);
        let exp = |segment: &str| segment.split('.').nth(1).unwrap().parse::<u64>().unwrap();
        assert_eq!(exp(&first), now_secs(&clock) + DEFAULT_TTL.as_secs());
        assert_eq!(exp(&second), exp(&first) + 1);
        assert_eq!(exp(&third), exp(&first) + 2);
        for segment in [&first, &second, &third] {
            let checked = book.check(segment).unwrap();
            book.consume(checked).unwrap();
        }
        assert_eq!(book.used_len(), 3);
        // Once the clock catches up, the exp follows the clock again.
        tokio::time::advance(Duration::from_secs(60)).await;
        let later = signer.segment("CA1").unwrap();
        assert_eq!(exp(&later), now_secs(&clock) + DEFAULT_TTL.as_secs());
    }

    #[tokio::test(start_paused = true)]
    async fn minting_never_goes_past_the_furthest_exp_the_book_accepts() {
        let clock: SharedClock = Arc::new(SimClock::new());
        let signer = StreamUrlSigner::new(&secret(), clock.clone(), MAX_TTL).unwrap();
        let book = StreamTokenBook::new(&secret(), clock.clone()).unwrap();
        for n in 0..20 {
            let segment = signer.segment("CA1").unwrap();
            assert!(book.check(&segment).is_ok(), "segment {n}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_follows_the_injected_clock() {
        let (_clock, signer, book) = rig();
        let segment = signer.segment("CA1").unwrap();
        assert!(book.check(&segment).is_ok());
        tokio::time::advance(DEFAULT_TTL - Duration::from_secs(5)).await;
        assert!(book.check(&segment).is_ok());
        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(book.check(&segment).unwrap_err(), Reject::Expired);
    }

    #[tokio::test(start_paused = true)]
    async fn an_exp_too_far_ahead_is_refused() {
        let (clock, _signer, book) = rig();
        let now = now_secs(&clock);
        let far = segment_for(&hand_key(), "CA1", now + MAX_TTL.as_secs() + 60).unwrap();
        assert_eq!(book.check(&far).unwrap_err(), Reject::TooFar);
        let edge = segment_for(&hand_key(), "CA1", now + MAX_TTL.as_secs()).unwrap();
        assert!(book.check(&edge).is_ok());
        let past = segment_for(&hand_key(), "CA1", now).unwrap();
        assert_eq!(book.check(&past).unwrap_err(), Reject::Expired);
    }

    #[tokio::test(start_paused = true)]
    async fn tampering_is_refused() {
        let (clock, signer, book) = rig();
        let segment = signer.segment("CA1").unwrap();
        let (sid, rest) = segment.split_once('.').unwrap();
        let (exp, mac) = rest.split_once('.').unwrap();
        let exp_up: u64 = exp.parse::<u64>().unwrap() + 1;
        for bad in [
            format!("CA2.{exp}.{mac}"),
            format!("{sid}.{exp_up}.{mac}"),
            format!("{sid}.{exp}.{mac}A"),
            format!("{sid}.{exp}.{}", &mac[1..]),
            format!("{sid}.{exp}."),
            format!("{sid}.{exp}"),
            format!("{sid}.{exp}.{mac}.x"),
            format!(".{exp}.{mac}"),
            format!("{sid}.x.{mac}"),
            format!("{sid}.{exp}.!!!"),
            String::new(),
            "x".repeat(MAX_SEGMENT_BYTES + 1),
        ] {
            assert!(book.check(&bad).is_err(), "{bad}");
        }
        // Another secret does not verify.
        let other = StreamUrlSigner::new(
            &Secret::new("another-secret-another-secret-another".to_owned()),
            clock,
            DEFAULT_TTL,
        )
        .unwrap();
        let foreign = other.segment("CA1").unwrap();
        assert_eq!(book.check(&foreign).unwrap_err(), Reject::BadMac);
    }

    #[tokio::test(start_paused = true)]
    async fn weak_secrets_and_odd_sids_are_refused() {
        let clock: SharedClock = Arc::new(SimClock::new());
        let short = Secret::new("short".to_owned());
        assert!(StreamUrlSigner::new(&short, clock.clone(), DEFAULT_TTL).is_err());
        assert!(StreamTokenBook::new(&short, clock.clone()).is_err());
        let signer = StreamUrlSigner::new(&secret(), clock, DEFAULT_TTL).unwrap();
        assert!(signer.segment("").is_none());
        assert!(signer.segment("CA.1").is_none());
        assert!(signer.segment("CA|1").is_none());
        let url = signer.url("wss://h.example/twilio/stream/", "CA1").unwrap();
        assert!(url.starts_with("wss://h.example/twilio/stream/CA1."));
    }

    #[tokio::test(start_paused = true)]
    async fn the_used_set_is_pruned_when_entries_expire_and_bounded() {
        let (clock, _signer, book) = rig();
        let exp = now_secs(&clock) + 100;
        for i in 0..50 {
            let segment = segment_for(&hand_key(), &format!("CA{i}"), exp).unwrap();
            let checked = book.check(&segment).unwrap();
            book.consume(checked).unwrap();
        }
        assert_eq!(book.used_len(), 50);
        // Fill the set to the prune threshold with expired entries, then consume one more.
        {
            let mut used = book.used.lock().unwrap();
            for i in 0..(MAX_USED / 2) {
                used.insert(i.to_be_bytes().to_vec(), 1);
            }
        }
        tokio::time::advance(Duration::from_secs(200)).await;
        let exp = now_secs(&clock) + 100;
        let segment = segment_for(&hand_key(), "CAnew", exp).unwrap();
        let checked = book.check(&segment).unwrap();
        book.consume(checked).unwrap();
        assert_eq!(book.used_len(), 1, "expired entries were dropped");
        // A full set of live entries refuses (fail closed).
        {
            let mut used = book.used.lock().unwrap();
            for i in 0..MAX_USED {
                used.insert(i.to_be_bytes().to_vec(), u64::MAX);
            }
        }
        let segment = segment_for(&hand_key(), "CAmore", exp).unwrap();
        let checked = book.check(&segment).unwrap();
        assert_eq!(book.consume(checked).unwrap_err(), Reject::Full);
    }
}
