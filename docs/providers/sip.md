# SIP / RTP — telephony transport (Rust stack selection)

Scope: research Rust SIP stacks; inbound/outbound call with RTP audio. Related:
`docs/providers/g711.md` (codec), `docs/providers/twilio.md` (the Media Streams transport; depth and
style example), `docs/providers/webrtc.md` (str0m based, the other RTP-carrying transport), `docs/providers/call-control.md`
(DTMF, transfer), `docs/ARCHITECTURE.md` "Transport".
Researched: 2026-10-08. Every fact below was read from the pages listed under Sources on that date
(crates.io API, GitHub API/pages, docs.rs, vendor docs, RFCs). Anything not confirmed is marked
UNVERIFIED. Caveat on method: the fetch tool summarizes pages with a small model, so code-level claims
(method names, feature lists) are second-hand extracts, not source reads. The spike (section 3 below)
must re-confirm the ones it depends on against the actual crate source before they are trusted.
This is a transport, not an STT/LLM/TTS provider: brisvox is a SIP user agent (UAS for inbound calls,
UAC for outbound calls); there is no HTTP/WebSocket leg.

- Docs: RFC 3261 (SIP), RFC 3264 (offer/answer), RFC 3550/3551 (RTP, G.711 payload types), RFC 4733
  (telephone-event), RFC 4028 (session timers), RFC 3581 (rport), RFC 4961 (symmetric RTP); stack
  docs per candidate in the tables; source list at the bottom (all fetched 2026-10-08).
- Auth / trust (what the carriers do, not the stacks):
  - Twilio Elastic SIP Trunking termination (brisvox -> Twilio -> PSTN): IP ACL, credential list (SIP
    digest) or both; if both are configured both are enforced; Twilio warns IP ACL alone does not
    protect against certain attacks (elastic SIP trunking overview).
  - Telnyx: credentials (SIP username/password, works with dynamic IPs), FQDN (hostname based inbound
    routing, credentials or IP for outbound), IP + token (`X-Telnyx-Token` header), IP + tech prefix,
    IP + `P-Charge-Info` (Telnyx trunking docs, re-read 2026-10-08 in the fact-check).
  - Inbound trust in brisvox: INVITEs must be authenticated by source-IP allowlist (carrier ranges) and/or
    digest challenge (401/407). Never log credentials. An open UDP 5060 listener is scanned within
    minutes on the public internet (general knowledge, not a fetched fact): default-deny unknown sources.
- Endpoint(s) / regions: see "Interop targets". Brisvox listens on UDP 5060 (default), optional TCP 5060
  and TLS 5061; RTP on a configured UDP port range (one socket per call).

## 1. Candidate stacks (state on 2026-10-08)

crates.io/GitHub data fetched 2026-10-08 (API JSON). "Pushed" = GitHub `pushed_at`.

| Stack | Latest (crates.io) | License | Last release / push | Popularity | Scope |
|---|---|---|---|---|---|
| **rsipstack** (restsend) | 0.7.3 (2026-10-08) | MIT | releases 0.7.0..0.7.3 on 2026-10-06/07/08, 0.6.13 on 2026-09-29; push 2026-10-08 | 212 stars, 69 forks, 2 open issues, 61.9k downloads, repo created 2024-11-04 (crate first published 2025-01-10) | SIP signaling only (transport, transaction, dialog, registration, digest). No RTP/SDP/DTMF. |
| **rsiprtp** (0x4D44) | 0.4.1 (2026-05-07; only 2 versions ever: 0.0.0 on 2026-04-28, then 0.4.1) | MIT OR Apache-2.0 | push 2026-05-08 (5 months old at research time) | 3 stars, 3 forks, 1 open issue, 261 downloads | Full audio UA: SIP + SDP + RTP/RTCP + jitter buffer + SRTP + codecs; sans-IO core + tokio transports. |
| **rvoip** (eisenzopf) | 0.3.12 (2026-10-01); `rvoip-sip` 0.3.12 | MIT | releases 0.3.6 (2026-08-03) .. 0.3.12 (2026-10-01), push 2026-10-08 | 175 stars, 28 forks, 79 open issues; `rvoip` 8.8k downloads, `rvoip-sip` 1.4k | Large workspace (46 crates): SIP, RTP, WebRTC, voice-AI helpers. |
| **ezk** (kbalt): `ezk-sip-core`, `ezk-sip-ua`, `ezk-rtc` | `ezk-sip-core` 0.9.2 (2026-07-16); `ezk-sip-ua` 0.9.1 (2026-04-16); `ezk-rtc` 0.0.1 (2026-04-16) | MIT | push 2026-09-27 | 50 stars, 15 forks, 11 open issues; `ezk-sip-core` 95k, `ezk-sip-ua` 100k downloads (`ezk-rtc` 298) | SIP core + UA (REGISTER, calls, REFER) + SDP-based media session. |
| **xphone** (x-phone) | 0.6.0 (2026-05-13) | MIT | push 2026-08-06 | 5 stars, 5 forks, 11 open issues, 1.4k downloads | High-level softphone lib: SIP + RTP + codecs; decoded PCM via crossbeam channels. |
| **wavekat-sip** | 0.3.1 (2026-10-06) | Apache-2.0 | push 2026-10-06; many releases (0.2.2..0.3.1 in 2026-09-20..10-06) | 2 stars, 1 fork, 0 open issues, 922 downloads, repo created 2026-05-12 | "SIP signaling and RTP transport for voice pipelines"; negotiation only, codecs left to caller. |
| **sip-core** (xmppjingle/sipr) | 0.3.0 (2026-03-19) | Apache-2.0 | push 2026-10-05 | 4 stars, 175 downloads; repo description "Rust CLI based SIP softphone" | "SIP protocol library for parsing, serialization, and dialog management". Not investigated further: UNVERIFIED capability. |
| **rsip** (vasilakisfil) | 0.4.0 (2022-04-01) | crates.io: "non-standard"; GitHub: NOASSERTION ("Other") | push 2024-06-15 | 108 stars, 161k downloads | Parser/generator only; stale; license unclear. Not a candidate (rsiprtp uses it only as a test oracle: `parser_diff` dev-dependency per rsiprtp `tests.md`). |
| **PJSIP via FFI** | `pjproject-sys` 1.0.0 (2023-07-13, single version) | crate MIT; PJSIP itself GPL-2.0-or-later with an alternative commercial license (contact licensing@pjsip.org) | crate untouched since 2023 | 1.5k downloads | Note only, see below. |
| RTP-only crates | `rtp-types` 0.1.2 (2024-11-14, MIT OR Apache-2.0, 463k downloads); `msf-rtp` 0.4.2 (2026-06-03, MIT, tokio based); `rtp-engine` 0.1.0 (2026-03-06, MIT OR Apache-2.0, 50 downloads, claims RTP/RTCP, G.711, SRTP, jitter buffer, audio devices); `rustrtc` 0.4.3 (2026-10-08, MIT, restsend, WebRTC-centric, has `TransportMode::Rtp`/`Srtp`, G.711 packetizer, jitter buffer, 112 stars) | | | | Building blocks for media if signaling is taken from rsipstack. |

### 1.1 Feature matrix (what the docs claim; "-" = not mentioned in what was fetched = UNVERIFIED unless stated absent)

| Feature | rsipstack 0.7.3 | rsiprtp 0.4.1 | rvoip 0.3.12 | ezk (sip-ua 0.9.1) | xphone 0.6.0 | wavekat-sip 0.3.1 |
|---|---|---|---|---|---|---|
| REGISTER (+digest, refresh) | yes (`Registration`, `Credential`); client example re-registers every `max(expires,50)` s | yes, digest incl. SHA-256 (RFC 7616), periodic refresh | yes (beta-qualified) | yes (`Registration`, `RegistrarConfig`) | yes, auto-reconnect and keepalive | yes, digest + keepalive |
| INVITE / ACK / BYE | yes (dialog layer: `do_invite`, server `accept`/`reject`/`ringing`, `bye`) | yes (`CallManager`) | yes | yes (`OutboundCall`, `InboundCall`, `Call`) | yes, in+out, 302 follow, early media 183 | yes (`Caller`, `IncomingCall`) |
| CANCEL | yes (`cancel`; `hangup_with_headers` = BYE if confirmed else CANCEL) | tested (`tests/integration_advanced.rs` covers CANCEL per `tests.md`); not named on docs.rs/README | yes (beta-qualified) | - | - | - |
| re-INVITE / UPDATE | yes (`reinvite`, `update` on dialogs) | yes (hold/resume via re-INVITE; UPDATE RFC 3311) | - | - | hold/resume via re-INVITE | hold/resume |
| PRACK (100rel) | yes | yes (RFC 3262) | - | - | - | - |
| Session timers RFC 4028 | - (no session-timer API in the README or dialog pages: UNVERIFIED) | yes (refresher + non-refresher; `tick`/`next_deadline`) | - | - | yes | yes |
| Digest auth (UAC) | yes ("built-in") | yes | yes (beta-qualified) | yes (`ezk-sip-auth` dependency) | yes | yes |
| UDP / TCP / TLS | UDP, TCP, TLS (rustls), WebSocket (TLS and WebSocket via default-on features `rustls`/`websocket`) | UDP, TCP, TLS (rustls); no SIP-over-WebSocket | UDP, TCP, TLS (beta-qualified); plain SIP-over-WebSocket "bounded evidence", WSS developer preview | - (TLS via features `tls-rustls`/`tls-native-tls` on `ezk-sip-core`, neither default; UDP/TCP: UNVERIFIED) | UDP, TCP, TLS | UDP; TLS behind `tls` feature (off by default) |
| SDP offer/answer | helper only: `InviteOption.offer` body; example parses with `sdp-rs`; no negotiation | yes (RFC 3264) | yes (negotiation) | yes (SDP-based media backend, `ezk-rtc` `SdpSession`) | yes (not documented in detail) | yes, Opus + G.711 only |
| RTP / RTCP | none in the crate; client example has its own RTP code in a `play_file` module (`build_rtp_conn`, `play_audio_file`; implementation not read) | yes (RTP, RTCP SR/RR/SDES/BYE/APP, rtcp-mux) | yes (beta-qualified) | `ezk-rtc` 0.0.1 (uses `rtp-types`, `rtcp-types`) | yes (own stack) | header parser + receive loop + send loop |
| G.711 PCMU/PCMA | none | yes (also G.722, Opus) | yes (beta-qualified) | - | yes (also G.722; Opus, G.729 optional feature flags) | negotiates only; "encode/decode stays with the consumer" |
| RFC 2833/4733 telephone-event | none (client example: no DTMF; `info` method exists for SIP INFO) | yes (RFC 4733) | yes (RFC 4733; `SessionHandle::send_dtmf`) | - | yes (RFC 4733 + SIP INFO RFC 2976) | yes (RFC 4733, INFO fallback) |
| Jitter buffer | none | yes, adaptive (reorder, duplicate, late) | yes, listed under OS audio devices as developer preview | - | yes, built in | - |
| SRTP | none | SDES (RFC 4568); no DTLS-SRTP | SDES beta-qualified; DTLS developer preview | `ezk-rtc` has ICE/SRTP (depends on `openssl`) | SDES (AES_CM_128_HMAC_SHA1_80, UNVERIFIED re-check); no DTLS | - |
| NAT helpers | `accept_with_public_contact` (server dialog); example `--external-ip` (or `EXTERNAL_IP` env) | ICE/STUN helper (host + server-reflexive candidates; TURN relay, trickle ICE, ICE restart not supported) | STUN; TURN via external config (no hosted TURN) | ICE crates exist | STUN, TURN, ICE-Lite | - |
| Async runtime | tokio (TCP/WS need `platform-tokio`); also an embassy no_std target (UDP + TLS client only: UNVERIFIED, README says core/codec/transaction/dialog/UDP are no_std, TCP/WS need `platform-tokio`) | tokio transports; core is sans-IO and runtime-agnostic | tokio ("full" in examples); sans-IO status unstated | tokio ^1 (`RtpReceiver` wraps tokio mpsc); `ezk-rtc` `SdpSession` is sans-IO, tokio feature optional | mixed: `listen().await` for server mode, one dedicated `std::thread` per call media pipeline, crossbeam channels | async/await with `tokio_util` `CancellationToken` in examples; runtime unconfirmed |
| Sans-IO | no (tokio tasks, transport layer) | yes (transactions, dialogs, `CallManager`; time passed as `now: std::time::Instant`) | unstated | partly (`SdpSession`) | not mentioned | not mentioned |
| MSRV | UNVERIFIED | 1.88 | 1.91 | UNVERIFIED | 1.87 | UNVERIFIED |
| Production users | README has a "Used by" heading with no entries (docs.rs copy of the README has none); RustPBX (same org, `rustpbx` 0.5.1 depends on `rsipstack` 0.7.1 and `rustrtc` 0.4.0) | none listed | none listed | none listed | README says it is "used in production alongside xbridge" (self-claim) | none listed |
| Interop evidence | not stated in README | only Asterisk named (`basic_call.rs` against live Asterisk); parser differential-tested vs `rsip` (`parser_diff`) and RFC 4475 fixtures; 29-target cargo-fuzz crate (all per `tests.md`) | release-gated against Asterisk, FreeSWITCH, Jambonz OSS 0.9.11, SIPp, baresip, Kamailio, OpenSIPS (self-reported; "not carrier certification"); Twilio/Telnyx not mentioned | none stated | claims Telnyx, Twilio SIP, VoIP.ms, Vonage (trunks), Asterisk, FreeSWITCH, 3CX (PBXes) ("not a comprehensive compatibility matrix") | none stated |
| Examples | `proxy`, `client` (REGISTER, invite, answer, file playback, echo), `bench_ua` | `basic_call` (REGISTER+INVITE+BYE vs Asterisk), `voicemail`, `ai_bridge` (SIMULATED: no real SIP/RTP/WebSocket code, re-verified 2026-10-08), `ice_call`, `session_timers` | `01-quickstart-p2p`..`14-vapi-agent` (14 examples); `endpoint_local_call` UNVERIFIED | `examples/` (need a remote endpoint; not runnable as is) | `examples/sipcli` | Quick Start snippets only |
| Stated maturity | none | pre-1.0, "pin an exact version for production"; CONTRIBUTING: personal project, no commitment to triage/merge | SIP "Beta-qualified"; media/WebRTC/voice-AI "Developer preview"; "published crate is evidence of availability, not a blanket production-readiness statement" | none found (README fetch was partial: UNVERIFIED) | Beta | "Early development. API will change between minor versions." |

Other facts worth knowing:
- rsipstack: `ClientInviteDialog` and `ServerInviteDialog` are marked deprecated in favour of
  `invite_dialog::InviteDialog` (docs.rs struct pages, latest; the `dialog` module index page itself shows no
  deprecation note). On `InviteDialog`, `hangup()` delegates to `hangup_with_headers()`, which picks BYE
  (confirmed) or CANCEL (early) and works for either role (CORRECTED from the 0.7.3 source in section 9: the
  docs.rs-derived claim that `hangup()` is BYE-only and a no-op on UAS dialogs was wrong). Four releases in
  three days (0.7.0-0.7.3) and a recent embassy/no_std refactor mean API churn: pin an exact version.
- rsiprtp `CallManager` (docs.rs): `create_call(remote_uri) -> CallId` does no DNS or socket I/O (resolve
  with `SipResolver`, RFC 3263), `handle_incoming_invite(dialog, offer_sdp) -> Option<(CallId, SessionDescription, u16)>`
  (the `u16` is undocumented),
  `answer_call`, `reject_call`, `terminate_call` (BYE; only for `Established` calls), `handle_bye`, `tick(now)` / `next_deadline()` for
  session timers, `drain_events()` and `drain_outbound_requests()` as the output side. The caller opens the
  client/server transactions itself for the requests the manager builds (PRACK, UPDATE, BYE, re-INVITE).
  This is a bring-your-own-glue sans-IO API, which fits brisvox style but means the integration glue
  (UDP socket loop, transaction pump, timers) is ours. `now` is `std::time::Instant` (docs.rs signature
  `tick(&mut self, now: Instant)`; the docs pass `next_deadline()` to `tokio::time::sleep_until`), so the
  injected `Clock` must convert to/from `std::time::Instant` (confirmed 2026-10-08; previously UNVERIFIED).
- rsiprtp dependencies (Cargo.toml): tokio, tokio-rustls/rustls/webpki-roots, hickory-resolver, `audio-codec-algorithms`,
  `ezk-g722`, `ropus`, aes/ctr/hmac/sha1/sha2/md5, `bytes`, `uuid`, `serde`; no optional features
  (`default = []`); the SIP parser is in-tree (`rsip` is a dev-dependency only). Version, license and MSRV are
  workspace-inherited (not in the crate Cargo.toml). No `unsafe` lint info available (workspace lints not read).
- rsiprtp repo contains `CLAUDE.md`, `wrk_docs/`, `wrk_journals/` (page does not say whether code is
  AI-written). 205 commits, no releases or tags on GitHub, CI workflow present. GitHub repo created 2025-12-05.
- ezk: `ezk-rtc` says its goal is use cases beyond WebRTC "including SIP, which does not always require
  ICE or even SRTP"; it depends on `openssl` (build burden) plus `ezk-ice`, `ezk-srtp`, `rtp-types`,
  `rtcp-types`. `ezk-rtc` is at 0.0.1 with 298 downloads; the fetched part of the `ezk` README has no maturity statement.
- rvoip: workspace of 46 crates, MSRV 1.91 (README badge), 79 open issues; audio API is `SessionHandle::audio()` with
  `AudioFrame`/`AudioSender`/`AudioReceiver`/`AudioStream` (`api::audio`), fields not documented on
  the page fetched; `send_dtmf` is RFC 4733. Dependency and compile-time weight would be large (UNVERIFIED, 46 crates).
- xphone: the dedicated-thread-per-call model conflicts with the brisvox goal of hundreds of calls per
  container and the "no blocking work on tokio workers" / cancellation invariants (a thread per call is
  not a tokio worker, but it breaks the `CancellationToken`/`Clock` style and costs memory per call).
- wavekat-sip: GitHub About says "built on rsipstack"; the README says "from-scratch ... no external
  SIP stack" and credits only `rsip` for message types. Contradictory, UNVERIFIED which is true.
- PJSIP via FFI (`pjproject-sys` 1.0.0, 2023; a higher-level community repo "THE-PJPROJECT" by JadKHaddad
  exists, status not checked: UNVERIFIED): mature C stack with its own media threads and global state,
  GPL-2.0-or-later unless a commercial license is bought. Conflicts with the project's license
  preference (MIT/Apache/BSD), with the no-FFI-surprises ethos and with per-session isolation and
  deterministic simulation. Note only; not recommended.

## 2. Scored comparison

Scale 1 (poor) to 5 (strong). The scores are the author's judgement from the evidence above, NOT
measured results; they were not validated against running code. Criteria are those named for the stack selection.
"+own media" means the cost of the missing pieces is counted (we would write them).

| Criterion | rsipstack +own media | rsiprtp | rvoip | ezk | xphone | wavekat-sip | PJSIP FFI |
|---|---|---|---|---|---|---|---|
| Maturity / maintenance | 4 (active, most adopted, API churn) | 2 (one author, last push 5 months ago, 261 downloads) | 3 (active, big, 79 issues, self-gated beta) | 3 (older, steady, small) | 3 (prod claim, small) | 2 (4 months old, "API will change") | 5 (C stack) but crate 1 |
| Inbound + outbound calls | 5 | 4 (APIs present; end-to-end example is simulated) | 5 | 4 | 5 | 4 | 5 |
| RTP / G.711 | 2 (nothing in crate) | 5 | 4 | 3 | 5 | 3 | 5 |
| DTMF (RFC 4733) | 2 (INFO send exists; 4733 ours) | 5 | 4 | 1 (UNVERIFIED) | 5 | 4 | 5 |
| tokio fit | 5 | 4 | 4 | 4 | 1 | 3 | 1 |
| License | 5 (MIT) | 5 (MIT OR Apache-2.0) | 5 (MIT) | 5 (MIT) | 5 (MIT) | 5 (Apache-2.0) | 1 (GPL-2.0+ / commercial) |
| Test-ability (deterministic sim, no sockets) | 3 (UDP loopback works; fake transport UNVERIFIED) | 5 (sans-IO, time injected as `std::time::Instant`) | 3 | 4 (sans-IO `SdpSession`; SIP core UNVERIFIED) | 3 | 3 | 1 |
| Unweighted total (max 35) | 26 | 30 | 28 | 24 | 27 | 24 | 23 |

Reading the table: rsiprtp scores highest on features and fit with the brisvox invariants but lowest
on the maintenance evidence that matters most for a dependency we would carry for years (single
maintainer who states no commitment to contributions, 5 months since the last push, 261 downloads,
interop shown only against Asterisk, the AI-bridge example is a stub). rsipstack has the strongest
maintenance and adoption signal and the permissive license, at the cost of writing the media half.
rvoip is the broadest and the only one with a published multi-PBX interop gate, but it is a 46-crate
workspace with an API that is still in preview outside SIP and 79 open issues.

## 3. Recommendation

Recommendation: **Use `rsipstack` (MIT) for SIP signaling and write the media
half in brisvox (`brisvox-transport` feature `sip`): minimal SDP offer/answer for PCMU/PCMA +
`telephone-event`, an RTP session task per call, RFC 4733 receive/send, a small jitter/reorder buffer,
G.711 through the existing `brisvox_audio::codec` (`ulaw_encode/decode`, `alaw_encode/decode`).**
Reasons: (1) signaling (transactions, dialogs, digest, TLS, retransmission timers) is the hard,
interop-sensitive code and rsipstack is the most active and most adopted option (also the base of
RustPBX); (2) the media half is small (12-byte RTP header, 160 G.711 bytes per 20 ms, 4-byte
telephone-event payload) and is the hot path, where brisvox's own pacer, `Clock`, buffer reuse and
`CancellationToken` rules matter and a library's media loop would get in the way; (3) tokio-native, MIT;
(4) brisvox already owns the G.711 codec and resampler. Rejected: xphone (thread per call), PJSIP (GPL,
FFI, global state), wavekat-sip (too young), ezk (no confirmed DTMF, OpenSSL in the media crate),
rvoip (size, preview media layer).

Hedge (NOT BUILT): the draft proposed keeping the SIP stack behind a narrow internal trait
(`SipSignaling`: register, accept invite, answer/reject, dial, hangup, in-dialog events) so `rsiprtp`
(sans-IO, complete) or `rvoip` can replace rsipstack if the spike fails. The implementation did not build that trait: the
spike worked, and `inbound.rs`, `outbound.rs` and `call.rs` use rsipstack types directly, so a stack swap
would rewrite those three files (the media half, `sdp.rs` and `rtp.rs`, is stack-independent). Pin an exact
`rsipstack` version (the 0.7.x line changed in 3 days; it is pinned with `=0.7.3`).
Revisit if: rsipstack cannot be driven with injected time/sockets for deterministic tests, session
timers are required by a target carrier and are painful to add, or an interop bug is unfixable.

Do-first spike (historical; superseded by the implementation, see section 9): build `examples/sip_echo` against rsipstack that answers an INVITE
from a local `sipp`/Asterisk-in-docker/second brisvox UA, negotiates PCMU + telephone-event, echoes
audio, sends BYE; read the real source for the UNVERIFIED items (session timers, fake transport,
`accept_with_public_contact`, behaviour of 200-without-ACK, CANCEL race, forked responses).

## 4. Minimal integration plan

Architecture follows `docs/ARCHITECTURE.md` Transport: a protocol adapter that bridges SIP/RTP
to `call_pair()` channels. `Transport::accept(&mut self, cancel) -> Result<Option<CallLink>>` stays the
inbound API; outbound needs one extra method (see below). Wire events used: `WireEvent::{Started,
Audio, Dtmf, MarkAck, Lead, Fault, Hangup}` and `WireOut::{Audio, Clear, Mark, Hangup}`
(`crates/brisvox-transport/src/call.rs`, read 2026-10-08, re-checked in the fact-check).

1. **One SIP UA endpoint per server (`SipServer`).** Owns the UDP socket (and optional TCP/TLS
   listeners), the rsipstack `Endpoint` and the `DialogLayer`, one accept task (tracked by a
   `TaskTracker`), and the registration tasks for REGISTER-based accounts (not for Twilio trunks, see
   interop). Config: bind addr, `public_ip`/advertised host (SDP `c=` and Contact/Via), RTP port range,
   allowed source CIDRs, digest credentials for inbound auth, call limit, accept queue size, timeouts.
2. **Inbound INVITE -> `CallLink`.** (a) Check source allowlist / digest (401/407). (b) Over the call limit
   or accept queue full -> `503 Service Unavailable` (plus `Retry-After`) like the Twilio server's 503;
   bad SDP/no common codec -> `488`. (c) `100 Trying`, bind the per-call RTP socket, `180 Ringing`
   optional, answer `200 OK` with SDP (our `c=`/port, PCMU/PCMA in the offerer's preference order
   restricted to what we support, `telephone-event` with the offerer's dynamic payload type, `a=ptime:20`,
   `sendrecv`). (d) On ACK: `call_pair()`, send `WireEvent::Started(CallInfo)` (metadata: `Call-ID`,
   From/To, remote address, selected codec, `x-*` headers; `CallInfo::SAMPLE_RATE_KEY` = 8000 so the
   engine uses its 8 kHz telephony path), hand the `CallLink` to the accept queue (the slot is reserved when the INVITE arrives and `permit.send` fills it)
   (a closed accept side -> `BYE`, counted). Call state ends on BYE, CANCEL, ACK timeout (64*T1 = 32 s for a 200 without
   ACK per RFC 3261, UNVERIFIED here: not re-read), RTP silence timeout, or `CallEnds::ended`.
3. **One RTP session per call (`rtp_session` task).** One `tokio::net::UdpSocket` per call from the
   configured even-port range (RTCP on port+1 or rtcp-mux; RTCP can start as receive-only SR/RR
   keepalive). Receive: parse (`rtp-types` MIT OR Apache-2.0, or hand-rolled 12-byte header), drop
   unknown SSRC/payload types, reorder with a small bounded buffer (suggest 2-3 packets, 40-60 ms;
   fixed first, adaptive later), conceal a gap with `AudioFrame` silence or an interpolated frame
   (policy TBD with the engine's PLC), then G.711 -> PCM16 with `ulaw_decode`/`alaw_decode`
   (selected by the negotiated payload type 0 or 8, RFC 3551) into 160-sample 8 kHz frames ->
   `WireEvent::Audio` (the engine resamples). Send: `WireOut::Audio` PCM16 8 kHz ->
   `ulaw_encode`/`alaw_encode` -> RTP: sequence +1, timestamp +160 per 20 ms, random initial
   sequence/timestamp/SSRC, marker bit on the first packet after a gap; pacing comes from the existing
   `OutputPacer`, so the RTP task sends when `WireOut::Audio` arrives (no second timer). Preallocate
   packet buffers (hot-path rule); never block the runtime thread.
4. **DTMF.** Receive RFC 4733 `telephone-event` packets (payload: event, E bit, R bit, volume, duration;
   events 0-9 = digits, 10 = `*`, 11 = `#`, 12-15 = A-D per RFC 4733 Table 3). Emit one
   `WireEvent::Dtmf` per event, de-duplicated by RTP timestamp, on the first packet or on the end
   packet (policy choice); the end packet SHOULD be sent three times by the sender (RFC 4733), so dedupe. Also accept SIP INFO
   (`application/dtmf-relay`, UNVERIFIED format) as fallback. Send DTMF (outbound) as
   telephone-event packets. Telnyx says RFC 2833 is its default DTMF mode and typically payload type 101
   (support article, re-read 2026-10-08); Twilio: RFC 2833 support not stated on the Twilio pages fetched,
   UNVERIFIED; always read the payload type from the SDP, never hard-code 101.
5. **Mapping hang-up.** Remote BYE -> reply `200 OK`, `WireEvent::Hangup`, cancel the RTP task, free the
   port. Engine `WireOut::Hangup` -> `BYE` (confirmed dialog) or `CANCEL` (pending outbound; rsipstack's
   `InviteDialog::hangup()` delegates to `hangup_with_headers`, which picks BYE/CANCEL by state for either role: corrected in section 9) with a transaction timeout, then cancel `ended`. Remote CANCEL before
   the 200 -> `487`, never create a link.
6. **Barge-in / marks (no SIP equivalent).** There is no wire-level `clear` or mark echo in RTP. `WireOut::Clear`
   only drops what the adapter itself still holds (the RTP send queue); audio already sent to the network
   plays out at the far end (carrier jitter buffer, unknown size: UNVERIFIED, measure on a live call).
   Synthesize `WireEvent::MarkAck` when the packets ahead of a `WireOut::Mark` have been sent (the lead is
   then the network/jitter-buffer delay, which brisvox cannot observe), and report `WireEvent::Lead`
   from an estimate (RTCP RR round trip and jitter fields can refine it later). Keep the lead small by
   sending close to real time (the pacer already does).
7. **Outbound call API (new).** Not part of `Transport::accept`. Add `SipServer::dial(DialRequest)
   -> Result<CallLink, DialError>` where `DialRequest { to: SipUri, from: SipUri/caller id, auth: Credential
   or trunk, extra_headers, offer: codecs, ring_timeout, cancel: CancellationToken }`. It resolves the
   target (RFC 3263 SRV/NAPTR where the stack supports it; else FQDN/IP), sends INVITE with SDP, handles 401/407
   digest retries, 18x (optionally early media), 200 + ACK, and returns the link after ACK with
   `WireEvent::Started` (direction = outbound). Failures return the SIP status (`486`, `603`, `408`...);
   cancel before answer sends CANCEL. Honour the same call limit. Idempotency: the caller supplies a
   dial key so a retried request never creates two calls (engine rule for side-effect tools).
8. **Timeouts, cancellation, limits.** Every await has a timeout (INVITE ring timeout, ACK wait, BYE
   transaction, RTP silence e.g. 30-60 s without packets -> `Fault` then hang up; session-timer expiry
   if negotiated). Each call = child tasks of the server's `TaskTracker`, all cancellable via
   `CancellationToken`; a panic or error in one call must not affect others (isolate per-call task;
   signaling state per dialog). Bounded channels as in `call_pair()`.
9. **Session timers and keepalive.** rsipstack has `reinvite`/`update`; a session-timer layer (RFC 4028
   `Session-Expires`/`Min-SE`, refresher role) would be ours (rsipstack has none: confirmed in the 0.7.3
   source, section 9; brisvox does not offer `timer`, so it does not implement them).
   Required only if a target demands it (not found in the Twilio/Telnyx pages fetched). OPTIONS pings for
   NAT binding keepalive (Twilio: no more than one per 10-15 s).
10. **Testing.** (a) Unit: SDP offer/answer, RTP header/telephone-event codec, jitter buffer, with
    vectors (RFC 3551 payload types, RFC 4733 events). (b) Sim: scripted SIP peer over UDP loopback
    (second endpoint in the test) driving INVITE/ACK/BYE/CANCEL, 401 challenge, 503, 488, glare, and an
    RTP peer sending G.711 + telephone-event with loss/reorder. (c) Live (needs
    infrastructure): Asterisk (docker) and a Twilio or Telnyx trunk, `BRISVOX_LIVE_TESTS=1`.
    SIPp as test peer: UNVERIFIED (not researched), rvoip claims SIPp in its own gate.

## 5. Interop targets

### 5.1 Twilio Elastic SIP Trunking (docs fetched 2026-10-08)
- Twilio is a trunk provider; brisvox plays the customer SBC/PBX. Do not confuse with Media Streams
  (`twilio.md`): SIP trunking carries RTP, not a WebSocket.
- Origination (PSTN -> brisvox): configure an origination SIP URI (IP or FQDN; the user part is the dialed
  number; Twilio adds a `Diversion` header carrying the dialed Twilio number). UDP default; `;transport=tcp`
  or `;transport=tls` (TLS default port 5061, other port allowed); the `sips` scheme is NOT supported;
  `;edge=<location>` selects the egress edge (e.g. `edge=dublin`; missing/wrong value -> the edge where the PSTN call arrives); up to ten origination URIs with priority (0-65535, lowest
  first) and weight (1-65535).
- Termination (brisvox -> PSTN): termination URI like `{name}.pstn.twilio.com` (localized: `{name}.pstn.<location>.twilio.com`; locations: Ashburn,
  Umatilla, Dublin, Frankfurt, Singapore, Tokyo, Sao Paulo, Sydney); IP ACL and/or credential list (at least one required);
  numbers in E.164 with a leading `+` or Twilio returns `400 Bad Request`; caller ID must be a Twilio
  number or verified caller ID. If an INVITE gets no response, Twilio recommends moving to the next
  available IP after 4 seconds (applies to us as UAC).
- Behaviour requirements: do not register the trunk; send `Max-Forwards: 70`; OPTIONS pings optional, at
  most one per 10-15 s; NAT bindings kept open with OPTIONS over UDP or OPTIONS/CRLF keepalives over
  TCP/TLS (the OPTIONS/CRLF keepalive sentence was not re-confirmed in the fact-check: UNVERIFIED); symmetric RTP is considered less secure, Twilio recommends leaving it disabled (enable only to traverse a non-SIP-aware NAT) so Twilio sends
  media only to the address in the SDP (so our advertised SDP address must be correct, see NAT).
- Signaling: ports 5060 (UDP/TCP) and 5061 (TLS); gateway ranges (IP page): Virginia 54.172.60.0/30,
  Oregon 54.244.51.0/30, Ireland 54.171.127.192/30, Frankfurt 35.156.191.128/30, Tokyo 54.65.63.192/30,
  Singapore 54.169.127.128/30, Sydney 54.252.254.64/30, Sao Paulo 177.71.206.192/30. Twilio says not
  all host active gateways at a time (accept signaling from all listed IPs) and recommends using the configured FQDN, not the IPs.
- Media: UDP, to 168.86.128.0/18, destination ports 10,000-60,000; source IP and source port ANY
  (confirmed on the IP page 2026-10-08). Bandwidth planning: peak = simultaneous calls x 100 kbps (G.711 plus overhead).
- Codecs (codecs page): PCMU and PCMA by default (GA); G.729, Opus, AMR-NB are Limited Availability and "not
  supported end to end". Origination offer order: PCMU then PCMA (without LA codecs; with LA codecs enabled: Opus, PCMU, PCMA, G.729, AMR-NB). Termination: Twilio matches the first
  supported codec in our SDP offer, so offer PCMU first.
- Secure trunking: TLS 1.2+ (1.0/1.1 dropped); SRTP origination accepts only `AES_CM_128_HMAC_SHA1_80`,
  termination accepts `_80` and `_32`; MKI not supported; with secure trunking enabled unencrypted calls
  are rejected; trust DigiCert Global Root CA, G2, G3. Source note: these come from Twilio search-result
  summaries (the Twilio secure-trunking URL tried in the fact-check returned 404; the Secure Media page
  covers Programmable Voice SIP Domains and confirms `_80`/`_32`, MKI unsupported, unencrypted rejected, but not trunking, TLS versions or DigiCert):
  treat the trunking-specific parts as UNVERIFIED against a trunking page. => SRTP (SDES) would be needed for secure trunks;
  not in the first scope, rsiprtp and xphone have SDES, rsipstack/rustrtc side to be evaluated.
- Telephone-event on Twilio: not stated on the pages fetched. UNVERIFIED.

### 5.2 Telnyx SIP (docs fetched 2026-10-08)
- Signaling (sip.telnyx.com page): US `sip.telnyx.com` 192.76.120.10 and 64.16.250.10; Europe
  `sip.telnyx.eu` 185.246.41.140/.141; Australia `sip.telnyx.com.au` 103.115.244.145/.146; Canada
  `sip.telnyx.ca` 192.76.120.31 and 64.16.250.13; Middle East `sip.telnyx.me` 185.246.42.128/.129;
  Asia (beta) `sip.telnyx.asia` 103.115.244.158/.159. Ports UDP/TCP 5060, TLS 5061, WSS 7443. (A search
  result showed 192.76.120.106 for US and a different `/27` media subnet; the live `sip.telnyx.com` page
  is authoritative: re-check before writing firewall rules.)
- Media: UDP 16384-32768; subnets 36.255.198.128/25, 50.114.136.128/25, 50.114.144.0/21, 64.16.226.0/24
  to 64.16.230.0/24, 64.16.248.0/24, 64.16.249.0/24, 103.115.244.128/25, 103.115.247.0/24,
  185.246.41.128/25, 185.246.42.128/28. ptime 20 ms. Codecs: G.711U/A, G.729, G.722, Opus, AMR-WB
  (plus video H.264/VP8). Whether PCMU/PCMA is the default offer order: UNVERIFIED (the Telnyx audio-and-codecs support
  article shows an example inbound offer listing G.722, PCMU, PCMA, G.729, telephone-event but does not say what is
  enabled by default; the claim that G.722 is enabled by default for LiveKit connections is UNVERIFIED; restrict our SDP to PCMU/PCMA and check the connection's codec list).
- Auth: credentials (REGISTER-capable, dynamic IPs), IP, FQDN, IP+`X-Telnyx-Token`, IP+tech prefix, IP+
  `P-Charge-Info`; error text examples: RG1 (480) not registered on a credential connection, R47 (403)
  multiple Contact addresses in one REGISTER, B3 (488) SRTP mismatch (all confirmed 2026-10-08).
- TLS 1.2 and 1.3 (1.0/1.1 not accepted, sip.telnyx.com); SRTP preferred cipher on the B2BUA `AEAD_AES_256_GCM_8` (first in the default offer), also `AES_CM_128_HMAC_SHA1_80` (sip.telnyx.com);
  the claim that a second Telnyx page disagrees on the preferred cipher was not reproduced in the fact-check (UNVERIFIED which is current).
- DTMF: RFC 2833 (default, "recommended"), SIP INFO, inband, selectable per connection; typical RFC 2833
  payload type 101.
- Media IP: the earlier claim that Telnyx "auto-detects your media IP address by monitoring early RTP packets" and adjusts if the
  source differs from the SDP ("Disable RTP Auto Adjust" / "Accept any RTP packets" expert options) could NOT be confirmed
  on any Telnyx page in the fact-check (only a community thread mentions the portal toggles "RTP Auto Adjust" and "accept any RTP packets"): UNVERIFIED.
  STUN `stun.telnyx.com:3478` (UDP), TURN `turn.telnyx.com:3478` (UDP/TCP; credentials from Telnyx support).

### 5.3 Asterisk and FreeSWITCH
- Asterisk (PJSIP NAT doc, fetched 2026-10-08): `local_net`, `external_media_address` (public IP written
  into the SDP media address for peers outside `local_net`), `external_signaling_address`; endpoint
  options `direct_media=no`, `rtp_symmetric`, `force_rport`, `rewrite_contact` for phones behind NAT.
  Expect Asterisk to latch symmetric RTP: brisvox should send RTP from the same port it advertises and
  listens on (RFC 4961). rsiprtp's `basic_call` example runs against Asterisk; rvoip's release gate lists it.
- FreeSWITCH: NAT parameters (`ext-rtp-ip`, `ext-sip-ip`, etc.) NOT confirmed in the docs fetched
  (the SignalWire page fetched was only an index): UNVERIFIED. rvoip lists FreeSWITCH in its gate; xphone
  claims it. Treat as a second live-test target.

### 5.4 NAT and deployment considerations
- Brisvox is the server: give it a public IP (or a static NAT) and advertise it in SDP `c=`, `o=`, Contact
  and Via (rsipstack `accept_with_public_contact`; example `--external-ip`). Behind Kubernetes/Docker
  bridge NAT the advertised address must be configured, not auto-detected.
- RTP at scale: one UDP socket per call, so hundreds of calls = hundreds of open ports in a range that
  must be forwarded/published; prefer host networking or a wide published port range. Per-call sockets
  also mean `ulimit -n` sizing. (Design consideration; scale numbers untested.)
- Symmetric RTP (RFC 4961, BCP 131: use one UDP port pair for sending and receiving; required for NATs
  without ALG and for SBCs): send from the same socket we receive on. Latching onto the first source
  address (rustrtc `enable_latching` is such a feature) helps with NATed peers but weakens security;
  default: accept RTP only from the SDP address, optionally latch within a short window for trusted peers
  (implemented as `SipConfig::rtp_latch` + `rtp_latch_window`: only a packet that passed the payload-type,
  SSRC and sequence checks moves the destination, only within the window after the call is up or a re-INVITE,
  and only once).
- SIP over UDP behind NAT: rport (RFC 3581) so responses return to the sender's address and port; TCP/TLS
  avoid that class of problem (connection reuse) at the price of head-of-line blocking.
- Firewall: allow carrier signaling ranges on 5060/5061 and media ranges (Twilio 168.86.128.0/18 UDP
  10000-60000; Telnyx list above) to our RTP range. Prefer FQDN/SRV over hard-coded IPs.
- SIP ALGs on routers/firewalls corrupt SDP: disable (general knowledge: UNVERIFIED against a doc).
- Latency note for `docs/LATENCY.md`: SIP/RTP paths carry the carrier's RTP packetization (20 ms) and
  jitter buffer; the transport adds our reorder buffer depth (40-60 ms suggested) plus 20 ms packetization on
  both legs; measure on a live trunk.

## 6. Protocol facts used (RFCs, fetched 2026-10-08)
- RFC 3551: PCMU = static payload type 0, PCMA = 8, clock 8000 Hz, 1 channel; default packetization 20 ms
  (160 octets); RTP timestamp advances 160 per 20 ms (derived from the RFC's rules, not a verbatim sentence).
- RFC 4733 (obsoletes RFC 2833): media type `audio/telephone-event`, no static payload type (assigned
  dynamically in SDP, e.g. `a=rtpmap:100 telephone-event/8000`); default clock 8000 Hz; event codes:
  0-9 digits, 10 `*`, 11 `#`, 12-15 A-D; payload fields event (8 bit), E (end) bit, R bit, volume
  (0 to -63 dBm0), duration (16 bit, timestamp units); the final packet of an event SHOULD be sent three times (a SHOULD, not a MUST).
- RFC 3581: `rport` Via parameter asks the server to answer to the actual source IP and port.
- RFC 4961 / BCP 131: symmetric RTP and RTCP (see above).
- RFC 3261, 3264, 4028, 3262, 3515 etc.: not re-read; behaviour described from the stacks' own docs only.

## 7. Turkish support
Not applicable to the transport (language-agnostic G.711 audio). Telephony quality note as in
`twilio.md`: 8 kHz narrowband G.711 is typically harder for STT; wideband codecs (G.722 at Telnyx, Opus)
could help but both carriers' default/guaranteed codecs are PCMU/PCMA, and rsipstack-based media would
need G.722/Opus added by us. Turkish PSTN numbers/trunks (e.g. a local carrier) were not researched
(UNVERIFIED).

## 8. Gotchas (research-time; implementation gotchas get appended later)
- `rsiprtp` `ai_bridge.rs` looks like the AI-agent integration but is a simulation with no SIP/RTP; do not
  treat it as proof the stack runs a real call to an AI.
- GitHub `license` fields can differ from crates.io (rsiprtp: GitHub shows Apache-2.0 only, crates.io says
  MIT OR Apache-2.0; rsip: NOASSERTION). Run `cargo deny` on the chosen set before merging.
- `rsipstack` versions move daily at the moment: `Cargo.lock` plus exact pin, and re-read the changelog on every bump.
- Twilio does not accept `sips:`; use `sip:` with `;transport=tls`.
- Telnyx and Twilio media IPs/ports differ from our own advertised RTP range; do not confuse the two lists in firewall rules.
- Dialog deprecations in rsipstack (`ClientInviteDialog`, `ServerInviteDialog` -> `InviteDialog`): code
  against `InviteDialog`.

## 9. Implementation notes (read from rsipstack 0.7.3 source and measured with local tests)

Module `brisvox_transport::sip` (cargo feature `sip`, off by default). Stack: `rsipstack` 0.7.3 pinned with
`default-features = false, features = ["platform-tokio", "rustls"]` (`rustls` with the `ring` provider is enabled
for the TLS connection types; still no WebSocket or SRV lookup; MIT, LICENSE file
read in the crate; `cargo deny check` passes). Media half (SDP, RTP, reorder buffer, RFC 4733) is ours.

Resolved from the source (previously UNVERIFIED above):
- rsipstack has its own SIP parser now (no `rsip` dependency); the only extra dependencies with
  `platform-tokio` are tokio, tokio-util, parking_lot, socket2, if-addrs and rand 0.10.
- Server side: the endpoint hands every new server transaction to `incoming_transactions()`; an INVITE
  without To tag becomes a dialog with `DialogLayer::get_or_create_server_invite`, and
  `InviteDialog::handle(&mut tx)` must run on its own task for the INVITE transaction (it takes the ACK or the
  CANCEL). CANCEL does not appear as a separate transaction: the dialog turns it into `Terminated(UacCancel)`
  and answers 487. In-dialog requests need `DialogLayer::match_dialog` + `handle`.
- BYE, INFO, UPDATE, re-INVITE, OPTIONS, REFER, NOTIFY and MESSAGE inside a dialog reach the application as
  `DialogState` events carrying a `TransactionHandle`; the application must answer within 64*T1 (32 s) or the
  stack answers 501. Brisvox answers INFO (DTMF) and re-INVITE/UPDATE (SDP) and refuses the rest with 501.
- Client side: `DialogLayer::do_invite` sends the INVITE, answers a 401/407 once when a `Credential` is set
  (otherwise the challenge is returned as the final response), ACKs a 2xx itself and returns the dialog.
  Dropping the future abandons the call: CANCEL (also from the Calling state, once a provisional arrives), and a
  2xx that crosses the CANCEL is ACKed and ended with BYE. The dialog is never removed from the layer
  automatically: the call task calls `remove_dialog`.
- Session timers (RFC 4028) are not implemented by rsipstack; Brisvox does not offer `timer`, so peers do not
  require it. Deterministic simulation of SIP signaling is not possible: transaction timers run on tokio time.
- `TransportLayer::set_whitelist` drops datagrams and TCP connections from other source IPs before parsing.
- rsipstack logs whole SIP messages at `info` level (`udp received` / `udp sent`). They contain digest
  responses and caller data, never a password; run production with `rsipstack=warn`.
- A CRLF CRLF datagram (RFC 5626 keepalive) is answered with CRLF by the stack.
- On Windows the next receive after a send to a closed RTP port fails with a connection reset (ICMP). The call
  task ignores `ConnectionReset`/`ConnectionRefused` on its RTP socket.

Behaviour of the stack that the review of the SIP transport relied on (0.7.3 source):
- Dropping a server `Transaction` after its final answer hands it to the endpoint cache of finished
  transactions; the cache absorbs the ACK and answers a retransmitted INVITE with the same answer until its cleanup
  timer fires. A refusal therefore does not have to wait for the ACK (it used to, and held a handler slot for the
  Timer K wait).
- A request that cannot make a transaction key (no From tag, no Via) is dropped by the endpoint with a warning
  and no answer. A CANCEL without a matching transaction is answered 481 by the endpoint itself.
- `DialogStateReceiver` is an unbounded channel (`UnboundedReceiver`). Brisvox bounds it indirectly: each state is
  produced by a request that holds one of 512 handler slots until the call task answers it.
- The stack reads `Require` only for 100rel; Brisvox answers any INVITE with another or unknown required tag
  with 420 and an `Unsupported` header (RFC 3261 section 8.2.2.3).
- `TransportLayer::set_whitelist` applies to everything received, so it also drops the answers of outbound calls
  from hosts outside `allowed_sources`.
- Contact: a call set up over TCP gets `;transport=TCP` in our Contact (inbound and outbound), because the peer
  sends its BYE and re-INVITE to the Contact (RFC 3261 sections 12.1.1 and 8.1.1.8).
- Secrets: `DialRequest::headers` can carry an authentication token (`X-Telnyx-Token`). Its `Debug` shows header
  names only, but rsipstack logs whole INVITEs at `info`: the token appears in logs unless `rsipstack=warn`.
- A held call (the peer re-INVITEs with `a=sendonly`, or with `c=0.0.0.0`) may send no RTP at all; the RTP silence
  timeout is suspended while the call has no media address or the peer does not listen, and restarts on resume.
  A re-INVITE that moves, holds or resumes the media starts the receive stream over (new SSRC, new sequence
  numbers are heard at once).
- A re-INVITE without a body (delayed offer; session refresh, 3PCC, hold done by a controller) is answered 200 with
  our current SDP (`o=` version bumped); the peer's answer comes in the ACK. The call task remembers the CSeq of that
  INVITE and, when the dialog confirms with an ACK of that CSeq (`InviteDialog::last_remote_ack`), parses the body
  like an offer and applies it (new media address, hold/resume, receive restart, reopened latch window). An ACK
  without a body leaves the stream as it was; an unusable one is logged and ignored. An initial INVITE without an
  offer is still refused with 488.
- Bind address and `public_ip` (found by the review tests): rsipstack reports the address of a socket bound to
  `0.0.0.0` as the address of the first non-loopback interface (`SipConnection::resolve_bind_address`). Brisvox
  therefore always (re)creates its UDP and TCP sockets on the configured `bind_addr` with the chosen port and uses
  the reported address only for the port and the comparison with `public_ip`. Before this a server bound to
  `0.0.0.0` with a `public_ip` listened only on the first interface (not on loopback, not on a second NIC).
  `tests/sip.rs` runs a `0.0.0.0` bind advertised as `127.0.0.2` and checks `c=`, `o=`, Contact and a BYE sent to
  the Contact.
- Digest `qop`: for a challenge with `qop="auth"` rsipstack answers `qop="auth"` (quoted), `nc=00000001` and a
  `cnonce`; RFC 2617 writes the qop value as a bare token. The hash is verified in `tests/sip.rs`; whether a
  carrier accepts the quoted form is UNVERIFIED (only a real carrier shows it). The hash function itself is pinned
  by known answers computed with an independent MD5 implementation (`src/sip/auth.rs` unit test).
- Marks: the session cuts a mark on a clear (`MarkCleared`) and ignores its later echo, so the engine never sees
  `MarkPlayed` for cut audio whether the adapter drops or echoes a mark on `Clear`. The SIP adapter drops them.
  The core pacer releases audio in real time; a long single frame queues all its packets in the adapter at once,
  which is why the mark tests use one 1 s frame.
- RTP ports: only even ports are used (odd ones stay free for RTCP). `rtp_ports` must contain at least one even port
  (`validate` rejects `(20001, 20001)`); the rotation cursor wraps inside the even ports `first..=last` where
  `last` is `high` rounded down to even, so an odd `high` is never bound.
- Accept queue: an inbound INVITE reserves its accept-queue slot (`Sender::try_reserve_owned`) before anything
  is sent, and holds it until the call is handed over, so a burst larger than `accept_queue` gets 503 instead of
  200 followed by a BYE.
- Bot audio that is not 8 kHz is dropped and counted in `dropped_audio_frames`; a warning is logged once per call.
  No `Fault` event is sent to the engine (the earlier text claiming one was wrong).
- Barge-in: the call task always reads the engine channel (a clear must not wait behind a backlog); a frame that
  does not fit the 500-packet send queue is dropped and counted, which the core pacer (real-time release) makes a
  rare case. (Observed in the tests, not read from the pacer source:) the core pacer passes a long frame on whole but did not release the next frame until the first one had played
  out, so a long backlog in the send queue comes from one long frame.

Design decisions: one task per call owning RTP socket, reorder buffer, send queue and dialog events;
one re-armed timer for send slot / reorder deadline / RTP silence; packets sent one per 20 ms on an absolute
schedule (never a burst); `Clear` drops queued packets and marks; marks resolve when the packets before them were
sent; no `Lead` is reported. Outbound DTMF is `SipHandle::send_dtmf(call_id, ..)` because `TransportCommand`
has no DTMF command yet. Not done: RTCP, REFER, delayed-offer INVITE, early media, SRV. (SIPS and
SDES-SRTP: `docs/TELEPHONY.md`, "SIP over TLS and SRTP"; the Twilio secure-trunking facts were fetched
again on 2026-10-13 from https://www.twilio.com/docs/sip-trunking and match the notes above: TLS 1.2+, port 5061,
`_80` only on origination, `_80`/`_32` on termination, no MKI, `sips:` treated as `sip:` over TLS.)
UNVERIFIED still: INFO DTMF body formats against a real carrier, Twilio/Telnyx telephone-event behaviour, carrier
jitter-buffer size (barge-in tail), interop beyond the scripted peers of `tests/sip.rs`.

## UNVERIFIED (summary list)
1. rsipstack: MSRV; embassy target limits; ability to inject a fake transport or time for deterministic tests (resolved the other way in section 9: none, tests use UDP loopback and the real clock). Resolved by the implementation: session timers, RFC 4733, jitter buffer and SDP negotiation are not in rsipstack (Brisvox has its own media half); CANCEL before the answer and a 200 that is never acknowledged are covered by `tests/sip.rs`.
2. rsiprtp: exact RTP/transport API for driving real sockets; real-world interop beyond Asterisk; any `unsafe`. (`now` type resolved: `std::time::Instant`; CANCEL is covered by its integration tests.)
3. rvoip: sans-IO status, audio frame fields, compile weight, session timers, MSRV claims beyond README badge, `endpoint_local_call` example.
4. ezk: DTMF, G.711, jitter buffer, session timers, UDP/TCP transport list, MSRV, maturity statement (README fetch partial); ezk-rtc maturity beyond 0.0.1.
5. wavekat-sip: tokio dependency, whether it uses rsipstack, interop tested, NAT support.
6. xphone: runtime used by server mode, SDES cipher suite detail; "used in production alongside xbridge" is a self-claim.
7. `sip-core` (xmppjingle/sipr) capabilities.
8. Twilio: telephone-event (RFC 2833/4733) in SDP; OPTIONS/CRLF keepalive wording; trunking-specific secure-trunking facts (TLS versions, DigiCert roots, SRTP suites per direction: search-summary sourced); a numeric carrier jitter-buffer size; barge-in tail size.
9. Telnyx: default codec order and G.722 default; the "auto-detects media IP"/RTP Auto Adjust behaviour; whether a second page names a different preferred SRTP cipher; media subnet discrepancy between cached copies; exact signaling IP for US (`.10` on the fetched page vs `.106` in a search snippet).
10. FreeSWITCH NAT parameters; SIPp usage; SIP INFO DTMF content type; 64*T1 ACK timeout (RFC 3261 not re-read); SIP ALG behaviour.
11. Production readiness of RustPBX (rsipstack's flagship user); THE-PJPROJECT status.
12. Third-party scale numbers (calls per core, memory per call) for any stack: none fetched.

## Sources (all fetched 2026-10-08)
- https://crates.io/api/v1/crates/rsipstack , /ezk-sip-ua , /ezk-sip-core , /ezk-rtc , /rvoip , /rvoip-sip , /rsip , /rsiprtp , /xphone , /wavekat-sip , /rtp-engine , /rtp-types , /rustrtc , /msf-rtp , /sip-core , /pjproject-sys
- https://api.github.com/repos/restsend/rsipstack , /kbalt/ezk , /eisenzopf/rvoip , /0x4D44/rsiprtp , /x-phone/xphone-rust , /wavekat/wavekat-sip , /vasilakisfil/rsip , /xmppjingle/sipr , /restsend/rustrtc
- https://github.com/restsend/rsipstack , https://docs.rs/crate/rsipstack/0.7.3/source/README.md , https://docs.rs/rsipstack/latest/rsipstack/dialog/ (+ `client_dialog::ClientInviteDialog`, `server_dialog::ServerInviteDialog`, `invite_dialog::InviteDialog`) , https://github.com/restsend/rsipstack/tree/main/examples/client , https://raw.githubusercontent.com/restsend/rsipstack/main/examples/client/main.rs , https://raw.githubusercontent.com/restsend/rustpbx/main/Cargo.toml , https://github.com/restsend/rustrtc
- https://github.com/0x4D44/rsiprtp , https://docs.rs/crate/rsiprtp/latest , https://docs.rs/rsiprtp/latest/rsiprtp/ , https://docs.rs/rsiprtp/latest/rsiprtp/session/struct.CallManager.html , https://raw.githubusercontent.com/0x4D44/rsiprtp/main/crates/rsiprtp/Cargo.toml , .../examples/ai_bridge.rs , https://raw.githubusercontent.com/0x4D44/rsiprtp/main/tests.md , .../ARCHITECTURE.md
- https://github.com/eisenzopf/rvoip , https://raw.githubusercontent.com/eisenzopf/rvoip/main/README.md , https://docs.rs/rvoip-sip/latest/rvoip_sip/
- https://github.com/kbalt/ezk , https://docs.rs/ezk-sip-ua/latest/ezk_sip_ua/ , https://docs.rs/ezk-sip-core/latest/ezk_sip_core/ , https://docs.rs/crate/ezk-sip-core/latest/features , https://docs.rs/ezk-rtc/latest/ezk_rtc/
- https://github.com/x-phone/xphone-rust , https://github.com/wavekat/wavekat-sip , https://raw.githubusercontent.com/wavekat/wavekat-sip/main/README.md
- https://docs.pjsip.org/en/latest/overview/license_pjsip.html
- Twilio: https://www.twilio.com/docs/sip-trunking , https://www.twilio.com/docs/sip-trunking/ip-addresses , https://www.twilio.com/docs/sip-trunking/codecs , https://www.twilio.com/docs/voice/api/secure-media (+ web-search summaries of the Twilio secure-trunking docs)
- Telnyx: https://sip.telnyx.com/ , https://developers.telnyx.com/public/llms/calling/sip-trunking-full.txt , https://support.telnyx.com/en/articles/1130710-what-is-dtmf-and-how-to-configure-it-on-telnyx , https://support.telnyx.com/en/articles/3192298-audio-and-codecs (plus web-search summaries of the Telnyx docs index)
- Asterisk: https://docs.asterisk.org/Configuration/Channel-Drivers/SIP/Configuring-res_pjsip/Configuring-res_pjsip-to-work-through-NAT/
- RFCs: https://www.rfc-editor.org/rfc/rfc3551.html , /rfc4733.html , /rfc4961.html , /rfc3581.html
- Repo files read: `crates/brisvox-transport/src/call.rs`, `transport.rs`, `crates/brisvox-audio/src/codec.rs` (function names `ulaw_encode/decode`, `alaw_encode/decode`), `docs/ARCHITECTURE.md` (Transport).

Fact-checked 2026-10-08: about 130 claims checked, 13 corrected (plus 6 downgraded to UNVERIFIED):
- rsiprtp `now` is `std::time::Instant` (was UNVERIFIED); rsiprtp CANCEL is covered by its integration tests (was "not named"); the "differential-tested vs rsip / RFC 4475 / fuzz" claim is supported by `tests.md`, not by the README; rsiprtp `terminate_call` only acts on `Established` calls; rsiprtp ICE: TURN relay, trickle and ICE restart unsupported.
- rvoip WebSocket: plain SIP-over-WebSocket has "bounded evidence", only WSS is developer preview (was "WS developer preview").
- xphone: wording is "used in production alongside xbridge", not "powers xbridge"; Opus/G.729 are optional feature flags; interop list split into trunks vs PBXes.
- rsipstack: `hangup()` is BYE-only and a no-op on UAS dialogs; BYE/CANCEL selection is in `hangup_with_headers`; the dialog module index page shows no deprecation (struct pages do); crate first published 2025-01-10; embassy "UDP + TLS client only" is not what the README says (downgraded to UNVERIFIED).
- RFC 4733: the triple send of the end packet is a SHOULD.
- Twilio: media source IP/port ANY is confirmed on the IP page (was UNVERIFIED); the "3-4 IPs" phrasing is not on the page (it says next available IP after 4 s); `edge=` fallback behaviour added; secure-trunking details (TLS 1.2+, DigiCert, per-direction SRTP suites) rest on search summaries, since the fetched Secure Media page covers Programmable Voice, not trunking (downgraded); OPTIONS/CRLF keepalive over TCP/TLS not re-confirmed (downgraded).
- Telnyx: the "auto-detects media IP / Disable RTP Auto Adjust" quote could not be found on any Telnyx page (downgraded to UNVERIFIED); TLS is 1.2 and 1.3; G.722-default claim unconfirmed (support article only shows G.722 first in an example offer); the "two pages disagree on cipher" claim not reproduced.
- ezk README maturity statement: fetch was partial, so "none" is UNVERIFIED.
- Confirmed unchanged: all crates.io versions, dates, licenses and download counts; GitHub stars/forks/issues/push dates; Twilio and Telnyx IP ranges, ports, codecs; Asterisk NAT options; RFC 3551/3581/4961 facts; PJSIP license; brisvox `WireEvent`/`WireOut`/codec function names.
