//! SRTP for the SIP transport: RFC 3711 with the SDES key exchange of RFC 4568.
//!
//! One suite family, the one every SIP trunk and PBX offers (`docs/TELEPHONY.md` section 8):
//! `AES_CM_128_HMAC_SHA1_80` (RFC 4568 section 6.2, mandatory to implement) and
//! `AES_CM_128_HMAC_SHA1_32` (the 32-bit tag, common for voice). AES-128 counter mode comes from
//! RustCrypto (`aes`, `ctr`), the HMAC-SHA1 from `ring`, so there is no C code and no second
//! crypto stack. There is no SRTCP: this endpoint sends no RTCP and ignores the peer's (a
//! secure peer's RTCP goes to the odd port next to the RTP port, which is not open).
//!
//! # What is here
//! - [`MasterKey`]: the 16-byte key and 14-byte salt of one direction, exchanged as base64 in
//!   `a=crypto:... inline:...`. Generated from the system CSPRNG, wiped on drop, never in `Debug`.
//! - [`SrtpTx`] ([`protect`](SrtpTx::protect)): in place, no allocation when the packet buffer has
//!   room for the tag; the 48-bit packet index follows the rollover counter (RFC 3711 section 3.3.1).
//! - [`SrtpRx`] ([`unprotect`](SrtpRx::unprotect)): estimates the index (appendix A), checks a
//!   128-packet replay window, verifies the tag in constant time over the packet and the ROC
//!   and only then decrypts and moves its state, so a forged packet changes nothing. Up to
//!   [`RX_STREAMS`] SSRCs are tracked (a transfer or a media server change brings a new one).
//!
//! The per-packet cost is one HMAC-SHA1 and one AES-CTR pass over about 200 bytes, a few
//! microseconds: it runs on the call task like the rest of the packet work.

use aes::Aes128;
use aes::cipher::{KeyIvInit, StreamCipher};
use base64::Engine as _;
use ctr::Ctr128BE;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use subtle::ConstantTimeEq as _;
use zeroize::Zeroize;

use super::sdp::{CryptoLine, LocalCrypto};

/// Master key length of both suites (RFC 4568 section 6.2).
pub const MASTER_KEY_LEN: usize = 16;
/// Master salt length of both suites.
pub const MASTER_SALT_LEN: usize = 14;
/// Session authentication key length (HMAC-SHA1, RFC 3711 section 4.2.1).
const AUTH_KEY_LEN: usize = 20;
/// SSRCs whose receive state is kept at once.
pub const RX_STREAMS: usize = 4;
/// The replay window in packets (RFC 3711 section 3.3.2 asks for at least 64).
const REPLAY_WINDOW: u64 = 128;

/// Why a packet or a key was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SrtpError {
    /// Too short, or the RTP header is not valid.
    #[error("malformed SRTP packet")]
    Malformed,
    /// The packet index was seen already, or is older than the replay window.
    #[error("replayed or too old SRTP packet")]
    Replay,
    /// The authentication tag did not match.
    #[error("SRTP authentication failed")]
    AuthFailed,
    /// The key material is unusable (wrong length, not base64).
    #[error("invalid SRTP key material: {0}")]
    Key(&'static str),
    /// The system random generator failed.
    #[error("no randomness for the SRTP key")]
    Random,
}

/// Whether calls use SRTP ([`SipConfig::srtp`](super::SipConfig::srtp), `BRISVOX_SIP_SRTP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrtpPolicy {
    /// Plain RTP only; an offer that has only `RTP/SAVP` is refused.
    #[default]
    Off,
    /// SRTP when the peer offers it on a TLS signaling connection, plain RTP otherwise.
    Optional,
    /// Only SRTP: a call whose offer has no acceptable `a=crypto` line, or that arrives on a
    /// connection that is not TLS, is refused with 488.
    Required,
}

impl std::str::FromStr for SrtpPolicy {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "optional" => Ok(Self::Optional),
            "required" => Ok(Self::Required),
            other => Err(format!(
                "unknown SRTP policy {other:?} (expected off, optional or required)"
            )),
        }
    }
}

/// A negotiated crypto suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SrtpSuite {
    /// `AES_CM_128_HMAC_SHA1_80`: 80-bit authentication tag.
    AesCm128HmacSha1_80,
    /// `AES_CM_128_HMAC_SHA1_32`: 32-bit authentication tag.
    AesCm128HmacSha1_32,
}

impl SrtpSuite {
    /// The suite name as written in `a=crypto`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::AesCm128HmacSha1_80 => "AES_CM_128_HMAC_SHA1_80",
            Self::AesCm128HmacSha1_32 => "AES_CM_128_HMAC_SHA1_32",
        }
    }

    /// The suite for a name, `None` for the ones this endpoint does not implement.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "AES_CM_128_HMAC_SHA1_80" => Some(Self::AesCm128HmacSha1_80),
            "AES_CM_128_HMAC_SHA1_32" => Some(Self::AesCm128HmacSha1_32),
            _ => None,
        }
    }

    /// Bytes the authentication tag adds to every packet.
    pub const fn tag_len(self) -> usize {
        match self {
            Self::AesCm128HmacSha1_80 => 10,
            Self::AesCm128HmacSha1_32 => 4,
        }
    }
}

/// The master key and salt of one direction. Wiped on drop; `Debug` shows nothing.
#[derive(Clone)]
pub struct MasterKey {
    key: [u8; MASTER_KEY_LEN],
    salt: [u8; MASTER_SALT_LEN],
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey([REDACTED])")
    }
}

impl PartialEq for MasterKey {
    fn eq(&self, other: &Self) -> bool {
        let mut a = [0u8; MASTER_KEY_LEN + MASTER_SALT_LEN];
        let mut b = [0u8; MASTER_KEY_LEN + MASTER_SALT_LEN];
        self.write_into(&mut a);
        other.write_into(&mut b);
        let same = bool::from(a.ct_eq(&b));
        a.zeroize();
        b.zeroize();
        same
    }
}

impl Eq for MasterKey {}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.key.zeroize();
        self.salt.zeroize();
    }
}

impl MasterKey {
    /// A fresh random key and salt.
    ///
    /// # Errors
    /// [`SrtpError::Random`] when the system generator fails.
    pub fn generate() -> Result<Self, SrtpError> {
        let rng = SystemRandom::new();
        let mut key = [0u8; MASTER_KEY_LEN];
        let mut salt = [0u8; MASTER_SALT_LEN];
        rng.fill(&mut key).map_err(|_| SrtpError::Random)?;
        rng.fill(&mut salt).map_err(|_| SrtpError::Random)?;
        Ok(Self { key, salt })
    }

    /// A key from raw bytes (key then salt).
    pub fn from_bytes(bytes: &[u8; MASTER_KEY_LEN + MASTER_SALT_LEN]) -> Self {
        let mut key = [0u8; MASTER_KEY_LEN];
        let mut salt = [0u8; MASTER_SALT_LEN];
        key.copy_from_slice(&bytes[..MASTER_KEY_LEN]);
        salt.copy_from_slice(&bytes[MASTER_KEY_LEN..]);
        Self { key, salt }
    }

    fn write_into(&self, out: &mut [u8; MASTER_KEY_LEN + MASTER_SALT_LEN]) {
        out[..MASTER_KEY_LEN].copy_from_slice(&self.key);
        out[MASTER_KEY_LEN..].copy_from_slice(&self.salt);
    }

    /// Parses the base64 `key||salt` of an `inline:` key parameter (30 bytes; with or without
    /// padding).
    ///
    /// # Errors
    /// [`SrtpError::Key`] for text that is not base64 or does not decode to 30 bytes.
    pub fn from_base64(text: &str) -> Result<Self, SrtpError> {
        let mut raw = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(text.trim_end_matches('='))
            .map_err(|_| SrtpError::Key("not base64"))?;
        let result = <[u8; MASTER_KEY_LEN + MASTER_SALT_LEN]>::try_from(raw.as_slice())
            .map(|bytes| Self::from_bytes(&bytes))
            .map_err(|_| SrtpError::Key("key||salt must be 30 bytes"));
        raw.zeroize();
        result
    }

    /// The base64 `key||salt` for `a=crypto`. The returned text is key material: put it in the
    /// SDP and nowhere else.
    pub fn to_base64(&self) -> String {
        let mut raw = [0u8; MASTER_KEY_LEN + MASTER_SALT_LEN];
        self.write_into(&mut raw);
        let text = base64::engine::general_purpose::STANDARD.encode(raw);
        raw.zeroize();
        text
    }
}

/// The session keys derived from a [`MasterKey`] (RFC 3711 section 4.3).
struct SessionKeys {
    cipher_key: [u8; MASTER_KEY_LEN],
    salt: [u8; MASTER_SALT_LEN],
    auth: hmac::Key,
}

impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.cipher_key.zeroize();
        self.salt.zeroize();
    }
}

/// Key derivation labels (RFC 3711 section 4.3.2) for SRTP.
const LABEL_CIPHER: u8 = 0x00;
const LABEL_AUTH: u8 = 0x01;
const LABEL_SALT: u8 = 0x02;

/// The key derivation function (section 4.3.1) with `key_derivation_rate` 0: AES-CM with
/// `IV = (master_salt XOR (label << 48)) << 16`, the first `out.len()` bytes of the keystream.
fn derive(master: &MasterKey, label: u8, out: &mut [u8]) -> Result<(), SrtpError> {
    let mut iv = [0u8; 16];
    iv[..MASTER_SALT_LEN].copy_from_slice(&master.salt);
    // key_id = label || (index DIV kdr) is 7 bytes wide, right-aligned in the 14-byte salt.
    iv[7] ^= label;
    out.fill(0);
    // From slices: no extra copy of the key on the stack. The cipher (key schedule) wipes itself
    // on drop (the `zeroize` features of `aes` and `ctr`).
    let result = Ctr128BE::<Aes128>::new_from_slices(&master.key, &iv)
        .map(|mut cipher| cipher.apply_keystream(out))
        .map_err(|_| SrtpError::Key("bad cipher key or IV length"));
    iv.zeroize();
    result
}

impl SessionKeys {
    /// The session keys of `master`. The intermediate buffers are wiped; `ring`'s HMAC key has no
    /// wipe of its own (its schedule lives until the key is dropped).
    fn derive(master: &MasterKey) -> Result<Self, SrtpError> {
        let mut keys = Self {
            cipher_key: [0u8; MASTER_KEY_LEN],
            salt: [0u8; MASTER_SALT_LEN],
            auth: hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &[0u8; AUTH_KEY_LEN]),
        };
        let mut auth = [0u8; AUTH_KEY_LEN];
        // `keys` wipes what it holds if a step fails (its `Drop`).
        derive(master, LABEL_CIPHER, &mut keys.cipher_key)?;
        derive(master, LABEL_SALT, &mut keys.salt)?;
        let derived = derive(master, LABEL_AUTH, &mut auth);
        if derived.is_ok() {
            keys.auth = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &auth);
        }
        auth.zeroize();
        derived.map(|()| keys)
    }

    /// XORs the AES-CM keystream of packet `index` of stream `ssrc` into `payload` (section 4.1.1).
    fn crypt(&self, ssrc: u32, index: u64, payload: &mut [u8]) -> Result<(), SrtpError> {
        let mut iv = [0u8; 16];
        iv[..MASTER_SALT_LEN].copy_from_slice(&self.salt);
        for (byte, x) in iv.iter_mut().skip(4).zip(ssrc.to_be_bytes()) {
            *byte ^= x;
        }
        let index = index.to_be_bytes();
        for (byte, x) in iv.iter_mut().skip(8).zip(index.iter().skip(2)) {
            *byte ^= *x;
        }
        let result = Ctr128BE::<Aes128>::new_from_slices(&self.cipher_key, &iv)
            .map(|mut cipher| cipher.apply_keystream(payload))
            .map_err(|_| SrtpError::Key("bad cipher key or IV length"));
        iv.zeroize();
        result
    }

    /// The authentication tag over `authenticated` and the rollover counter (section 4.2).
    fn tag(&self, authenticated: &[u8], roc: u32) -> hmac::Tag {
        let mut context = hmac::Context::with_key(&self.auth);
        context.update(authenticated);
        context.update(&roc.to_be_bytes());
        context.sign()
    }
}

/// What the fixed RTP header of a packet says.
struct HeaderInfo {
    /// Bytes of header, CSRC list and header extension: where the payload starts.
    len: usize,
    sequence: u16,
    ssrc: u32,
}

fn header_info(packet: &[u8]) -> Result<HeaderInfo, SrtpError> {
    let &[first, _, s0, s1, _, _, _, _, c0, c1, c2, c3, ..] = packet else {
        return Err(SrtpError::Malformed);
    };
    if first >> 6 != 2 {
        return Err(SrtpError::Malformed);
    }
    let mut len = 12 + usize::from(first & 0x0f) * 4;
    if first & 0x10 != 0 {
        let words = packet
            .get(len + 2..len + 4)
            .and_then(|b| <[u8; 2]>::try_from(b).ok())
            .map(u16::from_be_bytes)
            .ok_or(SrtpError::Malformed)?;
        len += 4 + usize::from(words) * 4;
    }
    Ok(HeaderInfo {
        len,
        sequence: u16::from_be_bytes([s0, s1]),
        ssrc: u32::from_be_bytes([c0, c1, c2, c3]),
    })
}

/// The sending half: protects the RTP packets of one call.
pub struct SrtpTx {
    suite: SrtpSuite,
    keys: SessionKeys,
    roc: u32,
    last_sequence: Option<u16>,
}

impl std::fmt::Debug for SrtpTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SrtpTx")
            .field("suite", &self.suite)
            .finish_non_exhaustive()
    }
}

impl SrtpTx {
    /// A sender for `suite` with `master`.
    ///
    /// # Errors
    /// [`SrtpError::Key`] when the cipher refuses the key (cannot happen with a [`MasterKey`]).
    pub fn new(suite: SrtpSuite, master: &MasterKey) -> Result<Self, SrtpError> {
        Ok(Self {
            suite,
            keys: SessionKeys::derive(master)?,
            roc: 0,
            last_sequence: None,
        })
    }

    /// Encrypts the payload of the RTP packet in `packet` in place and appends the tag.
    ///
    /// The packets must be given in the order they are sent: a sequence number below the
    /// previous one is taken as a wrap and moves the rollover counter.
    ///
    /// # Errors
    /// [`SrtpError::Malformed`] when `packet` is not an RTP packet.
    pub fn protect(&mut self, packet: &mut Vec<u8>) -> Result<(), SrtpError> {
        let info = header_info(packet)?;
        if packet.len() < info.len {
            return Err(SrtpError::Malformed);
        }
        if let Some(last) = self.last_sequence
            && info.sequence < last
        {
            self.roc = self.roc.wrapping_add(1);
        }
        self.last_sequence = Some(info.sequence);
        let index = (u64::from(self.roc) << 16) | u64::from(info.sequence);
        let payload = packet.get_mut(info.len..).ok_or(SrtpError::Malformed)?;
        self.keys.crypt(info.ssrc, index, payload)?;
        let tag = self.keys.tag(packet, self.roc);
        let tag = tag
            .as_ref()
            .get(..self.suite.tag_len())
            .ok_or(SrtpError::Malformed)?;
        packet.extend_from_slice(tag);
        Ok(())
    }
}

/// The receive state of one SSRC.
#[derive(Clone, Copy)]
struct RxStream {
    ssrc: u32,
    /// The highest packet index accepted so far.
    highest: u64,
    /// Bit `n` set: index `highest - n` was accepted (bit 0 is `highest`).
    window: u128,
}

/// The receiving half: authenticates and decrypts the RTP packets of one call.
pub struct SrtpRx {
    suite: SrtpSuite,
    keys: SessionKeys,
    streams: [Option<RxStream>; RX_STREAMS],
    /// The slot a new SSRC takes when none is free (round robin).
    next_slot: usize,
}

impl std::fmt::Debug for SrtpRx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SrtpRx")
            .field("suite", &self.suite)
            .finish_non_exhaustive()
    }
}

/// The packet index for `sequence` given the highest index so far (RFC 3711 appendix A).
/// `None` when the packet would belong to before the first rollover counter value.
fn estimate_index(highest: u64, sequence: u16) -> Option<u64> {
    let roc = highest >> 16;
    let last = (highest & 0xffff) as u16;
    let seq = u64::from(sequence);
    let roc_guess = if last < 0x8000 {
        if sequence.wrapping_sub(last) > 0x8000 && sequence > last {
            roc.checked_sub(1)?
        } else {
            roc
        }
    } else if last - 0x8000 > sequence {
        roc + 1
    } else {
        roc
    };
    if roc_guess > u64::from(u32::MAX) {
        return None;
    }
    Some((roc_guess << 16) | seq)
}

impl SrtpRx {
    /// A receiver for `suite` with the peer's `master`.
    ///
    /// # Errors
    /// [`SrtpError::Key`] when the cipher refuses the key (cannot happen with a [`MasterKey`]).
    pub fn new(suite: SrtpSuite, master: &MasterKey) -> Result<Self, SrtpError> {
        Ok(Self {
            suite,
            keys: SessionKeys::derive(master)?,
            streams: [None; RX_STREAMS],
            next_slot: 0,
        })
    }

    /// Verifies and decrypts the SRTP packet in `packet` in place and returns the length of the
    /// RTP packet that remains (the tag is cut off). Nothing is changed in the receiver and the
    /// packet is left as it was unless the tag is valid.
    ///
    /// # Errors
    /// [`SrtpError::Malformed`], [`SrtpError::Replay`] or [`SrtpError::AuthFailed`].
    pub fn unprotect(&mut self, packet: &mut [u8]) -> Result<usize, SrtpError> {
        let tag_len = self.suite.tag_len();
        let info = header_info(packet)?;
        let body_end = packet
            .len()
            .checked_sub(tag_len)
            .filter(|end| *end >= info.len)
            .ok_or(SrtpError::Malformed)?;

        let known = self
            .streams
            .iter()
            .position(|s| s.is_some_and(|s| s.ssrc == info.ssrc));
        let state = known.and_then(|slot| self.streams.get(slot).copied().flatten());
        let index = match state {
            Some(stream) => {
                let index =
                    estimate_index(stream.highest, info.sequence).ok_or(SrtpError::Replay)?;
                if index <= stream.highest {
                    let behind = stream.highest - index;
                    if behind >= REPLAY_WINDOW || stream.window & (1u128 << behind) != 0 {
                        return Err(SrtpError::Replay);
                    }
                }
                index
            }
            // A stream not seen yet starts at rollover counter 0 (RFC 3711 section 3.3.1).
            None => u64::from(info.sequence),
        };

        let roc = u32::try_from(index >> 16).map_err(|_| SrtpError::Malformed)?;
        let (authenticated, received_tag) = packet.split_at(body_end);
        let expected = self.keys.tag(authenticated, roc);
        let expected = expected
            .as_ref()
            .get(..tag_len)
            .ok_or(SrtpError::Malformed)?;
        if !bool::from(expected.ct_eq(received_tag)) {
            return Err(SrtpError::AuthFailed);
        }

        let payload = packet
            .get_mut(info.len..body_end)
            .ok_or(SrtpError::Malformed)?;
        self.keys.crypt(info.ssrc, index, payload)?;

        // Authenticated: now the state moves.
        let updated = match state {
            Some(mut stream) => {
                if index > stream.highest {
                    let shift = index - stream.highest;
                    stream.window = if shift >= 128 {
                        0
                    } else {
                        stream.window << shift
                    };
                    stream.window |= 1;
                    stream.highest = index;
                } else {
                    stream.window |= 1u128 << (stream.highest - index);
                }
                stream
            }
            None => RxStream {
                ssrc: info.ssrc,
                highest: index,
                window: 1,
            },
        };
        let slot = known.unwrap_or_else(|| {
            let free = self.streams.iter().position(Option::is_none);
            free.unwrap_or_else(|| {
                let slot = self.next_slot % RX_STREAMS;
                self.next_slot = (slot + 1) % RX_STREAMS;
                slot
            })
        });
        if let Some(entry) = self.streams.get_mut(slot) {
            *entry = Some(updated);
        }
        Ok(body_end)
    }
}

/// The SRTP state of one call: our key (what we send with), the two halves, and the line we
/// write in the SDP.
#[derive(Debug)]
pub struct SrtpSession {
    /// Our crypto line (tag, suite, our master key).
    pub(crate) local: LocalCrypto,
    /// The peer's master key, to tell a rekey from a repeat of the same offer.
    remote_key: MasterKey,
    pub(crate) tx: SrtpTx,
    pub(crate) rx: SrtpRx,
}

impl SrtpSession {
    /// A session for our `local` crypto line and the peer's `remote` line (same suite).
    ///
    /// # Errors
    /// [`SrtpError::Key`] when the cipher refuses a key (cannot happen with a [`MasterKey`]).
    pub(crate) fn new(local: LocalCrypto, remote: &CryptoLine) -> Result<Self, SrtpError> {
        Ok(Self {
            tx: SrtpTx::new(local.suite, &local.key)?,
            rx: SrtpRx::new(remote.suite, &remote.key)?,
            remote_key: remote.key.clone(),
            local,
        })
    }

    /// The peer re-offered (or answered a re-offer) with `remote`.
    ///
    /// - The peer's key and suite are unchanged: nothing changes, the streams go on (a hold, a
    ///   resume, a session refresh).
    /// - [`Remote::Offer`] with a new key (a re-key): a **fresh** key of our own, a fresh sender
    ///   and a fresh receiver. Every key that has been written into an SDP answer is used for one
    ///   stretch of audio only, so an answer that leaks later exposes nothing older.
    /// - [`Remote::Answer`] (the peer answers the offer we made in a 200): our key stays, as it
    ///   is what we offered; only the receiver follows the peer's new key. The suite must be the
    ///   one we offered.
    /// - A new suite **without** a new peer key is refused in both cases: the `_32` tag is the
    ///   first 4 bytes of the `_80` tag, so a receiver started over with the same key (an empty
    ///   replay window) would accept every earlier `_80` packet again, cut to a `_32` one.
    ///
    /// # Errors
    /// [`SrtpError::Key`] for a change that is refused, [`SrtpError::Random`] when a new key was
    /// needed and the generator failed. The session is left as it was on every error.
    pub(crate) fn renegotiate(
        &mut self,
        remote: &CryptoLine,
        from: Remote,
    ) -> Result<(), SrtpError> {
        let same_key = remote.key == self.remote_key;
        let same_suite = remote.suite == self.local.suite;
        if !same_suite && same_key {
            return Err(SrtpError::Key("a new suite needs a new key"));
        }
        if !same_suite && from == Remote::Answer {
            return Err(SrtpError::Key(
                "the answer changes the suite that was offered",
            ));
        }
        if same_suite && same_key {
            if from == Remote::Offer {
                self.local.tag = remote.tag;
            }
            return Ok(());
        }
        // Everything that can fail is built before anything is replaced.
        let rx = SrtpRx::new(remote.suite, &remote.key)?;
        let fresh = if from == Remote::Offer {
            let key = MasterKey::generate()?;
            let tx = SrtpTx::new(remote.suite, &key)?;
            Some((tx, key))
        } else {
            None
        };
        if let Some((tx, key)) = fresh {
            self.tx = tx;
            self.local = LocalCrypto {
                tag: remote.tag,
                suite: remote.suite,
                key,
            };
        }
        self.rx = rx;
        self.remote_key = remote.key.clone();
        Ok(())
    }
}

/// What the crypto line given to [`SrtpSession::renegotiate`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Remote {
    /// The peer's (re-)offer: we answer with our own line.
    Offer,
    /// The peer's answer to an offer we made.
    Answer,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let clean: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        (0..clean.len() / 2)
            .map(|i| u8::from_str_radix(&clean[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 3711 appendix B.3: the session keys derived from the master key and salt.
    #[test]
    fn key_derivation_matches_rfc_3711_appendix_b3() {
        let mut bytes = [0u8; 30];
        bytes[..16].copy_from_slice(&hex("E1F97A0D3E018BE0D64FA32C06DE4139"));
        bytes[16..].copy_from_slice(&hex("0EC675AD498AFEEBB6960B3AABE6"));
        let master = MasterKey::from_bytes(&bytes);
        let mut cipher_key = [0u8; 16];
        derive(&master, LABEL_CIPHER, &mut cipher_key).unwrap();
        assert_eq!(cipher_key.to_vec(), hex("C61E7A93744F39EE10734AFE3FF7A087"));
        let mut salt = [0u8; 14];
        derive(&master, LABEL_SALT, &mut salt).unwrap();
        assert_eq!(salt.to_vec(), hex("30CBBC08863D8C85D49DB34A9AE1"));
        let mut auth = [0u8; 20];
        derive(&master, LABEL_AUTH, &mut auth).unwrap();
        assert_eq!(
            auth.to_vec(),
            hex("CEBE321F6FF7716B6FD4AB49AF256A156D38BAA4")
        );
    }

    /// RFC 3711 appendix B.2: the AES-CM keystream for a known key and IV.
    #[test]
    fn aes_counter_mode_matches_rfc_3711_appendix_b2() {
        let key: [u8; 16] = hex("2B7E151628AED2A6ABF7158809CF4F3C").try_into().unwrap();
        let iv: [u8; 16] = hex("F0F1F2F3F4F5F6F7F8F9FAFBFCFD0000").try_into().unwrap();
        let mut stream = [0u8; 32];
        let mut cipher = Ctr128BE::<Aes128>::new(&key.into(), &iv.into());
        cipher.apply_keystream(&mut stream);
        assert_eq!(
            stream.to_vec(),
            hex("E03EAD0935C95E80E166B16DD92B4EB4 D23513162B02D0F72A43A2FE4A5F97AB")
        );
    }

    fn packet(seq: u16, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x80, 0x00];
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&u32::from(seq).wrapping_mul(160).to_be_bytes());
        out.extend_from_slice(&ssrc.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn pair(suite: SrtpSuite) -> (SrtpTx, SrtpRx) {
        let master = MasterKey::generate().unwrap();
        (
            SrtpTx::new(suite, &master).unwrap(),
            SrtpRx::new(suite, &master).unwrap(),
        )
    }

    #[test]
    fn a_packet_round_trips_and_the_wire_form_is_encrypted_and_tagged() {
        for suite in [
            SrtpSuite::AesCm128HmacSha1_80,
            SrtpSuite::AesCm128HmacSha1_32,
        ] {
            let (mut tx, mut rx) = pair(suite);
            let plain = packet(7, 0xdead_beef, &[0x55; 160]);
            let mut wire = plain.clone();
            tx.protect(&mut wire).unwrap();
            assert_eq!(wire.len(), plain.len() + suite.tag_len());
            assert_eq!(wire[..12], plain[..12], "the header stays in the clear");
            assert_ne!(wire[12..172], plain[12..172], "the payload is encrypted");
            let n = rx.unprotect(&mut wire).unwrap();
            assert_eq!(wire[..n], plain[..]);
        }
    }

    #[test]
    fn tampering_replay_and_a_wrong_key_are_refused_without_moving_the_state() {
        let (mut tx, mut rx) = pair(SrtpSuite::AesCm128HmacSha1_80);
        let mut first = packet(10, 1, &[1; 40]);
        tx.protect(&mut first).unwrap();
        // A flipped payload bit, header bit and tag bit.
        for at in [14usize, 5, first.len() - 1] {
            let mut bad = first.clone();
            bad[at] ^= 0x01;
            assert_eq!(rx.unprotect(&mut bad), Err(SrtpError::AuthFailed), "{at}");
        }
        // The forged copies changed nothing: the genuine packet still passes, once.
        let mut again = first.clone();
        assert!(rx.unprotect(&mut first).is_ok());
        assert_eq!(rx.unprotect(&mut again), Err(SrtpError::Replay));
        // Another key never authenticates.
        let (mut other_tx, _) = pair(SrtpSuite::AesCm128HmacSha1_80);
        let mut foreign = packet(11, 1, &[1; 40]);
        other_tx.protect(&mut foreign).unwrap();
        assert_eq!(rx.unprotect(&mut foreign), Err(SrtpError::AuthFailed));
        // Truncated packets.
        assert_eq!(rx.unprotect(&mut [0x80, 0, 0]), Err(SrtpError::Malformed));
        let mut short = vec![0x80; 14];
        assert_eq!(rx.unprotect(&mut short), Err(SrtpError::Malformed));
    }

    #[test]
    fn reordering_inside_the_window_is_accepted_and_older_packets_are_not() {
        let (mut tx, mut rx) = pair(SrtpSuite::AesCm128HmacSha1_80);
        let mut wire: Vec<Vec<u8>> = (0..200u16)
            .map(|seq| {
                let mut p = packet(seq, 9, &[seq as u8; 20]);
                tx.protect(&mut p).unwrap();
                p
            })
            .collect();
        // 150 first, then 149 (in the window), then 20 (150 - 20 > 128: too old).
        assert!(rx.unprotect(&mut wire[150]).is_ok());
        assert!(rx.unprotect(&mut wire[149]).is_ok());
        assert_eq!(rx.unprotect(&mut wire[20]), Err(SrtpError::Replay));
        assert!(rx.unprotect(&mut wire[30]).is_ok());
    }

    #[test]
    fn the_rollover_counter_follows_the_sequence_wrap() {
        let (mut tx, mut rx) = pair(SrtpSuite::AesCm128HmacSha1_80);
        let mut seen = 0usize;
        for seq in (65_500u32..65_536).chain(0..100) {
            let mut p = packet(seq as u16, 3, &[seq as u8; 30]);
            let plain = p.clone();
            tx.protect(&mut p).unwrap();
            let n = rx.unprotect(&mut p).unwrap();
            assert_eq!(p[..n], plain[..]);
            seen += 1;
        }
        assert_eq!(seen, 136);
        assert_eq!(tx.roc, 1);
    }

    #[test]
    fn a_second_ssrc_gets_its_own_state() {
        let (mut tx, mut rx) = pair(SrtpSuite::AesCm128HmacSha1_32);
        for (seq, ssrc) in [(1u16, 1u32), (1, 2), (2, 1), (2, 2)] {
            let mut p = packet(seq, ssrc, &[9; 10]);
            tx.protect(&mut p).unwrap();
            assert!(rx.unprotect(&mut p).is_ok(), "{ssrc}/{seq}");
        }
    }

    #[test]
    fn header_extensions_and_csrc_stay_in_the_clear() {
        let (mut tx, mut rx) = pair(SrtpSuite::AesCm128HmacSha1_80);
        // CC = 1, X = 1: one CSRC, a one-word extension.
        let mut plain = vec![0x91, 0x00, 0, 5, 0, 0, 0, 1, 0, 0, 0, 2];
        plain.extend_from_slice(&[0, 0, 0, 7]); // CSRC
        plain.extend_from_slice(&[0xbe, 0xde, 0, 1, 1, 2, 3, 4]); // extension
        plain.extend_from_slice(&[0xaa; 24]);
        let mut wire = plain.clone();
        tx.protect(&mut wire).unwrap();
        assert_eq!(wire[..24], plain[..24]);
        assert_ne!(wire[24..48], plain[24..48]);
        let n = rx.unprotect(&mut wire).unwrap();
        assert_eq!(wire[..n], plain[..]);
    }

    #[test]
    fn key_text_round_trips_and_is_checked_and_never_printed() {
        let key = MasterKey::generate().unwrap();
        let text = key.to_base64();
        assert_eq!(text.len(), 40);
        assert_eq!(MasterKey::from_base64(&text).unwrap(), key);
        assert_eq!(
            MasterKey::from_base64(text.trim_end_matches('=')).unwrap(),
            key
        );
        assert!(MasterKey::from_base64("AAAA").is_err());
        assert!(MasterKey::from_base64("!!!!").is_err());
        assert!(!format!("{key:?}").contains(&text));
        assert!(format!("{key:?}").contains("REDACTED"));
    }

    /// Our packets are read by an independent implementation (webrtc-rs `webrtc-srtp`) and
    /// theirs by ours, for both suites and across the sequence wrap.
    #[test]
    fn packets_interoperate_with_the_webrtc_rs_implementation() {
        use webrtc_srtp::context::Context;
        use webrtc_srtp::protection_profile::ProtectionProfile;
        for (suite, profile) in [
            (
                SrtpSuite::AesCm128HmacSha1_80,
                ProtectionProfile::Aes128CmHmacSha1_80,
            ),
            (
                SrtpSuite::AesCm128HmacSha1_32,
                ProtectionProfile::Aes128CmHmacSha1_32,
            ),
        ] {
            let master = MasterKey::generate().unwrap();
            let mut raw = [0u8; 30];
            master.write_into(&mut raw);
            let mut theirs_tx = Context::new(&raw[..16], &raw[16..], profile, None, None).unwrap();
            let mut theirs_rx = Context::new(&raw[..16], &raw[16..], profile, None, None).unwrap();
            let mut tx = SrtpTx::new(suite, &master).unwrap();
            let mut rx = SrtpRx::new(suite, &master).unwrap();
            for seq in (65_520u32..65_536).chain(0..40) {
                let plain = packet(seq as u16, 0x0102_0304, &[seq as u8; 160]);
                // Ours to theirs.
                let mut wire = plain.clone();
                tx.protect(&mut wire).unwrap();
                let decrypted = theirs_rx.decrypt_rtp(&wire).unwrap();
                assert_eq!(decrypted.as_ref(), plain.as_slice(), "ours->theirs {seq}");
                // Theirs to ours.
                let mut wire = theirs_tx.encrypt_rtp(&plain).unwrap().to_vec();
                let n = rx.unprotect(&mut wire).unwrap();
                assert_eq!(wire[..n], plain[..], "theirs->ours {seq}");
            }
        }
    }

    #[test]
    fn index_estimation_follows_rfc_3711_appendix_a() {
        // In the first half of the sequence space.
        assert_eq!(estimate_index(100, 90), Some(90));
        assert_eq!(estimate_index(100, 105), Some(105));
        // A packet just before the start of the stream.
        assert_eq!(estimate_index(100, 65_530), None);
        // In the second half: a small sequence number means the counter rolled.
        assert_eq!(estimate_index(65_530, 5), Some(65_536 + 5));
        // And a late packet from before the rollover.
        assert_eq!(estimate_index(65_536 + 3, 65_530), Some(65_530));
    }

    // -- adversarial-review regressions ---------------------------------------

    fn session(suite: SrtpSuite) -> (SrtpSession, MasterKey) {
        let remote_key = MasterKey::generate().unwrap();
        let local = LocalCrypto {
            tag: 1,
            suite,
            key: MasterKey::generate().unwrap(),
        };
        let remote = CryptoLine {
            tag: 1,
            suite,
            key: remote_key.clone(),
        };
        (SrtpSession::new(local, &remote).unwrap(), remote_key)
    }

    /// Proof 1 of the review: with a fresh receiver on the same key (what the old suite switch
    /// built), a `_80` packet cut to a `_32` packet is accepted again. This is the primitive's
    /// property; the session must never build such a receiver (next test).
    #[test]
    fn a_fresh_receiver_on_an_old_key_would_accept_a_truncated_replay() {
        let master = MasterKey::generate().unwrap();
        let mut tx80 = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &master).unwrap();
        let plain = packet(100, 0x1234, &[0x42; 160]);
        let mut wire = plain.clone();
        tx80.protect(&mut wire).unwrap();
        let mut rx32 = SrtpRx::new(SrtpSuite::AesCm128HmacSha1_32, &master).unwrap();
        let mut replay = wire.clone();
        replay.truncate(wire.len() - 10 + 4);
        let n = rx32.unprotect(&mut replay).unwrap();
        assert_eq!(&replay[..n], &plain[..]);
    }

    #[test]
    fn a_suite_switch_with_an_unchanged_peer_key_is_refused_and_replays_stay_refused() {
        let (mut session, remote_key) = session(SrtpSuite::AesCm128HmacSha1_80);
        // The peer's genuine packet, as the peer's sender makes it.
        let mut peer_tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &remote_key).unwrap();
        let plain = packet(100, 0x1234, &[0x42; 160]);
        let mut wire = plain.clone();
        peer_tx.protect(&mut wire).unwrap();
        assert_eq!(session.rx.unprotect(&mut wire.clone()), Ok(plain.len()));
        let local_before = session.local.key.to_base64();

        for from in [Remote::Offer, Remote::Answer] {
            let switch = CryptoLine {
                tag: 9,
                suite: SrtpSuite::AesCm128HmacSha1_32,
                key: remote_key.clone(),
            };
            assert!(matches!(
                session.renegotiate(&switch, from),
                Err(SrtpError::Key(_))
            ));
        }
        // Nothing changed: the suite, our key, and the receiver's replay window.
        assert_eq!(session.local.suite, SrtpSuite::AesCm128HmacSha1_80);
        assert_eq!(session.local.key.to_base64(), local_before);
        assert_eq!(
            session.rx.unprotect(&mut wire.clone()),
            Err(SrtpError::Replay)
        );
        let mut cut = wire.clone();
        cut.truncate(wire.len() - 10 + 4);
        assert!(
            session.rx.unprotect(&mut cut).is_err(),
            "the cut copy must not pass"
        );
    }

    #[test]
    fn a_rekey_by_the_peer_gets_a_fresh_local_key_and_fresh_state_on_both_halves() {
        let (mut session, remote_key) = session(SrtpSuite::AesCm128HmacSha1_80);
        let first_local = session.local.key.to_base64();
        // The same offer again (a hold, a refresh): nothing changes, the tag follows the offer.
        let same = CryptoLine {
            tag: 4,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key: remote_key,
        };
        session.renegotiate(&same, Remote::Offer).unwrap();
        assert_eq!(session.local.key.to_base64(), first_local);
        assert_eq!(session.local.tag, 4);

        // A new peer key: a new key of ours, a restarted sender.
        let new_key = MasterKey::generate().unwrap();
        let rekey = CryptoLine {
            tag: 7,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key: new_key.clone(),
        };
        session.renegotiate(&rekey, Remote::Offer).unwrap();
        let second_local = session.local.key.to_base64();
        assert_ne!(second_local, first_local);
        assert_eq!(session.local.tag, 7);
        // Each further re-key brings yet another one.
        let newer = CryptoLine {
            tag: 7,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key: MasterKey::generate().unwrap(),
        };
        session.renegotiate(&newer, Remote::Offer).unwrap();
        assert_ne!(session.local.key.to_base64(), second_local);
        // The sender uses the new key (the peer, holding it, decrypts), the receiver the peer's.
        let mut packet_out = packet(1, 5, &[3; 20]);
        session.tx.protect(&mut packet_out).unwrap();
        let mut theirs = SrtpRx::new(SrtpSuite::AesCm128HmacSha1_80, &session.local.key).unwrap();
        assert!(theirs.unprotect(&mut packet_out).is_ok());
        let mut peer_tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &newer.key).unwrap();
        let mut packet_in = packet(1, 6, &[4; 20]);
        peer_tx.protect(&mut packet_in).unwrap();
        assert!(session.rx.unprotect(&mut packet_in).is_ok());
    }

    #[test]
    fn an_answer_keeps_our_offered_key_and_cannot_change_the_suite() {
        let (mut session, _) = session(SrtpSuite::AesCm128HmacSha1_80);
        let ours = session.local.key.to_base64();
        let answer = CryptoLine {
            tag: 1,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key: MasterKey::generate().unwrap(),
        };
        session.renegotiate(&answer, Remote::Answer).unwrap();
        assert_eq!(session.local.key.to_base64(), ours, "our offered key stays");
        let mut peer_tx = SrtpTx::new(SrtpSuite::AesCm128HmacSha1_80, &answer.key).unwrap();
        let mut wire = packet(1, 6, &[4; 20]);
        peer_tx.protect(&mut wire).unwrap();
        assert!(
            session.rx.unprotect(&mut wire).is_ok(),
            "the receiver follows the answer"
        );

        let other_suite = CryptoLine {
            tag: 1,
            suite: SrtpSuite::AesCm128HmacSha1_32,
            key: MasterKey::generate().unwrap(),
        };
        assert!(session.renegotiate(&other_suite, Remote::Answer).is_err());
        // A re-offer may change suite together with the key (a new key, a new everything).
        session.renegotiate(&other_suite, Remote::Offer).unwrap();
        assert_eq!(session.local.suite, SrtpSuite::AesCm128HmacSha1_32);
        assert_ne!(session.local.key.to_base64(), ours);
    }

    /// Proofs 2 and 3 of the review: forged jumps and bent sequence numbers never move the
    /// rollover counter, and a pre-wrap packet replayed after the wrap is refused.
    #[test]
    fn forged_jumps_and_replays_across_the_wrap_never_desynchronise_the_receiver() {
        let (mut tx, mut rx) = pair(SrtpSuite::AesCm128HmacSha1_80);
        for seq in 65_000u16..65_010 {
            let mut p = packet(seq, 7, &[1; 20]);
            tx.protect(&mut p).unwrap();
            rx.unprotect(&mut p).unwrap();
        }
        let mut forged = packet(39_474, 7, &[9; 20]);
        forged.extend_from_slice(&[0xAA; 10]);
        assert!(matches!(
            rx.unprotect(&mut forged),
            Err(SrtpError::AuthFailed | SrtpError::Replay)
        ));
        for seq in (65_010u16..=65_535).chain(0..50) {
            let mut p = packet(seq, 7, &[2; 20]);
            tx.protect(&mut p).unwrap();
            let mut bent = p.clone();
            bent[2] ^= 0x80;
            assert!(matches!(
                rx.unprotect(&mut bent),
                Err(SrtpError::AuthFailed | SrtpError::Replay)
            ));
            rx.unprotect(&mut p).unwrap();
        }

        let (mut tx, mut rx) = pair(SrtpSuite::AesCm128HmacSha1_32);
        let mut early = packet(5, 3, &[1; 20]);
        tx.protect(&mut early).unwrap();
        rx.unprotect(&mut early.clone()).unwrap();
        for seq in (6u16..=65_535).step_by(97).chain(0..4) {
            let mut p = packet(seq, 3, &[1; 20]);
            tx.protect(&mut p).unwrap();
            rx.unprotect(&mut p).unwrap();
        }
        assert_eq!(rx.unprotect(&mut early), Err(SrtpError::AuthFailed));
    }
}
