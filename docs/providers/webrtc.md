# WebRTC stack for a server-side Rust voice agent — TRANSPORT

- Researched and fetched: 2026-10-08. Every number below was read from the cited page on that date.
  Anything not read from a page is marked UNVERIFIED.
- Purpose: choose the library for the WebRTC transport (WHIP-style HTTP signaling, browser demo,
  Opus encode/decode, one peer per call) behind the sans-IO `CallEnds` boundary of
  `crates/brisvox-transport` (docs/ARCHITECTURE.md "Transport").
- Auth: n/a for the libraries. WHIP uses HTTP Bearer tokens (RFC 9725 section 4.7).
- Endpoint(s) / regions: n/a (we host the signaling endpoint ourselves).
- Protocol (messages in/out, with field names): see section 5 (WHIP) and section 4 (Opus RTP).
- Keepalive / timeouts: ICE consent/keepalive is inside the library; see per-library notes.
- Finalize / flush / cancel semantics: WHIP DELETE on the session URL ends a session; see section 5.
- Timestamps / interim results / tool-call deltas: n/a.
- Concurrency / rate limits: n/a (CPU/socket bound; see performance notes).
- Turkish support: n/a (transport only; audio is language-neutral).
- Gotchas found during implementation (2026-10-08; details in section 10):
  - `Event::MediaAdded` is not emitted by the first `poll_output` after `accept_offer`; find the audio
    section in the offer text (`a=mid`) and ask `rtc.media(mid)`.
  - ICE-lite declares a silent browser gone only after 15 s without a STUN binding request
    (`RECENT_BINDING_REQUEST` in the `is` crate): hang-up by `DELETE` or DTLS close is immediate,
    a vanished browser takes about 15 s plus `disconnect_grace`.
  - `RtcConfig::build` panics on failure (`expect`); the DTLS certificate takes 20-36 ms to generate
    (rcgen): one certificate per server, setup on `spawn_blocking`.
  - With `rust-crypto`, `dimpl` still builds `aws-lc-rs` (through `rcgen`) for certificate
    generation; it compiled here without NASM.
  - `opus` 0.4.0 / `opusic-sys` 0.7.5 build libopus with cmake; it works on this machine because
    the `cmake` crate finds the Visual Studio copy, but the build fails when the target directory
    path is long (MSBuild 260-character limit inside `.../out/build/CMakeFiles/...`): keep
    `CARGO_TARGET_DIR` short on Windows.
  - Closing a socket with unread request data resets the connection: the signaling server sends
    FIN and drains briefly after every response, so a `413` reaches the client.

## Sources (all fetched 2026-10-08)
- crates.io API: webrtc, webrtc/0.21.0, rtc, str0m, str0m/0.24.1, dimpl, livekit, libwebrtc, webrtc-sys-build,
  datachannel, gstreamer-webrtc, rustrtc, opus, opus/0.4.0, opusic-sys, audiopus, audiopus_sys (+ /dependencies),
  rusty-opus, opus-rs, ruopus (`https://crates.io/api/v1/crates/<name>`).
- GitHub API repo metadata: webrtc-rs/webrtc, webrtc-rs/rtc, algesten/str0m, livekit/rust-sdks,
  lerouxrgd/datachannel-rs; commits lists of str0m and webrtc-rs/webrtc; issue webrtc-rs/webrtc#836.
- READMEs: https://raw.githubusercontent.com/webrtc-rs/webrtc/master/README.md,
  .../webrtc-rs/rtc/master/README.md, .../webrtc-rs/webrtc/master/CHANGELOG.md,
  .../algesten/str0m/main/README.md, .../algesten/str0m/main/CHANGELOG.md,
  .../algesten/str0m/main/examples/http-post.rs, .../livekit/rust-sdks/main/README.md,
  .../livekit/rust-sdks/main/webrtc-sys/build/src/lib.rs, .../paullouisageneau/libdatachannel/master/README.md and BUILDING.md,
  .../lerouxrgd/datachannel-rs/master/README.md, .../restsend/rustrtc/main/README.md,
  .../DoumanAsh/opusic-sys/master/README.md, .../SpaceManiac/opus-rs/master/README.md,
  https://github.com/remade-with-rust/rusty-opus.
- docs.rs: webrtc 0.21.0 (crate, PeerConnectionBuilder, PeerConnectionEventHandler, media_stream),
  rtc 0.21.0 (crate, SettingEngine, SettingEngineBuilder), rtc-interceptor, str0m 0.24.1 (crate, RtcConfig,
  MediaData, Writer, SdpOffer, SdpAnswer), libwebrtc, gst-plugin-webrtc, opus 0.4.0 (Decoder, Encoder).
- Specs: https://www.rfc-editor.org/rfc/rfc9725.html (WHIP), https://www.rfc-editor.org/rfc/rfc7587.html (Opus RTP),
  https://datatracker.ietf.org/doc/draft-ietf-wish-whep/ (WHEP), https://opus-codec.org/downloads/,
  https://opus-codec.org/license/, https://opus-codec.org/docs/opus_api-1.5/group__opus__decoder.html,
  https://developer.mozilla.org/en-US/docs/Web/API/MediaDevices/getUserMedia,
  https://gstreamer.freedesktop.org/documentation/webrtc/index.html,
  https://gstreamer.freedesktop.org/documentation/rswebrtc/index.html, https://aws.github.io/aws-lc-rs/requirements/index.html.

---

## 1. Candidates at a glance

| Option | Version (crates.io) | Last release | License | Repo activity (GitHub API) | Downloads (total / recent) |
|---|---|---|---|---|---|
| str0m | 0.24.1 | 2026-10-03 | MIT OR Apache-2.0 (crates.io; GitHub repo metadata shows "MIT") | pushed 2026-10-04, 634 stars, 36 open issues | 2,428,919 / 1,002,780 |
| webrtc (webrtc-rs, async layer) | 0.21.0 | 2026-09-19 | MIT/Apache-2.0 (crates.io; GitHub shows Apache-2.0) | pushed 2026-10-03, 5,158 stars, 7 open issues | 7,664,986 / 2,690,025 (history includes the old 0.x line; 0.21.0 itself 18,000) |
| rtc (webrtc-rs sans-IO core) | 0.21.0 | 2026-09-19 | MIT/Apache-2.0 | pushed 2026-10-04, 164 stars, 10 open issues | 948,062 / 921,281 |
| livekit + libwebrtc (Google libwebrtc via FFI) | livekit 0.9.4, libwebrtc 0.3.51 | 2026-10-07 | Apache-2.0 | pushed 2026-10-07, 495 stars, 157 open issues | libwebrtc 3,254,718 / 1,439,204 |
| datachannel (libdatachannel wrapper) | 0.16.1 | 2026-07-27 | MPL-2.0 | pushed 2026-07-27, 151 stars | 178,803 / 19,034 |
| gstreamer-webrtc + gst-plugin-webrtc (webrtcbin) | gstreamer-webrtc 0.25.4; gst-plugin-webrtc 0.15.4 | 2026-09-21; 2026-09-22 | MIT OR Apache-2.0 (bindings); MPL-2.0 (gst-plugin-webrtc) | n/a | gstreamer-webrtc 1,156,344 / 208,114 |
| rustrtc (restsend) | 0.4.2 | 2026-10-07 | MIT | crate created 2025-11-27 | 15,498 / 6,052 |

Pion via FFI is excluded by the project's requirements.

---

## 2. Per-library facts

### 2.1 str0m 0.24.1 (https://github.com/algesten/str0m)
- Architecture: pure sans-IO. "A Sans I/O WebRTC implementation in Rust"; `Rtc` has no internal threads or async tasks and
  never calls `Instant::now()` itself (time is passed in). Drive loop: after any mutation (`handle_input`, `Writer::write`,
  `Channel::write`, SDP apply, add candidate) drain `poll_output` until `Output::Timeout`. Outputs are `Output::Transmit`,
  `Output::Event`, `Output::Timeout(Instant)`. Fits the repo's injected-`Clock` rule and deterministic simulation.
  Not a W3C `RTCPeerConnection` API (deliberate). Tokio is not involved: we own the UDP socket and loop.
- Panic policy: "should never panic on any user input"; panics mean bugs. README suggests `catch_unwind` as an extra layer
  (relevant for the "one session's panic must not affect others" invariant).
- ICE: full ICE agent; application gathers candidates (`Candidate::host(addr, "udp")` + `rtc.add_local_candidate`),
  `add_remote_candidate`. ICE-lite: `RtcConfig::set_ice_lite(bool)` ("Ice lite is a mode for WebRTC servers with public IP address";
  default false per the `ice_lite()` getter; an `Rtc` in ice lite mode "will not make STUN binding requests, but only answer to requests
  from the remote peer"). ICE-lite is not mentioned in the README/docs.rs feature table (it is documented only on `RtcConfig`). TURN: README/docs.rs
  feature table says TURN "No"; docs.rs: "We consider TURN similar to enumerating local network interfaces – it's a way of obtaining sockets"
  (application's job); NIC enumeration also "No". IPv4/IPv6/UDP/TCP claimed in FAQ.
  Changelog 0.24.0: `Rtc::accepts` recognises validated remote addresses in ICE-lite mode before nomination.
- DTLS-SRTP: DTLS via the `dimpl` crate (sans-IO DTLS 1.2/1.3, MIT OR Apache-2.0, 0.7.5 released 2026-10-03, 1,452,331 downloads).
  Crypto backends (feature flags): `aws-lc-rs` (default), `rust-crypto`, `openssl` (DTLS 1.2 only), `openssl-dimpl`,
  `apple-crypto`, `wincrypto` / `wincrypto-dimpl` (Windows CNG). Priority when several are on: aws-lc-rs, rust-crypto,
  openssl-dimpl, openssl, apple-crypto, wincrypto. Per-instance: `Rtc::builder().set_crypto_provider(...)`; process-wide:
  `install_process_default()` (panics if called twice); `RtcConfig::set_crypto_provider(Arc<CryptoProvider>)` overrides the process default.
  Changelog 0.24.0 removed SChannel: Windows DTLS now uses only dimpl with CNG.
- Opus: `RtcConfig::enable_opus(enabled, use_red)` (default enabled, no RED). The library payloads/depayloads Opus frames but does NOT
  encode or decode (README: capture/encode/decode/render are left to the application). Frame API: receive `Event::MediaData`,
  send `Writer::write(pt, wallclock: Instant, rtp_time: MediaTime, data)`. Audio uses a 48 kHz media clock (README: "48_000Hz for audio").
  Per docs.rs `Writer::write`: media written before `IceConnectionState` is `Connected` is dropped. `MediaData` fields: `mid, pt, rid, params, time (MediaTime numerator=RTP ts,
  denominator=clock rate), network_time (Instant), seq_range, contiguous, data (Arc<[u8]>, one depayloaded frame),
  ext_vals, codec_extra, last_sender_info, audio_start_of_talk_spurt`. `contiguous=false` means loss ("usually irrelevant for audio");
  for Opus PLC we would detect loss via `seq_range`/`contiguous` and call the decoder with an empty packet.
  Writer options: `audio_level(i8, voice_activity)`, `start_of_talkspurt(bool)` (marker bit; for DTX), `telephone_event`.
  0.24.0 added RFC 2198 RED for all audio codecs (breaking), opt-in telephone-event (DTMF) SDP negotiation, audio reordering default 1 s (video 2 s).
- Jitter buffer: feature table: "Adaptive Jitter Buffer: No", "Fixed Depacketize Buffer: Yes". Reordering knobs:
  `set_reordering_size_audio` (default 15 packets per the getter docs; ignored in RTP mode), `set_reordering_timeout_audio` (default 1 s; `None` disables; frame mode only).
  There is no adaptive playout; playout/wallclock estimation is the application's job. Consequence: if inbound network jitter
  matters for STT, we need our own small de-jitter/PLC stage (the first version can start with the reorder buffer only; UNVERIFIED how
  well the 1 s default behaves under jitter, to be measured).
- RTCP/feedback: SR/RR, Transport-Wide CC, bandwidth estimation, NACK, simulcast: "Yes". Stats events via `set_stats_interval` (default None = off).
- Data channels: Yes (SCTP).
- WHIP/WHEP: not built in and not named in the README (the word WHIP appears only inside "BitWHIP"). The `http-post` example (browser POSTs a JSON `SdpOffer`
  deserialized with serde_json; the answer is returned in the HTTP response; host candidates are added before `accept_offer` so there is no trickle; one
  thread and one UDP socket per connection) is a direct starting point. Example uses JSON, not `application/sdp`; we adapt using
  `SdpOffer::from_sdp_string(&str) -> Result<SdpOffer, SdpError>` and `SdpAnswer::to_sdp_string(&self) -> String` (both verified on docs.rs 0.24.1).
  Example bug to avoid: it calls `process::exit(1)` on a connection error.
  Example note: Firefox rejects 127.0.0.1 candidates, so use a non-loopback host address.
- Production users: README: "Str0m was originally developed by Martin Algesten of Lookback" and "We use str0m for a specific use case: str0m as a server SFU
  (as opposed to peer-2-peer)" (the README does not explicitly say Lookback runs it in production as an SFU). BitWHIP (CLI WebRTC agent in Rust) is cited as a real-world example.
  No other users named. README says peer-to-peer paths have received less testing than SFU use; our use (browser <-> server peer)
  resembles the SFU ingest path.
- Performance: README says "very good", with optimization ongoing. No numbers published on the page (UNVERIFIED; measure).
  Sans-IO plus one socket per process (or shared UDP port demux) is the cheap design. Changelog 0.24.1: pacer burst limited to ~63 kB.
- Maintenance: 8 releases between 2026-05-04 and 2026-10-03 (0.19.0 ... 0.24.1); commits on 2026-10-04 (NACK timer fix "Stop NACK timer wakeups for paused receive streams (#1064)",
  RTCP receiver report loss-fraction wraparound fix (#1065); both listed under "Unreleased" in the changelog, i.e. not in 0.24.1). Breaking changes land in minor versions
  (0.21 dimpl 0.7.0 upgrade, 0.24 RED), so pin exact versions.
- Build: MSRV 1.85.0 (crates.io; our toolchain is 1.99). README lists x86_64/aarch64 Windows MSVC, Linux GNU, macOS as compiled and tested.
  Default `aws-lc-rs` needs a C/C++ compiler (CMake never required for non-FIPS per aws-lc-rs requirements page; NASM/Windows
  specifics UNVERIFIED). `wincrypto` avoids C toolchains on Windows; `rust-crypto` is pure Rust. crate_size 6.08 MB.

### 2.2 webrtc 0.21.0 (webrtc-rs) and rtc 0.21.0
- History/status: v0.21.0 (2026-09-19) is "the recommended choice today" and still pre-1.0 (minor versions may break). Issue #836
  ("The path to webrtc 1.0") closed as completed 2026-08-22 with all rows marked done, including "C3 Jitter buffering".
  Releases: 0.20.4 (2026-08-30), 0.20.5 (2026-09-04), 0.21.0-alpha.2 (2026-08-16) ... 0.21.0 (2026-09-19): fast churn, rewrite is about 7 weeks old.
  A web search snippet (not a primary source) said v0.17.x was the last feature release of the old Tokio-coupled design (UNVERIFIED; not in the CHANGELOG).
- Architecture: `webrtc` is "a thin async layer over `rtc`", the sans-IO core ("Sans-I/O protocol core with complete WebRTC stack
  (95%+ W3C API compliance)", wording from the webrtc README; the rtc README itself gives no percentage). `webrtc` has `PeerConnection` (async handle), `PeerConnectionDriver` (background loop owning sockets,
  timeouts, event dispatch) and a `Runtime` trait. Default `runtime-tokio`; also `runtime-smol`, `runtime-mock` (virtual clock).
  Core `rtc` exposes `sansio::Protocol` (`handle_read`, `poll_write`, `poll_read`, `poll_event`, `poll_timeout`/`handle_timeout`,
  `handle_write`, `close`); no ambient clock: methods take `Instant`. 0.21: "The core no longer reads a clock. Time comes from
  Runtime::now()". Using `rtc` directly would give the same determinism as str0m with a W3C-shaped API; using `webrtc`
  puts a driver task per peer connection inside tokio (interaction with our "no blocking on worker threads" rule is UNVERIFIED).
- API: `PeerConnectionBuilder::new().with_configuration(..).with_media_engine(..).with_setting_engine(..).with_interceptor_registry(..)
  .with_handler(..).with_runtime(..).with_udp_addrs(vec![..]).build().await`. Handler trait `PeerConnectionEventHandler` has
  `on_track(Arc<dyn TrackRemote>)`, `on_ice_candidate`, `on_connection_state_change`, `on_ice_connection_state_change`,
  `on_ice_gathering_state_change`, `on_data_channel`, `on_negotiation_needed`, ... (all provided no-op async methods).
  Receive: `track.poll().await` yields `TrackRemoteEvent::OnRtpPacket(packet)`. Send: `TrackLocalStaticSample` (docs.rs media_stream index: for raw media samples such as
  VP8, H.264 or Opus frames, the library packetizes and sequences them; the struct-level doc text on docs.rs is a copy of the RTP variant's, so no verbatim quote) or `TrackLocalStaticRTP`
  (pre-packetized RTP). Opus registration in the `reflect` example (`examples/reflect/reflect.rs`):
  `MIME_TYPE_OPUS`, clock rate 48000, 2 channels, payload type 120; `register_default_interceptors(registry, &mut media_engine)`.
  The `reflect` example waits for ICE gathering to complete (no trickle) and exchanges one offer/answer.
- ICE/TURN: STUN/TURN with relay candidates, mDNS, trickle ICE, ICE restart, ICE over TCP (rtc README: "NAT traversal with STUN/TURN,
  plus mDNS candidates"; TURN RFC 8656 in spec compliance list). TURN client released allocations on close (commit 2026-09-27).
  ICE-lite: `rtc::SettingEngineBuilder::with_lite(bool)` exists ("only responds to connectivity checks but does not initiate them"); whether the `webrtc`
  crate forwards it via `with_setting_engine` is implied by the builder method but UNVERIFIED. ICE timeouts default
  disconnected 5 s / failed 25 s / keepalive 2 s (`with_ice_timeouts`). `with_nat_1to1_ips(ips, candidate_type)` advertises public IPs for 1:1 NAT (Host replaces the private IP and disables mDNS; Srflx adds a server-reflexive candidate).
- DTLS-SRTP/crypto: crypto lives in `rtc-crypto`; default `ring` (no C toolchain), optional `aws-lc-rs` (`crypto-aws-lc-rs` feature); provider selectable per connection
  (`SettingEngineBuilder::with_crypto_provider`). Cipher suites and SRTP profiles configurable.
- Interceptors (`rtc-interceptor` 0.21.0): `JitterBufferInterceptor` ("Holds each stream's packets for a fixed span of time, then releases them in order"; not adaptive),
  `NackGeneratorInterceptor`, `NackResponderInterceptor` (+RTX), `TwccSender/ReceiverInterceptor`, `SenderReportInterceptor`,
  `ReceiverReportInterceptor` (also congestion control, FlexFEC, pacer, PLI, RFC 8888). Whether `register_default_interceptors` enables the jitter buffer by default is UNVERIFIED.
- Data channels: yes (SCTP/DCEP, flow control back-pressure).
- WHIP/WHEP: none among the 39 examples (no whip/whep/audio-named example directory). We implement it ourselves.
- Production users: not stated in README; sponsors are Recall.ai (Gold), Stream and ChannelTalk (Silver), AdrianEddy (Bronze); sponsorship is
  not evidence of production use.
- Performance: README 0.21 reports per-packet crypto improvements (DTLS GCM record encrypt 1.015 us -> 262 ns; SRTP MAC keyed once per
  context, about 40% faster per RTCP packet). No end-to-end concurrency numbers (UNVERIFIED). A competing project's self-reported benchmark
  claims 3.3x throughput vs webrtc-rs (see rustrtc); treat as marketing.
- Maintenance: very active (commits 2026-09-26..2026-10-03 by multiple contributors), 7 open issues on webrtc, 10 on rtc.
- Build: MSRV not stated; `rtc` requires Rust edition 2024 (we have it). `ring` default needs no C toolchain.

### 2.3 LiveKit Rust SDK / libwebrtc (https://github.com/livekit/rust-sdks)
- `libwebrtc` crate (Apache-2.0) = "Livekit safe bindings to libwebrtc" (Google's C++ WebRTC): modules include `peer_connection`,
  `peer_connection_factory`, `audio_source`, `audio_stream`, `data_channel`, `rtp_*`. `livekit` crate = client SDK to connect to LiveKit rooms
  (needs a LiveKit server URL and token; README examples are server-based). Whether `libwebrtc` can be used standalone as a plain
  peer without a LiveKit server is not addressed on the pages read; the module list suggests yes (UNVERIFIED).
- Binary acquisition (webrtc-sys build code): downloads prebuilt zips from GitHub Releases,
  `https://github.com/livekit/rust-sdks/releases/download/{WEBRTC_TAG}/webrtc-{triple}.zip`, `WEBRTC_TAG = "webrtc-89d790b"`, triple
  e.g. `win-x64-release`; `LK_CUSTOM_WEBRTC` overrides. A network fetch at build time; supply-chain and offline-CI concern.
- README: Linux needs clang 21+ (libwebrtc artifact's bundled libc++); macOS needs `-ObjC`; rustflags from the repo `.cargo/config.toml` must be added to your cargo config; Tokio required.
  No Windows build section on the README.
- Pros (inherent to libwebrtc, UNVERIFIED on these pages): full audio stack (adaptive jitter buffer/NetEq, AEC) and mature interop.
  Cons: large C++ FFI blob, own threads (not sans-IO, not deterministic with the injected `Clock`), 157 open issues, build coupling.
- Conflicts with the project goal (hundreds of calls per container, deterministic sims, Rust-native); keep as a fallback reference only.

### 2.4 libdatachannel via `datachannel` crate
- libdatachannel: C++ with C API, MPL-2.0 (since 0.18), ICE per RFC 8445 with STUN (RFC 8489)/TURN (RFC 8656), SCTP data channels, optional media
  transport (SRTP via libsrtp, RTX, "Only SDP BUNDLE mode is supported"), WebSocket. Deps: OpenSSL/GnuTLS/MbedTLS, libjuice (or libnice),
  usrsctp, libsrtp, plog. README does not mention an Opus packetizer, ICE-lite, jitter buffer or WHIP.
- Rust wrapper `datachannel` 0.16.1 (MPL-2.0, 2026-07-27, 151 stars, 1 open issue): README shows only data-channel usage; a `media` Cargo
  feature exists ("Enables media support through libdatachannel") but the media API is undocumented there. Needs CMake + OpenSSL (`vendored` feature builds OpenSSL statically).
  Verdict: media/audio path UNVERIFIED and likely thin; C++ build chain; MPL-2.0 (weak copyleft, outside the preferred MIT/Apache/BSD set). Reject.

### 2.5 GStreamer webrtcbin (gstreamer-rs + gst-plugin-webrtc)
- webrtcbin implements much of the W3C PeerConnection API (offers/answers, data channels, `stun-server`, `turn-server`/`add-turn-server`,
  `ice-transport-policy`, `latency` property = jitter buffer, default 200 ms). Codecs are chosen by caps (Opus via other GStreamer
  elements). ICE-lite not mentioned on the page. License of the Bad Plug-ins webrtc plugin is not stated on the page read (UNVERIFIED).
- `gst-plugin-webrtc` 0.15.4 (2026-09-22, MPL-2.0): the docs.rs crate page lists only `webrtcsink`/`webrtcsrc`; the GStreamer rswebrtc plugin page
  (12 elements) additionally lists `whipclientsink`, `whipserversrc`, `whepclientsrc`, `whepserversink`, plus LiveKit/Janus/AWS KVS/Unreal signaller elements
  (no element literally named `whepsrc`). So a WHIP server exists in this ecosystem as a GStreamer element. The rswebrtc plugin license is not stated on that page.
- Cons: requires a system GStreamer install (Windows packaging pain, UNVERIFIED), GLib main loop and threads, not sans-IO, heavy for
  hundreds of calls per container. Reject for the engine; acceptable only for experiments.

### 2.6 rustrtc 0.4.2 (restsend; https://github.com/restsend/rustrtc)
- MIT, crate created 2025-11-27, 15,498 downloads, 0.3.146 (2026-10-05) -> 0.4.0 (2026-10-06) -> 0.4.2 (2026-10-07): nine releases 0.3.141..0.4.2 between 2026-09-29 and 2026-10-07. README: transport modes
  WebRtc, Srtp, Rtp (T.38 is a separate `t38` feature, not a `transport_mode` variant), no_std path (some parts still std-only); ICE with STUN, TURN (UDP and TCP), ICE Lite, ICE-TCP, mDNS; DTLS-SRTP; packetizers include
  Opus; jitter buffer in the media pipeline; NACK/RTX, PLI/FIR, TWCC, REMB; GCC; SCTP data channels. No WHIP coverage (WHEP appears only as "single-port UDP mux for SFU/WHEP deployments").
  Benchmarks are self-reported on one i7-9700T machine (3.3x throughput and ~52% less memory than webrtc-rs; 0.45 ms setup latency vs 25.6 ms).
  No production users named; single-vendor; very young. Feature-rich on paper but least proven. Watch list, not a pick.

---

## 3. Scored comparison

Scale 0-5 (5 best). Weights reflect Brisvox invariants (sans-IO/determinism, Windows+Linux build, correctness over features).
Scores are my judgement from the facts above, not measured; performance has NOT been benchmarked (UNVERIFIED for all).

| Criterion (weight) | str0m | webrtc 0.21 (async) | rtc 0.21 (direct) | libwebrtc (LiveKit) | rustrtc | libdatachannel | GStreamer |
|---|---|---|---|---|---|---|---|
| Maturity / maintenance (20) | 4 | 3 | 3 | 4 | 1 | 3 | 4 |
| Sans-IO / determinism / injected Clock (20) | 5 | 3 | 5 | 0 | 3 | 1 | 0 |
| Opus path (payloading, loss, DTX/RED) (15) | 4 | 4 | 3 | 5 | 3 | 1 | 4 |
| ICE incl. ICE-lite and TURN client (15) | 3 | 4 | 4 | 5 | 5 | 3 | 4 |
| Build on Windows + Linux, no network at build (10) | 4 | 5 | 5 | 2 | 4 | 2 | 1 |
| Performance / scalability headroom (10) | 4 | 3 | 4 | 3 | 3 | 3 | 2 |
| License fit (MIT/Apache/BSD) (5) | 5 | 5 | 5 | 5 | 5 | 2 | 3 |
| WHIP / signaling effort (5) | 3 | 3 | 3 | 2 | 2 | 2 | 4 |
| Weighted total (max 500) | 405 | 360 | 395 | 315 | 305 | 210 | 265 |

Weighted totals are sum(weight x score); e.g. str0m = 20*4 + 20*5 + 15*4 + 15*3 + 10*4 + 10*4 + 5*5 + 5*3 = 405.
(Fact-check 2026-10-08: the totals in the original draft were arithmetically wrong for 6 of 7 columns and were recomputed; the ranking of the top three is unchanged.)
Key facts behind the biggest gaps: str0m has no TURN client and no adaptive jitter buffer; webrtc-rs 0.21 has TURN and a jitter buffer interceptor but
is only about three weeks old in its rewritten form; direct `rtc` use has the best architecture but a thinner Opus/WHIP story (no audio example) and 164 stars.

## 4. Recommendation

Primary: **str0m 0.24.1** (exact-version pin, `MIT OR Apache-2.0`).
Reasons: (1) sans-IO with caller-supplied `Instant`, no threads, matching the repo's `Clock`, bounded-channel and cancellation invariants
and giving deterministic simulation; (2) ICE-lite fits a server with a reachable address; (3) frame-level Opus payloading, reordering knobs,
RED, DTX talkspurt marker; (4) active releases and a project developed by Lookback for a server-SFU use case (production use itself not stated); (5) crypto backend choice including
`rust-crypto` (pure Rust) or `wincrypto` so Windows builds do not need a C toolchain.
Accepted gaps and mitigations: no TURN client (acceptable for a server on a public/routable address; for NATed deployments rely on
`with_nat_1to1_ips`-style advertising, i.e. str0m `Candidate::host/srflx` with the public IP, or revisit); no adaptive jitter buffer (build a small
sequence-aware de-jitter + PLC stage in front of the decoder if measurements show need); WHIP is ours to write (~one POST handler).
Reconsider if: TURN-relayed server-side is a hard requirement (then evaluate `rtc` directly, with `webrtc` only if tokio-driver integration
passes the invariants), or str0m's pre-1.0 breaking minors cause churn.

Fallback: **rtc 0.21.0 (webrtc-rs sans-IO core)**; second fallback `webrtc` 0.21.0 once it has several months of stability.
Rejected: libdatachannel (MPL-2.0, C++ chain, media API unclear), GStreamer webrtcbin (system dependency, threads), libwebrtc via LiveKit (FFI blob,
build-time download, no determinism), rustrtc (too young; revisit later).

Recommendation: "WebRTC stack =
str0m (sans-IO, ICE-lite, Opus frames), Opus codec = libopus via `opus` crate; criteria and scoring in docs/providers/webrtc.md."

---

## 5. WHIP (RFC 9725) request/response shape (source: https://www.rfc-editor.org/rfc/rfc9725.html)
- Status: RFC 9725, "WebRTC-HTTP Ingestion Protocol (WHIP)", March 2025, Standards Track; updates RFC 8840 and RFC 8842; authors
  S. Garcia Murillo (Millicast), A. Gouaillard (CoSMo Software). (I did not compare with draft-ietf-wish-whip; not covered by the page.)
- Session setup (4.2): client `POST` to the endpoint, `Content-Type: application/sdp`, body = SDP offer. Success = `201 Created`, body = SDP answer
  (`Content-Type: application/sdp`), `Location` header = session URL. Wrong content type / malformed SDP = 4xx. Client SHOULD offer `sendonly`
  (MAY `sendrecv`), MUST NOT offer `inactive` or `recvonly`; the endpoint MUST answer `recvonly`.
  (For a bidirectional voice agent we need to send audio back: plain WHIP is ingest only. Options: (a) `sendrecv`
  offer, which the RFC permits for the client but whose answer direction the RFC says must be `recvonly`, so the agent's reply audio would need a
  WHEP-style second session or a documented Brisvox extension; (b) our own endpoint that answers `sendrecv`, i.e. "WHIP-style" as the project brief
  says. UNVERIFIED which browsers' `RTCPeerConnection` accept a sendrecv answer to a sendrecv offer: that is standard JSEP, so yes in practice, but
  not read from a page now. Decision: implement WHIP-shaped HTTP (POST/201/Location/DELETE/OPTIONS) with a documented `sendrecv` deviation.)
- `OPTIONS`: required for CORS; 200 response SHOULD include `Accept-Post: application/sdp`.
- `DELETE` on the session URL terminates the session.
- ICE (4.3): Trickle ICE and ICE restart RECOMMENDED, via `PATCH` with `application/trickle-ice-sdpfrag`; ETags required (428 if missing, 412 on mismatch);
  support for only one of the two returns `422`. The endpoint gathers all candidates before answering (4.3.2), so non-trickle is sufficient for a server.
  Adding candidates without restart returns `204 No Content`; restart returns `200 OK` with a new ETag.
- Constraints (4.4): BUNDLE with max-bundle and `rtcp-mux-only`; single MediaStream (one msid); no partially successful answers (reject the whole POST);
  DTLS setup: client SHOULD offer `actpass`; client MUST implement full ICE, endpoint SHOULD support full ICE but MAY support only ICE lite
  (ICE lite only when the endpoint is reachable at an address accessible to each authorized client, 4.4.5).
- Redirects (4.5): 307 preferred; 301/302 MUST NOT be used for POST; 503 + `Retry-After` under load (both optional, MAY).
- STUN/TURN (4.6): endpoint may return `Link: <url>; rel="ice-server"; username=..; credential=..` in the 201 response.
- Auth (4.7): HTTP auth must be supported; Bearer tokens (RFC 6750) MUST be supported by all WHIP entities (endpoints and sessions MAY require them, 4.7.1); token format out of scope.
- Extensions (4.9): `Link` headers in the 201 response with `rel` prefix `urn:ietf:params:whip:ext`.
- Codecs: none mandated. RFC examples use Opus PT 111 and VP8 PT 96. Status codes named in the RFC: 200, 201, 204, 301/302 (prohibited for POST), 307, 400 (4.4.x), 412, 422, 428, 503;
  405, 401 and 403 are not mentioned.
- WHEP (egress) is NOT an RFC: draft-ietf-wish-whep-04 (document date 2026-06-22; datatracker last updated 2026-10-06; expires 2026-12-24;
  WG states "Waiting for WG Chair Go-Ahead" and "Revised I-D Needed - Issue raised by WG", IESG "I-D Exists"). Same POST/201/Location/DELETE model; 406 with counter-offer allowed.

## 6. Opus on the wire (RFC 7587, https://www.rfc-editor.org/rfc/rfc7587.html)
- RTP clock rate is always 48000 Hz regardless of encode sample rate; SDP `a=rtpmap:<pt> opus/48000/2` (channels field always 2).
- fmtp (defaults): `maxplaybackrate`(48000), `maxaveragebitrate` (any positive integer; values outside 6000-510000 SHOULD be ignored; default depends on mode and maxplaybackrate), `stereo`(0), `sprop-stereo`(0), `useinbandfec`(0),
  `usedtx`(0), `cbr`(0), `ptime`(20), `maxptime`(120), `sprop-maxcapturerate`(48000). Mono voice: ask `stereo=0`, `useinbandfec=1` (UNVERIFIED that
  browsers/str0m emit/accept these without extra config; check in the demo).
- One Opus packet per RTP payload (may contain several frames up to 120 ms). Timestamp increments at 48 kHz: 10 ms = 480, 20 ms = 960, 40 ms = 1920, 60 ms = 2880.
- DTX: receivers detect it by sequence-number gaps; timestamps jump by multiples of 120; comfort noise via RFC 3389 is discouraged.
- Duplicates possible: pass only one copy to the decoder.

## 7. Opus codec options for Rust

| Crate | Version / date | License | Nature | Build requirements | Notes |
|---|---|---|---|---|---|
| `opus` | 0.4.0, 2026-08-23 (0.3.1 on 2026-01-03; 2,662,657 downloads, 1,415,281 recent) | MIT/Apache-2.0 | Safe bindings to libopus (repo SpaceManiac/opus-rs, 120 stars); depends on `opusic-sys ^0.7.3` (crates.io dependencies) | README: by default needs `cmake` and a C compiler | `Decoder::new(sample_rate: u32, Channels)`, `decode(&[u8], &mut [i16], fec: bool)`, `decode_float(.., &mut [f32], fec)`; PLC = pass empty slice `&[]`; `Encoder::new(sample_rate, Channels, Application)`, `encode`, `encode_float`, `encode_vec`, `set_bitrate(Bitrate)`, `set_vbr(bool)`. `set_inband_fec`/`set_packet_loss_perc` exist by name (signatures not shown on the page). The crate takes a plain `u32` rate; libopus API docs say the decoder rate must be one of 8000, 12000, 16000, 24000 or 48000 Hz (from opus-codec.org docs; the crate's own validation not read). |
| `opusic-sys` | 0.7.5, 2026-08-06 (1,562,959 downloads) | BSD-3-Clause | libopus FFI | `bundled` feature (default) builds bundled libopus 1.6.1, needs `cmake`; optional `ninja`; without `bundled` finds libopus on PATH / `OPUS_LIB_DIR`, `OPUS_LIB_STATIC=true` for static; `build-bindgen` needs `LIBCLANG_PATH` | No Windows-specific notes on the page. |
| `audiopus` / `audiopus_sys` | 0.3.0-rc.0 / 0.2.2, both 2021-04-22 | ISC | libopus bindings | `audiopus_sys` build-deps: cmake, pkg-config, log (+optional bindgen 0.58) | Stale (no release since 2021). Do not use. |
| `rusty-opus` | 1.0.0, 2026-10-05 (created 2026-07-29; 51,981 downloads) | BSD-3-Clause | Pure Rust encoder+decoder, SILK/CELT/Hybrid, DTX, FEC, PLC, SIMD | none (zero deps), MSRV 1.85 | Claims bit-exact on the 12 official RFC 6716/8251 decoder vectors; self-reported speed 1.50x libopus for CELT speech, ~4% slower for SILK 16 kHz VoIP; 5 stars, 157 commits; README lists "v1.0.0 gates 14/15". Very new: treat as experimental. |
| `opus-rs` | 0.1.37, 2026-10-07 (created 2026-02-28; 186,746 downloads) | BSD-3-Clause | Pure Rust port of libopus 1.6 (restsend, repo restsend/opus-rs, single owner) | none | README headline says "Production-ready" but its release notes show recent correctness fixes (SIGILL on AVX-without-FMA, FEC desync, fuzz-found panics in 0.1.35/0.1.36) and one known CELT gap at 5-6 byte packets; maturity beyond that UNVERIFIED. Hybrid is documented as supported. |
| `ruopus` | 0.1.2, 2026-07-01 (2 releases; 7,391 downloads) | MIT | Pure Rust decoder + encoder | none | Too new; single crates.io owner. |

libopus itself: latest release 1.6.1 (2026-01-14). Reference implementation is three-clause BSD; royalty-free patent grants from Xiph.Org/Broadcom,
a separate (still royalty-free) Microsoft license, and IPR disclosures by Qualcomm, Huawei, France Telecom and Ericsson with external counsel advising no license is needed (https://opus-codec.org/license/).

Recommendation: `opus` 0.4.0 (libopus 1.6.1 via opusic-sys `bundled`) for correctness and the reference codec; requires cmake + C compiler on the
build machine (Windows: MSVC build tools, UNVERIFIED that cmake generator picks it automatically; check in CI). Keep a trait (`OpusEncode`/`OpusDecode`)
so `rusty-opus`/`opus-rs` can replace it once conformance and PLC/FEC are verified by our own tests. Encoding/decoding is CPU work: per CONTRIBUTING.md
anything above ~50 us belongs on the `brisvox-inference` pool, not on tokio workers (a 20 ms Opus frame decode cost is not measured here; UNVERIFIED).

## 8. Minimal integration plan

Architecture (fits the transport boundary):
1. `crates/brisvox-transport/src/webrtc/` (new module or crate `brisvox-transport-webrtc`): a `WebRtcTransport` implementing `Transport::accept`.
   The adapter per call keeps `CallEnds`: pushes `WireEvent::Audio` (decoded caller PCM), `Hangup`; consumes `WireOut::Audio` (engine PCM),
   `Clear` (barge-in: flush encoder queue and stop sending), `Hangup`. The core already handles pacing (OutputPacer), marks and epochs; the
   adapter just emits frames when told. The sample rate goes through `CallInfo::metadata[CallInfo::SAMPLE_RATE_KEY]` (`CallRates { input, output }`).
2. Signaling endpoint (axum/hyper, whatever `brisvox-server` already uses): `POST /whip` (or `/webrtc/offer`) with `Content-Type: application/sdp`
   and optional `Authorization: Bearer`. Steps: validate size and content type; create an `Rtc` (str0m) with ICE-lite on when the server has
   a public/routable address; add local host (and configured public) candidates; `sdp_api().accept_offer(offer)` (offer parsed with `SdpOffer::from_sdp_string`, answer returned via `SdpAnswer::to_sdp_string`); return `201 Created`,
   `Content-Type: application/sdp`, `Location: /whip/<session-id>`, SDP answer body. `DELETE /whip/<session-id>` cancels the session token and
   returns 200/204; `OPTIONS` returns CORS headers and `Accept-Post: application/sdp`. `PATCH` can be 405/422 at first (non-trickle).
3. One peer per call: one `Rtc` + one task per call, child of the call's `CancellationToken`. Sockets: a single shared UDP socket
   (or small pool) with a demux map (remote `SocketAddr` or ICE ufrag -> call). `handle_input`/`poll_output` loop per call, timeouts via
   the injected `Clock` (never `Instant::now()` in logic, per CONTRIBUTING.md). `catch_unwind` around the per-call loop for isolation. Bounded channels
   between the socket reader and each call, with a documented drop-oldest overflow policy for inbound datagrams.
4. Media: negotiate Opus (48 kHz clock, 2 channels in SDP; send mono content). Inbound: `Event::MediaData.data` is one Opus frame ->
   decoder (`opus::Decoder`, 48 kHz or 16 kHz output; if 48 kHz, use the existing rubato pipeline to reach the engine's 16 kHz input; PLC when
   `seq_range`/`contiguous` shows a gap). Outbound: engine PCM (24 kHz TTS output resampled to the call's output rate, preferably 48 kHz) ->
   20 ms frames (960 samples at 48 kHz) -> `opus::Encoder` (voice/VoIP application, bitrate ~24-32 kbit/s, in-band FEC on: UNVERIFIED exact setters) ->
   `Writer::write(pt, wallclock, MediaTime(960*n, 48 kHz), data)` once ICE is `Connected`. Drop writes while not connected.
   Barge-in: on `WireOut::Clear` discard queued PCM/frames immediately; RTP timestamps keep advancing by elapsed time so the stream stays valid.
5. Hang-up: on `IceConnectionState::Disconnected` or DELETE, emit `WireEvent::Hangup`; the transport supervisor maps it to stop reasons.
6. Tests without a browser: str0m-to-str0m loopback in-process (both `Rtc`s driven by a virtual clock, which sans-IO makes possible); Opus
   encode->decode round trip; signaling handler tests with `tower::ServiceExt`; protocol fixtures for the WHIP shapes in section 5. A live browser test
   is `BRISVOX_LIVE_TESTS=1` / a pending follow-up.
7. Browser demo page (served by the same server at `/demo/webrtc` as static HTML; microphone needs a secure context: HTTPS, `localhost` or `file:///`; otherwise
   `navigator.mediaDevices` is undefined, source MDN getUserMedia page):
   ```js
   const stream = await navigator.mediaDevices.getUserMedia({
     audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true, channelCount: 1 }, video: false });
   const pc = new RTCPeerConnection({ iceServers: [] /* add stun/turn if needed */ });
   stream.getAudioTracks().forEach(t => pc.addTrack(t, stream));
   pc.ontrack = e => { audioEl.srcObject = e.streams[0]; audioEl.play(); };
   await pc.setLocalDescription(await pc.createOffer());
   await new Promise(r => pc.iceGatheringState === 'complete' ? r() : pc.addEventListener('icegatheringstatechange',
     () => pc.iceGatheringState === 'complete' && r()));          // non-trickle
   const res = await fetch('/whip', { method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp });
   const sessionUrl = res.headers.get('Location');                // 201 Created
   await pc.setRemoteDescription({ type: 'answer', sdp: await res.text() });
   // hang up: await fetch(sessionUrl, { method: 'DELETE' }); pc.close();
   ```
   (The JS is my sketch of the standard browser API usage, not copied from a fetched page; check in the demo. Only the secure-context rule is from MDN's
   getUserMedia page; the MDN page does NOT list the constraint names `echoCancellation`/`noiseSuppression`/`autoGainControl`/`channelCount`
   (they belong to MediaTrackConstraints; UNVERIFIED here). Browser host candidates may be mDNS `.local` names; the server learns the real address from the STUN
   binding request, not from the SDP: UNVERIFIED behaviour in str0m, test it.)
8. Rates: caller side 48 kHz Opus <-> engine 16 kHz input (libopus decoder API accepts 16000 Hz per the libopus docs, so the decoder can produce 16 kHz directly; the `opus` crate passes `u32` through, validation UNVERIFIED; otherwise 48 -> 16 kHz
   with rubato, see docs/providers/rubato.md) and TTS 24 kHz -> 48 kHz output. The adapter writes `CallRates { input: 16000-or-48000, output: 48000 }`
   into `CallInfo` metadata. Keep all resampling in the existing pipeline (`StreamResampler`), not in the adapter.
9. Config/limits: max concurrent peers, per-IP session cap, SDP size limit, Bearer token check, ICE timeouts; every network/IO await has a timeout.

## 9. UNVERIFIED list (carried into the implementation)
- Whether the `webrtc` 0.21 crate forwards ICE-lite (`with_lite`) through `PeerConnectionBuilder::with_setting_engine`, and whether `register_default_interceptors` enables the jitter buffer.
- str0m end-to-end performance (CPU per call, calls per core) and behaviour of the default 1 s audio reordering timeout under jitter; none benchmarked.
- (Resolved 2026-10-08: str0m SDP string API is `SdpOffer::from_sdp_string` / `SdpAnswer::to_sdp_string`.)
- str0m behaviour with browser mDNS host candidates and with ICE-lite when the browser is behind NAT (server learns peer-reflexive address).
- Windows build recipe for `aws-lc-rs` (NASM / prebuilt objects) and for `opusic-sys` bundled (cmake generator, MSVC); `wincrypto` and `rust-crypto` as C-free alternatives not yet built by us.
- `opus` crate: `set_inband_fec` / `set_packet_loss_perc` signatures, `Application` enum variants, and whether the crate itself restricts sample rates (docs page did not show them).
- Whether `libwebrtc` can run as a standalone peer without a LiveKit server; the GStreamer webrtc plugin licenses; `datachannel` crate media API and license beyond MPL-2.0 metadata.
- Real-world production users of str0m beyond what the README states (Lookback is its originating company, BitWHIP is an example), and of webrtc 0.21.
- Browser acceptance of `sendrecv` answers to a WHIP-style POST (standard JSEP, but not read from a page now).
- MDN/MediaTrackConstraints support for the demo's audio constraint names (not on the getUserMedia page).
- webrtc-rs "v0.17.x last feature release of the old design" (search snippet only).
- crates.io license/GitHub license metadata disagree for str0m (crates.io "MIT OR Apache-2.0"; GitHub API "MIT") and webrtc (crates.io "MIT/Apache-2.0"; GitHub "Apache-2.0"); read the LICENSE files in the repo before adding the dependency.

---
Fact-checked 2026-10-08: 120 claims checked, 17 corrected. Corrections:
1. Section 3 weighted totals were arithmetically wrong (originals 410/365/395/270/310/205/235); recomputed 405/360/395/315/305/210/265 (str0m example formula also 405, not 410).
2. rustrtc release window: "0.3.146 -> 0.4.2 within five days (2026-10-03..10-07)" was wrong; 0.3.146 is 2026-10-05, 0.4.0 2026-10-06, 0.4.2 2026-10-07.
3. Lookback: README only says str0m was originally developed by Lookback and that the project uses it "as a server SFU"; it does not explicitly say Lookback runs it in production. Reworded in 2.1 and section 4.
4. str0m NACK-timer and RTCP receiver-report fixes (commits 2026-10-04) are under "Unreleased" in the changelog, not in 0.24.1.
5. str0m ICE-lite: wording is "will not make STUN binding requests, but only answer"; ICE-lite is not in the README feature table (documented only on `RtcConfig`).
6. str0m TURN quote replaced with the verbatim docs.rs sentence.
7. gst-plugin-webrtc: whep elements do exist (`whepclientsrc`, `whepserversink`); the docs.rs page lists only webrtcsink/webrtcsrc, the GStreamer rswebrtc page lists 12 elements incl. whip/whep ones.
8. rustrtc: T.38 is a separate feature, not a transport_mode variant; WHEP is mentioned (UDP mux), WHIP is not.
9. WHEP draft: WG state also "Revised I-D Needed - Issue raised by WG".
10. MDN: secure context also includes `file:///`; the constraint names are NOT on that MDN page (previous text said they were).
11. libopus decoder sample rates (8000/12000/16000/24000/48000) now verified from opus-codec.org API docs (previously "from memory").
12. opus-rs: "Older releases lacked Hybrid" is not supported by the README (Hybrid documented as supported); replaced with README maturity facts.
13. webrtc `TrackLocalStaticSample`: quoted phrase not found verbatim (docs.rs struct doc is a copy of the RTP variant's); replaced with the media_stream index paraphrase.
14. RFC 9725: ICE lite is only permitted when the endpoint is reachable at an address accessible to each authorized client (4.4.5); 503/Retry-After are optional; Bearer: endpoints/sessions MAY require it.
15. RFC 9725 status-code list expanded (200, 201, 204, 301/302/307 named too; 401/403 not mentioned); previous "Error codes used" list was incomplete but not wrong for 4xx/5xx.
16. str0m `SdpOffer::from_sdp_string` / `SdpAnswer::to_sdp_string` verified to exist (previously UNVERIFIED); DTMF in 0.24.0 is opt-in negotiation; RFC 7587 `maxaveragebitrate` range is "SHOULD ignore outside" rather than a default.
17. libdatachannel STUN is RFC 8489 (TURN RFC 8656); livekit README says to add the config.toml rustflags to your cargo config; Microsoft patent license and IPR-disclosure companies (Qualcomm, Huawei, France Telecom, Ericsson) specified.
Remaining UNVERIFIED: see section 9 (webrtc ICE-lite forwarding and default jitter buffer, str0m performance and jitter behaviour, mDNS handling, Windows build recipes, opus crate setter signatures, libwebrtc standalone use, plugin licenses, production users, browser sendrecv acceptance, MediaTrackConstraints names, webrtc 0.17.x snippet, license-metadata mismatches).

---

## 10. Implementation notes (2026-10-08)

Code: `crates/brisvox-transport/src/webrtc/` (cargo feature `webrtc`, off by default), tests in
`crates/brisvox-transport/tests/webrtc.rs` and the unit tests of the module. Design: docs/ARCHITECTURE.md
"WebRTC transport"; the library decision is recorded in section 4.

### 10.1 Facts read from the sources (not from memory)
- str0m 0.24.1 source (`~/.cargo/registry/src/.../str0m-0.24.1`): the crate ships only `LICENSE-MIT.txt`
  (MIT, Copyright 2022 Martin Algesten) while `Cargo.toml` says `MIT OR Apache-2.0`. Treat it as MIT, which
  `deny.toml` allows (`cargo deny check`: advisories, bans, licenses, sources ok).
- Default features are `aws-lc-rs` and `examples` (the examples pull `rouille`): use
  `default-features = false, features = ["rust-crypto"]`. `str0m::crypto::from_feature_flags()` picks the
  backend and panics if none is enabled; `Rtc::builder().set_crypto_provider(Arc<..>)` and
  `.set_dtls_cert(..)` avoid the process-wide default and a certificate per call.
- API used: `Rtc::builder().clear_codecs().enable_opus(true, false).set_ice_lite(..)
  .set_reordering_size_audio(..).set_reordering_timeout_audio(..).build(Instant)`,
  `rtc.add_local_candidate(Candidate::host(addr, "udp")?)`, `rtc.sdp_api().accept_offer(SdpOffer)`,
  `rtc.accepts(&Input)`, `handle_input(Input::Receive|Timeout)`, `poll_output()`,
  `rtc.writer(mid).start_of_talkspurt(bool).write(pt, wallclock, MediaTime, payload)`, `Event::Connected`,
  `Event::MediaData`, `Event::Closed`, `Event::IceConnectionStateChange`, `rtc.close()`.
  `RtcConfig::build` calls `expect` on its own errors; everything else returns `Result`.
- `Event::Connected` fires when ICE and DTLS are up; `Event::MediaAdded` comes from `poll_output` only after
  that (it is not available right after `accept_offer`), so the audio `mid` is read from the offer text.
- `MediaData` timestamps are 64-bit `MediaTime` (numerator = RTP ticks at 48 kHz for Opus); `contiguous` is
  false after a sequence gap.
- Candidate lines whose address is not an IP literal (browser mDNS names) are removed from the offer before
  parsing: the server does not need the browser's candidates (it learns the address from the STUN request).
  Not tested against a real browser (UNVERIFIED), tested with a crafted offer line.
- ICE-lite: after nomination the remote is considered gone 15 s after its last binding request
  (`is-0.11.1/src/pair.rs`, `RECENT_BINDING_REQUEST`). Chrome sends consent checks about every 2.5 s
  (UNVERIFIED here, from memory), so a live browser is never close to that.
- Windows: `UdpSocket::recv_from` can fail with `ConnectionReset` (WSAECONNRESET) after a send to a closed
  port; the UDP reader ignores that error kind.

### 10.2 Opus: libopus works here, pure Rust was not needed
Order of preference: (1) `opus` 0.4.0 if it builds without installing cmake; it does. The
`cmake` crate (0.1.58) located Visual Studio 2022's bundled cmake 3.31.6 although `cmake` is not on `PATH`, and
`opusic-sys` 0.7.5 built libopus 1.6.1 (BSD-3-Clause) with MSVC. Caveat: MSBuild fails when the build path
exceeds 260 characters (it did in a deep scratch directory; the worktree's own `target/` is fine). On Linux
the build needs `cmake` and a C compiler, which is why the feature is off by default. The pure-Rust crates
(rusty-opus, opus-rs, ruopus) were not evaluated: they stay candidates behind `OpusFactory` (section 7).

Measured here (release build, 20 ms mono frame at 48 kHz, VoIP, `Signal::Voice`, 24 kbit/s, tone input):

| setting | encode per frame | decode per frame |
|---|---|---|
| complexity 9, fullband | 376 us | 44 us |
| complexity 5, superwideband (the default) | 185 us | 44 us |
| complexity 3, superwideband | 148 us | 41 us |
| complexity 5, wideband | 135 us | 26 us |
| complexity 2, wideband | 93 us | 25 us |

All of it is above the 50 us budget for tokio workers (CONTRIBUTING.md), so the call driver runs codec jobs on the
inference pool when `WebRtcOptions::codec_pool` is set (`WebRtcOptions::with_pool`; tested: the pool completes
the jobs, none dropped); without a pool they run on the call's task, i.e. on a tokio worker (tests, simulator).
Decision (from the review): the inline default stays, because the simulator and the tests need it, but
`WebRtcServer::serve` (and so `bind` and `bind_with`) logs a warning when there is no pool and `max_calls` is above
`INLINE_CODEC_MAX_CALLS` (8): 100 talking calls cost roughly 100 x 50 frames/s x 0.2 ms = 1 core-second per second
on the workers. The per-call packet rate limit (section 10.6) bounds what one client can add to that. A speech-like
input encodes slower than the tone; real figures per call need a load test (UNVERIFIED).

Round trip (`webrtc::opus` tests, libopus 1.6.1, 24 kbit/s, in-band FEC on): 1 kHz tone SNR 25.2 dB; the
`en_two_utterances` fixture (linearly up-sampled to 48 kHz) SNR 8.8 dB (waveform SNR of a perceptual codec
on speech; not a quality score); codec delay 309 samples = 6.4 ms (the tone's reported lag of 21 samples is
309 modulo the 48-sample period). PLC returns a non-silent frame after a lost packet.

### 10.3 Signaling shape implemented
`POST <whip_path>` (`Content-Type: application/sdp`, optional `Authorization: Bearer`) answers `201 Created`,
`Content-Type: application/sdp`, `Location: <whip_path>/<32 hex digits>`; `DELETE <whip_path>/<id>` answers 200
(404 unknown); `OPTIONS` answers 200 with `Allow` and `Accept-Post: application/sdp` (plus CORS headers when
`cors_origin` is set); `PATCH` answers 405 (non-trickle only). Refusals: 400 (SDP does not parse, no usable
audio, anything but exactly one live audio media section, no `rtcp-mux`), 401 + `WWW-Authenticate: Bearer`, 413 (body above `max_sdp_bytes`),
415 (not `application/sdp`; RFC 9725 names only "4xx"), 429 (per-address limit), 503 + `Retry-After: 1` (call
limit), 408 (request too slow). Deviation from RFC 9725: the answer to a `sendrecv` offer is `sendrecv`
(RFC: `recvonly`), so the agent can speak; a `sendonly` offer is answered `recvonly` and the bot cannot speak.
The `Location` is relative. No ETag/`Link` headers (no trickle, no ICE servers in the answer).
RFC 9725 4.4 "no partially successful answers" is kept: an offer with anything besides one live audio section
(audio+video, an `m=application` data channel, a second audio section) is refused whole with 400, and `rtcp-mux`
is required (an offer without `a=rtcp-mux` gets 400). BUNDLE is not checked because one media section makes it moot;
`max-bundle` / `rtcp-mux-only` offer attributes are accepted but not required. Single MediaStream (one msid) follows
from the single section.

### 10.4 Marks, clear and the RTP timeline
- Browsers do not report playout: a mark is acknowledged `mark_delay` (default 80 ms) after the audio ahead of
  it (including the flushed resampler tail and the padded last 20 ms frame) was handed to RTP. An estimate;
  `Lead` is not reported. A data-channel echo from the demo page would make it exact (not done).
- `Clear` drops what the adapter holds (resampler, partial frame, encoder history, pending marks). Packets
  already sent are played by the browser; measured: nothing arrives at the browser 250 ms after the clear
  (loopback, 2 s of audio queued).
- Outgoing RTP timestamps are contiguous while audio keeps coming and jump to the wall-clock position with the
  marker bit after a pause above 60 ms. Tested end to end: the browser peer logs the RTP time and talkspurt bit
  of every packet (`the_outgoing_rtp_time_line_is_contiguous_jumps_after_a_pause_and_never_goes_back`: steps of 960
  ticks inside an utterance, a jump of about the pause (500 ms = 24 000 ticks) with the marker bit after it, a
  marker and no backward step after a clear) and `plan_packet` is unit tested.
- Measured on loopback (debug build, inline codec, Windows, 10 rounds of 5 frames after 300 ms of idle, taken at
  the decoded packet's arrival in the browser peer): engine `Audio` command to first decoded packet at the browser
  1.8-3.8 ms, median 2.6 ms, one round 33 ms (timer tick). The 44-46 ms recorded earlier was an upper bound from a
  polling test (10 ms poll sleeps rounded up to tokio's 15.6 ms Windows timer tick, 30 frames queued first), not
  the adapter's latency. The test `first_bot_audio_reaches_the_browser_fast_at_the_median` asserts a median below
  50 ms (about 20 times the measurement, room for a loaded runner, still far below the 100 ms reordering wait or an
  extra pacer stage); a hang guard of 400 ms remains in the audio test. Not a browser measurement.

### 10.5 Remaining UNVERIFIED after the first implementation
- Behaviour with real Chrome, Firefox and Safari (the demo page was written against the standard API and
  exercised only by a str0m peer): sendrecv answer acceptance, mDNS candidates, `getUserMedia` constraint
  names, ICE consent timing.
- Whether the 100 ms / 5 packet audio reordering limits suit real jitter (defaults were 1 s / 15 packets).
- CPU per call and calls per core under load (no load test yet).
- A vanished browser (no DTLS close, no DELETE) is noticed after 15.5 s (ICE-lite consent window plus the 500 ms
  `disconnect_grace` of the test config) and the engine sees `PeerDisconnected`. The test
  `a_vanished_browser_frees_its_slot_and_ends_the_engines_call` runs in every normal test run: it pauses tokio's
  clock once the call is up, so the server's timers (all on the injected clock, handed to str0m as `Instant`s) run
  in simulated time, and it asserts the 5-30 s window, the stop reason, the freed slot and the freed per-address
  count. A unit test covers the `disconnect_grace` and connect deadlines without waiting. str0m 0.24.1 has no
  setting for the ICE consent timeout.
- Verified on loopback with a str0m peer: an offer whose only candidate is an mDNS `.local` name, and one with no
  candidate at all, connect (the server learns the peer-reflexive address from the first STUN request), pass DTLS
  and carry audio both ways. Real browsers remain unverified.
- Routing: a STUN request's ufrag outranks a known source address (same-5-tuple reconnect). An off-path attacker
  who spoofs a victim's exact ip:port can send it to the attacker's own call until the victim's next connectivity
  check re-binds it; the attacker's call never gets the victim's media (no ICE password).
- TURN-relayed browsers cannot reach the server unless it has a routable address (`candidate_ip`).

### 10.6 Review fixes (verified findings)
- Inbound media rate limit. A client with valid SRTP keys could send tens of thousands of packets a second, each
  one a codec job and an audio frame for the engine, and starve its own timers. Now `WebRtcConfig::
  max_audio_packets_per_sec` (default 200, at least 50) is a token bucket per call (burst of one second, driven by
  the injected clock) applied to each `MediaData` before the decode job; the excess is dropped and counted in
  `WebRtcStats::audio_packets_rate_limited`. The default is four times the usual 50 packets/s and twice the 100/s
  of 10 ms packets, so network bursts (the reorder buffer releasing a few packets at once) never reach it. Timers:
  every loop round checks the due deadline before it waits, so datagrams cannot starve the mark acknowledgements,
  the connect deadline or the disconnect grace; additionally the loop yields to the runtime after 32 datagrams in a
  row. Tested with a browser peer writing 20 packets per 20 ms tick (1000/s): the engine gets at most burst plus
  rate frames, `audio_packets_rate_limited` counts the rest, and a mark sent during the flood is still acknowledged.
- Packet loss concealment is tested on the wire: the browser peer's network drops every 8th RTP datagram. The
  engine's caller audio keeps its length (one 20 ms frame per packet sent, concealed ones included), `frames_concealed`
  equals the number of lost packets that were followed by another one, and a sender that jumps its RTP clock over
  contiguous sequence numbers (DTX) is not concealed. `lost_frames` is a pure function with unit tests.
- Codec pool failure paths are tested: a panic inside a pool worker (poisoning the call's codec lock) drops that frame,
  the call survives with `call_panics == 0`, later decode and encode jobs work, and a neighbouring call on the same
  pool is unaffected; a pool with one busy worker and a queue of one makes jobs time out or be refused: the frames are
  dropped (`codec_dropped`, pool `timed_out`/`rejected`), the call stays up and recovers when the worker is free.
- The demo page hangs up (DELETE, once) on every exit: stop, a rejected answer, a failed connection, `pagehide`.


## 11. Reachability: ICE-TCP and TURN credentials (fetched 2026-10-13)

Why: the browser initiates, so a server with a public UDP port works from any NAT type, but fails when the
client network blocks UDP, when the host has no UDP ingress (a one-port PaaS such as Railway), and gives no relay to symmetric-NAT
clients. The server is ICE-lite; this section covers what the *browser* gets (relay servers), and the server's own TURN client is section 12.

### 11.1 Facts read now (sources fetched 2026-10-13)
- **RFC 9725 section 4.6** (https://www.rfc-editor.org/rfc/rfc9725.html): the endpoint MAY return STUN/TURN
  servers in the `201 Created` as `Link` headers, one URI per header, e.g.
  `Link: <turn:turn.example.net?transport=udp>; rel="ice-server"; username="user"; credential="myPassword"`.
  The target is an RFC 7064/7065 URI (`stun:`, `turn:`, `turns:`); `username` is the TURN user, `credential` a
  long-term password (RFC 8489 section 9.2). **`credential-type` is not defined** by the RFC, so the server does
  not send it. Returning the servers in `OPTIONS` is "NOT RECOMMENDED"; a CORS preflight SHOULD NOT carry them.
  Implemented: one `Link` per URL on the `201`; `Access-Control-Expose-Headers` lists `Link` so a page on
  another origin can read them.
- **coturn REST credentials** (https://raw.githubusercontent.com/coturn/coturn/master/README.turnserver):
  `--use-auth-secret` + `--static-auth-secret=<secret>`; username `<timestamp>:<userid>` (separator `-C`,
  default `:`), password `base64(hmac-sha1(secret, username))`. Only HMAC-SHA1 is documented for this scheme.
  `--tls-listening-port` defaults to 5349; `--cert` / `--pkey` are PEM files. Implemented with
  `ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY`; the timestamp is the unix expiry (wall clock of the injected
  `Clock`), the user id is the call's session id (so one credential maps to one call). Reference vector in
  the unit test `the_coturn_password_matches_a_reference_vector`.
- **Cloudflare Realtime TURN** (https://developers.cloudflare.com/realtime/turn/generate-credentials/):
  `POST https://rtc.live.cloudflare.com/v1/turn/keys/$TURN_KEY_ID/credentials/generate-ice-servers`,
  `Authorization: Bearer $TURN_KEY_API_TOKEN`, `Content-Type: application/json`, body `{"ttl": <seconds>}`;
  `201`, body `{"iceServers":[{"urls":["stun:stun.cloudflare.com:3478"]},{"urls":[turn:...:3478?transport=udp,
  turn:...:443?transport=udp, turn:...:3478?transport=tcp, turn:...:80?transport=tcp, turns:...:5349?transport=tcp,
  turns:...:443?transport=tcp],"username":...,"credential":...}]}`. The page warns that port 53 is blocked by
  browsers and can time out ICE gathering: the server drops any URL on port 53. No TTL limits are documented.
  The key (id + API token) is created in the Cloudflare dashboard (Realtime > TURN) and never leaves the server.
- **Twilio Network Traversal Service** (https://www.twilio.com/docs/stun-turn/api):
  `POST https://api.twilio.com/2010-04-01/Accounts/{AccountSid}/Tokens.json`, HTTP basic auth (account SID +
  auth token, or an API key SID + secret), form parameter `Ttl` (seconds; default and maximum 86400); response
  JSON has `ice_servers` (entries with `url`/`urls`, TURN ones with `username`/`credential`), plus `username`,
  `password`, `ttl`. TURN URLs: `turn:global.turn.twilio.com` on 3478 (udp, tcp) and 443 (tcp); no `turns:`.
  The parser accepts `urls` as a string or an array.
- **str0m ICE-TCP** (read in `is-0.11.1` and `str0m-proto-0.7.0` sources, no network): a passive TCP host
  candidate is `Candidate::builder().tcp().host(addr).tcptype(TcpType::Passive).build()`; `Protocol::Tcp` is
  documented "See RFC 4571 for framing" (2-byte big-endian length before every packet; str0m itself does no
  framing, the caller does). ICE-lite accepts host candidates of any protocol (`add_local_candidate` rejects
  only non-host ones). A STUN request is accepted only when `Receive.destination` equals a local candidate
  address of the same protocol; the sender becomes a peer-reflexive remote candidate. Type preference:
  host/UDP 126, host/other 90, so UDP is preferred whenever both work (checked in the answer SDP by test).
- **Fly.io** (https://docs.fly.io/networking/udp-and-tcp): the UDP side must bind the `fly-global-services`
  address (replies must come from it), a `[[services]]` block with `protocol = "udp"` per UDP port, external
  and internal port equal, **a dedicated IPv4 address** (no shared IPv4 or IPv6 for UDP), assume about 1300
  bytes of UDP payload (WireGuard + proxy overhead). Not covered by the page: anycast, regions.
- **Railway** (https://docs.railway.com/reference/tcp-proxy, /reference/public-networking, web search
  2026-10-13): public networking is HTTP/HTTPS plus a TCP proxy (`domain:port`, a Railway-chosen port); the docs
  do not mention UDP, private networking carries UDP only inside a project, and Railway staff/community posts
  (station.railway.com/feedback/allow-outbound-udp-traffic-0f74101c, /feedback/adding-inbound-udp-fad19847) say
  inbound UDP is not supported. TCP proxy idle timeouts are not documented. Treat Railway as HTTP + TCP only.

### 11.2 What was built
- `GET <whip_path>/ice` (same bearer check as `POST`; `Cache-Control: no-store`; CORS like `POST`): `{"iceServers":[...]}`
  in the browser's `RTCIceServer` shape, `[]` when nothing is configured or the provider is down. Rate limit
  per client: a burst of 5, then one per 2 s. coturn usernames are `<expiry>:<label>`, the label being the first
  16 hex digits of SHA-256 of the session token's `sid` (a random id when the request had no session token);
  Cloudflare/Twilio credentials are shared by every caller for the cache period.
- `webrtc::turn_creds`: coturn computed locally per request; Cloudflare/Twilio fetched with a 5 s deadline,
  cached 60 s (one entry, at most half the credential lifetime), a failure remembered for 10 s so an outage
  costs one slow request per backoff and never fails a call. Answers are validated before they reach a header
  or JSON (schemes, no space/quote/angle bracket/backslash/control, 8 servers, 16 URLs, port 53 dropped).
- `webrtc::tcp` (`BRISVOX_WEBRTC_TCP_ADDR`): see ARCHITECTURE.md "WebRTC transport" for routing and bounds.
- Pages (`demo.html`, `examples/web-assistant/web/index.html`) fetch `<whip>/ice` before `createOffer` and pass
  the list to `RTCPeerConnection`; an empty list or any failure (404 from an older server) keeps the old behaviour.

### 11.3 UNVERIFIED (to check with real browsers and a live TURN relay)
- A real Chrome/Firefox/Safari connecting to the passive TCP candidate of an ICE-lite answer: only a str0m
  client (TCP-active candidate) was tested. Chrome's handling of ICE-TCP candidates is from memory, not read now.
- A real relay path (browser -> Cloudflare/Twilio/coturn -> the server's UDP candidate): the credentials are
  tested against mocks of the REST APIs and the coturn formula against a reference vector, not a live TURN server.
- Whether a TURN service reaches the server: a TURN relay allocates UDP, so a relayed browser still needs the
  server's UDP port (or a server-side TURN client). Browsers cannot relay to a TCP candidate.
- Cloudflare's `generate-ice-servers` response shape for `iceServers` as an object (the single-credential
  endpoint does that, per memory): the parser accepts an array or an object.
- RFC 6544 (ICE-TCP) and RFC 4571 were not re-fetched; behaviour is taken from the str0m sources.

## 12. Server-side TURN client (fetched 2026-10-13)

Why: a host with HTTP(S) and no inbound UDP (a one-port PaaS such as Railway) cannot be reached by the ICE-lite server address, and a
*browser* TURN relay only forwards to the server's UDP candidate. The server therefore holds its own TURN allocation on a
managed service, reached over TCP/TLS, and advertises the relayed address (section 12.3).

### 12.1 Facts read now (sources fetched 2026-10-13)
- **RFC 8656** (https://www.rfc-editor.org/rfc/rfc8656.txt, sections cited by number): the client talks to the
  server over UDP, TCP or TLS-over-TCP and asks for a UDP allocation with `REQUESTED-TRANSPORT`; the default
  allocation lifetime is 600 s (10 minutes), a server SHOULD cap it at 3600 s, and a `Refresh` with `LIFETIME` 0
  deletes the allocation (section 7); a **permission** is installed by `CreatePermission` for an **IP address**
  (the port is ignored), lasts **300 s** unless refreshed (section 9), and a UDP datagram from a peer without one
  is **silently discarded** by the server (section 5); a **channel binding** lasts **10 minutes** unless refreshed
  (section 12) and `ChannelBind` also installs a permission; data goes as `Send`/`Data` indications or
  ChannelData (4-byte header: channel number 0x4000 to 0x7FFF and the length of the data). **Section 12.5:
  over TCP and TLS-over-TCP the ChannelData message MUST be padded to a multiple of four bytes**; the padding is
  not in the length field. Over UDP padding is optional.
- **RFC 7064 / 7065** (TURN URIs, from memory, not re-fetched): `turn:host[:port][?transport=udp|tcp]`, default
  port 3478; `turns:` is TLS over TCP, default port 5349. The parser in `relay.rs` follows this and uses only
  URLs that mean TCP (`transport=tcp`, or `turns:` without a transport).
- **Cloudflare Realtime TURN FAQ** (https://developers.cloudflare.com/realtime/turn/faq/): billed per GB of data the
  TURN server sends to a TURN client ($0.05/GB, 1000 GB free); client-to-server traffic is free; TURN over TLS is
  supported; **RFC 6062 (TCP allocations) is not implemented and `REQUESTED-TRANSPORT` is ignored** (so the
  allocation is UDP, which is what the server asks for); no maximum allocation duration but the client must
  keep refreshing with a valid credential, credentials may expire up to 48 h ahead; billing stops when the
  credential expires. The page does not say whether non-browser clients may use the credentials (UNVERIFIED,
  see 12.4).
- **turn-client-proto 0.7.3 / turn-server-proto 0.7.3 / stun-proto 2.0.3** (crates.io, published 2026-10-06 /
  2026-10-03, license MIT OR Apache-2.0, repository https://github.com/ystreet/turn-proto; sources read in
  `~/.cargo/registry`): sans-IO RFC 5766/6062/6156/8656 clients and servers, time passed in as
  `stun_proto::Instant` (`sans-io-time`: nanoseconds on a caller-chosen time line, so the injected `Clock` maps
  straight onto it). TLS is not in the crate (separate `turn-client-rustls`); the TCP client takes plaintext
  bytes. **Deviations found by running it against a TLS-less server and reading the source:**
  1. it writes ChannelData *unpadded* and parses `4 + length` bytes on receipt; RFC 8656 12.5 requires padding on a
     stream. `relay.rs` frames the stream itself: it hands the crate each frame without its padding and pads the
     ChannelData the crate writes. (The in-process test server pads like a real one.)
  2. with `anonymous_username` on `Auto` it switches from `USERNAME` to the RFC 8489 `USERHASH` when the server
     advertises `PASSWORD-ALGORITHMS`; the server side cannot then see the REST user name. `relay.rs` sets it to
     `Disabled`; coturn and the managed services use `USERNAME`.
  3. its `Debug` output (`TurnCredentials`, client, config) contains the TURN password: never formatted.
     Its `tracing` output (targets `turn_client_proto`, `stun_proto`, `turn_types`; `#[instrument]` spans too)
     was checked by a log-capture test (`the_turn_client_crates_logs_hold_no_password_and_the_filter_removes_them`,
     everything at `trace`, a whole call): the **password and the shared secret never appear**, but at `trace`
     every *received* STUN message is logged in full (NONCE, REALM, the response's MESSAGE-INTEGRITY HMAC), and at
     `info` the crate logs on every poll. (An earlier note here said the logs "expose the TURN password at
     info"; that was wrong, they do not. The credential-adjacent trace output and the noise are why they are
     filtered.) The server filters those targets **below `warn` whatever `RUST_LOG` says**
     (`brisvox_core::telemetry::turn_client_log_allowed`, a per-layer filter in `telemetry::build_subscriber`;
     `BRISVOX_LOG_TURN_CLIENT=1` lifts it, for debugging only). **A subscriber an embedder installs itself must
     apply the same filter** (`filter_fn(brisvox_transport::webrtc::turn_client_log_allowed)`, combined with the
     layer's own filter by `FilterExt::and`).

### 12.2 Why the design is shaped this way
- `str0m` ICE-lite refuses non-host local candidates (`is-0.11.1` agent.rs `add_local_candidate`), so a relayed
  address is added as a *host* candidate. The browser needs nothing special: it sends STUN to an IP:port.
- A TURN server drops data from an IP without a permission, and the browser (controlling agent) sends the first
  connectivity check; so `CreatePermission` for every IP in the offer happens **before the answer is sent**, and
  the 201 waits for it (bounded, 3 s default).
- One allocation per call, one connection per allocation: allocations are per 5-tuple (RFC 8656), a shared one
  needs a demultiplexer, and one connection would put every call behind one head-of-line stall.
- Refresh timing is the client crate's, driven by the injected clock (`ProtoInstant` = clock time since the
  session start): allocation before its lifetime ends, permissions and channels before their 5 / 10 minutes.

- Permissions are created for IPs the *client* supplies in its offer. Bounds (`relay.rs` module docs): at most
  `max_permissions` (8), only the allocation's address family, never unspecified, multicast, broadcast, loopback
  or link-local addresses (loopback and link-local only with `RelayConfig::allow_local_peers`, for tests). The cost
  of a hostile offer is up to 8 `CreatePermission` requests on the operator's TURN quota per call (the per-IP
  offer rate limit applies first). A permission only lets the TURN server pass packets *from* that IP to the
  relayed address; `str0m` sends only to peers whose ICE check it verified, so a permission for an address the
  server never talks to is inert.
- **Trust assumption.** The server opens an outbound TCP/TLS connection to the host in the chosen TURN URL.
  Those URLs come only from operator configuration (coturn) or from the TURN provider's API answer over TLS
  (Cloudflare, Twilio). A hostile or compromised provider response could point the server's connection at an
  internal host or port; operators who do not trust that path should restrict the server's egress. Nothing from
  the connection is returned to a caller.
- DNS for the TURN host uses tokio's blocking pool: at most 8 lookups at once and a 60 s cache per host.
- A setup cut off by the setup timeout closes its connection without `Refresh` 0; the server frees the allocation
  on the closed connection (coturn does) or when its lifetime runs out (600 s by default, section 12.1). A setup
  that fails with an error releases its allocation from a task of its own.
- Queues: 20 packets per direction by default (400 ms of audio; was 128, i.e. 2.56 s of stale audio); a backlog
  goes out in one write of up to 16 packets. `relay_queue_dwell_max_us`, `relay_queue_slow` (> 40 ms),
  `relay_write_max_us` and `relay_slow_writes` (> 20 ms) show a slow TURN connection.

### 12.3 What was built
`webrtc/relay.rs` (see its module docs), `WebRtcConfig::turn_relay: Option<RelayConfig>`, env
`BRISVOX_WEBRTC_TURN_RELAY=on|only|off` and `BRISVOX_WEBRTC_TURN_RELAY_TIMEOUT`, counters `relay_*` in
`WebRtcStats`, `tests/webrtc_relay.rs` (13 tests: a str0m browser that can reach only the relayed address
completes ICE and DTLS and carries audio both ways with `datagrams_in == 0`; the permission exists before the
first packet and nothing is dropped for lack of one; refresh of allocation, permissions and channels across 35
virtual minutes on a stepped clock with the call carrying on; release with `Refresh` 0 on hang-up and shutdown;
silent TURN server and wrong secret fall back to the normal candidates, or `503` in relay-only mode, within the
setup timeout; permission for the wrong IP means no connection; garbage, oversize and bad-length frames end that
allocation only; TLS with a test certificate, and an untrusted certificate refused; no allocation for an offer
without an IP candidate or a malformed one).

### 12.4 UNVERIFIED
- A real TURN service (Cloudflare, Twilio, coturn) accepting the *server's* allocation with the same REST
  credential the browser gets, over `turns:...:443?transport=tcp`; per-user allocation limits; whether more than
  one allocation per credential is allowed (the credential is cached and shared by calls and the browser).
- TLS server-name and ALPN expectations of a 443 multiplexer (no ALPN is sent; RFC 7443 defines `stun.turn`).
- A real browser (Chrome, Firefox, Safari) connecting to a relayed host candidate whose base is not local:
  only a str0m controlling client was tested.
- Latency and loss behaviour of a media path through TURN-over-TCP (head-of-line blocking) on a real network.
- Whether a TURN service pads or accepts unpadded ChannelData from the client: padded is what RFC 8656 requires
  and what the tests' server expects; an unpadded-tolerant server would also accept it.
