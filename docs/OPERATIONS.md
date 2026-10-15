# Operating brisvox-server

How the server process starts, hosts its call listeners, reports its health and shuts down
. Code: `brisvox_server::{settings, hosting, runner, providers, tools,
health, lifecycle}`, `crates/brisvox-server/src/main.rs`.

The binary loads one agent definition (`BRISVOX_AGENT_FILE`, `docs/CONFIG.md`), builds its STT,
LLM and TTS adapters once (their keys come from the environment variables the definition
names), starts the inference pool (half the cores) and binds the call listeners below. Every
call runs the engine (`run_call`) as one session of the session manager, with the bundled
Silero VAD (feature `silero-tract`, on by default). The generic websocket, Twilio, Telnyx,
RTVI and (with the `sip` / `webrtc` features) SIP and WebRTC listeners are hosted.
The provider adapters are cargo features of `brisvox-server` (`provider-*`, or
`all-providers`); the default build has none, so build the binary with the ones the agent
names (`cargo build --release -p brisvox-server --features all-providers`), otherwise the
definition is refused at startup with the feature to turn on.

**Tools** (`docs/TOOLS.md` "In the server binary"): every tool the definition lists
in `agent.tools` must exist, or the start fails with the tool's position in the file and what
would provide it. The binary provides the telephony tools `transfer_call`, `send_dtmf` and
`end_call` when a carrier's call control is configured (`BRISVOX_CALL_CONTROL`, cargo feature
`call-control`) and, with the cargo feature `demo-tools` (never for callers), canned
`lookup_order` and `book_appointment` for the examples. An application's own tools plug in
through `brisvox_server::tools::AppTools` in its own `main`, one line over this library:
`brisvox_server::app::run_with_tools(Arc::new(MyTools))` (`docs/TOOLS.md` "Rust tools in your
own binary", `examples/rust-tools`).

**Command line.** `brisvox-server` with no arguments runs the server. `brisvox-server check
<agent-file> [--require-secrets]` validates an agent file offline (`docs/CONFIG.md` "Checking a
file"; exit 0 ok, 1 errors), `--version` and `--help` print and exit, and any other argument is
refused with exit status 2 instead of starting a server.

**Smart Turn** (cargo feature `smart-turn-ort`; the ONNX Runtime binaries are downloaded at
build time): an agent whose turn policy is adaptive (the default) gets the Smart Turn v3.2
model, loaded at startup from `BRISVOX_SMART_TURN_MODEL` (default
`$BRISVOX_MODELS_DIR/smart-turn-v3.2-cpu.onnx`, `models/` when unset; fetch it with
`scripts/models/fetch_models.py`). A model that does not load stops the start; `off` runs
without it. A build without the feature logs a warning and adaptive turns end at their fallback
deadline. The analyses run on an executor of their own (`BRISVOX_SMART_TURN_THREADS`):
its queue holds 32 analyses per thread and refuses the rest, and a refused analysis makes that turn
end at its fallback deadline (logged as `smart turn analysis failed`); the VAD's pool, and the
admission control reading it, are not affected, so watch `brisvox_smart_turn_rejected_total` and
`brisvox_smart_turn_timed_out_total` (see Metrics). The executor's threads are named
`brisvox-turn-<i>`.

Provider resilience: every call runs its STT, LLM and TTS behind the resilience layer
with its default settings. The circuit breakers are per provider instance and shared by all calls
(5 consecutive failures open one for 30 s), retries are budgeted per call (3) and per process (50),
a dead STT or TTS session is recovered in place, and an idle TTS session is replaced at 80 % of
the provider's idle limit (Cartesia: 4 of its 5 minutes). Nothing is configured for this; a
second provider per stage to fail over to is the agent definition's `[stt|llm|tts].secondary`
(`docs/CONFIG.md`), whose API keys must be set like the primaries'.

Build for a phone deployment with everything:
`cargo build --release -p brisvox-server --features all-providers,call-control,smart-turn-ort`.

## Settings (environment)

Agent definitions (`docs/CONFIG.md`) describe agents; the process itself is configured by
environment variables. Unset or blank means the default; an invalid value stops the start with
exit status 1 and a message naming the variable.

| Variable | Default | Meaning |
|---|---|---|
| `BRISVOX_MAX_SESSIONS` | 256 per core (at least 256) | Sessions live or reserved at once (1 to 100000); more are refused, never queued. The default follows the cores the process may use (a container's CPU quota counts): about four times what a core serves, so on any machine the pool pressure and the learned ceiling (below) refuse first and this only bounds memory and the queues sized from it. Set it lower to promise a fixed maximum. It also sizes the defaults derived from it (twice it for the listeners' sockets, a quarter for `BRISVOX_WEBRTC_MAX_HANDSHAKING`). With pressure shedding off (`BRISVOX_OVERLOAD_QUEUE_PER_THREAD=0` and `BRISVOX_OVERLOAD_QUEUE_WAIT=0`) the cautious start and the learned ceiling are off too, and this limit is the only one. |
| `BRISVOX_OVERLOAD_QUEUE_PER_THREAD` | `4` | Jobs queued per inference-pool thread at or above which new calls are refused (overloaded). `0` ignores the queue depth. |
| `BRISVOX_OVERLOAD_RESUME_QUEUE_PER_THREAD` | half of the above | Queued jobs per thread at or below which the server may recover (must be below the above). |
| `BRISVOX_OVERLOAD_CEILING` | `true` | Learn a ceiling of held calls from pool pressure: when the pool turns hot hold at most 90% of the calls held then, when it is calm again at most the calls held at that moment (one hot spell at most halves the cap), and raise the cap by 10% (at least one call) per calm window spent at it. The pressure signals lag the calls (a call loads the pool when its caller speaks), so without it a fast ramp is admitted far past capacity. `false`: each admission is judged on the pool alone. |
| `BRISVOX_OVERLOAD_QUEUE_CONFIRM` | `100ms` | How long the queue must stay at or over `BRISVOX_OVERLOAD_QUEUE_PER_THREAD` (in every sample) before it counts. A burst of call starts queues one job per call for microseconds; a pool that is really behind keeps its queue. `0` sheds on the first such sample. |
| `BRISVOX_OVERLOAD_QUEUE_WAIT` | `50ms` | Mean time pool jobs waited in the queue over a window, at or above which new calls are refused. `0` ignores waits. With both at `0` pressure shedding is off. |
| `BRISVOX_OVERLOAD_WINDOW` | `2s` | The window of the wait average, and the calm time needed to recover. |
| `BRISVOX_PROVIDER_LIMITS` | none | `name=n,...`: concurrent calls per provider, e.g. `deepgram=50,openai=100,cartesia=50`. Names are the agent file's `provider` values (`deepgram`, `assemblyai`, `gladia`, `sherpa_onnx`, `openai_realtime_stt`, `openai`, `anthropic`, `gemini`, `openai_compatible`, `cartesia`, `elevenlabs`, `deepgram_aura`, `openai_speech`). |
| `BRISVOX_HEALTH_ADDR` | `0.0.0.0:8081` | Address of the `/healthz` + `/readyz` listener; `off` disables it; `${PORT}` is allowed (see "Port placeholder" below). On a non-loopback address `/metrics` needs `BRISVOX_METRICS_TOKEN` (below). |
| `BRISVOX_METRICS_TOKEN` | unset | Bearer token `/metrics` requires when `BRISVOX_HEALTH_ADDR` is not loopback. Without it `/metrics` answers 404 there (a warning is logged at start). Never logged. |
| `BRISVOX_INSECURE_LISTENERS` | `false` | `1`/`true`: listeners that would refuse to start without authentication (generic websocket, RTVI, WebRTC signaling, Telnyx on a non-loopback address) start anyway, and `/metrics` is served without a token. Logged loudly. See "Authentication" below and `docs/SECURITY.md`. |
| `BRISVOX_SESSION_SECRET` | unset | Signs the short-lived single-use session tokens the generic websocket, RTVI and WebRTC listeners accept (at least 32 bytes; shorter is refused). `docs/SECURITY.md`. |
| `BRISVOX_SESSION_SECRET_PREV` | unset | The previous session secret, still verified while old tokens expire (rotation). Needs `BRISVOX_SESSION_SECRET`. |
| `BRISVOX_TRUSTED_PROXIES` | unset | Comma-separated CIDR blocks of the reverse proxies whose forwarding header the WebRTC listener believes (`10.0.0.0/8,172.16.0.0/12`); the client address is the last hop that is not one of them. Unset: the TCP peer is the client. Never `0.0.0.0/0`: that makes every client its own proxy. |
| `BRISVOX_CLIENT_IP_HEADER` | `x-forwarded-for` | The only forwarding header believed behind a trusted proxy: `x-forwarded-for`, `forwarded` (RFC 7239) or `x-real-ip` (one address; what Railway's edge writes and overwrites). Set the one your proxy writes. |
| `BRISVOX_WEBRTC_HTTP_MAX_PER_IP` | `8` | Signaling connections one client address (an IPv6 `/64`) may hold at once; `0` = no limit. Connections from a trusted proxy are exempt. |
| `BRISVOX_DRAIN_TIMEOUT` | `30s` | How long a drain waits for calls to end by themselves. `0s` cancels at once. |
| `BRISVOX_CANCEL_GRACE` | `5s` | After cancelling, how long sessions get to return. |
| `BRISVOX_ABORT_WAIT` | `1s` | After aborting, how long the task cleanup gets. |
| `BRISVOX_LOG_FORMAT` | `text` | `text` or `json` (`docs/TRACING.md`). |
| `RUST_LOG` | `info` | Log filter. |
| `BRISVOX_DEFAULT_AGENT_FILE` | unset (the container image: `/etc/brisvox/agent.toml`) | The agent definition file used when neither `BRISVOX_AGENT_FILE` nor `BRISVOX_AGENT_DEFINITION` is set: a file mounted at the image's path works as is, and an inline definition alone works too. |
| `BRISVOX_AGENT_FILE` | unset | The agent definition every call runs, as a file path. Required when any listener is on, unless `BRISVOX_AGENT_DEFINITION` is set. |
| `BRISVOX_AGENT_DEFINITION` | unset | The agent definition as text (TOML), for a host that takes variables but no file mount. Same loader, same size limit (1 MiB) and same messages as the file; errors name the source `$BRISVOX_AGENT_DEFINITION`. Setting it together with `BRISVOX_AGENT_FILE` is a startup error. Secrets stay out of it: it names the variables that hold them (`api_key_env`). |
| `BRISVOX_AGENT_FORMAT` | `toml` | The syntax of `BRISVOX_AGENT_DEFINITION`: `toml` or `yaml` (needs the `yaml` cargo feature). Never guessed from the text. Setting it without `BRISVOX_AGENT_DEFINITION` is an error (a file's format follows its extension). |
| `BRISVOX_WS_ADDR` | `127.0.0.1:8080` when no other listener is set, otherwise off | Generic websocket listener (`docs/WEBSOCKET_PROTOCOL.md`, any path); `off` disables it. On a non-loopback address it needs `BRISVOX_WS_TOKEN` or `BRISVOX_SESSION_SECRET`: see "Authentication" below. |
| `BRISVOX_WS_AGENT_FILE` | unset | An agent definition file for the calls of the generic websocket listener only; the other listeners run `BRISVOX_AGENT_FILE`'s agent. One process then serves two agents on one inference pool, one Smart Turn executor and one admission control (each with its own providers and agent links; the websocket agent gets no application or telephony tools, and its `[recording]` table is ignored with a warning). `BRISVOX_PROVIDER_LIMITS` covers the providers of both agents and every call holds a permit of each limited one (admission comes before the listener's agent is known). `/metrics` shows the main agent's provider breakers and retries only. Needs `BRISVOX_WS_ADDR`. Example: synthetic test calls on fake providers next to real calls on real providers. |
| `BRISVOX_WS_TOKEN` | unset | Static token of the generic websocket listener (development): `Authorization: Bearer`, the subprotocol `brisvox-token.<token>` or `?token=`. |
| `BRISVOX_TWILIO_ADDR` | off | Twilio Media Streams listener (path `/twilio/stream`). |
| `TWILIO_AUTH_TOKEN` | unset | The account's Auth Token; required with the Twilio listener (every stream's `X-Twilio-Signature` is checked). |
| `BRISVOX_TWILIO_PUBLIC_URL` | unset | The stream URL exactly as written in the TwiML (`wss://host/path`); required with the Twilio listener. With per-call URLs (below) this is the base: each call's URL is this plus `/<segment>`. |
| `BRISVOX_TWILIO_STREAM_SECRET` | unset | At least 32 bytes. Turns on **per-call signed stream URLs**: every stream URL ends in `/<callSid>.<exp>.<mac>`, valid once and until `exp` (`docs/providers/twilio.md`, "Per-call stream URLs"). The server also signs the URLs of the streams it restarts itself (DTMF, failed warm transfer). Required with the Twilio listener unless `BRISVOX_TWILIO_ALLOW_STATIC_URL=1`. |
| `BRISVOX_TWILIO_ALLOW_STATIC_URL` | off | `1` accepts the fixed stream URL (instead of, or besides, per-call URLs). **Legacy**: the fixed URL's `X-Twilio-Signature` is one constant; anyone who saw it once (a proxy log) can replay it. Logs a warning at startup. |
| `BRISVOX_TELNYX_ADDR` | off | Telnyx media streaming listener (path `/telnyx/stream`). |
| `BRISVOX_TELNYX_STREAM_TOKEN` | unset | The `stream_auth_token` Telnyx must send. **Required when `BRISVOX_TELNYX_ADDR` is not a loopback address** (Telnyx signs nothing: the path and this token are the only checks), unless `BRISVOX_INSECURE_LISTENERS=1`. |
| `BRISVOX_RTVI_ADDR` | off | RTVI listener for Pipecat client SDKs (path `/rtvi`, `docs/RTVI.md`). |
| `BRISVOX_RTVI_TOKEN` | unset | Static token RTVI clients must send (development): `?token=`, `Authorization: Bearer` or the subprotocol `brisvox-token.<token>`. With neither this nor `BRISVOX_SESSION_SECRET` the listener is open, which it refuses on a non-loopback address. |
| `BRISVOX_RTVI_ALLOWED_ORIGINS` | unset | Comma-separated browser `Origin`s allowed (unset: any). |
| `BRISVOX_SIP_ADDR` | off | SIP trunk listener, UDP (feature `sip`; a binary without it refuses the setting). |
| `BRISVOX_SIP_PUBLIC_IP` | unset | Address advertised in SDP, Contact and Via; required when `BRISVOX_SIP_ADDR` is unspecified (`0.0.0.0`). |
| `BRISVOX_SIP_TCP` | `false` | Also take SIP over TCP on the same port. |
| `BRISVOX_SIP_RTP_PORTS` | unset | UDP port range for RTP, `low-high` (unset: the OS picks a port per call). Forward the whole range. |
| `BRISVOX_SIP_ALLOWED_SOURCES` | unset | Comma-separated networks allowed to send SIP (`10.0.0.0/8`, `203.0.113.7`): the carrier's signaling ranges. |
| `BRISVOX_SIP_AUTH_USER` / `BRISVOX_SIP_AUTH_PASSWORD` | unset | Digest credentials inbound INVITEs must present (both or neither). |
| `BRISVOX_SIP_AUTH_REALM` | `brisvox` | The digest realm. |
| `BRISVOX_SIP_TLS_ADDR` | off | SIPS listener (SIP over TLS, conventionally `0.0.0.0:5061`). Needs the two files below; the port must differ from `BRISVOX_SIP_ADDR`'s. TLS 1.2 and 1.3, no client certificate. |
| `BRISVOX_SIP_TLS_CERT` / `BRISVOX_SIP_TLS_KEY` | unset | Paths of the PEM certificate chain (leaf first) and the PEM private key. Read once at startup (at most 256 KiB each; restart to rotate); setting one without `BRISVOX_SIP_TLS_ADDR` is an error. The key is never logged or printed. |
| `BRISVOX_SIP_TLS_CA` | unset | PEM bundle that outbound `sips:` / `;transport=tls` calls verify the callee against (the stack has no built-in roots: list the carrier's CA here). |
| `BRISVOX_SIP_TLS_MAX_CONNECTIONS` / `BRISVOX_SIP_TLS_MAX_PER_IP` | `1024` / `32` | TLS connections at once, and from one source address. Over either the connection is closed before the handshake (`tls_rejected`). |
| `BRISVOX_SIP_TLS_HANDSHAKE_MS` | `10000` | The longest a client may take for the TLS handshake. |
| `BRISVOX_SIP_TLS_IDLE_TIMEOUT` | `300` | Seconds an established SIPS connection may carry no byte (in either direction; RFC 5626 CRLF keepalives count) before it is closed and its slots freed (`tls_idle_closed`). `0` turns the check off. |
| `BRISVOX_SIP_SRTP_AUTH_FAIL_LIMIT` | `50` | SRTP packets of one call that may fail authentication within one second before the call is hung up (logged once; `srtp_flood_hangups`). `0` turns the limit off. A burst means tag guessing (the `_32` tag has 32 bits) or keys out of sync; the price is that whoever can send UDP to the RTP port can end the call. |
| `BRISVOX_SIP_SRTP` | `optional` with TLS, else `off` | SDES-SRTP policy: `off` (plain RTP only), `optional` (SRTP when the offer has a usable `a=crypto` line and came in over TLS, plain otherwise), `required` (anything else gets 488). SDES keys are never accepted over UDP or TCP signaling; anything but `off` without the TLS listener is a startup error. Details and the carriers checked: `docs/TELEPHONY.md`, "SIP over TLS and SRTP". |
| `BRISVOX_LOG_SIP_STACK` | `false` | Let the SIP stack's own log events (`rsipstack`) below `warn` through. Off by default and not for production: the stack logs whole SIP messages at `info`, including the SDES SRTP master keys of a secure call. `RUST_LOG` cannot lift the hold; only this can. |
| `BRISVOX_WEBRTC_ADDR` | off | WebRTC signaling listener, TCP (feature `webrtc`; a binary without it refuses the setting): WHIP-style `POST /whip` and `DELETE` on the session, and the transport's `/demo` page. See "WebRTC" below. |
| `BRISVOX_WEBRTC_UDP_ADDR` | the signaling address | The one UDP socket every call's media shares (forward this port). |
| `BRISVOX_WEBRTC_CANDIDATE_IP` | unset | The address advertised to browsers as the server's media address; required when the UDP socket is bound to an unspecified address (`0.0.0.0`), and behind NAT or in a container the one browsers can reach. |
| `BRISVOX_WEBRTC_TOKEN` | unset | Static bearer token `POST`/`DELETE` require (RFC 9725 section 4.7; development). With neither this nor `BRISVOX_SESSION_SECRET` the endpoint is open, which it refuses on a non-loopback address. |
| `BRISVOX_WEBRTC_MAX_CALLS_PER_IP` | `8` | Calls one client address may hold at once (`429` over it), or `off`. |
| `BRISVOX_WEBRTC_MAX_HANDSHAKING` | a quarter of `BRISVOX_MAX_SESSIONS` | Calls that may be in their ICE/DTLS handshake at once (`503` over it): offers that never connect cannot starve real calls. |
| `BRISVOX_WEBRTC_OFFERS_PER_MINUTE` | `5` | Offers one client address may make per minute, burst of as many (`429` with `Retry-After`), or `off`. |
| `BRISVOX_WEBRTC_WS_PATH` | `/ws` | The WebSocket fallback: a WebSocket upgrade on this path of the signaling port serves the generic WebSocket protocol (`docs/WEBSOCKET_PROTOCOL.md`), with the credentials, `Origin` rule and limits of `POST /whip` (`docs/DEPLOY.md` "WebSocket fallback"). `off`: the path is 404. Counted in `ws_upgrades` / `ws_refused`. |
| `BRISVOX_WEBRTC_CORS_ORIGIN` | unset | `Access-Control-Allow-Origin` for pages on another origin (`*` or one origin). Unset: same origin only. `*` needs `BRISVOX_WEBRTC_TOKEN` or `BRISVOX_SESSION_SECRET`. |
| `BRISVOX_WEBRTC_TCP_ADDR` | off | ICE-TCP listener: a passive TCP candidate next to the UDP one, for clients whose network blocks UDP and hosts with a TCP port but no UDP. `docs/DEPLOY.md` "WebRTC: making the server reachable". |
| `BRISVOX_WEBRTC_TCP_CANDIDATE_PORT` | the listener's port | The port advertised in the TCP candidate when a proxy or port mapping changes it (a platform's TCP proxy). |
| `BRISVOX_WEBRTC_TCP_MAX_CONNECTIONS` / `_MAX_PER_IP` / `_IDLE_TIMEOUT` / `_MAX_FRAME` / `_QUEUE` | `256` / `16` / `10s` / `2048` / `64` | ICE-TCP limits: connections served at once, per source IP (`0` = off; use it behind a TCP proxy), time to name a live call, largest frame, frames queued per connection. `docs/CONFIG.md` "WebRTC reachability". |
| `BRISVOX_TURN_PROVIDER` | off | `coturn`, `cloudflare` or `twilio`: browsers get short-lived TURN credentials on the WHIP `201` (`Link: rel="ice-server"`) and from `GET /whip/ice`. Variables per provider and `BRISVOX_TURN_TTL`: `docs/CONFIG.md` "WebRTC reachability", which also says who may mint relay credentials (without `BRISVOX_WEBRTC_TOKEN`: anyone) and why the pages use `GET /whip/ice` rather than the `Link` headers. |
| `BRISVOX_TURN_URLS` | - | `coturn` provider: comma-separated `turn:`/`turns:` URLs handed to browsers (`docs/CONFIG.md` "WebRTC reachability"). |
| `BRISVOX_TURN_SECRET` | - | `coturn` provider: the `static-auth-secret` shared with coturn (`use-auth-secret`); credentials are derived per call and expire after `BRISVOX_TURN_TTL`. Never sent to browsers. |
| `BRISVOX_TWILIO_PATH` / `BRISVOX_TELNYX_PATH` / `BRISVOX_RTVI_PATH` | `/twilio/stream` / `/telnyx/stream` / `/rtvi` | The websocket path of each listener (Twilio: the path of `BRISVOX_TWILIO_PUBLIC_URL`). |
| `BRISVOX_LISTENER_HANDSHAKE_TIMEOUT` | `5s` | Every websocket listener: TCP accept to the end of the upgrade. |
| `BRISVOX_LISTENER_START_TIMEOUT` | `5s` | Upgrade to the protocol's start message. |
| `BRISVOX_LISTENER_PING_INTERVAL` / `BRISVOX_LISTENER_PING_TIMEOUT` | `10s` / `30s` | Server pings; a client silent for the timeout (longer than the interval) ends its call. |
| `BRISVOX_LISTENER_WRITE_TIMEOUT` | `5s` | Longest single write to a client. |
| `BRISVOX_LISTENER_SEND_TIMEOUT` | `5s` | Longest wait for the engine's queues (a control event, the hand-over of a new call). |
| `BRISVOX_LISTENER_CLOSE_GRACE` | `2s` | How long the two sides of a closing connection wait for each other. |
| `BRISVOX_LISTENER_ACCEPT_QUEUE` | `16` | Calls waiting to be handed to a session (1 to 1000000). |
| `BRISVOX_LISTENER_MAX_AUDIO_BYTES` / `BRISVOX_LISTENER_MAX_CONTROL_BYTES` | `65536` / `16384` | Largest binary / text frame accepted (2 / 512 to 16 MiB). |
| `BRISVOX_RTVI_BOT_SPEAKING_EVENTS` | `true` | The RTVI bridge sends `bot-started/stopped-speaking` and `bot-interrupted` from the engine. |
| `BRISVOX_RTVI_METRICS` | `true` | The RTVI bridge sends `metrics` once per turn. |
| `BRISVOX_RTVI_REPLY_TIMEOUT` | `5s` | How long an RTVI client message's handler may take. |
| `BRISVOX_RTVI_EVENT_CAPACITY` / `BRISVOX_RTVI_TEXT_CAPACITY` | `1024` / `8` | An RTVI call's engine event ring / typed messages waiting for the turn driver. |
| `BRISVOX_CALL_CONTROL` | `off` | Carriers whose REST call control the telephony tools use: `twilio`, `telnyx` or both, comma separated (feature `call-control`). |
| `BRISVOX_TWILIO_ACCOUNT_SID` | unset | The Twilio account (`AC...`); required with Twilio call control (with `TWILIO_AUTH_TOKEN`). |
| `BRISVOX_TWILIO_CALLER_ID` | unset | E.164 caller id of Twilio transfers (the `From` of a warm transfer's receiving leg; required for warm transfers). |
| `BRISVOX_TWILIO_API_BASE` | `https://api.twilio.com/2010-04-01` | Twilio REST base URL (a regional endpoint, or a test double on loopback). |
| `BRISVOX_TWILIO_HOLD_URL` | unset | `waitUrl` of the conference a warm transfer's caller waits in (hold music). |
| `BRISVOX_TWILIO_STATUS_CALLBACK` | unset | Where Twilio posts the status events of a warm transfer's receiving leg; the application behind it calls the callback listener. |
| `TELNYX_API_KEY` | unset | The Telnyx API key (v2); required with Telnyx call control. |
| `BRISVOX_TELNYX_CALLER_ID` | unset | E.164 caller id of Telnyx transfers (required for warm transfers). |
| `BRISVOX_TELNYX_CONNECTION_ID` | unset | The Telnyx Call Control application of a warm transfer's receiving leg (required for warm transfers). |
| `BRISVOX_TELNYX_HOLD_URL` | unset | Audio a warm transfer's caller hears while waiting (without it: silence). |
| `BRISVOX_TELNYX_API_BASE` | `https://api.telnyx.com/v2` | Telnyx REST base URL (a test double on loopback). |
| `BRISVOX_CALLBACK_ADDR` | `off` | Where the callback listener binds (`host:port`); needs `BRISVOX_CALL_CONTROL` (feature `call-control`). See "Callback listener". |
| `BRISVOX_CALLBACK_TOKEN` | unset | The shared secret of the callback listener, at least 16 characters; required with `BRISVOX_CALLBACK_ADDR`. |
| `BRISVOX_SMART_TURN_MODEL` | `$BRISVOX_MODELS_DIR/smart-turn-v3.2-cpu.onnx` | The Smart Turn model of adaptive turns (feature `smart-turn-ort`), or `off`. |
| `BRISVOX_MODELS_DIR` | `models` | Directory of the model files. |
| `BRISVOX_EXTRA_CA_FILE` | unset | **Tests and benchmarks only.** PEM file of CA certificates that every provider adapter trusts besides the public roots (certificate and host name are still verified). It lets the adapters talk TLS to the fake providers of `bench/fake_providers`, whose certificate is signed by a throwaway CA. Read once, when the first TLS connection is set up; an unreadable or invalid file fails that connection. Not for production: use a real certificate, or `BRISVOX_AGENT_TLS_CA` for a private agent. |
| `BRISVOX_SMART_TURN_THREADS` | the inference pool's threads (half the cores) | Threads of the Smart Turn executor: the 20-25 ms analyses run on their own threads so they never delay the VAD windows of other calls. `0` runs them on the VAD's pool as before (use it to compare). One thread is about 55% busy at 200 calls and delays verdicts when turns end together; the default carries ~180 calls per thread with headroom. Small machines: the default is half the cores (at least 1), so 1-3 cores give 1 executor thread; the server logs a warning at start when the VAD pool, the executor and the ONNX Runtime intra-op workers together exceed the available cores. |

A listener address is `host:port` or `off`. Without any listener (`BRISVOX_WS_ADDR=off` and
the others unset) the binary serves its health endpoints only and needs no agent file.

**Port placeholder.** The listener address variables (`BRISVOX_HEALTH_ADDR`, `BRISVOX_WS_ADDR`,
`BRISVOX_TWILIO_ADDR`, `BRISVOX_TELNYX_ADDR`, `BRISVOX_RTVI_ADDR`, `BRISVOX_SIP_ADDR`,
`BRISVOX_SIP_TLS_ADDR`, `BRISVOX_WEBRTC_ADDR`, `BRISVOX_WEBRTC_UDP_ADDR`,
`BRISVOX_WEBRTC_TCP_ADDR`, `BRISVOX_CALLBACK_ADDR`) accept the exact token `${PORT}`, replaced by
the `PORT` variable that PaaS hosts (Railway, Heroku, Fly, Cloud Run) inject: `BRISVOX_WS_ADDR=[::]:${PORT}`.
No other expansion happens (`$PORT`, `${port}`). A value that uses the token while `PORT` is unset
or not a number from 1 to 65535 stops the start with a message naming both variables. A value
without the token never reads `PORT`.

**Agent definition in a variable.** `BRISVOX_AGENT_DEFINITION` holds the text of the agent
definition instead of a file path (`docs/CONFIG.md`, "The agent definition from the
environment"); `brisvox-server check --env` validates it offline.

**Authentication (`docs/SECURITY.md`).** The generic websocket protocol has no
authentication of its own (`docs/WEBSOCKET_PROTOCOL.md`), and its connections take the same
session reservations as every other listener's: anyone who reaches an open one can run the agent
on your STT/LLM/TTS keys, and a few hundred idle sockets (each holds a reservation for its 5 s
handshake) fill `BRISVOX_MAX_SESSIONS`, so that signed Twilio calls get 503. That is why it is
never on by default next to another listener, and listens on loopback only when it is the only
one. And the generic websocket, RTVI, WebRTC signaling and Telnyx listeners **refuse to start on
a non-loopback address without authentication**: set `BRISVOX_SESSION_SECRET` (your backend
mints short-lived single-use tokens; the production way) or the listener's static token
(`BRISVOX_WS_TOKEN`, `BRISVOX_RTVI_TOKEN`, `BRISVOX_WEBRTC_TOKEN`,
`BRISVOX_TELNYX_STREAM_TOKEN`; development, logged as a warning), or accept the risk with
`BRISVOX_INSECURE_LISTENERS=1` (logged as a warning per exposed listener). Loopback binds need
nothing. Twilio always verifies its signature; SIP has its own source ACL and digest rule.
Terminate TLS in a reverse proxy in front of every public listener.

**The WebRTC listener** (feature `webrtc`: `cargo build -p brisvox-server --features
all-providers,webrtc`, which compiles libopus and needs a C compiler and cmake, `docs/TESTING.md`)
hosts `brisvox_transport::webrtc` (`docs/ARCHITECTURE.md` "WebRTC transport"; a runnable page:
`examples/web-assistant`). Media is UDP on one shared socket: forward `BRISVOX_WEBRTC_UDP_ADDR`
and set `BRISVOX_WEBRTC_CANDIDATE_IP` to the address browsers reach. A network that blocks UDP connects
through ICE-TCP (`BRISVOX_WEBRTC_TCP_ADDR`) or a browser-side TURN relay (`BRISVOX_TURN_PROVIDER`);
a *server* host with no inbound UDP (a one-port PaaS such as Railway) uses the WebSocket fallback or the server's own TURN
relay (`BRISVOX_WEBRTC_TURN_RELAY`, the server allocates a relayed address on a TURN service over
TLS/443). All of it is in `docs/DEPLOY.md`. Differences from the
websocket listeners:

- **No gate before the handshake.** A call reaches the engine once its ICE and DTLS completed and
  is admitted then; with no room (`BRISVOX_MAX_SESSIONS`, overload, provider budget, draining)
  the browser connects and the call is hung up at once, logged as `call without a reservation
  refused`. The transport's own call limit (twice the manager's) bounds the sockets.
- **Authentication.** Signaling needs `BRISVOX_SESSION_SECRET` (a session token per call in
  `Authorization: Bearer`) or `BRISVOX_WEBRTC_TOKEN` (shared, for development) on a non-loopback
  address, like the other exposed listeners (above): anyone who reaches an open one runs the
  agent on your provider keys. **Signaling must be served over HTTPS** (a proxy in front of the
  TCP port): the DTLS fingerprint travels in the SDP, and over plain HTTP an on-path attacker
  can swap it and hear the call. `DELETE` is authorized by the session URL when session tokens
  are in use. A session token's `max_s` caps the call.
- **Flood limits.** Per client address: 5 offers a minute (`BRISVOX_WEBRTC_OFFERS_PER_MINUTE`,
  `429`), 8 calls at once (`BRISVOX_WEBRTC_MAX_CALLS_PER_IP`, `429`); calls still handshaking are
  capped at a quarter of `BRISVOX_MAX_SESSIONS` (`BRISVOX_WEBRTC_MAX_HANDSHAKING`, `503`). Behind a
  proxy set `BRISVOX_TRUSTED_PROXIES` so the address is the visitor's, not the proxy's. The
  counters are in `WebRtcServer::stats()`.
- **Codec on the inference pool.** The Opus work of every call (90 to 376 us a frame) runs on the
  server's inference pool, never on tokio workers.
- Like every listener it stops with the server: the cancel stage ends its calls.

**The SIP listener** needs `BRISVOX_SIP_ALLOWED_SOURCES` or the digest credentials (or both):
an open SIP port is scanned within minutes, and the binary refuses to start without either.
For a trunk across the internet add **SIPS and SRTP**: `BRISVOX_SIP_TLS_ADDR` (5061) with
`BRISVOX_SIP_TLS_CERT` and `BRISVOX_SIP_TLS_KEY`, and `BRISVOX_SIP_SRTP=required`. SDES keys are only
ever taken from TLS signaling, so with `required` the cleartext UDP/TCP port refuses every call (488);
keep it for health checks or close it with the ACL. The certificate must be one the carrier trusts
(a public CA for Twilio and Telnyx). `SipStats` counts `tls_connections`, `tls_rejected`,
`tls_handshake_failures`, `tls_idle_closed`, `srtp_calls`, `rejected_insecure`, `srtp_packets_rejected`,
`srtp_flood_hangups` and `insecure_dialog_requests` (in-dialog requests of a TLS dialog that did not come
over TLS: refused or stripped of their SDP; a steady count means someone on the signaling network is
probing). Never run
the server with `RUST_LOG` lifting `rsipstack` (it cannot: the filter is fixed, see
`BRISVOX_LOG_SIP_STACK`). A start-up warning from `rsipstack` about an unsupported listener type is
expected with SIPS (the placeholder that makes requests to TLS callees carry a TLS `Via`).

Durations are a whole number with `ms`, `s` or `m` (`500ms`, `45s`, `10m`); a bare number is
seconds; at most one day.

### Callback listener

With a carrier's call control (`BRISVOX_CALL_CONTROL`, feature `call-control`) and
`BRISVOX_CALLBACK_ADDR` set, the binary serves a small HTTP listener through which the world
tells a live call what the carriers' REST call control cannot return (the endpoints, the
authentication and the limits are in `brisvox_server::callbacks`):

| Request | Effect |
|---|---|
| `POST /v1/calls/{twilio\|telnyx}/{call}/warm/complete` | The receiving party of a warm transfer accepted: they join the caller, the bot leaves the call. `200 {"status":"joined"}`. |
| `POST /v1/calls/{twilio\|telnyx}/{call}/warm/abort` | They did not answer, declined or are a machine: their leg ends and the caller goes back to the bot (Twilio; the bot says so) or, with no way back (Telnyx), is hung up. `200 {"status":"aborted","caller":"resumed\|parked"}`. |
| `POST /v1/amd/twilio` | A Twilio answering-machine callback (`AsyncAmdStatusCallback`, form body): the verdict goes to the call's voicemail handling (`docs/TELEPHONY.md` 6). `200 {"status":"delivered"}`, or `202 {"status":"queued"}` when the call has not started yet: an asynchronous detector can beat the media stream, so the verdict waits up to a minute for its call (256 at most). |
| `POST /v1/amd/telnyx` | A Telnyx answering-machine webhook (JSON body). |

`{call}` is the carrier's id of the caller's call: Twilio's `CallSid`, Telnyx's
`call_control_id`. The application that originates or receives the call has it; it is also in
the call's stream metadata.

Every request needs the shared secret as `Authorization: Bearer <secret>`, or as the password
of `Authorization: Basic` for carriers that can only put credentials in a URL
(`https://user:<secret>@host/v1/amd/twilio`); the compare is constant time and a request
without it gets `401` before its body is read. The secret travels in a header: serve the
listener over TLS (a proxy) and keep it on a private address when you can. Carrier webhooks are
not signature-checked by this listener. Errors are `{"error":"<code>","message":"..."}`:
`404 no_such_call` (the call ended, or never was), `409 no_warm_transfer` / `no_voicemail`,
`502 carrier_error`, `504 timeout`, `400`/`413`/`431`/`501` for malformed requests.

The listener is bounded: 32 requests at once (more are closed unanswered), 5 s to read a
request or write the answer, 25 s for the handler, head 8 KiB, body 16 KiB, no keep-alive and no
chunked bodies. It serves through the drain (a call that is still running may need its callbacks)
and stops after the transports. It is a separate port from the health probes on purpose: it
changes calls and is authenticated; the probes are neither.

A warm transfer nobody finishes does not leave the caller alone: when the call's session ends
(the stream's linger passed, shutdown) with the transfer pending, the server aborts it and hangs
the caller up.

### Agent process

| Variable | Default | Meaning |
|---|---|---|
| `BRISVOX_AGENT_URL` | unset | The agent process every call links to (`ws://` or `wss://`, `docs/AGENT_PROTOCOL.md`). Unset: calls run on the agent file alone. |
| `BRISVOX_AGENT_TOKEN` | unset | Sent as `Authorization: Bearer <token>` to the agent; never logged. |
| `BRISVOX_AGENT_ALLOW_INSECURE` | off | `1` accepts a plaintext `ws://` agent URL whose host is outside the private networks (loopback, RFC 1918, link-local, `localhost`, `*.internal`, `*.local`, a single-label service name, which gets a one-time warning because a resolver's search list can make it public); otherwise such a URL fails the startup. Logs a warning. |
| `BRISVOX_AGENT_CALLER_IDENTITY` | off | `1` sends the caller's and called number and the carrier's call id of telephony calls to the default agent process (`session.start.call`, `docs/AGENT_PROTOCOL.md`, "Caller identity"). Withheld while phone numbers are redacted (`docs/PRIVACY.md`). The definition's `[agent_link] caller_identity` wins; routes never inherit it. |
| `BRISVOX_AGENT_TLS_CA` | unset | PEM bundle of a private CA, trusted besides the public roots for `wss://` agent URLs. |
| `BRISVOX_AGENT_CONNECT_TIMEOUT_MS` | `2000` | Bound on connecting to the agent. |
| `BRISVOX_AGENT_HANDSHAKE_TIMEOUT_MS` | `2000` | Bound on the `session.start` write and the agent's `session.accept`, together. |
| `BRISVOX_AGENT_RECONNECT_ATTEMPTS` | `5` | Reconnection attempts per series for an agent that declared `resume`; `0` never reconnects. |
| `BRISVOX_AGENT_RECONNECT_WINDOW_MS` | `15000` | No new reconnection attempt starts later than this after the link was lost. |

An agent that is down, slow or rejects a call never fails the call: it runs on the agent file
(logged as a warning), and a link that fails during a call leaves the call running on the agent
file's tools. The call waits for its agent before it starts: the caller hears up to
`CONNECT_TIMEOUT_MS + HANDSHAKE_TIMEOUT_MS` (4 s by default) of silence when the agent URL is
unreachable or slow, so keep both small. The agent's `transfer` command goes through the call's
carrier call control (`BRISVOX_CALL_CONTROL`, `docs/TELEPHONY.md`) when the call has one, and
answers `unsupported` otherwise. A link that fails during a call is reopened (bounded backoff)
when the agent declared `resume`; the definition's `[agent_link]` section and the route named by
a call's start message pick the agent process per definition and per call
(`docs/AGENT_PROTOCOL.md`, "Reconnecting" and "Routing").

## Admission

Every listener admits through the one session manager (`brisvox_server::hosting`):

- Right after the TCP accept, before the websocket upgrade, the listener reserves a session
  (`SessionManager::try_admit`). No room (`BRISVOX_MAX_SESSIONS` calls live or reserved,
  counting every listener): HTTP 503 with `Retry-After: 1`. Draining: HTTP 503 without
  `Retry-After` (go to another instance). No upgrade, no work spent on a refused connection.
- SIP reserves on every new INVITE, before the digest challenge and the SDP: no room is a
  `503` with `Retry-After`, draining a `503` without it. The reservation is held through the
  `200 OK` and the ACK; an INVITE that does not become a call (refused, cancelled, no ACK)
  gives it back.
- The reservation travels with the connection through the handshake and the protocol's start;
  the call's session is spawned into it. A connection that never becomes a call (a failed
  handshake, a start timeout, a bad Twilio signature) gives the reservation back.
- `/readyz` and the listeners agree: `full` when every slot is taken, `overloaded` or
  `provider_budget` while admission control refuses (below), `draining` once the drain began.
- Each listener also bounds its sockets at twice `BRISVOX_MAX_SESSIONS` (a connection keeps its
  socket through the close handshake after its call ended; SIP: its own call limit, at most
  100000); the session limit is the one that refuses calls.

### Pool pressure and provider budgets

After the drain and session-limit checks, `try_admit` applies two more rules, so a server that
is not at `BRISVOX_MAX_SESSIONS` still refuses calls it cannot serve well. Every refusal is
counted by reason and is answered cleanly by the transport that met it.

- **Pool pressure.** The calls' DSP and inference run on the shared inference pool, so when it
  falls behind every call degrades together; so do the Smart Turn analyses when their executor
  falls behind (a late analysis delays the end of the turn), so that executor is judged too, on
  its queue waits and refused or timed-out jobs (not its depth: its jobs take 20-25 ms, so a
  few queued ones are ordinary), and either one hot refuses new calls. The server reads the pools at each admission
  attempt and each `/readyz` probe: queued jobs per pool thread, jobs refused or timed out, and
  the mean time jobs waited in the queue over `BRISVOX_OVERLOAD_WINDOW` (a job queues only while
  the cores are busy, so the wait is the CPU signal; it is judged only with at least 8 jobs in
  the window). It turns **overloaded** on the first sample over the wait limit or with new
  refused or timed-out jobs, and on a queue that has stayed over its limit for
  `BRISVOX_OVERLOAD_QUEUE_CONFIRM` (a burst of call starts makes a short queue that drains in
  microseconds: on a 2-thread pool, 50 calls dialing within a second refused dozens of them
  with the pool almost idle until the confirmation was added). It recovers only after
  one full window of samples at or below the resume limit (hysteresis: a pool hovering at the
  limit does not flap). Sampling is lazy (no background task): an orchestrator polling
  `/readyz` also advances the recovery.
- **Learned ceiling.** The pressure signals lag the calls: a call admitted now loads the pool
  a second or more later, when its caller speaks. Measured on Railway (4 vCPU, a capacity of
  about 250 calls), dialing 900 calls within seconds admitted about 730 before the pool turned
  hot, and every call degraded (V2V p50 3.7 s). With `BRISVOX_OVERLOAD_CEILING` (default) the
  pool turning hot caps the held calls at 90% of the calls held then, the pool turning calm
  caps them at the calls held at that moment (what it served well), and the cap grows by 10%
  per calm window spent at it, so it settles near what the machine serves. Running calls are never ended to get under it. Until the pool was first seen hot it starts cautiously: at most 16 calls, doubling per calm window spent at the cap (16, 32, 64, ... every 2 s), so a rush of dials on a fresh server (a restart, a new instance behind a least-connections balancer) cannot overshoot before the pool shows its load. For a hard limit from the first second, set `BRISVOX_MAX_SESSIONS` to your measured capacity. One hot spell lowers the cap to at most half of the cap it started from, and never below 16 calls; an idle spell longer than two windows is not a recovery. At the cap `/readyz` reports `overloaded`, as it does when the Smart Turn executor is behind.
- **Provider budgets.** With `BRISVOX_PROVIDER_LIMITS`, every call holds one permit of each
  limited provider its agent uses (the STT, LLM and TTS named in the agent file) for its whole
  life; permits are all-or-none and come back on every end path (hang-up, error, panic,
  cancel, abort, a connection that never became a call). A limit for a provider the agent does
  not use is ignored with a warning. Because every call needs a permit of every limited
  provider, one exhausted provider means no call can be admitted until one ends.
- **Refusal per transport.** Websocket, Twilio, Telnyx and RTVI listeners answer the HTTP
  upgrade with `503` and `Retry-After`: `1` s at the session limit, `5` s overloaded, `2` s
  provider budget, none when draining; the body names the reason (`server busy`,
  `server overloaded`, `provider capacity reached`, `server shutting down`). SIP answers `503`
  with `Retry-After` for all three, none when draining.
- **Metrics.** `/readyz` carries the counters and the numbers behind the verdict (below), and
  `SessionManager::stats` has `rejected_at_capacity`, `rejected_overloaded`,
  `rejected_provider_budget` and `rejected_shutting_down`. Entering and leaving overload is
  logged once each (`brisvox::server`, warn and info).

## Health endpoints

Served on `BRISVOX_HEALTH_ADDR` over plain HTTP/1.1, `GET` or `HEAD`, one request per
connection (`Connection: close`), `Cache-Control: no-store`.

| Path | 200 when | 503 when | Body |
|---|---|---|---|
| `/healthz` | the process serves the listener (also while draining) | never | `{"status":"ok"}` |
| `/readyz` | sessions are admitted | draining, every slot is taken, the pool is overloaded or a provider budget is exhausted | see below |
| `/metrics` | `BRISVOX_METRICS` is not off | never (404 when off, or on a non-loopback address without `BRISVOX_METRICS_TOKEN`; 401 without the bearer token) | Prometheus text, see [Observability](#observability) |

```json
{"status":"ready","accepting":true,"active":3,"reserved":0,"max":256,
 "rejected":{"full":0,"overloaded":0,"provider_budget":0,"shutting_down":0},
 "pressure":{"overloaded":false,"queue_depth":0,"threads":4,"queue_per_thread_milli":0,"mean_wait_us":120,"entered":0},
 "providers":[{"name":"deepgram","limit":50,"in_use":3,"rejected":0}]}
```

`status` is `ready`, `full` (at `max`; `accepting` stays true, a slot frees when a call ends),
`overloaded` (pool pressure, see above), `provider_budget` (some provider has no permit left)
or `draining` (`accepting` false, permanent). `pressure` is present when the server hosts calls
(it reads the inference pool); `queue_per_thread_milli` is queued jobs per thread times 1000,
`mean_wait_us` the mean queue wait of the last judged window and `entered` how many times the
server turned overloaded. `providers` lists the limited providers. `active` counts running sessions, `reserved` the
admissions a transport accepted but has not started yet. Other paths answer 404, other methods
405, a malformed request 400, a request head over 4 KiB 431. At most 32 probes are served at
once (more are closed unanswered) and each has 2 s to send its request and read the answer.

Use `/healthz` as the liveness probe and `/readyz` as the readiness probe. Liveness never
fails because of a drain or a full server, so neither restarts the container.

**`/metrics` and the health address.** The probes stay open on `0.0.0.0:8081` by default:
Kubernetes probes the pod's address and a container runtime probes from outside the network
namespace, so a loopback default would break both, and they carry only counts. `/metrics` (call
rates, error rates, latencies) is served without a token only on a loopback health address. On any
other address it answers 404 until `BRISVOX_METRICS_TOKEN` is set, and then requires
`Authorization: Bearer <token>` (constant time, 401 otherwise); the server logs a warning when it
turns `/metrics` off for this reason. `BRISVOX_INSECURE_LISTENERS=1` serves it openly (private
networks). Prometheus: `authorization: { credentials_file: /path/to/token }` in the scrape job.

## Observability

Metrics, traces and log redaction. Code: `brisvox_server::observability`;
conventions: `docs/TRACING.md`.

| Variable | Default | Meaning |
|---|---|---|
| `BRISVOX_METRICS` | `true` | Serve `GET /metrics` on the health listener. The counters are kept either way. |
| `BRISVOX_TRACES` | `off` | `otlp` ships one trace per turn to an OpenTelemetry collector (needs the `otlp` build feature). |
| `BRISVOX_OTLP_ENDPOINT` | `http://127.0.0.1:4318/v1/traces` | `http://host[:port][/path]` of the collector's OTLP/HTTP receiver. Plain HTTP only: run a collector or agent next to the server and let it carry TLS and credentials to the backend. |
| `BRISVOX_OTLP_QUEUE` | `1024` | Traces waiting for the exporter task; when full the newest is dropped and counted. |
| `BRISVOX_OTLP_BATCH` | `64` | Most traces per request. |
| `BRISVOX_OTLP_FLUSH` | `2s` | Longest wait before a partial batch is sent. |
| `BRISVOX_OTLP_TIMEOUT` | `5s` | Deadline of one request (connect, send, answer). A failed batch is dropped and counted, never retried. |
| `BRISVOX_OTLP_SERVICE` | `brisvox-server` | The `service.name` resource attribute. |

Build the OTLP exporter with `--features otlp`. It has no dependency of its own.

### Prometheus metrics

`GET /metrics` on `BRISVOX_HEALTH_ADDR` (text exposition 0.0.4). Scrape it every 5 to 15 s;
the cost of a scrape is a few hundred atomic reads and one string. Nothing is recorded per audio
frame: histograms and counters are updated once per finished turn, per call end, or read at
scrape time.

| Family | Type | Labels | Meaning |
|---|---|---|---|
| `brisvox_stage_latency_seconds` | histogram | `stage` | One observation per finished turn: `end_of_turn`, `stt_final`, `llm_first_token`, `llm_first_sentence`, `tts_first_byte`, `into_llm`, `out_of_tts`, `post_endpoint`, `response`, `time_to_silence`; `llm_round` has one per LLM request. Definitions: `docs/LATENCY.md`. |
| `brisvox_turns_total` | counter | `outcome` (`completed`, `interrupted`) | Turns finished. |
| `brisvox_sessions_active`, `_reserved`, `_max`, `_peak` | gauge | | Session manager. |
| `brisvox_sessions_started_total`, `brisvox_sessions_ended_total{reason}` | counter | `reason` | Sessions started and ended (`completed`, `failed`, `panicked`, `cancelled`, `aborted`). |
| `brisvox_session_admissions_rejected_total` | counter | `reason` (`full`, `overloaded`, `provider_budget`, `shutting_down`) | Admissions refused. |
| `brisvox_pool_overloaded`, `_queue_depth`, `_mean_wait_seconds`, `brisvox_pool_overload_entered_total` | gauge/counter | | Inference pool pressure; present when the server hosts calls. |
| `brisvox_smart_turn_queue_depth` (gauge), `brisvox_smart_turn_rejected_total`, `brisvox_smart_turn_timed_out_total` (counters) | gauge/counter | | The Smart Turn executor's own queue: analyses waiting, refused because the queue was full, and answered too late. Absent with `BRISVOX_SMART_TURN_THREADS=0` (the analyses then share the VAD pool and show in `brisvox_pool_*`). Admission control reads only the VAD pool, so these are the only signal of an overloaded executor. |
| `brisvox_provider_concurrency_limit`, `_in_use`, `brisvox_provider_budget_refusals_total` | gauge/counter | `provider` | Per-provider concurrency budgets (`BRISVOX_PROVIDER_LIMITS`). |
| `brisvox_provider_errors_total` | counter | `stage`, `kind` | Provider failures the engine counted (connect failures, session deaths, LLM request/stream errors and timeouts, TTS send failures and stalls), added when a call ends. |
| `brisvox_engine_events_total` | counter | `event` | Engine trouble of finished calls: queue and stale drops, pool trouble, finals-gate, settle and mark timeouts, truncations. |
| `brisvox_calls_ended_total` | counter | | Calls whose end-of-call statistics were added. |
| `brisvox_breaker_state` | gauge | `stage`, `role`, `provider` | Circuit breaker: 0 closed, 1 half-open, 2 open. |
| `brisvox_breaker_events_total` | counter | `stage`, `role`, `provider`, `event` | Attempts admitted, refused, failed, probes, openings, closings. |
| `brisvox_retry_budget_available`, `brisvox_retry_budget_total{outcome}` | gauge/counter | `outcome` (`granted`, `refused`) | The process retry budget. |
| `brisvox_resilience_events_total` | counter | `stage`, `event` | Per-call resilience counters summed: `attempts`, `retries`, `failovers` (served by the secondary), `breaker_skips`, `budget_refusals`, `unavailable`, `recoveries`, `recovery_failures`, `replayed`, `idle_refreshes`. Added at every finished turn and when a call ends. |
| `brisvox_traces_total`, `brisvox_trace_batches_total` | counter | `outcome` | Traces built and dropped; OTLP batches sent and failed. |

Useful alerts: `rate(brisvox_smart_turn_rejected_total[5m]) > 0` or
`rate(brisvox_smart_turn_timed_out_total[5m]) > 0` (the Smart Turn executor is overloaded: every
affected turn ends at its fallback silence deadline instead of the model's verdict; add executor
threads with `BRISVOX_SMART_TURN_THREADS` or shed calls);
`brisvox_breaker_state == 2` for more than a minute (a provider is down);
`rate(brisvox_resilience_events_total{event="failovers"}[5m]) > 0` (calls are running on a
secondary); `rate(brisvox_resilience_events_total{event="budget_refusals"}[5m]) > 0` (the retry
budget is spent: an outage is bigger than the budget allows); `rate(brisvox_session_admissions_rejected_total[5m])`
by `reason`; `histogram_quantile(0.95, sum by (le) (rate(brisvox_stage_latency_seconds_bucket{stage="response"}[5m])))`.

### Traces

With `BRISVOX_TRACES=otlp` each finished turn is one trace: a root span `turn` (attributes
`brisvox.session.id`, `brisvox.agent`, `brisvox.turn.epoch`, `brisvox.turn.interrupted`,
`brisvox.turn.llm_rounds`, the response latency) and child spans `user_speech`, `end_of_turn`,
`stt_final`, `llm` (one per request of a tool loop), `tts`, `audio_out` and, after a barge-in,
`interruption`. The spans carry the real timing of the call's stages (the per-stage marks of
the V2V bench), built once when the turn is over: tracing adds no cost while a turn runs. An interrupted
turn is exported when the output reports silence (or when the next turn ends), so its
`interruption` span is complete.

### Logs

`BRISVOX_LOG_FORMAT=json` writes one JSON object per line. Every line is scrubbed before it is
written: the values of environment variables that look like secrets (`*_KEY`, `*_TOKEN`,
`*_SECRET`, `*_PASSWORD`, `*_CREDENTIAL`, `AUTH_*`) are replaced by `[REDACTED]` wherever they
occur, and so are values after credential-looking names (`api_key=`, `"token":"..."`,
`Authorization: Bearer ...`, `?key=...`) and the password of `scheme://user:password@host`. The
scrub over-redacts rather than under-redacts. Credentials in code are `brisvox_core::Secret`
anyway; this is the second line of defence.

The line `call admitted` (target `brisvox::server`, info, one per started call) carries the fields
`transport` (`websocket`, `webrtc`, `twilio`, ...), `session` (the engine session id) and
`call_id`: the transport's own id of the call, which for the websocket transport is the
server-assigned `ws-<n>` (a counter per listener, not the `call_id` a client may send in `start`,
which is only kept as call metadata `client_call_id`), and `-` for a transport that does not label
its calls. Use it to match the line to a benchmark or client log of the same call, then follow
`session` through the rest of the call's lines.

## Recording and privacy

Optional transcript and audio recording, PII redaction and retention. Off unless
`BRISVOX_RECORDING_DIR` is set; what is written, what the redaction does not do and a checklist for
the operator (not legal advice) are in `docs/PRIVACY.md`. A recording variable without the
directory is a start error.

| Variable | Default | Meaning |
|---|---|---|
| `BRISVOX_RECORDING_DIR` | unset | Directory for `call-*.transcript.jsonl`, `call-*.caller.wav`, `call-*.bot.wav`; created at start. |
| `BRISVOX_RECORDING_TRANSCRIPTS` | `true` | Write each call's transcript (older name `BRISVOX_RECORD_TRANSCRIPTS` still works). |
| `BRISVOX_RECORDING_AUDIO` | `false` | Write the caller's and the bot's audio (never redacted; older name `BRISVOX_RECORD_AUDIO`). |
| `BRISVOX_RECORDING_REDACT_PII` | `true` | Replace emails, IBANs, national IDs, card and phone numbers in transcripts before they are written (older name `BRISVOX_REDACT_PII`). |
| `BRISVOX_RECORDING_REDACT_KINDS` | all | Which kinds: `email,iban,id,card,phone,number`. |
| `BRISVOX_RECORDING_RETENTION` | `30d` | Delete recordings this old (`15m`, `36h`, `30d`, at least `1h`); `forever` keeps them. |
| `BRISVOX_RECORDING_SWEEP_INTERVAL` | `1h` | Time between retention sweeps (at least `1m`). |
| `BRISVOX_RECORDING_MAX_MINUTES` | `120` | Longest audio kept per call and track (1 to 600). |
| `BRISVOX_RECORDING_AGE_RECIPIENTS` | unset | Comma-separated age public keys (`age1...`): recordings are written encrypted as `<name>.age`; the server holds no private key. Needs a build with the cargo feature `recording-encryption` (`docs/PRIVACY.md`, "Encryption at rest"). |
| `BRISVOX_LOG_REDACT_PII` | `false` | Also scrub emails, IBANs, national IDs, card and phone numbers from log lines (`docs/PRIVACY.md`, "Logs"). |
| `BRISVOX_LOG_REDACT_KINDS` | `email,iban,id,card,phone` | The kinds the log scrub replaces; needs `BRISVOX_LOG_REDACT_PII=true`. |

The agent file's `[recording]` table (`docs/CONFIG.md`) overrides the content settings above. Use a
directory dedicated to the recordings: the retention sweeper reads every entry of it.
## Provider resilience settings

The circuit breakers and retry budgets of the resilience layer (`docs/PROVIDERS.md`).
The defaults are the ones in the table; each applies process-wide.

| Variable | Default | Meaning |
|---|---|---|
| `BRISVOX_BREAKER_FAILURES` | `5` | Consecutive failures that open a provider's breaker (1 to 1000). |
| `BRISVOX_BREAKER_OPEN_FOR` | `30s` | How long an open breaker refuses attempts before one probe is let through. |
| `BRISVOX_CALL_RETRY_BUDGET` | `3` | Retries (failovers, recoveries) one call may make. |
| `BRISVOX_CALL_RETRY_REFILL` | `20s` | One retry token comes back to a call per this long; `0s` keeps the budget full. |
| `BRISVOX_PROCESS_RETRY_BUDGET` | `50` | Retries the whole process may make. |
| `BRISVOX_PROCESS_RETRY_REFILL` | `100ms` | One token comes back to the process budget per this long; `0s` keeps it full. |
| `BRISVOX_PROVIDER_ATTEMPT_TIMEOUT` | `2s` | Deadline of a provider attempt that may be followed by another. |
| `BRISVOX_LLM_FIRST_EVENT` | `4s` | With a secondary LLM: silence after which a response is re-issued to the secondary. |
| `BRISVOX_PROVIDER_HEALTHY_AFTER` | `10s` | How long a session must live to count as healthy. |

The agent file names the secondaries (`[stt|llm|tts].secondary`, `docs/CONFIG.md`); these
variables set how eagerly the process fails over to them. Breaker states, the budget and the
failovers are on `/metrics` (above).

## Shutdown

Signals: `SIGTERM` or `SIGINT` on Unix; Ctrl-C or Ctrl-Break on Windows.

1. **Drain starts at once.** The session manager stops admitting: `/readyz` answers 503
   `draining`, and every listener answers new connections with HTTP 503 before the upgrade
   (see Admission; the caller retries elsewhere). The listeners stay bound, so running calls
   keep their connections. `/healthz` stays 200. An admission a listener reserved
   before the signal is honoured: that call runs and is drained like the others.
2. **Wait.** Running calls get `BRISVOX_DRAIN_TIMEOUT` (on the injected clock) to end by
   themselves. The drain ends as soon as none is left. A **second signal** ends this stage
   early (an operator who pressed Ctrl-C twice wants out).
3. **Cancel.** Every remaining session's cancellation token fires; sessions stop their pipeline
   (bounded by the engine's stop timeout) and return within `BRISVOX_CANCEL_GRACE`. The
   listeners' connections are closed at the same moment (the client gets the protocol's
   `stop`/close), and a connection still in its handshake gives its reservation back. The SIP
   listener hangs up its calls with a BYE (bounded by 2 s) while its signaling endpoint still
   runs, and an INVITE still waiting for its ACK gives its reservation back.
4. **Abort.** Sessions still running are aborted (their task is dropped; drop guards clean the
   registry) and get `BRISVOX_ABORT_WAIT`.
5. **Exit.** The listeners stop (each transport's shutdown is bounded by 2 s), the health
   listener closes (up to 1 s), then the tokio runtime is shut down
   with a 1 s bound (`RUNTIME_SHUTDOWN_TIMEOUT`): a blocking task left behind by an aborted
   session (inference, an offload pool shutdown) is abandoned instead of holding the exit, so
   the status below is still reported. The worst case is the sum of the three timeouts plus
   about seven seconds (the figure the container stop timeouts in `docs/DEPLOY.md` use).

Exit status:

| Status | Meaning |
|---|---|
| 0 | Every session ended by itself (or none was running). |
| 1 | Startup failed: invalid setting, agent definition invalid or naming tools, a provider that does not build, a listener or health address not bindable, no signal handler. |
| 3 | Some sessions were cancelled (drain timeout or second signal) and stopped in time. |
| 4 | Some sessions ignored cancellation and were aborted. |
| 5 | Some sessions were still registered after the abort stage (a bug; alert on it). |

The drain's result is logged once (`drain finished`, with `initial`, `drained`, `cancelled`,
`aborted`, `leaked`, `forced`, `elapsed`).

### Kubernetes

Set `terminationGracePeriodSeconds` above `drain + cancel_grace + abort_wait + 7 s`
(default 30 + 5 + 1 + 7 = 43 s; use 45 s or more), otherwise the kubelet's `SIGKILL` cuts the
drain. Calls are long: a drain timeout of several minutes with a matching grace period lets
them end naturally. The pod leaves the Service endpoints when it is deleted; the readiness
flip at drain start covers load balancers that only watch `/readyz`. No pre-stop sleep is
needed: the listeners refuse new calls with 503 from the first moment of the drain instead of
accepting and then dropping them.

Probes need no token (`/healthz`, `/readyz`); give Prometheus `BRISVOX_METRICS_TOKEN` from a
Secret.

```yaml
livenessProbe:  { httpGet: { path: /healthz, port: 8081 }, periodSeconds: 10 }
readinessProbe: { httpGet: { path: /readyz,  port: 8081 }, periodSeconds: 2 }
terminationGracePeriodSeconds: 45
```

### Docker / systemd

`docker stop` sends `SIGTERM` and kills after 10 s by default: pass `--time 45` (or
`stop_grace_period: 45s` in compose). For systemd use `KillSignal=SIGTERM` and
`TimeoutStopSec=45`.

The container image, its healthcheck and an example compose file with a websocket and a Twilio
agent are in `docs/DEPLOY.md`.
