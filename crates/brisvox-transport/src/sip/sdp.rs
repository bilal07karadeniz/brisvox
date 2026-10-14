//! A minimal SDP (RFC 4566) offer/answer (RFC 3264) for one G.711 audio stream.
//!
//! Only what a voice-agent endpoint needs: one `m=audio` section over `RTP/AVP` with PCMU
//! (payload type 0) and/or PCMA (8) and, optionally, RFC 4733 `telephone-event` at the payload
//! type the offerer chose. Everything else in an offer is ignored, and every other `m=` section is
//! declined (port 0) in the answer, as RFC 3264 section 6 requires.
//!
//! # SRTP (SDES, RFC 4568)
//! An `RTP/SAVP` audio section with `a=crypto` lines is taken when the [`Security`] allows it:
//! the policy is not `off` **and** the signaling that carried the SDP is TLS (the keys are in the
//! text). The first crypto line with a supported suite wins; the answer repeats its tag and
//! suite with our own key. With both a secure and a plain section in the offer, the secure one
//! is preferred. Lines this module cannot honour are ignored, never half-read: a suite other
//! than `AES_CM_128_HMAC_SHA1_80` / `_32`, an MKI, session parameters (`KDR`, `WSH`,
//! `UNENCRYPTED_SRTP`, ...) or more than one `inline` key. `RTP/AVP` with `a=crypto` (optimistic
//! SRTP) is read as plain RTP.
//!
//! Parsing is strict about structure (bounded size, bounded number of sections, valid ports and
//! payload types) and forgiving about content it does not use.

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};

use brisvox_audio::Codec;

use super::srtp::{MasterKey, SrtpPolicy, SrtpSuite};

/// Largest SDP body accepted, in bytes. Larger bodies are rejected before parsing.
pub const MAX_SDP_BYTES: usize = 16 * 1024;

/// Most `m=` sections read from one SDP; more are rejected.
const MAX_MEDIA_SECTIONS: usize = 16;

/// Most formats read from one `m=` line; more are rejected.
const MAX_FORMATS: usize = 64;

/// Most `a=crypto` lines kept per media section.
const MAX_CRYPTO_LINES: usize = 8;

/// The telephone-event payload type offered by [`build_offer`] (the conventional 101).
pub const DEFAULT_DTMF_PAYLOAD_TYPE: u8 = 101;

/// A G.711 variant (RFC 3551: static payload types 0 and 8, 8000 Hz, mono).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum G711 {
    /// mu-law, static payload type 0.
    Pcmu,
    /// A-law, static payload type 8.
    Pcma,
}

impl G711 {
    /// The static RTP payload type (RFC 3551 table 4).
    pub const fn payload_type(self) -> u8 {
        match self {
            Self::Pcmu => 0,
            Self::Pcma => 8,
        }
    }

    /// The encoding name used in `a=rtpmap`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pcmu => "PCMU",
            Self::Pcma => "PCMA",
        }
    }

    /// The matching [`Codec`] of the audio layer.
    pub const fn codec(self) -> Codec {
        match self {
            Self::Pcmu => Codec::Ulaw,
            Self::Pcma => Codec::Alaw,
        }
    }

    const fn from_static(payload_type: u8) -> Option<Self> {
        match payload_type {
            0 => Some(Self::Pcmu),
            8 => Some(Self::Pcma),
            _ => None,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("PCMU") {
            Some(Self::Pcmu)
        } else if name.eq_ignore_ascii_case("PCMA") {
            Some(Self::Pcma)
        } else {
            None
        }
    }
}

/// The media direction attribute (RFC 4566 section 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Direction {
    /// Both directions (the default when no attribute is present).
    #[default]
    SendRecv,
    /// The sender of the SDP only sends.
    SendOnly,
    /// The sender of the SDP only receives.
    RecvOnly,
    /// No media.
    Inactive,
}

impl Direction {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "sendrecv" => Some(Self::SendRecv),
            "sendonly" => Some(Self::SendOnly),
            "recvonly" => Some(Self::RecvOnly),
            "inactive" => Some(Self::Inactive),
            _ => None,
        }
    }

    /// The attribute text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SendRecv => "sendrecv",
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::Inactive => "inactive",
        }
    }

    /// The direction the answerer states for an offer with this direction (RFC 3264 section 6.1).
    pub const fn answer(self) -> Self {
        match self {
            Self::SendRecv => Self::SendRecv,
            Self::SendOnly => Self::RecvOnly,
            Self::RecvOnly => Self::SendOnly,
            Self::Inactive => Self::Inactive,
        }
    }

    /// Whether the SDP's author wants to send media.
    pub const fn author_sends(self) -> bool {
        matches!(self, Self::SendRecv | Self::SendOnly)
    }

    /// Whether the SDP's author wants to receive media.
    pub const fn author_receives(self) -> bool {
        matches!(self, Self::SendRecv | Self::RecvOnly)
    }
}

/// Why an SDP could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdpError {
    /// The text is not a well-formed SDP (SIP 400 for an offer).
    #[error("malformed SDP: {0}")]
    Malformed(&'static str),
    /// There is no usable `RTP/AVP` audio section, for example only SRTP (SIP 488).
    #[error("no usable RTP/AVP audio stream")]
    NoAudio,
    /// The audio section offers none of the codecs this endpoint supports (SIP 488).
    #[error("no common audio codec")]
    NoCommonCodec,
    /// The SRTP policy refuses the SDP: SRTP is required and not offered, or SDES keys arrived
    /// over signaling that is not TLS, or no crypto line is acceptable (SIP 488).
    #[error("no media security this endpoint accepts")]
    SrtpRefused,
}

/// What an SDP may use for media security: the policy and whether the signaling that carried it
/// is TLS (SDES keys in cleartext SIP are worthless).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Security {
    /// The configured policy.
    pub policy: SrtpPolicy,
    /// Whether the SDP arrived (or goes out) on a TLS connection.
    pub secure_signaling: bool,
}

impl Security {
    /// Plain RTP only (SRTP policy off).
    pub const PLAIN: Self = Self {
        policy: SrtpPolicy::Off,
        secure_signaling: false,
    };
}

/// An accepted `a=crypto` line: the tag, the suite and the sender's master key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoLine {
    /// The tag that pairs the answer with the offer's line.
    pub tag: u32,
    /// The crypto suite.
    pub suite: SrtpSuite,
    /// The key the author of this SDP protects its own outgoing media with.
    pub key: MasterKey,
}

/// Our side of the crypto negotiation: the line we write and the key we send with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCrypto {
    /// The tag (echoed from the offer when answering).
    pub tag: u32,
    /// The suite.
    pub suite: SrtpSuite,
    /// Our master key.
    pub key: MasterKey,
}

impl LocalCrypto {
    fn write(&self, out: &mut String) {
        let _ = write!(
            out,
            "a=crypto:{} {} inline:{}\r\n",
            self.tag,
            self.suite.name(),
            self.key.to_base64()
        );
    }
}

/// Parses the value of an `a=crypto` attribute (RFC 4568 section 9.1). `None` for a line this
/// module does not honour (see the module docs).
fn parse_crypto(value: &str) -> Option<CryptoLine> {
    let mut parts = value.split_ascii_whitespace();
    let tag = parts.next()?.parse::<u32>().ok()?;
    let suite = SrtpSuite::from_name(parts.next()?)?;
    let key_params = parts.next()?;
    if parts.next().is_some() || key_params.contains(';') {
        return None; // session parameters, or several keys
    }
    let mut fields = key_params.strip_prefix("inline:")?.split('|');
    let key = MasterKey::from_base64(fields.next()?).ok()?;
    for field in fields {
        // A lifetime (`2^31` or a number) is accepted only when it is long enough that it never
        // runs out here; an MKI (`1:4`) is not supported.
        if !lifetime_is_long_enough(field) {
            return None;
        }
    }
    Some(CryptoLine { tag, suite, key })
}

/// The shortest key lifetime in packets that is accepted: 2^31 (RFC 4568 section 6.1 writes
/// `2^<n>` or a decimal number; the default for SRTP is 2^48). This endpoint neither counts the
/// packets of a key nor re-keys by itself, so a peer that asks for a shorter life (and expects
/// the key to be replaced before it ends) is refused instead of being served on a key past its
/// limit. At 50 packets/s 2^31 packets are more than a year, far beyond any call.
const MIN_KEY_LIFETIME_LOG2: u32 = 31;

fn lifetime_is_long_enough(field: &str) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if let Some(exponent) = field.strip_prefix("2^") {
        // A huge exponent is long, not an error; only a non-number is rejected.
        return digits(exponent)
            && exponent
                .parse::<u32>()
                .map_or(true, |n| n >= MIN_KEY_LIFETIME_LOG2);
    }
    // A number of packets: all digits; one that does not fit a u64 is far above 2^31.
    digits(field)
        && field
            .parse::<u64>()
            .map_or(true, |n| n >= 1u64 << MIN_KEY_LIFETIME_LOG2)
}

/// An `a=rtpmap` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpMap {
    /// The payload type.
    pub payload_type: u8,
    /// The encoding name, as written.
    pub name: String,
    /// The clock rate in Hz.
    pub clock_rate: u32,
}

/// One `m=` section with the attributes this module reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSection {
    /// Media type (`audio`, `video`, ...).
    pub kind: String,
    /// Transport port; 0 means the stream is declined or disabled.
    pub port: u16,
    /// Transport protocol (`RTP/AVP`, `RTP/SAVP`, ...).
    pub proto: String,
    /// The format tokens of the `m=` line, in the author's order of preference.
    pub formats: Vec<String>,
    /// The section's own `c=` address, if any.
    pub connection: Option<IpAddr>,
    /// The section's `a=rtpmap` attributes.
    pub rtpmap: Vec<RtpMap>,
    /// The section's direction attribute, if any.
    pub direction: Option<Direction>,
    /// The section's usable `a=crypto` lines, in the author's order of preference.
    pub crypto: Vec<CryptoLine>,
}

impl MediaSection {
    fn payload_types(&self) -> impl Iterator<Item = u8> + '_ {
        self.formats.iter().filter_map(|f| f.parse::<u8>().ok())
    }

    fn rtpmap_of(&self, payload_type: u8) -> Option<&RtpMap> {
        self.rtpmap.iter().find(|m| m.payload_type == payload_type)
    }

    /// The G.711 variant carried by `payload_type` in this section: by the static assignment, or
    /// by a dynamic `rtpmap` naming PCMU/PCMA at 8000 Hz.
    fn g711_of(&self, payload_type: u8) -> Option<G711> {
        match self.rtpmap_of(payload_type) {
            Some(map) => (map.clock_rate == 8000).then(|| G711::from_name(&map.name))?,
            None => G711::from_static(payload_type),
        }
    }

    /// The first payload type of this section that is `telephone-event/8000`.
    fn dtmf_payload_type(&self) -> Option<u8> {
        self.payload_types().find(|pt| {
            self.rtpmap_of(*pt).is_some_and(|m| {
                m.clock_rate == 8000 && m.name.eq_ignore_ascii_case("telephone-event")
            })
        })
    }
}

/// A parsed session description (the parts this module uses).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionDescription {
    /// The session-level `c=` address.
    pub connection: Option<IpAddr>,
    /// The session-level direction attribute.
    pub direction: Option<Direction>,
    /// The `m=` sections, in order.
    pub media: Vec<MediaSection>,
}

/// Parses an address from a `c=` line value (`IN IP4 192.0.2.1`, optionally with `/ttl`).
fn parse_connection(value: &str) -> Result<Option<IpAddr>, SdpError> {
    let mut parts = value.split_ascii_whitespace();
    let (Some("IN"), Some(kind), Some(addr)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(SdpError::Malformed("bad c= line"));
    };
    if kind != "IP4" && kind != "IP6" {
        return Err(SdpError::Malformed("unsupported address type in c="));
    }
    let addr = addr.split('/').next().unwrap_or(addr);
    // A host name cannot be resolved here: treat it as unusable rather than failing the parse,
    // so an offer with a name only in an unused section still works.
    Ok(addr.parse::<IpAddr>().ok())
}

fn parse_rtpmap(value: &str) -> Option<RtpMap> {
    let (pt, encoding) = value.split_once(' ')?;
    let payload_type = pt.trim().parse::<u8>().ok()?;
    let mut parts = encoding.trim().split('/');
    let name = parts.next()?.to_owned();
    let clock_rate = parts.next()?.trim().parse::<u32>().ok()?;
    Some(RtpMap {
        payload_type,
        name,
        clock_rate,
    })
}

fn parse_media_line(value: &str) -> Result<MediaSection, SdpError> {
    let mut parts = value.split_ascii_whitespace();
    let (Some(kind), Some(port), Some(proto)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(SdpError::Malformed("bad m= line"));
    };
    // `port/count` is legal; the count is ignored.
    let port = port.split('/').next().unwrap_or(port);
    let port = port
        .parse::<u16>()
        .map_err(|_| SdpError::Malformed("bad port in m="))?;
    let formats: Vec<String> = parts.map(str::to_owned).collect();
    if formats.len() > MAX_FORMATS {
        return Err(SdpError::Malformed("too many formats in m="));
    }
    Ok(MediaSection {
        kind: kind.to_ascii_lowercase(),
        port,
        proto: proto.to_ascii_uppercase(),
        formats,
        connection: None,
        rtpmap: Vec::new(),
        direction: None,
        crypto: Vec::new(),
    })
}

impl SessionDescription {
    /// Parses `body`.
    ///
    /// # Errors
    /// [`SdpError::Malformed`] when the body is too large, is not UTF-8, lacks `v=0`, or has a
    /// broken `m=`/`c=` line, or too many sections.
    pub fn parse(body: &[u8]) -> Result<Self, SdpError> {
        if body.len() > MAX_SDP_BYTES {
            return Err(SdpError::Malformed("SDP too large"));
        }
        let text =
            std::str::from_utf8(body).map_err(|_| SdpError::Malformed("SDP is not UTF-8"))?;
        let mut session = Self::default();
        let mut saw_version = false;
        for raw in text.split('\n') {
            let line = raw.trim_end_matches('\r').trim();
            if line.is_empty() {
                continue;
            }
            let Some((kind, value)) = line.split_once('=') else {
                return Err(SdpError::Malformed("line without ="));
            };
            let value = value.trim();
            match kind {
                "v" => {
                    if value != "0" {
                        return Err(SdpError::Malformed("unsupported SDP version"));
                    }
                    saw_version = true;
                }
                "c" => {
                    let address = parse_connection(value)?;
                    match session.media.last_mut() {
                        Some(media) => media.connection = address,
                        None => session.connection = address,
                    }
                }
                "m" => {
                    if session.media.len() >= MAX_MEDIA_SECTIONS {
                        return Err(SdpError::Malformed("too many media sections"));
                    }
                    session.media.push(parse_media_line(value)?);
                }
                "a" => {
                    let (name, attr) = value.split_once(':').unwrap_or((value, ""));
                    let direction = Direction::parse(name);
                    match (name, session.media.last_mut()) {
                        ("rtpmap", Some(media)) => {
                            if let Some(map) = parse_rtpmap(attr) {
                                media.rtpmap.push(map);
                            }
                        }
                        ("crypto", Some(media)) => {
                            if media.crypto.len() < MAX_CRYPTO_LINES
                                && let Some(line) = parse_crypto(attr)
                            {
                                media.crypto.push(line);
                            }
                        }
                        (_, Some(media)) => {
                            if direction.is_some() {
                                media.direction = direction;
                            }
                        }
                        (_, None) => {
                            if direction.is_some() {
                                session.direction = direction;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if !saw_version {
            return Err(SdpError::Malformed("missing v=0"));
        }
        Ok(session)
    }

    /// The effective address of `media`: its own `c=` or the session's.
    fn address_of(&self, media: &MediaSection) -> Option<IpAddr> {
        media.connection.or(self.connection)
    }

    fn direction_of(&self, media: &MediaSection) -> Direction {
        media.direction.or(self.direction).unwrap_or_default()
    }
}

/// The negotiated audio stream of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    /// The G.711 variant in use.
    pub codec: G711,
    /// The payload type of audio packets (both directions).
    pub payload_type: u8,
    /// The payload type of RFC 4733 telephone-event packets, if the peer supports them.
    pub dtmf_payload_type: Option<u8>,
    /// Where to send RTP; `None` when the peer's address is unusable or is the "on hold"
    /// address 0.0.0.0 (RFC 3264 section 8.4 says port 0 is declined; 0.0.0.0 is the legacy hold).
    pub remote: Option<SocketAddr>,
    /// Whether this endpoint sends audio to the peer.
    pub send: bool,
    /// Whether this endpoint takes audio from the peer.
    pub receive: bool,
    /// Further G.711 payload types the peer may send, besides `payload_type` (RFC 3264 section 7:
    /// an answerer may send any format its answer lists). Filled by [`read_answer`] only; an
    /// answer we write names one codec, so there it is empty. We still send `payload_type`.
    pub alt: [Option<(u8, G711)>; MAX_ALT_FORMATS],
}

/// Most alternative receive formats kept in [`AudioParams::alt`].
pub const MAX_ALT_FORMATS: usize = 4;

impl AudioParams {
    /// The G.711 variant carried by RTP payload type `payload_type` in the stream we receive, or
    /// `None` when that payload type is not an audio format of this call.
    pub fn receive_codec(&self, payload_type: u8) -> Option<G711> {
        if payload_type == self.payload_type {
            return Some(self.codec);
        }
        self.alt
            .iter()
            .flatten()
            .find(|(pt, _)| *pt == payload_type)
            .map(|(_, codec)| *codec)
    }
}

fn remote_address(ip: Option<IpAddr>, port: u16) -> Option<SocketAddr> {
    let ip = ip?;
    (!ip.is_unspecified() && port != 0).then(|| SocketAddr::new(ip, port))
}

fn ip_kind(ip: IpAddr) -> &'static str {
    if ip.is_ipv4() { "IP4" } else { "IP6" }
}

/// The stream picked from an SDP.
struct Picked {
    index: usize,
    params: AudioParams,
    /// The crypto line the author protects its media with, for a secure section.
    crypto: Option<CryptoLine>,
}

/// Picks the audio stream from `description` for an answerer: the first audio section with a port
/// that offers a codec from `supported`, in the offerer's order of preference. A secure
/// (`RTP/SAVP`) section with an acceptable crypto line comes first when `security` allows SRTP;
/// a plain `RTP/AVP` section is the fallback unless SRTP is required.
fn pick_audio(
    description: &SessionDescription,
    supported: &[G711],
    security: Security,
) -> Result<Picked, SdpError> {
    let mut saw_audio = false;
    let mut saw_codec_mismatch = false;
    let mut refused_for_security = false;
    for secure_pass in [true, false] {
        for (index, media) in description.media.iter().enumerate() {
            if media.kind != "audio" || media.port == 0 {
                continue;
            }
            let wanted_proto = if secure_pass { "RTP/SAVP" } else { "RTP/AVP" };
            if media.proto != wanted_proto {
                continue;
            }
            let crypto = if secure_pass {
                if security.policy == SrtpPolicy::Off {
                    continue;
                }
                // The keys are in this text: never from a connection that is not TLS.
                let line = security
                    .secure_signaling
                    .then(|| media.crypto.first().cloned())
                    .flatten();
                let Some(line) = line else {
                    refused_for_security = true;
                    continue;
                };
                Some(line)
            } else {
                if security.policy == SrtpPolicy::Required {
                    refused_for_security = true;
                    continue;
                }
                None
            };
            saw_audio = true;
            let chosen = media.payload_types().find_map(|pt| {
                let codec = media.g711_of(pt)?;
                supported.contains(&codec).then_some((pt, codec))
            });
            let Some((payload_type, codec)) = chosen else {
                saw_codec_mismatch = true;
                continue;
            };
            let direction = description.direction_of(media);
            let params = AudioParams {
                codec,
                payload_type,
                dtmf_payload_type: media.dtmf_payload_type(),
                remote: remote_address(description.address_of(media), media.port),
                send: direction.author_receives(),
                receive: direction.author_sends(),
                alt: [None; MAX_ALT_FORMATS],
            };
            return Ok(Picked {
                index,
                params,
                crypto,
            });
        }
    }
    if saw_audio && saw_codec_mismatch {
        Err(SdpError::NoCommonCodec)
    } else if refused_for_security {
        Err(SdpError::SrtpRefused)
    } else {
        Err(SdpError::NoAudio)
    }
}

/// A parsed offer with the stream this endpoint picked from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    description: SessionDescription,
    chosen: usize,
    params: AudioParams,
    crypto: Option<CryptoLine>,
}

impl Offer {
    /// Parses `body` and picks the audio stream (RFC 3264 section 6): the first `RTP/AVP` audio
    /// section offering a codec from `supported`, in the offerer's order of preference.
    ///
    /// # Errors
    /// [`SdpError`] when the offer cannot be parsed or has no audio stream this endpoint can
    /// serve.
    pub fn parse(body: &[u8], supported: &[G711]) -> Result<Self, SdpError> {
        Self::parse_secure(body, supported, Security::PLAIN)
    }

    /// [`parse`](Self::parse) with media security: `security` says whether SRTP may be taken.
    ///
    /// # Errors
    /// [`SdpError`] as for [`parse`](Self::parse), and [`SdpError::SrtpRefused`] when the policy
    /// leaves nothing acceptable in the offer.
    pub fn parse_secure(
        body: &[u8],
        supported: &[G711],
        security: Security,
    ) -> Result<Self, SdpError> {
        let description = SessionDescription::parse(body)?;
        let Picked {
            index,
            params,
            crypto,
        } = pick_audio(&description, supported, security)?;
        Ok(Self {
            description,
            chosen: index,
            params,
            crypto,
        })
    }

    /// The negotiated stream.
    pub const fn params(&self) -> AudioParams {
        self.params
    }

    /// The peer's crypto line when the picked stream is SRTP: the answer must echo its tag and
    /// suite with a key of ours ([`answer_with`](Self::answer_with)).
    pub const fn srtp(&self) -> Option<&CryptoLine> {
        self.crypto.as_ref()
    }

    /// The answer text: one G.711 stream at `local` (address and RTP port to advertise), every
    /// other section declined in place. `session_id` and `version` go into the `o=` line (the
    /// version must grow when the answer to a re-offer changes).
    pub fn answer(&self, local: SocketAddr, session_id: u64, version: u64) -> String {
        self.answer_with(local, session_id, version, None)
    }

    /// [`answer`](Self::answer) for an SRTP stream: `crypto` is our key and the echoed tag and
    /// suite of [`srtp`](Self::srtp). Without it the answer is plain `RTP/AVP`.
    pub fn answer_with(
        &self,
        local: SocketAddr,
        session_id: u64,
        version: u64,
        crypto: Option<&LocalCrypto>,
    ) -> String {
        let direction = self
            .description
            .media
            .get(self.chosen)
            .map_or(Direction::SendRecv, |m| self.description.direction_of(m))
            .answer();
        let mut out = String::with_capacity(512);
        write_session_header(&mut out, local.ip(), session_id, version);
        for (index, media) in self.description.media.iter().enumerate() {
            if index != self.chosen {
                // Decline: same type, protocol and formats, port 0 (RFC 3264 section 6).
                let formats = if media.formats.is_empty() {
                    "0".to_owned()
                } else {
                    media.formats.join(" ")
                };
                let _ = write!(
                    out,
                    "m={} 0 {} {}\r\n",
                    media.kind,
                    media.proto.to_ascii_uppercase(),
                    formats
                );
                continue;
            }
            write_audio_section(&mut out, local.port(), &self.params, direction, crypto);
        }
        out
    }
}

fn write_session_header(out: &mut String, ip: IpAddr, session_id: u64, version: u64) {
    let _ = write!(
        out,
        "v=0\r\no=brisvox {session_id} {version} IN {kind} {ip}\r\ns=brisvox\r\nc=IN {kind} {ip}\r\nt=0 0\r\n",
        kind = ip_kind(ip),
    );
}

fn write_audio_section(
    out: &mut String,
    port: u16,
    params: &AudioParams,
    direction: Direction,
    crypto: Option<&LocalCrypto>,
) {
    let proto = if crypto.is_some() {
        "RTP/SAVP"
    } else {
        "RTP/AVP"
    };
    let _ = write!(out, "m=audio {port} {proto} {}", params.payload_type);
    if let Some(dtmf) = params.dtmf_payload_type {
        let _ = write!(out, " {dtmf}");
    }
    out.push_str("\r\n");
    let _ = write!(
        out,
        "a=rtpmap:{} {}/8000\r\n",
        params.payload_type,
        params.codec.name()
    );
    if let Some(dtmf) = params.dtmf_payload_type {
        let _ = write!(
            out,
            "a=rtpmap:{dtmf} telephone-event/8000\r\na=fmtp:{dtmf} 0-16\r\n"
        );
    }
    if let Some(crypto) = crypto {
        crypto.write(out);
    }
    let _ = write!(out, "a=ptime:20\r\na={}\r\n", direction.as_str());
}

/// Describes the live stream `params` as an SDP (our direction follows `send` and `receive`), for
/// a re-INVITE that carried no offer: the 200 then holds the offer.
pub fn describe(
    local: SocketAddr,
    params: &AudioParams,
    session_id: u64,
    version: u64,
    crypto: Option<&LocalCrypto>,
) -> String {
    let direction = match (params.send, params.receive) {
        (true, true) => Direction::SendRecv,
        (true, false) => Direction::SendOnly,
        (false, true) => Direction::RecvOnly,
        (false, false) => Direction::Inactive,
    };
    let mut out = String::with_capacity(512);
    write_session_header(&mut out, local.ip(), session_id, version);
    write_audio_section(&mut out, local.port(), params, direction, crypto);
    out
}

/// Answers `offer` in one step: [`Offer::parse`] and [`Offer::answer`] with equal session id and
/// version. Returns the answer text and the negotiated stream.
///
/// # Errors
/// [`SdpError`] as for [`Offer::parse`].
pub fn answer_offer(
    offer: &[u8],
    local: SocketAddr,
    supported: &[G711],
    session_id: u64,
) -> Result<(String, AudioParams), SdpError> {
    let offer = Offer::parse(offer, supported)?;
    Ok((offer.answer(local, session_id, session_id), offer.params()))
}

/// Builds an SDP offer for an outgoing call: `codecs` in order of preference plus
/// telephone-event at [`DEFAULT_DTMF_PAYLOAD_TYPE`], `a=ptime:20`, `sendrecv`.
pub fn build_offer(
    local: SocketAddr,
    codecs: &[G711],
    session_id: u64,
    crypto: Option<&LocalCrypto>,
) -> String {
    let ip = local.ip();
    let mut out = String::with_capacity(512);
    let proto = if crypto.is_some() {
        "RTP/SAVP"
    } else {
        "RTP/AVP"
    };
    let _ = write!(
        out,
        "v=0\r\no=brisvox {session_id} {session_id} IN {kind} {ip}\r\ns=brisvox\r\nc=IN {kind} {ip}\r\nt=0 0\r\nm=audio {} {proto}",
        local.port(),
        kind = ip_kind(ip),
    );
    for codec in codecs {
        let _ = write!(out, " {}", codec.payload_type());
    }
    let dtmf = DEFAULT_DTMF_PAYLOAD_TYPE;
    let _ = write!(out, " {dtmf}\r\n");
    for codec in codecs {
        let _ = write!(
            out,
            "a=rtpmap:{} {}/8000\r\n",
            codec.payload_type(),
            codec.name()
        );
    }
    let _ = write!(
        out,
        "a=rtpmap:{dtmf} telephone-event/8000\r\na=fmtp:{dtmf} 0-16\r\na=ptime:20\r\n"
    );
    if let Some(crypto) = crypto {
        crypto.write(&mut out);
    }
    out.push_str("a=sendrecv\r\n");
    out
}

/// Reads the answer to an offer made with [`build_offer`] for the codecs `offered`. When the offer
/// carried `crypto`, the answer must be an `RTP/SAVP` stream whose crypto line echoes the tag and
/// suite; the answerer's key is returned with the stream.
///
/// # Errors
/// [`SdpError`] when the answer is malformed, declines the audio stream, selects a codec that
/// was not offered, or (for an SRTP offer) does not accept our crypto line.
pub fn read_answer(
    answer: &[u8],
    offered: &[G711],
    crypto: Option<&LocalCrypto>,
) -> Result<(AudioParams, Option<CryptoLine>), SdpError> {
    let description = SessionDescription::parse(answer)?;
    // The answerer's first usable format is the one it prefers to send and the one we send;
    // `pick_audio` takes the first supported one in the answer's order, which is exactly that.
    // The other formats it lists are accepted for receiving: it may send any of them, and switch
    // between them during the call (RFC 3264 section 7).
    let security = Security {
        policy: if crypto.is_some() {
            SrtpPolicy::Required
        } else {
            SrtpPolicy::Off
        },
        secure_signaling: true,
    };
    let Picked {
        index,
        mut params,
        crypto: remote,
    } = pick_audio(&description, offered, security)?;
    if let (Some(ours), Some(theirs)) = (crypto, remote.as_ref())
        && (theirs.tag != ours.tag || theirs.suite != ours.suite)
    {
        return Err(SdpError::SrtpRefused);
    }
    if let Some(media) = description.media.get(index) {
        let mut next = 0usize;
        for payload_type in media.payload_types() {
            if payload_type == params.payload_type
                || params
                    .alt
                    .iter()
                    .flatten()
                    .any(|(pt, _)| *pt == payload_type)
            {
                continue;
            }
            let Some(codec) = media.g711_of(payload_type) else {
                continue;
            };
            if !offered.contains(&codec) {
                continue;
            }
            if let Some(slot) = params.alt.get_mut(next) {
                *slot = Some((payload_type, codec));
                next += 1;
            }
        }
    }
    Ok((params, remote))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\n\
        o=- 1 1 IN IP4 203.0.113.9\r\n\
        s=-\r\n\
        c=IN IP4 203.0.113.9\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 18 8 0 101\r\n\
        a=rtpmap:18 G729/8000\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=fmtp:101 0-16\r\n\
        a=ptime:20\r\n\
        a=sendrecv\r\n";

    fn read_plain(answer: &[u8], offered: &[G711]) -> AudioParams {
        read_answer(answer, offered, None).unwrap().0
    }

    fn local() -> SocketAddr {
        "192.0.2.10:20000".parse().unwrap()
    }

    #[test]
    fn the_offerers_most_preferred_supported_codec_wins() {
        let (answer, params) =
            answer_offer(OFFER.as_bytes(), local(), &[G711::Pcmu, G711::Pcma], 7).unwrap();
        // G.729 is not supported; PCMA comes before PCMU in the offer.
        assert_eq!(params.codec, G711::Pcma);
        assert_eq!(params.payload_type, 8);
        assert_eq!(params.dtmf_payload_type, Some(101));
        assert_eq!(params.remote, Some("203.0.113.9:40000".parse().unwrap()));
        assert!(params.send && params.receive);
        assert!(
            answer.contains("m=audio 20000 RTP/AVP 8 101\r\n"),
            "{answer}"
        );
        assert!(answer.contains("a=rtpmap:8 PCMA/8000\r\n"));
        assert!(answer.contains("a=rtpmap:101 telephone-event/8000\r\n"));
        assert!(answer.contains("c=IN IP4 192.0.2.10\r\n"));
        assert!(answer.contains("a=sendrecv\r\n"));
        // The answer parses back to the same stream.
        let again = read_plain(answer.as_bytes(), &[G711::Pcmu, G711::Pcma]);
        assert_eq!(again.codec, G711::Pcma);
        assert_eq!(again.remote, Some(local()));
    }

    #[test]
    fn a_codec_restriction_is_respected() {
        let (_, params) = answer_offer(OFFER.as_bytes(), local(), &[G711::Pcmu], 1).unwrap();
        assert_eq!(params.codec, G711::Pcmu);
        assert_eq!(params.payload_type, 0);
    }

    #[test]
    fn dynamic_payload_types_named_pcmu_are_understood() {
        let offer = "v=0\r\nc=IN IP4 198.51.100.1\r\nt=0 0\r\nm=audio 5004 RTP/AVP 96 97\r\n\
            a=rtpmap:96 PCMU/8000\r\na=rtpmap:97 telephone-event/8000\r\n";
        let (answer, params) = answer_offer(offer.as_bytes(), local(), &[G711::Pcmu], 1).unwrap();
        assert_eq!(params.payload_type, 96);
        assert_eq!(params.dtmf_payload_type, Some(97));
        assert!(
            answer.contains("m=audio 20000 RTP/AVP 96 97\r\n"),
            "{answer}"
        );
        assert!(answer.contains("a=rtpmap:96 PCMU/8000\r\n"));
    }

    #[test]
    fn no_telephone_event_is_fine() {
        let offer = "v=0\r\nc=IN IP4 198.51.100.1\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n";
        let (answer, params) = answer_offer(offer.as_bytes(), local(), &[G711::Pcmu], 1).unwrap();
        assert_eq!(params.dtmf_payload_type, None);
        assert!(!answer.contains("telephone-event"));
    }

    #[test]
    fn offers_without_a_usable_stream_are_refused_with_the_right_reason() {
        let no_codec = "v=0\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\nm=audio 5004 RTP/AVP 18\r\n";
        assert_eq!(
            answer_offer(no_codec.as_bytes(), local(), &[G711::Pcmu], 1).unwrap_err(),
            SdpError::NoCommonCodec
        );
        let srtp = "v=0\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\nm=audio 5004 RTP/SAVP 0\r\n";
        assert_eq!(
            answer_offer(srtp.as_bytes(), local(), &[G711::Pcmu], 1).unwrap_err(),
            SdpError::NoAudio
        );
        let video = "v=0\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\nm=video 5004 RTP/AVP 96\r\n";
        assert_eq!(
            answer_offer(video.as_bytes(), local(), &[G711::Pcmu], 1).unwrap_err(),
            SdpError::NoAudio
        );
    }

    #[test]
    fn malformed_sdp_is_reported_not_guessed() {
        for bad in [
            "",
            "hello",
            "v=1\r\nm=audio 1 RTP/AVP 0\r\n",
            "v=0\r\nm=audio notaport RTP/AVP 0\r\n",
            "v=0\r\nm=audio 70000 RTP/AVP 0\r\n",
            "v=0\r\nc=IN IP4\r\n",
            "v=0\r\nc=ATM IP4 1.2.3.4\r\n",
            "v=0\r\nm=audio\r\n",
        ] {
            assert!(
                matches!(
                    SessionDescription::parse(bad.as_bytes()),
                    Err(SdpError::Malformed(_))
                ),
                "{bad:?} parsed"
            );
        }
        assert!(SessionDescription::parse(&[0xff, 0xfe, b'v']).is_err());
        let huge = vec![b'a'; MAX_SDP_BYTES + 1];
        assert!(SessionDescription::parse(&huge).is_err());
        let sections = "m=audio 1 RTP/AVP 0\r\n".repeat(MAX_MEDIA_SECTIONS + 1);
        assert!(SessionDescription::parse(format!("v=0\r\n{sections}").as_bytes()).is_err());
    }

    #[test]
    fn hold_addresses_and_directions_are_mapped() {
        let hold = "v=0\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\na=sendonly\r\n";
        let (answer, params) = answer_offer(hold.as_bytes(), local(), &[G711::Pcmu], 1).unwrap();
        assert_eq!(params.remote, None);
        // The offerer only sends: we only receive.
        assert!(!params.send && params.receive);
        assert!(answer.contains("a=recvonly\r\n"), "{answer}");
        let inactive =
            "v=0\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\na=inactive\r\n";
        let (answer, params) =
            answer_offer(inactive.as_bytes(), local(), &[G711::Pcmu], 1).unwrap();
        assert!(!params.send && !params.receive);
        assert!(answer.contains("a=inactive\r\n"));
    }

    #[test]
    fn a_media_level_address_and_session_level_direction_are_used() {
        let offer = "v=0\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\na=recvonly\r\n\
            m=audio 5004 RTP/AVP 0\r\nc=IN IP4 10.0.0.2\r\n";
        let (_, params) = answer_offer(offer.as_bytes(), local(), &[G711::Pcmu], 1).unwrap();
        assert_eq!(params.remote, Some("10.0.0.2:5004".parse().unwrap()));
        assert!(params.send && !params.receive);
    }

    #[test]
    fn other_sections_are_declined_in_place() {
        let offer = "v=0\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\n\
            m=video 6000 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
            m=audio 5004 RTP/AVP 0\r\n";
        let (answer, _) = answer_offer(offer.as_bytes(), local(), &[G711::Pcmu], 1).unwrap();
        let video = answer.find("m=video 0 RTP/AVP 96\r\n").unwrap();
        let audio = answer.find("m=audio 20000 RTP/AVP 0\r\n").unwrap();
        assert!(video < audio, "{answer}");
    }

    #[test]
    fn an_offer_lists_codecs_in_order_with_telephone_event() {
        let offer = build_offer(local(), &[G711::Pcmu, G711::Pcma], 5, None);
        assert!(
            offer.contains("m=audio 20000 RTP/AVP 0 8 101\r\n"),
            "{offer}"
        );
        assert!(offer.contains("a=rtpmap:0 PCMU/8000\r\n"));
        assert!(offer.contains("a=rtpmap:8 PCMA/8000\r\n"));
        assert!(offer.contains("a=rtpmap:101 telephone-event/8000\r\n"));
        assert!(offer.contains("a=ptime:20\r\n"));
        // Our own offer is a valid SDP that we would answer.
        answer_offer(
            offer.as_bytes(),
            "192.0.2.20:30000".parse().unwrap(),
            &[G711::Pcma],
            1,
        )
        .unwrap();
    }

    #[test]
    fn an_answer_that_declines_or_picks_an_unoffered_codec_is_an_error() {
        let declined = "v=0\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\n";
        assert_eq!(
            read_answer(declined.as_bytes(), &[G711::Pcmu], None).unwrap_err(),
            SdpError::NoAudio
        );
        let wrong = "v=0\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\nm=audio 5004 RTP/AVP 8\r\n";
        assert_eq!(
            read_answer(wrong.as_bytes(), &[G711::Pcmu], None).unwrap_err(),
            SdpError::NoCommonCodec
        );
    }

    #[test]
    fn an_answer_listing_several_codecs_is_accepted_for_receiving() {
        let answer = "v=0\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0 8 18 101\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";
        let params = read_plain(answer.as_bytes(), &[G711::Pcmu, G711::Pcma]);
        // We send the first one; PCMA may arrive as well; G.729 and the DTMF type are no audio.
        assert_eq!((params.codec, params.payload_type), (G711::Pcmu, 0));
        assert_eq!(params.receive_codec(0), Some(G711::Pcmu));
        assert_eq!(params.receive_codec(8), Some(G711::Pcma));
        assert_eq!(params.receive_codec(18), None);
        assert_eq!(params.receive_codec(101), None);
        // A codec we did not offer is not accepted even when the answer lists it.
        let only_pcmu = read_plain(answer.as_bytes(), &[G711::Pcmu]);
        assert_eq!(only_pcmu.receive_codec(8), None);
        // Our own answers name one codec.
        let (_, params) =
            answer_offer(OFFER.as_bytes(), local(), &[G711::Pcmu, G711::Pcma], 1).unwrap();
        assert_eq!(params.receive_codec(0), None);
        assert_eq!(params.receive_codec(8), Some(G711::Pcma));
    }

    #[test]
    fn ipv6_addresses_are_written_and_read() {
        let local6: SocketAddr = "[2001:db8::10]:20000".parse().unwrap();
        let offer = "v=0\r\nc=IN IP6 2001:db8::9\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n";
        let (answer, params) = answer_offer(offer.as_bytes(), local6, &[G711::Pcmu], 1).unwrap();
        assert!(answer.contains("c=IN IP6 2001:db8::10\r\n"), "{answer}");
        assert_eq!(params.remote, Some("[2001:db8::9]:5004".parse().unwrap()));
    }

    // -- SRTP (SDES) ---------------------------------------------------------------------------

    fn key(seed: u8) -> MasterKey {
        MasterKey::from_bytes(&[seed; 30])
    }

    fn secure_offer(crypto: &[String]) -> String {
        let mut sdp = "v=0\r\nc=IN IP4 198.51.100.1\r\nt=0 0\r\nm=audio 5004 RTP/SAVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n"
            .to_owned();
        for line in crypto {
            sdp.push_str(&format!("a=crypto:{line}\r\n"));
        }
        sdp
    }

    fn line(tag: u32, suite: &str, key: &MasterKey) -> String {
        format!("{tag} {suite} inline:{}", key.to_base64())
    }

    const TLS_OPTIONAL: Security = Security {
        policy: SrtpPolicy::Optional,
        secure_signaling: true,
    };

    #[test]
    fn a_secure_offer_is_answered_with_our_own_key_under_the_same_tag_and_suite() {
        let theirs = key(7);
        let offer = secure_offer(&[line(4, "AES_CM_128_HMAC_SHA1_32", &theirs)]);
        let offer = Offer::parse_secure(offer.as_bytes(), &[G711::Pcmu], TLS_OPTIONAL).unwrap();
        let remote = offer.srtp().unwrap();
        assert_eq!(
            (remote.tag, remote.suite),
            (4, SrtpSuite::AesCm128HmacSha1_32)
        );
        assert_eq!(remote.key, theirs);
        let ours = LocalCrypto {
            tag: remote.tag,
            suite: remote.suite,
            key: key(9),
        };
        let answer = offer.answer_with(local(), 1, 1, Some(&ours));
        assert!(
            answer.contains("m=audio 20000 RTP/SAVP 0 101\r\n"),
            "{answer}"
        );
        assert!(
            answer.contains(&format!(
                "a=crypto:4 AES_CM_128_HMAC_SHA1_32 inline:{}\r\n",
                ours.key.to_base64()
            )),
            "{answer}"
        );
        // Read back as the offerer would: the tag and suite are echoed.
        let (params, line) = read_answer(answer.as_bytes(), &[G711::Pcmu], Some(&ours)).unwrap();
        assert_eq!(params.codec, G711::Pcmu);
        assert_eq!(line.unwrap().key, ours.key);
    }

    #[test]
    fn keys_are_never_taken_from_signaling_that_is_not_tls_and_the_policy_decides() {
        let offer = secure_offer(&[line(1, "AES_CM_128_HMAC_SHA1_80", &key(1))]);
        let parse = |policy, secure_signaling| {
            Offer::parse_secure(
                offer.as_bytes(),
                &[G711::Pcmu],
                Security {
                    policy,
                    secure_signaling,
                },
            )
        };
        assert!(parse(SrtpPolicy::Optional, true).unwrap().srtp().is_some());
        assert!(parse(SrtpPolicy::Required, true).unwrap().srtp().is_some());
        assert_eq!(
            parse(SrtpPolicy::Optional, false).unwrap_err(),
            SdpError::SrtpRefused
        );
        assert_eq!(
            parse(SrtpPolicy::Required, false).unwrap_err(),
            SdpError::SrtpRefused
        );
        // Off: a secure-only offer has no audio this endpoint can use, as before SRTP existed.
        assert_eq!(parse(SrtpPolicy::Off, true).unwrap_err(), SdpError::NoAudio);
        // A plain offer: fine unless SRTP is required.
        let plain = Offer::parse_secure(OFFER.as_bytes(), &[G711::Pcmu], TLS_OPTIONAL).unwrap();
        assert!(plain.srtp().is_none());
        let required = Security {
            policy: SrtpPolicy::Required,
            secure_signaling: true,
        };
        assert_eq!(
            Offer::parse_secure(OFFER.as_bytes(), &[G711::Pcmu], required).unwrap_err(),
            SdpError::SrtpRefused
        );
    }

    #[test]
    fn crypto_lines_this_module_cannot_honour_are_ignored_never_half_read() {
        let good = key(3);
        let b64 = good.to_base64();
        let lines = [
            // Unsupported suites, an MKI, session parameters, two keys, a short key, no inline.
            format!("1 AEAD_AES_128_GCM inline:{b64}"),
            format!("2 AES_CM_128_HMAC_SHA1_80 inline:{b64}|2^31|1:4"),
            format!("3 AES_CM_128_HMAC_SHA1_80 inline:{b64} UNENCRYPTED_SRTP"),
            format!("4 AES_CM_128_HMAC_SHA1_80 inline:{b64};inline:{b64}"),
            "5 AES_CM_128_HMAC_SHA1_80 inline:AAAA".to_owned(),
            format!("6 AES_CM_128_HMAC_SHA1_80 {b64}"),
            format!("x AES_CM_128_HMAC_SHA1_80 inline:{b64}"),
            // The first usable one: a lifetime is fine.
            format!("7 AES_CM_128_HMAC_SHA1_80 inline:{b64}|2^31"),
        ];
        let offer = secure_offer(&lines);
        let offer = Offer::parse_secure(offer.as_bytes(), &[G711::Pcmu], TLS_OPTIONAL).unwrap();
        assert_eq!(offer.srtp().unwrap().tag, 7);
        // Only unusable lines: refused, not served in the clear.
        let offer = secure_offer(&lines[..7]);
        assert_eq!(
            Offer::parse_secure(offer.as_bytes(), &[G711::Pcmu], TLS_OPTIONAL).unwrap_err(),
            SdpError::SrtpRefused
        );
    }

    #[test]
    fn a_key_lifetime_below_two_to_the_31_is_refused() {
        let b64 = key(3).to_base64();
        let value = |life: &str| format!("1 AES_CM_128_HMAC_SHA1_80 inline:{b64}|{life}");
        for short in ["2^20", "2^30", "1048576", "2147483647", "2^0", "0"] {
            assert!(parse_crypto(&value(short)).is_none(), "{short} accepted");
        }
        for long in [
            "2^31",
            "2^48",
            "2147483648",
            "281474976710656",
            "2^999999999999",
        ] {
            assert!(parse_crypto(&value(long)).is_some(), "{long} refused");
        }
        for junk in ["2^", "2^x", "abc", "-5", ""] {
            assert!(parse_crypto(&value(junk)).is_none(), "{junk:?} accepted");
        }
        // No lifetime at all is the default (2^48).
        assert!(parse_crypto(&format!("1 AES_CM_128_HMAC_SHA1_80 inline:{b64}")).is_some());
    }

    #[test]
    fn the_secure_section_wins_and_the_plain_one_is_declined() {
        let sdp = format!(
            "{OFFER}m=audio 5006 RTP/SAVP 0\r\na=rtpmap:0 PCMU/8000\r\na=crypto:{}\r\n",
            line(1, "AES_CM_128_HMAC_SHA1_80", &key(5))
        );
        let offer = Offer::parse_secure(sdp.as_bytes(), &[G711::Pcmu], TLS_OPTIONAL).unwrap();
        assert!(offer.srtp().is_some());
        assert_eq!(
            offer.params().remote,
            Some("203.0.113.9:5006".parse().unwrap())
        );
        let ours = LocalCrypto {
            tag: 1,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key: key(6),
        };
        let answer = offer.answer_with(local(), 1, 1, Some(&ours));
        assert!(
            answer.contains("m=audio 0 RTP/AVP 18 8 0 101\r\n"),
            "{answer}"
        );
        assert!(answer.contains("RTP/SAVP"), "{answer}");
    }

    #[test]
    fn an_answer_to_a_secure_offer_must_be_secure_and_echo_the_tag_and_suite() {
        let ours = LocalCrypto {
            tag: 1,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key: key(1),
        };
        let offer = build_offer(local(), &[G711::Pcmu], 5, Some(&ours));
        assert!(offer.contains(" RTP/SAVP 0 101\r\n"), "{offer}");
        assert!(
            offer.contains("a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:"),
            "{offer}"
        );
        let reply = |proto: &str, crypto: &str| {
            format!(
                "v=0\r\nc=IN IP4 198.51.100.2\r\nt=0 0\r\nm=audio 6000 {proto} 0\r\na=rtpmap:0 PCMU/8000\r\n{crypto}"
            )
        };
        let theirs = key(2).to_base64();
        let good = reply(
            "RTP/SAVP",
            &format!("a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{theirs}\r\n"),
        );
        assert!(read_answer(good.as_bytes(), &[G711::Pcmu], Some(&ours)).is_ok());
        for bad in [
            reply("RTP/AVP", ""),
            reply("RTP/SAVP", ""),
            reply(
                "RTP/SAVP",
                &format!("a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:{theirs}\r\n"),
            ),
            reply(
                "RTP/SAVP",
                &format!("a=crypto:1 AES_CM_128_HMAC_SHA1_32 inline:{theirs}\r\n"),
            ),
        ] {
            assert_eq!(
                read_answer(bad.as_bytes(), &[G711::Pcmu], Some(&ours)).unwrap_err(),
                SdpError::SrtpRefused,
                "{bad}"
            );
        }
    }

    #[test]
    fn key_material_stays_out_of_debug_output_of_the_parsed_sdp() {
        let secret = key(8);
        let offer = secure_offer(&[line(1, "AES_CM_128_HMAC_SHA1_80", &secret)]);
        let parsed = Offer::parse_secure(offer.as_bytes(), &[G711::Pcmu], TLS_OPTIONAL).unwrap();
        let text = format!(
            "{parsed:?} {:?}",
            SessionDescription::parse(offer.as_bytes()).unwrap()
        );
        assert!(!text.contains(&secret.to_base64()), "{text}");
    }
}
