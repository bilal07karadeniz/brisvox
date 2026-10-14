# Web assistant (WebRTC, with a WebSocket fallback)

A voice assistant in a web page: one static HTML file plus the shared browser module
`sdk/web/brisvox-call.js` (no framework, no build step, nothing loaded from the network) that talks to `brisvox-server` over **WebRTC**. The browser's microphone
goes up as Opus, the assistant's voice comes back as Opus, and the browser's own echo
cancellation keeps the assistant from interrupting itself. Where WebRTC media cannot get through
(a host with one HTTP port such as a PaaS, a network that passes only HTTPS) the page falls back,
by itself, to a **WebSocket on the same port** (below), so a call works wherever the page loads.

| File | What it is |
|---|---|
| `agent.toml` | The agent: Deepgram (STT) + OpenAI (LLM) + Cartesia (TTS), no tools. Copy it and change the prompt and the voice. |
| `server.env.example` | The server's settings for the WebRTC listener and the provider keys, no values. |
| `web/index.html` | The page: start, mute, hang up, two level meters (you, the assistant), the server URL and an optional bearer token. It says "Connected (WebRTC)" or "Connected (WebSocket fallback)". The call itself is the shared browser module `sdk/web/brisvox-call.js`, which is also what you import on your own website (`sdk/web/README.md`). |
| `test_web_page.py` | Static checks of this page and the transport's `/demo` page (no framework or CDN, they import only the shared module, every element their script uses exists, the script parses). The module's own tests (WebRTC, the WebSocket fallback, barge-in, hang-up, the AudioWorklet code) are in `sdk/web/test`. |

How the pieces talk: the page makes **one HTTP request** (WHIP style, RFC 9725): `POST /whip`
with its SDP offer; the server answers `201` with the answer and a `Location` for the session.
ICE and DTLS then run over the server's UDP port and Opus flows both ways. `DELETE` on the
session URL hangs up. There is no data channel and no transcript: the audio is the interface.
(The transport and its limits: `docs/ARCHITECTURE.md` "WebRTC transport", `docs/providers/webrtc.md`.)

## Run it

Needs keys for Deepgram, OpenAI and Cartesia, Rust, and for the server build a C compiler and
cmake (the Opus codec is libopus; `docs/TESTING.md`, "Toolchain for --all-features").

1. **Build the server** with the providers, the WebRTC listener and Smart Turn (the agent uses the
   adaptive turn policy):

   ```sh
   cargo build --release -p brisvox-server --features all-providers,webrtc,smart-turn-ort
   python3 scripts/models/fetch_models.py        # the Smart Turn model, into models/
   ```

2. **Fill the environment** and set a Cartesia voice id in `agent.toml`:

   ```sh
   cp examples/web-assistant/server.env.example examples/web-assistant/server.env
   # edit it: the three provider keys, and BRISVOX_WEBRTC_UDP_ADDR / _CANDIDATE_IP = this
   # machine's LAN address (see the note in step 3)
   ```

3. **Start the server** from the repository root. The signaling listener binds
   `127.0.0.1:8088`; the media socket is UDP on the address in `BRISVOX_WEBRTC_UDP_ADDR`.
   **Use the machine's LAN address for media, not 127.0.0.1:** Chrome does not gather loopback
   ICE candidates and Firefox rejects them, so with a loopback media socket the call fails with
   "ICE/DTLS did not complete in time" (seen live 2026-10-13):

   ```sh
   set -a; . examples/web-assistant/server.env; set +a
   ./target/release/brisvox-server
   curl http://127.0.0.1:8081/readyz             # ready
   ```

4. **Serve the page** (any static file server; this one is in Python's standard library) and open
   it. A page on `http://localhost` may use the microphone; any other address needs HTTPS:

   ```sh
   python3 -m http.server 8000 --bind 127.0.0.1      # from the repository root: the page imports sdk/web
   # open http://localhost:8000/examples/web-assistant/web/
   ```

   Press **Start**, allow the microphone and say "Hello". Talk over the assistant to interrupt it.
   The page's "Server" section holds the signaling URL (`http://localhost:8088`) and the token.
   The server's `BRISVOX_WEBRTC_CORS_ORIGIN=http://localhost:8000` is what lets a page on
   another origin call the signaling endpoint; it must be the page's origin exactly.

The transport also serves a plain built-in test page at `http://127.0.0.1:8088/demo` (same
protocol, no setup, same-origin): use it to check the server before the page.

## The WebSocket fallback

The page tries WebRTC first. It switches to `wss://<server>/ws` (`ws://` for an `http://` server)
and closes the WebRTC attempt when `RTCPeerConnection` is missing or throws, the WHIP request
fails at the network level, the connection is `failed`, or it is not `connected` after 4 seconds.
It sends the same token, asks the microphone for echo cancellation, noise suppression and gain
control, and resamples it in an AudioWorklet (loaded from a Blob URL, no extra files; `sdk/web/README.md` has the module's options) to 24 kHz
PCM16 in 20 ms binary frames, the generic protocol of `docs/WEBSOCKET_PROTOCOL.md`. The assistant's
audio plays through a second worklet with an 80 ms jitter buffer (at most 400 ms, the oldest
audio dropped beyond that); `clear` (barge-in) empties it at once and `mark`s are echoed once
their audio has played, so the engine still knows what the caller heard. The meters work for both.
The server side is `BRISVOX_WEBRTC_WS_PATH` (default `/ws`, `off` to disable) with the
credentials and limits of `/whip` (`docs/DEPLOY.md`, `docs/SECURITY.md`).

**Trying it in Chrome** (three ways, easiest first):

1. **Add `?force=ws` to the page's address** (`http://localhost:8000/examples/web-assistant/web/?force=ws`): WebRTC is
   skipped and the call goes straight over the WebSocket. `?force=webrtc` does the opposite
   (never fall back, to see the WebRTC error), and `?fallback_ms=8000` changes the 4 s wait.
2. **Make WebRTC fail for real**: run the server with `BRISVOX_WEBRTC_UDP_ADDR=127.0.0.1:0` and
   open the page from another machine (its media address is unreachable there, like a host with
   no UDP), or block outbound UDP for the browser with a firewall rule. The log shows
   `WebSocket fallback: WebRTC did not connect within 4 s` (or `WebRTC could not connect`).
3. **No provider keys**: `cargo run -p brisvox-transport --example webrtc_echo --features webrtc`
   serves `/demo` (same fallback code) with a bot that answers a tone to speech, says so on
   `mark`, and is interrupted when you speak over it. Add the page's origin as the second
   argument to use this page against it:
   `webrtc_echo 127.0.0.1:8088 http://localhost:8000`, then open
   `http://localhost:8000/examples/web-assistant/web/?force=ws`.

In the page, "Server" > log shows each step; the status line says which transport carries the call.

## Putting it on a server

* **Media needs a reachable UDP port and an address to advertise.** WebRTC media does not go
  through the HTTP port. Bind the public interface and say which address browsers should send to:

  ```
  BRISVOX_WEBRTC_ADDR=0.0.0.0:8088
  BRISVOX_WEBRTC_UDP_ADDR=0.0.0.0:8088
  BRISVOX_WEBRTC_CANDIDATE_IP=203.0.113.10      # the host's public IP
  ```

  Forward/open UDP 8088. The server is ICE-lite: a server behind NAT without that port forwarding
  cannot be reached directly. A host with no inbound UDP at all (a one-port PaaS such as Railway) uses the WebSocket
  fallback (below) or the server's own TURN relay (`BRISVOX_WEBRTC_TURN_RELAY`, `docs/DEPLOY.md`,
  "The server's own TURN relay"). For browsers behind a network that blocks UDP,
  set `BRISVOX_WEBRTC_TCP_ADDR` (ICE-TCP) and/or `BRISVOX_TURN_PROVIDER` (the page then fetches
  short-lived relay credentials from `/whip/ice` before it connects; an empty answer or a 404 from
  an older server keeps the direct behaviour): `docs/DEPLOY.md`, "WebRTC: making the server
  reachable from browsers".
* **TLS and the page.** A page on `https://` may only call an `https://` signaling URL (browsers
  block mixed content) and the microphone needs HTTPS on any host but localhost, so put a TLS
  reverse proxy in front of the **signaling** port (the media stays UDP) and serve `index.html`
  from the same host or set `BRISVOX_WEBRTC_CORS_ORIGIN` to the page's origin.
* **Authentication.** The server refuses to listen on a non-loopback address without
  credentials (`docs/SECURITY.md`): anyone who reaches an open signaling port starts a call on
  **your provider keys**. For a demo, `BRISVOX_WEBRTC_TOKEN` is a shared token the page sends
  from its "Server" field. For a real site set `BRISVOX_SESSION_SECRET` and have your backend
  (after your own login) mint a short-lived, single-use token per visitor
  (`docs/SECURITY.md` has Rust, Python and TypeScript snippets; `sdk/python` and `sdk/typescript`
  export `mint_session_token` / `mintSessionToken`) and hand it to the page, which sends it from
  the same field; the page clears a session token after the call starts, since it is
  spent. Behind an HTTPS proxy also set `BRISVOX_TRUSTED_PROXIES` so the per-address limits
  (`BRISVOX_WEBRTC_MAX_CALLS_PER_IP`, `BRISVOX_WEBRTC_OFFERS_PER_MINUTE`) see the visitor. The
  listener's other guards are `BRISVOX_MAX_SESSIONS` (a full server accepts the browser's
  connection and hangs up at once: WebRTC has no gate before the handshake) and the
  per-provider limits `BRISVOX_PROVIDER_LIMITS`.
* **One HTTP port is enough.** On a host that exposes only HTTP(S) (a one-port PaaS such as Railway), set
  `PORT=8080`, `BRISVOX_WEBRTC_ADDR=0.0.0.0:8080` (the same number, written out) and
  `BRISVOX_WEBRTC_UDP_ADDR=127.0.0.1:0` and no TURN, and build the image with the `webrtc`
  feature (a Docker build argument, `docs/DEPLOY.md`): the page falls back to the WebSocket (`docs/DEPLOY.md`, "WebSocket fallback";
  the whole recipe is its section "Platforms with one HTTPS port and no UDP (PaaS)", with Railway as the worked example). The proxy must pass the
  HTTP `Upgrade`. A page on another origin needs `BRISVOX_WEBRTC_CORS_ORIGIN` set to its origin for
  the WebSocket too.
* The settings are listed in `docs/OPERATIONS.md` ("WebRTC"). Calls are `BRISVOX_MAX_SESSIONS` at
  most; each call's Opus work runs on the server's inference pool.

## What was and was not tested

* `cargo nextest run -p brisvox-server --all-features -E 'binary(examples) | binary(hosting)'`:
  the definition loads through the server's config loader and builds its adapters (placeholder
  keys), `server.env.example` reads through the server's own settings parsers (listener on,
  CORS origin, agent path, keys named), and the hosted listener serves the signaling endpoint
  with bearer-token and CORS rules and stops with the server.
* `python3 -m unittest discover -s examples/web-assistant -p 'test_*.py'`: the page.
* The transport's own tests (`crates/brisvox-transport/tests/webrtc.rs`) run real calls against a
  second WebRTC peer, in-process: audio both ways, barge-in marks, hang-up, limits.
* **By hand, once (Chrome, Windows, 2026-10-13):** this page, served by `python3 -m http.server`
  on port 8000, against a server built with `all-providers,webrtc` and placeholder keys, with a
  synthetic microphone (a WebAudio oscillator in place of `getUserMedia`). The page connected
  (`connection connected`), the server logged `call admitted transport="webrtc"` and started the
  engine on the call, **Mute** toggled, and **Hang up** (the `DELETE`) took `/readyz` back to
  `active: 0`. Not exercised: a real microphone, speech, and bot audio (they need real keys).
* **By hand, once (Chrome 152 in the desktop app, Windows, 2026-10-14), WebSocket fallback:** this page and
  `/demo` against the `webrtc_echo` example with a synthetic microphone. `?force=ws` connected
  ("Connected (WebSocket fallback)", the server logged `call ws-1 started`), the greeting tone played
  without gaps, the server saw the microphone and the echoed `mark` (`mark played`), speaking over
  the bot cut it within a buffer's time (`barge-in`), and **Hang up** ended the call. With
  `RTCPeerConnection.connectionState` held at `connecting` the page switched at 4 s on its own, and
  with an unreachable server it showed the failure. A real microphone, a real conversation, other
  browsers and the audio quality on a lossy network were not tried.
* **Not tested at all:** other browsers, and a conversation. `docs/providers/webrtc.md` lists what
  is still unverified against real browsers (mDNS host candidates, Firefox with loopback
  candidates, browser-reported playout). If the call connects and you hear nothing, look at the
  page's "Server" log and the server's log (`RUST_LOG=debug`) first.
