# Deploying brisvox-server in a container

The container image, the example compose file and how to put them behind TLS.
The process itself (settings, listeners, admission, drain, exit status) is
`docs/OPERATIONS.md`; agent definitions are `docs/CONFIG.md`; the carriers are
`docs/TELEPHONY.md`.

Files: `Dockerfile`, `.dockerignore`, `deploy/compose.yaml`, `deploy/agents/{web,phone}.toml`,
`deploy/{ws,twilio}.env.example`, `deploy/healthcheck.sh`.

## The image

```sh
docker build -t brisvox/server .
# another feature set (the SIP trunk listener, a smaller provider list):
docker build -t brisvox/server --build-arg BRISVOX_FEATURES=all-providers,call-control,smart-turn-ort,sip .
```

Build arguments: `BRISVOX_FEATURES` (cargo features of `brisvox-server` on top of its defaults
`mimalloc`, `yaml`, `silero-tract`; default `all-providers,call-control,smart-turn-ort`),
`STRIP_BINARY` (`1`), `RUST_VERSION` (`1.99.0`, checked against `rust-toolchain.toml` so the
two cannot drift), `DEBIAN_RELEASE` (`trixie`; not `bookworm`, see below), `BUILD_JOBS` (empty: cargo uses every core; `4` caps rustc jobs for small build hosts), `CARGO_CHEF_VERSION` (`0.1.78`),
`PYTHON_VERSION` (`3.12`). The build needs network access (crates, the ONNX Runtime binaries
that `ort` downloads, the Smart Turn model) and BuildKit (`docker build` on current Docker).

| Stage | Base | Does |
|---|---|---|
| `chef` | `rust:1.99.0-slim-trixie` | compiler, `build-essential`, `cmake`, `cargo-chef` |
| `planner` | chef | `cargo chef prepare`: the dependency recipe (`recipe.json`) |
| `builder` | chef | `cargo chef cook` (dependencies only, cached until a `Cargo.toml`/`Cargo.lock` changes), then `cargo build --release --locked -p brisvox-server`, stripped |
| `models` | `python:3.12-slim-trixie` | `scripts/models/fetch_models.py`: pinned downloads, SHA-256 verified, the build fails on a mismatch |
| `runtime` | `debian:trixie-slim` | `ca-certificates`, `libstdc++6`, a non-root user, the binary, the model |

**What is in the runtime image.** One binary (`/usr/local/bin/brisvox-server`, uid/gid 10001,
exec-form `ENTRYPOINT`, so it is PID 1 and gets `SIGTERM` itself), the Smart Turn model
(`/opt/brisvox/models/smart-turn-v3.2-cpu.onnx`, 8.7 MB, `BRISVOX_MODELS_DIR`), the two
reference agent definitions in `/etc/brisvox/agents/`, the health probe script, CA
certificates (TLS to the providers and the carriers) and `libstdc++6` (the ONNX Runtime of
`smart-turn-ort` is linked statically but needs the C++ runtime). The Silero VAD is compiled
into the binary. No shell tooling beyond what debian-slim has, no curl, no package manager
state (apt lists are removed).

**Size (measured 2026-10-13, `docker build` on WSL2/Docker 29.8, default features).**
`docker images`: 64.5 MB of compressed content, 160.7 MB unpacked (`du -sx /` in the container;
the `DISK USAGE` column, 231 MB, also counts the containerd snapshot copy). Layers: debian
trixie-slim 87.7 MB, `ca-certificates` + `libstdc++6` 4.7 MB, stripped `brisvox-server`
65.4 MB (every provider adapter, the Silero VAD, mimalloc, the static ONNX Runtime), Smart
Turn model 8.7 MB, the probe and the reference agents 45 kB. The first estimate of this page
(90 to 120 MB) was low: the static ONNX Runtime makes the binary 65 MB. Build time on 4 cargo
jobs (`--build-arg BUILD_JOBS=4`): 411 s cold (dependency layer, binary and model); a source-only
change rebuilds just the last cargo layer.

**Why trixie (Debian 13) and not bookworm.** With bookworm the link step failed: the prebuilt
ONNX Runtime that `ort` downloads for `smart-turn-ort` references glibc 2.38 symbols
(`__isoc23_strtol`, `__isoc23_strtoull`) and a GCC 13 `libstdc++`
(`std::__cxx11::basic_string::_M_replace_cold`), and bookworm has glibc 2.36 and GCC 12. The
build stage and the runtime stage must both be trixie (the runtime's `libstdc++6` must be at
least as new as the one the binary was linked against). The `DEBIAN_RELEASE` build argument
still selects the release, so this can be revisited when `ort` ships older-glibc binaries.

**Why debian-slim and not distroless.** `distroless/cc` would save about 15 MB, but it has no
shell, so the `HEALTHCHECK` would need either a probe subcommand in the binary or a separate
static binary. `deploy/healthcheck.sh` uses bash's `/dev/tcp`, which debian-slim has, and the
orchestrators that do their own HTTP probes (Kubernetes) can ignore it. If the size matters
more, switch the runtime stage to `gcr.io/distroless/cc-debian12:nonroot`, copy `libstdc++`
if needed and drop the `HEALTHCHECK` for an external probe.

**Models: baked in, not mounted.** The model is 8.7 MB, pinned by hash and tied to the
`smart-turn-ort` feature of this binary; baking it makes the image self-contained and
immutable (no volume to provision, no unverified file at startup). To use another model mount
it and set `BRISVOX_SMART_TURN_MODEL`, or set it to `off`.

**Healthcheck.** `HEALTHCHECK` runs `deploy/healthcheck.sh`: `GET /healthz` on
`BRISVOX_HEALTH_ADDR` (default `0.0.0.0:8081`), healthy on a `200`. The probe dials the configured
host, or loopback when that is a wildcard (`0.0.0.0`, `[::]`, empty), so a specific bind such as
`10.0.0.5:8081` or `[::1]:8081` stays healthy.
`/healthz` is liveness: it stays `200` during a drain and when the server is full, so docker
never restarts a draining container. Load balancers that route calls should watch `/readyz`
(`503` while draining, full, overloaded or out of a provider budget). Probe timing: every 10 s,
4 s timeout, 15 s start period, 3 retries.

**Stopping.** `STOPSIGNAL SIGTERM` starts the drain (docs/OPERATIONS.md, "Shutdown"). Give the
container a grace period above `BRISVOX_DRAIN_TIMEOUT + BRISVOX_CANCEL_GRACE +
BRISVOX_ABORT_WAIT + 7 s` (43 s by default): `docker stop -t 45`, compose
`stop_grace_period: 45s`, Kubernetes `terminationGracePeriodSeconds: 45`. For long calls raise
the drain timeout and the grace period together.

**Configuration** is environment variables (docs/OPERATIONS.md) and one agent file mounted at
`BRISVOX_AGENT_FILE` (default `/etc/brisvox/agent.toml`; the image does not contain one at
that path, mount yours, or point the variable at a reference file such as
`/etc/brisvox/agents/web.toml`). A host that cannot mount a file (a one-port PaaS such as Railway)
passes it in a variable or bakes it in with a one-line Dockerfile layer: see "The agent definition"
in "Platforms with one HTTPS port and no UDP (PaaS)" below. The image enables no call listener: choose one with
`BRISVOX_WS_ADDR`, `BRISVOX_TWILIO_ADDR`, `BRISVOX_TELNYX_ADDR`, `BRISVOX_RTVI_ADDR`
(`BRISVOX_SIP_ADDR` with the `sip` feature). Inside a container a listener must bind
`0.0.0.0` to be reachable through a published port.

The generic websocket listener (and RTVI, WebRTC signaling, Telnyx) refuses to bind a
non-loopback address without credentials (`docs/SECURITY.md`): inside a container that means
`BRISVOX_SESSION_SECRET` (or the listener's static token), or `BRISVOX_INSECURE_LISTENERS=1` when
the published port is loopback-only, as below. `/metrics` is off on the container's `0.0.0.0:8081`
health address until `BRISVOX_METRICS_TOKEN` is set; `/healthz` and `/readyz` work as before.

```sh
docker run --rm -p 127.0.0.1:8080:8080 -p 127.0.0.1:8081:8081 --stop-timeout 45   --ulimit nofile=65536:65536 \
  -e BRISVOX_WS_ADDR=0.0.0.0:8080 -e BRISVOX_INSECURE_LISTENERS=1 --env-file deploy/ws.env \
  -v "$PWD/deploy/agents/web.toml:/etc/brisvox/agent.toml:ro" brisvox/server
```

## Compose: a websocket agent and a Twilio agent

`deploy/compose.yaml` runs the same image twice with different definitions and listeners:

| Service | Listener | Agent | Publishes (loopback) |
|---|---|---|---|
| `agent-ws` | generic websocket, `0.0.0.0:8080` | `deploy/agents/web.toml`: Deepgram + OpenAI + Cartesia | `8080` (calls), `8081` (health) |
| `agent-twilio` | Twilio Media Streams, `0.0.0.0:8082`, path `/twilio/stream` | `deploy/agents/phone.toml`: AssemblyAI + Groq + Deepgram Aura, tools `transfer_call` and `end_call` over Twilio call control | `8082` (calls), `8083` (health) |

```sh
cp deploy/ws.env.example deploy/ws.env            # keys for agent-ws
cp deploy/twilio.env.example deploy/twilio.env    # keys and Twilio settings for agent-twilio
docker compose -f deploy/compose.yaml up --build
curl http://127.0.0.1:8081/readyz
```

`deploy/*.env` is git-ignored (`deploy/.gitignore`); the templates carry names only, no
values. Both services run hardened (read-only root filesystem with a small `/tmp` tmpfs, all
capabilities dropped, `no-new-privileges`, a process limit) with a 45 s stop grace period.
Edit the two agent files for your prompt, voices and destinations: they are validated like the
files in `examples/agents`. The Twilio file's `+1555...` number is a placeholder.

Twilio setup: set `BRISVOX_TWILIO_PUBLIC_URL=wss://agent.example.com/twilio/stream` and a
`BRISVOX_TWILIO_STREAM_SECRET` of at least 32 random bytes. Your number's voice webhook answers
every call with TwiML whose stream URL is that URL plus a **per-call segment**,
`<Connect><Stream url="wss://agent.example.com/twilio/stream/CAxxxx.1760000120.mac"/></Connect>`,
which your webhook computes from the call's `CallSid` (a few lines of HMAC:
`docs/providers/twilio.md`, "Per-call stream URLs"; a ready webhook that does it, with tests, is
`examples/phone-receptionist/twiml_server.py`, which reads the same
`BRISVOX_TWILIO_STREAM_SECRET`). The server checks the segment (one use, until
its expiry, for that call only) and `X-Twilio-Signature` over the whole URL, so a proxy must not
change the path (so the proxy route must cover the per-call path, see the Caddyfile below), and the
URL must be the public one, not the container's. The secret is not optional: the Twilio listener does
not start without `BRISVOX_TWILIO_STREAM_SECRET`, so `deploy/twilio.env.example` has the line
active and empty. A static TwiML Bin that cannot compute the segment needs
`BRISVOX_TWILIO_ALLOW_STATIC_URL=1` instead (and no secret), which accepts the fixed URL and its
replayable signature; use it for a first try only.

## TLS and authentication in front

Twilio needs `wss://`, and the generic websocket listener refuses to start on a non-loopback
address without credentials (`docs/SECURITY.md`): anyone who reaches an open one runs your agent
on your provider keys. `agent-ws` in `deploy/compose.yaml` sets `BRISVOX_INSECURE_LISTENERS=1`
because compose publishes its ports on `127.0.0.1` only. To expose it, put a reverse proxy on
the host that terminates TLS, set `BRISVOX_SESSION_SECRET` in `deploy/ws.env` (your backend mints
single-use session tokens; `docs/SECURITY.md`) and, if you like, authenticate at the proxy too.
**WebRTC signaling must always be behind HTTPS** (the DTLS fingerprint travels in it) and the
proxy must be listed in `BRISVOX_TRUSTED_PROXIES` so the per-address limits see the visitor. For
example with Caddy:

```
agent.example.com {
    # With the star: every call connects to its own path, /twilio/stream/<segment>. An exact
    # `/twilio/stream` would answer those with 404.
    handle /twilio/stream* {
        reverse_proxy 127.0.0.1:8082
    }
    # Your voice webhook (examples/phone-receptionist/twiml_server.py), if it runs next to the server:
    handle /voice {
        reverse_proxy 127.0.0.1:8000
    }
    handle /ws {
        # authenticate here (basic_auth, forward_auth, mTLS, ...), then:
        reverse_proxy 127.0.0.1:8080
    }
}
```

Caddy and nginx upgrade websocket connections without extra settings in this form (nginx needs
the usual `Upgrade`/`Connection` headers and a `proxy_read_timeout` above the longest silence
you expect; the server pings every 10 s). The health ports (`8081`, `8083`) carry counts only
and are open for probes, but should not be public either; `/metrics` on them needs
`BRISVOX_METRICS_TOKEN` (set it in the env files, then scrape with
`Authorization: Bearer <token>`).

## Sizing

- **Calls per container** are bounded by `BRISVOX_MAX_SESSIONS` (default 256) and, before
  that, by CPU: the inference pool takes half the cores the container can use, and admission
  refuses new calls when it falls behind (`BRISVOX_OVERLOAD_*`, docs/OPERATIONS.md). Rust
  reads cgroup CPU quotas, so `cpus: 4` gives a pool of two threads. Load-test with your agent
  (docs/LATENCY.md) and set `BRISVOX_MAX_SESSIONS` and `BRISVOX_PROVIDER_LIMITS` from the
  result; scale out with more containers rather than a larger limit.
- **File descriptors.** Every open call is at least one socket, and Docker gives a container a
  soft `RLIMIT_NOFILE` of **1024**. A process that keeps that limit stops accepting at about
  1017 calls (`accept()` fails with `EMFILE`, the callers see "connect timed out", CPU is idle:
  measured in the capacity run, which first stopped at exactly 1017 concurrent
  calls, `bench/results/capacity-brisvox-nofile1024-linux-2026-10-13.md`). `brisvox-server`
  raises its own soft limit to the hard limit at startup (an `open-file limit`
  line at `info` with the before and after, a `warn` when it cannot; it never fails the start;
  Windows has no such limit and does nothing; `src/fdlimit.rs`, the `rlimit` crate, MIT, Unix
  only). That only helps up to the **hard** limit, which you still set: keep
  `--ulimit nofile=65536:65536` (compose: `ulimits.nofile`, already in `deploy/compose.yaml`;
  Kubernetes: the container runtime's default or a `LimitNOFILE` drop-in on the node) to at
  least `2 x BRISVOX_MAX_SESSIONS` plus a margin for provider sockets. A container whose hard
  limit is also 1024 (`--ulimit nofile=1024:1024`) cannot be helped by the server.
- **Memory** is per call (audio buffers, STT/LLM/TTS sessions) plus the models; set a
  container memory limit from your load test and leave headroom.
- **SIP** (feature `sip`) needs UDP: publish `BRISVOX_SIP_ADDR`'s port and the whole
  `BRISVOX_SIP_RTP_PORTS` range, set `BRISVOX_SIP_PUBLIC_IP`, and use host networking or a
  load balancer that preserves source addresses. Docker's userland proxy is a poor fit for
  RTP at scale. SIPS (`BRISVOX_SIP_TLS_ADDR`) is TCP: publish its port too, mount the certificate
  chain and key as files (`BRISVOX_SIP_TLS_CERT`, `BRISVOX_SIP_TLS_KEY`; read once at startup) and
  terminate nothing in front of it (a TLS-terminating load balancer would see the SDES keys and the
  carrier's source address is lost). Rotating the certificate is a restart (a drain loses no call).

## Kubernetes

The same image works: set `BRISVOX_*` through a `Secret`/`ConfigMap`, mount the agent file,
and use the probes from docs/OPERATIONS.md (`/healthz` liveness, `/readyz` readiness, port
`8081`, `terminationGracePeriodSeconds: 45` or more). The image's `HEALTHCHECK` is ignored by
Kubernetes.

## WebRTC: making the server reachable from browsers

The browser initiates the connection, the server is ICE-lite and only answers. So a call needs a
path *to the server*: a public UDP port (`BRISVOX_WEBRTC_UDP_ADDR`, advertised as
`BRISVOX_WEBRTC_CANDIDATE_IP`), or a public TCP port for ICE-TCP (`BRISVOX_WEBRTC_TCP_ADDR`).
Protocol facts and sources: `docs/providers/webrtc.md` section 11; every variable: "WebRTC
reachability" in `docs/CONFIG.md`.

| Client network | What gets a call through |
|---|---|
| Open internet, any NAT, server with public UDP | nothing extra (the default) |
| UDP blocked, TCP out allowed to the server's port | ICE-TCP (`BRISVOX_WEBRTC_TCP_ADDR`), or TURN over TLS/443 |
| Strict firewall (443 only, TLS inspection) or symmetric NAT | TURN `turns:` on 443 (`BRISVOX_TURN_PROVIDER`) |
| Anything where a web page loads (one HTTP(S) port, HTTPS-only networks) | **nothing to configure**: the bundled pages fall back to a WebSocket on the signaling port (below) |
| Server host has no inbound UDP (a one-port PaaS such as Railway) | the WebSocket fallback (below); to keep WebRTC media: the server's own TURN relay (`BRISVOX_WEBRTC_TURN_RELAY=only`, below), ICE-TCP through a TCP proxy, the websocket/RTVI transports, or media on a UDP host; a *browser* TURN alone does **not** help |

### WebSocket fallback on the signaling port

The signaling listener also accepts a WebSocket upgrade on `BRISVOX_WEBRTC_WS_PATH` (default
`/ws`, `off` to disable) and serves the generic WebSocket audio protocol there
(`docs/WEBSOCKET_PROTOCOL.md`): the same port, so the same HTTP proxy, TLS certificate and domain
that already carry the page and `POST /whip`. The browser module `sdk/web/brisvox-call.js`
(`sdk/web/README.md`; a website imports it, and the bundled pages `/demo` and
`examples/web-assistant/web/index.html` are built on it) tries WebRTC first and switches to `wss://<same host>/ws`
when `RTCPeerConnection` is missing or throws, the WHIP request fails at the network level, or
the connection is not `connected` after 4 seconds (or `failed`). The page then shows "Connected
(WebSocket fallback)". Nothing else has to be reachable: no UDP, no second TCP port, no TURN.

- **What it costs.** Audio is 24 kHz PCM16 over TCP (48 KB/s each way) instead of Opus over UDP:
  more bandwidth, TCP head-of-line blocking after a lost packet, and no browser jitter
  buffer or loss concealment (the page keeps an 80 ms buffer of its own, at most 400 ms). On a
  clean network it sounds the same; on a lossy one WebRTC is better. Echo cancellation is the
  browser's, on the microphone, as with WebRTC.
- **Same protection as `POST /whip`.** The same static token or session tokens (audience
  `brisvox-webrtc`, single use), the same `Origin`/CORS rule, offer rate, per-address call cap,
  handshake pool and `BRISVOX_MAX_SESSIONS` admission: `docs/SECURITY.md`. A session token that
  opened a WebRTC session which then never connected (the page hangs it up with `DELETE` before
  falling back) is given back, once, before the `DELETE` is answered, so the page presents the
  same token to the WebSocket. A session that timed out, closed or connected keeps its token
  spent. The page's 4 s timer can fire while the browser is still finishing ICE and the server has
  already connected: the WebSocket attempt then gets `403`, and the page says that the token was
  already used and that your backend should mint a new one. If you wrap the page, mint a fresh
  token for each attempt (WebRTC, then fallback) when you can.
- **Behind a proxy**: it must pass the HTTP/1.1 `Upgrade` (Railway, Fly, nginx with `proxy_set_header
  Upgrade`, Cloudflare all do) and keep an idle connection for longer than 30 s (the server pings
  every 10 s). Set `BRISVOX_TRUSTED_PROXIES` so per-address limits see visitors, and keep the
  `Host` header (the default `Origin` rule compares it with the page's origin; if your proxy
  rewrites it, set `BRISVOX_WEBRTC_CORS_ORIGIN` to the page's origin).
- **When TURN is still worth it.** The fallback is the right answer for "it must work wherever
  a page loads". TURN (or the server's own relay below) keeps the lower latency and the Opus and
  loss handling of WebRTC for visitors behind UDP-blocking firewalls and symmetric NATs, at the
  cost of a TURN provider; ICE-TCP is the middle road when you can expose a TCP port. Start with
  the fallback; add TURN if you measure a quality or latency problem on those networks.

Two limits to know before choosing a WebRTC reachability option:

- **TURN relays UDP to the server.** A relayed browser still sends its media to the server's
  *UDP* candidate, from the TURN server's address. TURN fixes the client side (blocked UDP,
  symmetric NAT) but needs the server to have inbound UDP. Browsers cannot relay to a TCP
  candidate. A server with no UDP at all needs the server's own TURN client ("The server's own
  TURN relay" below).
- **ICE-TCP is the second choice.** The server advertises UDP with a higher priority, so UDP is
  used whenever it works. TCP adds head-of-line blocking to real-time audio: expect worse
  glitches under loss than UDP.

### Browser TURN credentials

Set `BRISVOX_TURN_PROVIDER` and the server hands short-lived credentials to the page, on the
WHIP `201` as `Link: <turns:...>; rel="ice-server"; username="..."; credential="..."` (RFC 9725
section 4.6) and as JSON from `GET /whip/ice` (`{"iceServers":[...]}`, same bearer token as
`POST /whip`, `Cache-Control: no-store`). The demo page and `examples/web-assistant` fetch
`/whip/ice` before creating the offer. The long-lived secret never leaves the server; a provider
outage only means no relay is offered (logged, the call still connects).

- **coturn** (self-hosted, no per-minute fee): `BRISVOX_TURN_PROVIDER=coturn`,
  `BRISVOX_TURN_URLS=turns:turn.example.com:443?transport=tcp,turn:turn.example.com:3478?transport=udp`,
  `BRISVOX_TURN_SECRET=<static-auth-secret>`. Credentials are computed locally (no network call).
- **Cloudflare Realtime TURN**: in the Cloudflare dashboard create a TURN key (Realtime > TURN);
  you get a key id and an API token. `BRISVOX_TURN_PROVIDER=cloudflare`,
  `BRISVOX_TURN_CLOUDFLARE_KEY_ID`, `BRISVOX_TURN_CLOUDFLARE_API_TOKEN`. Its URLs include
  `turns:turn.cloudflare.com:443?transport=tcp`, which passes most firewalls.
- **Twilio Network Traversal Service**: `BRISVOX_TURN_PROVIDER=twilio`,
  `BRISVOX_TURN_TWILIO_ACCOUNT_SID`, `BRISVOX_TURN_TWILIO_AUTH_TOKEN` (or an API key:
  `BRISVOX_TURN_TWILIO_API_KEY_SID` plus its secret as the auth token). Twilio offers `turn:` on
  443/TCP but no `turns:` (no TLS), which TLS-inspecting firewalls may refuse.

`BRISVOX_TURN_TTL` (seconds, 30 to 86400, default 600) is the credential lifetime. Set it above
your longest call: the browser refreshes its relay allocation with the same credential, so a
call that outlives it can lose the relay (expected from the REST-credential scheme, not tested
against a live TURN server). Fetched (Cloudflare, Twilio) credentials are shared for 60 s by all
callers; coturn credentials are computed per request.

#### Running coturn with TLS on 443

On a small VPS (public IP, ports 443/tcp, 3478/udp+tcp and the relay range open). `turnserver.conf`:

```
realm=turn.example.com
use-auth-secret
static-auth-secret=<long random secret, the same as BRISVOX_TURN_SECRET>
listening-port=3478
tls-listening-port=443
cert=/etc/letsencrypt/live/turn.example.com/fullchain.pem
pkey=/etc/letsencrypt/live/turn.example.com/privkey.pem
min-port=49160
max-port=49200
no-cli
fingerprint
```

Run it with `CAP_NET_BIND_SERVICE` (or as root) to bind 443, and keep 443 for coturn alone: if
the same host also serves HTTPS, give coturn its own IP or put it on another host. coturn relays
to the media address of the brisvox server, so the relay must be allowed to reach it
(`allowed-peer-ip` / `denied-peer-ip` in coturn; deny private ranges you do not want browsers to
reach through your relay and allow the brisvox server's address). Option names are from the
coturn README fetched 2026-10-13 for `use-auth-secret`, `static-auth-secret`,
`tls-listening-port`, `cert`, `pkey` and `realm`; check the rest against your coturn version.
This exact file was not run.

### Per-host recipes

**VPS or bare metal (public IP).** The simplest and the best for latency.

```
BRISVOX_WEBRTC_ADDR=127.0.0.1:8088          # signaling: behind your TLS proxy (microphones need HTTPS)
BRISVOX_WEBRTC_UDP_ADDR=0.0.0.0:50000       # media: open 50000/udp in the firewall and cloud security group
BRISVOX_WEBRTC_CANDIDATE_IP=203.0.113.10    # the public IPv4 of the host
BRISVOX_WEBRTC_TOKEN=...                    # or session tokens (BRISVOX_SESSION_SECRET)
```

Add `BRISVOX_WEBRTC_TCP_ADDR=0.0.0.0:50000` (TCP and UDP ports do not clash, so the same number
works) to also accept ICE-TCP, and a TURN provider for strict client networks. In Docker use
`network_mode: host` or publish `50000/udp` (and `/tcp`) explicitly; the userland proxy is a poor
fit at scale (see SIP above).

**Fly.io.** UDP works (docs: https://docs.fly.io/networking/udp-and-tcp, read 2026-10-13) with
three conditions: a **dedicated IPv4** address on the app, the UDP socket bound to the
`fly-global-services` address (not `0.0.0.0`), and the same external and internal port. In
`fly.toml`:

```toml
[[services]]            # signaling over HTTPS
  protocol = "tcp"
  internal_port = 8088
  [[services.ports]]
    port = 443
    handlers = ["tls", "http"]

[[services]]            # media
  protocol = "udp"
  internal_port = 50000
  [[services.ports]]
    port = 50000
```

`BRISVOX_WEBRTC_UDP_ADDR` takes an IP, not a host name: resolve the address in the entrypoint,
for example `export BRISVOX_WEBRTC_UDP_ADDR="$(getent hosts fly-global-services | cut -d' ' -f1):50000"`,
and set `BRISVOX_WEBRTC_CANDIDATE_IP` to the app's dedicated IPv4. Assume about 1300 bytes of UDP
payload (Fly's note); brisvox's packets are under 1200. Not deployed to Fly.

**Kubernetes.** Media is one UDP port per pod and the SDP answer names the address of the pod
that handled the `POST`, so signaling can go through any Ingress, but media must reach *that*
pod directly: do not put the media port behind a load balancer that spreads UDP across pods.
Run one pod per node with `hostPort: 50000` (UDP, and TCP for ICE-TCP) or `hostNetwork: true`,
and set `BRISVOX_WEBRTC_CANDIDATE_IP` per pod from the node's public address (the downward API
gives `status.hostIP`, which is the *internal* node IP on most clouds: use the node's external
IP, e.g. from a label your provisioner sets). Open the port in the cloud firewall. TURN
credentials need no cluster support. Use the probes from docs/OPERATIONS.md as above.

**One HTTPS port and no UDP (Railway and similar platforms).** A host that exposes only HTTP(S)
cannot receive WebRTC media. The call continues on the WebSocket fallback above, or on the server's
own TURN relay; the whole recipe is the next section, "Platforms with one HTTPS port and no UDP
(PaaS)".

## Platforms with one HTTPS port and no UDP (PaaS)

Many application platforms (Railway is the example used below) give a service exactly this: a public
domain whose TLS is terminated at the platform's edge, HTTP and WebSockets forwarded to one port
inside the container (the platform tells the container which one in `PORT`), a private network
between the services of a project, environment variables instead of mounted files, a build done by
the platform, and **no inbound UDP** from the internet. This section is what to know to run
`brisvox-server` there for a website voice button (or any client that only speaks HTTPS), in general
terms; the facts that differ per platform are in "Examples" at the end. Hosts that can receive UDP
(a VPS, Fly.io, Kubernetes with `hostPort`) are in "Per-host recipes" above and give lower latency.

### How a call works there

```
browser --https / wss--> platform edge --> brisvox-server  (one port: page, POST /whip, GET /ws)
                                               |
                                               +--ws://<tools service, private network>:8765--> your SDK agent (optional)
```

The browser talks only to the one HTTPS domain. The WebRTC signaling listener
(`BRISVOX_WEBRTC_ADDR`) serves the bundled page, `POST /whip` and the WebSocket audio endpoint `/ws`
on that port. WebRTC media cannot reach the server, so the call runs on WebSocket audio on the same
port ("WebSocket fallback on the signaling port" above). Two ways to get there from the page:

- **Say so in the page.** `new BrisvoxCall({ ..., transport: "websocket" })` skips the WebRTC attempt:
  no 4 s wait, no second session token. The bundled `/demo` page does the same with `?force=ws`.
  This is the recommendation on a host known to have no UDP.
- **Leave `transport: "auto"`.** The WebRTC attempt cannot connect, and after 4 s (or when ICE fails)
  the call continues on `wss://<your-domain>/ws` with a second session token.

The media options, best first:

1. **One HTTP port, WebSocket audio** (above). Nothing else is needed. The server still wants a media
   socket and an address to advertise: `BRISVOX_WEBRTC_UDP_ADDR=127.0.0.1:0` is a socket nobody can
   reach (WebRTC cannot connect, which is the point here). The other transports also work over the
   HTTPS port (`BRISVOX_WS_ADDR`, `BRISVOX_RTVI_ADDR`, for your own clients and the Pipecat SDKs),
   or put a small brisvox instance with `BRISVOX_WEBRTC_*` on a UDP host (VPS, Fly.io) for WebRTC
   media with lower latency.
2. **The server's own TURN relay** (`BRISVOX_WEBRTC_TURN_RELAY=only`): WebRTC media over TLS/443
   through a managed TURN service, with nothing extra on the client; described below. Not yet run
   against a real TURN service.
3. **ICE-TCP through a TCP proxy**, when the platform offers one. Add a TCP proxy to the service for
   the ICE-TCP port, then `BRISVOX_WEBRTC_TCP_ADDR=0.0.0.0:<the service's internal port>`,
   `BRISVOX_WEBRTC_TCP_CANDIDATE_PORT=<the proxy's public port>` and
   `BRISVOX_WEBRTC_CANDIDATE_IP=<the IP the proxy domain resolves to>` (an IP, not a name; check it
   stays stable), and `BRISVOX_WEBRTC_TCP_MAX_PER_IP=0`: every connection comes from the proxy, so
   the default per-address cap of 16 would limit all browsers together. The UDP candidate advertised
   next to it is unreachable and is simply skipped by the browser. Limits: clients must be able to
   reach a non-443 TCP port (many corporate networks cannot), TCP proxy idle timeouts may not be
   documented (a live call exchanges packets every few seconds), the server sees the proxy's address
   instead of the client's, and a TURN relay does not help (it cannot reach a TCP candidate).
   Untested against a platform; the TCP path is tested with an in-process str0m client.

A *browser* TURN provider alone (`BRISVOX_TURN_PROVIDER` for the page) does **not** help: TURN relays
UDP to the server, which has no UDP ingress.

### Build

- **Features.** The image default (`all-providers,call-control,smart-turn-ort`) has **no `webrtc`**,
  and a binary without it refuses `BRISVOX_WEBRTC_ADDR` at start. Build with
  `BRISVOX_FEATURES=all-providers,smart-turn-ort,webrtc` (the browser row of "What to build with" in
  the README); `webrtc` compiles libopus (cmake and a C compiler are in the build stage). Add
  `call-control` if the same service also takes phone calls (transfer, DTMF, hang-up); add `sip` only
  on a UDP host. A full Docker build with `webrtc` has not been run by the project (the documented
  pass used `sip`): watch the first build. Whether platform variables reach the build depends on the
  platform: a Dockerfile `ARG` is the portable way, and ours declares `BRISVOX_FEATURES`,
  `BUILD_JOBS` and `STRIP_BINARY` in its build stage.
- **`BUILD_JOBS`.** Optional cap on rustc jobs (for example `2`); a full build peaks near 1 GB of RAM
  per job and a platform's builder size is often not documented.
- **Why `webrtc` for a WebSocket call.** The page, `POST /whip` and the `/ws` fallback all live in
  the WebRTC signaling listener. If you only need your own clients to speak the WebSocket protocol
  (`docs/WEBSOCKET_PROTOCOL.md`), the generic `BRISVOX_WS_ADDR` listener needs no `webrtc` feature
  (no libopus, shorter build), but it has no bundled page, no `Origin` rule, no per-address limits
  and does not enforce a token's `max_s` (`docs/SECURITY.md`); use `BRISVOX_WEBRTC_ADDR` for
  browsers.
- **Build time.** The image compiles the whole server. The project measured 411 s cold on a 32-core
  host with `BUILD_JOBS=4` (the section "The image"). A platform's build timeout and builder size
  may be unknown or small, so a cold build on a small plan can run out of time. Then use a prebuilt
  image: build once (locally or in CI), push it to a registry the platform can pull from (this
  repository does not publish an image), and give the platform a repository of one file (see "The
  agent definition").

### The agent definition

A platform service usually cannot mount a file, and the image has none at `BRISVOX_AGENT_FILE`'s
default path. The options, simplest first:

- *A variable:* paste the agent file's text into `BRISVOX_AGENT_DEFINITION` (multi-line values work
  on platforms that allow them) and leave `BRISVOX_AGENT_FILE` unset; `BRISVOX_AGENT_FORMAT=yaml`
  for a YAML definition. Setting both variables is an error. Check it first with
  `BRISVOX_AGENT_DEFINITION="$(cat agent.toml)" brisvox-server check --env`. Changing the agent is
  then a variable edit and a redeploy, no image build. `docs/CONFIG.md`, "The agent definition from
  the environment".
- *A fork or your own copy of the repository:* put your file in `deploy/agents/` (the Dockerfile
  copies that directory to `/etc/brisvox/agents/`) and set
  `BRISVOX_AGENT_FILE=/etc/brisvox/agents/my-agent.toml`. Nothing else changes.
- *A prebuilt image plus a one-line layer:* a repository of one file:

  ```dockerfile
  FROM ghcr.io/YOUR-ORG/brisvox-server:YOUR-TAG
  COPY agent.toml /etc/brisvox/agent.toml
  ```

  With `BRISVOX_AGENT_FILE=/etc/brisvox/agent.toml` (the image's default). The build is seconds
  instead of minutes and does not depend on the platform's builder limits.
- *A platform volume* also works for a file, but check who owns it (the image runs as uid 10001) and
  whether a volume makes a redeploy interrupt the service. Prefer baking the file in.

Secrets stay out of the file: the definition names variables (`api_key_env`), and the keys are the
platform's variables.

### Server settings that matter on such a host

```
PORT=...                                     # injected by the platform (set it yourself if its docs say so)
BRISVOX_WEBRTC_ADDR=0.0.0.0:${PORT}          # the server replaces the exact token ${PORT}
BRISVOX_WEBRTC_UDP_ADDR=127.0.0.1:0          # a media socket nobody can reach; the call uses /ws
BRISVOX_AGENT_DEFINITION=<the agent file's text>     # or BRISVOX_AGENT_FILE=<path of a baked-in file>
BRISVOX_SESSION_SECRET=<at least 32 random bytes>   # your backend mints single-use tokens (docs/SECURITY.md)
BRISVOX_WEBRTC_CORS_ORIGIN=https://www.example.com  # only when the page is on another origin
BRISVOX_MAX_SESSIONS=64                      # from a load test; the default is 256
DEEPGRAM_API_KEY=...                         # whatever the agent file's api_key_env names
BRISVOX_AGENT_URL=ws://<tools service>:8765  # the SDK agent, if you run one
BRISVOX_AGENT_TOKEN=...                      # the same token on both sides
```

For a quick demo `BRISVOX_WEBRTC_TOKEN=<token>` replaces the session secret; anyone who has the token
can start calls on your provider keys, so use session tokens for a real site.

- **Listen on the platform's port.** The platform routes its public domain to the one port the
  service listens on and tells you which in `PORT`. Write the placeholder instead of a number:
  `BRISVOX_WEBRTC_ADDR=0.0.0.0:${PORT}` (or `[::]:${PORT}`). The exact token `${PORT}` in any
  listener address variable is replaced by the value of `PORT`; if `PORT` is unset or not a port
  number the start fails naming both variables. The server does this itself because the container
  starts the binary directly, with no shell to expand `$PORT`. Details: `docs/OPERATIONS.md`, "Port
  placeholder". Platforms often have their own reference syntax for variables (Railway's is
  `${{...}}`); a single-brace `${PORT}` reaches the server as written, and writing the port out
  (`0.0.0.0:8080`) works too. A mismatch between the listening port and the platform's target port is
  a 502 from the platform's edge. The listener must bind `0.0.0.0` (or `::`), never loopback.
- **Credentials.** Off loopback the signaling listener refuses to start without
  `BRISVOX_SESSION_SECRET` or a static token (`docs/SECURITY.md`). Your backend mints single-use
  session tokens for the page.
- **Health check.** A platform probe reaches the service's public port, not the health listener:
  `/healthz` and `/readyz` are on `BRISVOX_HEALTH_ADDR` (port 8081), which the probe cannot reach,
  and the image's `HEALTHCHECK` is not used. Set the platform's health check path to `/demo`: the
  signaling listener answers `GET /demo` with 200 and no credentials (the bundled page;
  `BRISVOX_WEBRTC_*` as above). For liveness and readiness during operation keep
  `BRISVOX_HEALTH_ADDR` on the private network and scrape or probe it from another service (`/readyz`
  is 503 while draining or full); `/metrics` needs `BRISVOX_METRICS_TOKEN` there.
- **Client addresses and the proxy.** Behind the platform's edge the TCP peer is the edge, so the
  per-address limits (5 offers a minute, 8 calls at once, 8 signaling connections per address;
  `docs/SECURITY.md`) would treat every visitor as one client, and the fallback uses two offers per
  call. brisvox reads the client address from a header (`BRISVOX_CLIENT_IP_HEADER`: `x-real-ip`,
  `x-forwarded-for` or `forwarded`) only on connections from addresses listed in
  `BRISVOX_TRUSTED_PROXIES`. That needs two facts from the platform: which header its edge writes
  (and overwrites when a client sends one), and which addresses its proxies connect from. Many
  platforms document the first and not the second. So:

  1. *Safe default (the proxy range is not published).* Leave `BRISVOX_TRUSTED_PROXIES` unset and
     relax the per-address limits that would throttle all visitors together:
     `BRISVOX_WEBRTC_OFFERS_PER_MINUTE=off`, `BRISVOX_WEBRTC_MAX_CALLS_PER_IP=off`,
     `BRISVOX_WEBRTC_HTTP_MAX_PER_IP=0`. Abuse control is then the single-use session token (your
     backend decides who gets one), `BRISVOX_MAX_SESSIONS`, the handshake pool and
     `BRISVOX_PROVIDER_LIMITS`; add rate limiting where you mint tokens.
  2. *Per-visitor limits through the header.* Set `BRISVOX_CLIENT_IP_HEADER` to the header the edge
     writes and list the proxy addresses you observe in your own logs and nothing wider in
     `BRISVOX_TRUSTED_PROXIES`. Never `0.0.0.0/0`. Requests that arrive over the platform's private
     network do not pass the edge, so any service of your project can send its own header; if your
     project's own services fall in the range you trusted, they are trusted too. Use this only when
     no other service of the project is reachable by strangers, and re-check the range after the
     platform changes, since an unpublished range is not a contract.

- **WebSockets through the proxy.** The proxy must pass the HTTP/1.1 `Upgrade` and keep an idle
  connection for longer than 30 s (the server pings every 10 s); keep the `Host` header or set
  `BRISVOX_WEBRTC_CORS_ORIGIN` (see "WebSocket fallback on the signaling port").
- **One replica.** The single-use token record, the sessions and the admission counters live in the
  process (`docs/SECURITY.md`), and a platform may not offer sticky routing for HTTP domains. Run
  one replica and scale by splitting traffic per service.
- **Deploys and live calls.** On a new deployment a platform sends `SIGTERM` to the old instance
  and `SIGKILL` after its own drain time; if that time is short, every open call is cut. Set the
  platform's drain time to the server's drain budget: at least 45 s (the default drain is
  `BRISVOX_DRAIN_TIMEOUT` 30 s + `BRISVOX_CANCEL_GRACE` + `BRISVOX_ABORT_WAIT` + 7 s = 43 s), more if
  you raise `BRISVOX_DRAIN_TIMEOUT` for long calls (and check the platform's maximum). The drain
  stops new calls and lets the open ones finish (`docs/OPERATIONS.md`, "Shutdown"). A browser whose
  call is cut gets a closed WebSocket and your page should offer to reconnect.

### The server's own TURN relay

For a host that has HTTP(S) and nothing else inbound (a one-port PaaS, as in this section), the server itself is a TURN
*client*. Per call it opens one TLS connection to a managed TURN service, allocates a relayed UDP
address there, creates permissions for every IP in the browser's offer, and advertises that relayed
address in the SDP answer as a host candidate. The browser sends its media to the TURN service's
public address; the service forwards it down the TLS connection to the server, and the server's
packets go back the same way (ChannelData). Design, bounds and failure behaviour: the `relay`
module docs and "WebRTC transport" in `docs/ARCHITECTURE.md`; protocol facts and sources:
`docs/providers/webrtc.md` section 12; every variable: "WebRTC reachability" in `docs/CONFIG.md`.

Example with Cloudflare Realtime TURN (create a TURN key in the Cloudflare dashboard,
Realtime > TURN):

```
PORT=8080                                      # injected by the platform, or set it yourself
BRISVOX_WEBRTC_ADDR=0.0.0.0:${PORT}            # signaling over the platform's HTTPS domain (microphones need HTTPS)
BRISVOX_WEBRTC_UDP_ADDR=0.0.0.0:50000          # bound but unreachable; not advertised in `only` mode
BRISVOX_WEBRTC_TURN_RELAY=only                 # no direct UDP candidate; a failed allocation answers 503
BRISVOX_TURN_PROVIDER=cloudflare
BRISVOX_TURN_CLOUDFLARE_KEY_ID=...
BRISVOX_TURN_CLOUDFLARE_API_TOKEN=...          # a secret: a platform variable, never in the repo
BRISVOX_WEBRTC_TOKEN=...                       # relay traffic costs money: do not leave signaling open
```

- The same provider serves both sides: the *browser* is told the TURN URLs too (`GET /whip/ice`),
  and the *server* picks the best `turn:`/`turns:` URL over TCP from the provider's answer, `turns:`
  on port 443 first (Cloudflare lists `turns:turn.cloudflare.com:443?transport=tcp`). A provider
  that offers only UDP URLs (Twilio lists `turn:` on 443/TCP only) works for the server through
  that `turn:` URL, unencrypted at the TURN layer (the media is still DTLS-SRTP end to end).
- `BRISVOX_WEBRTC_TURN_RELAY=on` keeps the direct candidates (set `BRISVOX_WEBRTC_CANDIDATE_IP`)
  and adds the relayed one at a lower priority: a fallback for servers that are *probably*
  reachable. `only` is for hosts that are certainly not.
- `BRISVOX_WEBRTC_TURN_RELAY_TIMEOUT` (default 3 s) bounds the whole setup of one call. On expiry
  the call is answered with its direct candidates (`on`) or refused with `503 Retry-After`
  (`only`), and `relay_setup_failures` counts it.
- **Latency.** A relayed packet makes two extra hops (browser to the TURN edge, edge to your server
  over a TCP connection) instead of one. Expect tens of milliseconds more than a direct path,
  depending on where the TURN service is relative to the server and the caller (not measured);
  TCP head-of-line blocking after a lost packet adds more, in bursts.
  The call setup adds about six round trips to the TURN service (TCP, TLS, allocate with its
  authentication challenge, permissions) before the answer is sent: budget a few hundred ms.
- **Cost.** Cloudflare Realtime TURN bills the data the TURN server sends to a TURN client
  ($0.05/GB after 1000 GB free per month, fetched 2026-10-13): here that is the caller's audio
  towards your server (and, if the browser also relays, towards the browser). Opus at the default
  settings is a few tens of kbit/s, i.e. well under 20 MB per call-hour per direction. One TLS
  connection and one allocation exist per live call; check the provider's allocation and
  connection limits for your call volume.
- **What it needs from the offer.** The browser's offer must carry its candidates (a complete,
  non-trickle offer: the bundled pages wait for ICE gathering) because a TURN service drops data
  from an IP without a permission, and the permissions are created before the answer is sent.
  Browsers hide host addresses behind mDNS names; their server-reflexive and relay candidates (which
  appear when the page has TURN or STUN servers, as with `GET /whip/ice`) carry the IPs that
  matter. An offer without any IP candidate gets no relay.
- **Secrets and logs.** The relay uses the credentials the provider issues; they are never logged
  (a log-capture test at `trace` over a whole call finds neither the password nor the shared
  secret). The TURN client crates' own events (`turn_client_proto`, `stun_proto`, `turn_types`)
  are chatty and, at `trace`, print whole STUN messages (nonce, integrity HMAC), so the server
  drops them below `warn` **even when you set `RUST_LOG`** (`BRISVOX_LOG_TURN_CLIENT=1` lifts
  that, for debugging). **If you embed the library and install your own `tracing` subscriber you
  must apply the same filter**: `filter_fn(brisvox_transport::webrtc::turn_client_log_allowed)`
  next to your level filter (`FilterExt::and`); a `RUST_LOG` directive does not do it.
- **Trust and bounds.** The TURN URLs come from your configuration or the provider's API; the
  server connects to whatever host they name, so do not point `BRISVOX_TURN_*` at an untrusted
  endpoint (and restrict egress if the provider path is not trusted). Per call the server asks the
  TURN service for at most 8 permissions, for the public (non-loopback, non-link-local) IPs of
  the offer's candidates in the allocation's address family: that is the cost a hostile offer can
  put on your TURN quota, after the per-IP offer rate limit. Queues are 20 packets (400 ms) per
  direction; `relay_queue_slow` and `relay_slow_writes` count a TURN connection that is too slow.
- Not verified against a real service yet: the tests run the client against an
  RFC 8656 server implementation in-process, over TCP and TLS.

### Your tools service (SDK agent)

The tools are an optional second service in the same project; without `BRISVOX_AGENT_URL` every
call runs on the agent file alone (no tools of your own). Give it no public domain; the server
reaches it over the platform's private network. Use `ws://` (a private network is encrypted by some
platforms, and brisvox accepts plain `ws://` for `*.internal` hosts, `docs/AGENT_PROTOCOL.md`) and
the token:

- **Bind address.** Both SDKs listen on **127.0.0.1 by default**
  (`sdk/typescript/src/agent.ts`, `serve`: `options.host ?? "127.0.0.1"`; Python: `host="127.0.0.1"`),
  which is unreachable from another service, and the symptom is quiet: the server's handshake
  with the agent times out and calls run on the agent file alone, without your tools. Bind all
  interfaces:
  - TypeScript: `await agent.run({ host: "::", port: 8765 })`
  - Python: `agent.run(host="::", port=8765)`

  `::` is dual-stack on Linux, so it accepts IPv4 and IPv6; use it when the private network may be
  IPv6-only, otherwise `0.0.0.0` is enough (*the library notes recommend `::`; not tested here*).
- **Token.** Set `BRISVOX_AGENT_TOKEN` on both services (a shared variable or a reference); the
  SDKs read it, and the server sends it as `Authorization: Bearer`.
- **Port.** Any port; the URL names it (`:8765`). The private network needs no exposed port.
- **Health check.** None: the SDK agent answers a plain HTTP request with an error status (the
  TypeScript one with `426`), which a platform health check would count as unhealthy. Leave the
  platform's health check off for this service.
- **Image.** The SDKs are not on PyPI or npm. Build the service from your own repository with the
  SDK vendored into it (`pip install ./sdk/python` after copying the directory; `npm install` a
  built `sdk/typescript`), as in `book/src/quickstart/sdk-agent.md`. Two ready Dockerfiles show it,
  both built with the **repository root as the build context** (a service whose source is your clone
  or fork, with the platform told which Dockerfile to use): `examples/booking-agent/Dockerfile`
  (Python: `pip install` of the copied `sdk/python`, the booking agent) and
  `sdk/typescript/examples/Dockerfile` (TypeScript: `npm ci && npm run build` of the copied
  `sdk/typescript` in a build stage, compiled `dist` and production `node_modules` in the final one).
  Both listen on `0.0.0.0` (pass `--host ::` on an IPv6-only private network), run as a non-root
  user and need no health check. For your own agent, copy one of them and swap the `COPY` lines of
  the example for your code.
- **Restarts.** The agent link reconnects an agent that declared `resume`
  (`docs/AGENT_PROTOCOL.md`); a call whose agent is unreachable at its start falls back to the
  agent file alone.

### Checklist

| Service | Setting | Meaning |
|---|---|---|
| brisvox | `BRISVOX_FEATURES` (build) | Cargo features of the image; must include `webrtc` for browsers. |
| brisvox | `BUILD_JOBS` (build) | Optional cap on rustc jobs. |
| brisvox | `PORT` | The port the platform routes its domain to; equals the domain's target port. |
| brisvox | `BRISVOX_AGENT_DEFINITION` or `BRISVOX_AGENT_FILE` | The agent definition's text, or the path of a baked-in file (not both). |
| brisvox | `BRISVOX_WEBRTC_ADDR` | `0.0.0.0:${PORT}`: page, `POST /whip` and the `/ws` fallback on the public port. |
| brisvox | `BRISVOX_WEBRTC_UDP_ADDR` | `127.0.0.1:0`: a media socket nobody can reach. |
| brisvox | `BRISVOX_SESSION_SECRET` or `BRISVOX_WEBRTC_TOKEN` | Credentials for starting a call (required on a non-loopback address). |
| brisvox | `BRISVOX_WEBRTC_CORS_ORIGIN` | The page's origin when it is not served by this service. |
| brisvox | `BRISVOX_TRUSTED_PROXIES`, `BRISVOX_CLIENT_IP_HEADER` | See "Client addresses and the proxy"; unset by default. |
| brisvox | `BRISVOX_WEBRTC_OFFERS_PER_MINUTE`, `BRISVOX_WEBRTC_MAX_CALLS_PER_IP`, `BRISVOX_WEBRTC_HTTP_MAX_PER_IP` | Per-address limits; relaxed when the proxy's header cannot be trusted. |
| brisvox | `BRISVOX_MAX_SESSIONS`, `BRISVOX_PROVIDER_LIMITS` | Admission and per-provider concurrency. |
| brisvox | provider keys | The variables the agent file's `api_key_env` names. |
| brisvox | `BRISVOX_AGENT_URL`, `BRISVOX_AGENT_TOKEN` | The SDK agent (second service) and its bearer token. |
| brisvox | `BRISVOX_METRICS_TOKEN` | Only if you scrape `/metrics` on the health address. |
| platform | drain time | At least 45 s between `SIGTERM` and `SIGKILL`. |
| tools | `BRISVOX_AGENT_TOKEN` | The same token. |
| tools | your own | Database URL and the like. |

Also: the health check path is `/demo` on brisvox and none on the tools service; one replica; the
tools service binds `::` or `0.0.0.0`.

### Examples

What differs per platform, from the platform's own documentation. Only platforms the project has
researched are listed.

**Railway.** Facts read from docs.railway.com on 2026-10-14 (what is only a forum report is marked as such); **this recipe has
not been deployed to Railway by the project**, and the points marked *unverified* are the ones to
test first. Railway offers HTTP/HTTPS with WebSockets on a public domain, a TCP proxy, a private
network between the services of a project, and no inbound UDP from the internet (its docs list none;
staff and community threads say so; re-checked 2026-10-13).

- *Build.* Railway uses the `Dockerfile` in the repository root when there is one (the build log
  says "Using detected Dockerfile"; another path: the variable `RAILWAY_DOCKERFILE_PATH`). Service
  variables reach a Docker build only when the Dockerfile declares them with `ARG`, so
  `BRISVOX_FEATURES` and `BUILD_JOBS` are service variables. A private registry for a prebuilt image
  needs Railway's Pro plan. Railway's build timeout and builder size are not in its documentation (a
  forum report puts the Hobby limit near 20 minutes: *unverified*). A volume is mounted as root at
  run time (Railway's `RAILWAY_RUN_UID=0` is the documented workaround for the uid 10001 of the
  image) and makes a redeploy interrupt the service.
- *`PORT`.* Railway injects `PORT` and routes the public domain to the port the service listens on;
  its documentation does not state the default value (reports say 8080: *unverified*) and says to
  set `PORT` yourself when you use a domain target port. Set `PORT=8080` and the domain's target
  port to 8080. Railway's own references use double braces (`${{...}}`); a single-brace `${PORT}`
  reaches the server as written (*not verified on Railway*; `0.0.0.0:8080` written out works too).
- *Health check.* Railway probes the service's `PORT`, only at deploy time, from the host
  `healthcheck.railway.app`; set the path to `/demo`. Not run against Railway's probe: *unverified*.
- *Client addresses.* Railway documents `X-Real-IP` as the client-address header and overwrites a
  client-sent one; it does not list `X-Forwarded-For`, publishes **no address range for its
  proxies** and says the `100.x` peer addresses seen in practice are not a contract (staff answer,
  2026-10). So option 1 above is the safe default; for option 2 use
  `BRISVOX_CLIENT_IP_HEADER=x-real-ip` and, for example,
  `BRISVOX_TRUSTED_PROXIES=100.64.0.0/10` (the CGNAT block the reported `100.x` edge peers fall in;
  *unverified as a contract*: re-check after Railway changes). Requests over `*.railway.internal` do
  not pass the edge, and the edge peers seen so far were `100.x` addresses, so your project's own
  services may fall in the same block.
- *WebSockets.* Railway's WebSocket connections are not subject to its HTTP duration and idle limits
  (HTTP/1.1 only); the domain accepts 10,000 concurrent connections.
- *Drain.* By default Railway gives the old deployment **0 seconds** between `SIGTERM` and
  `SIGKILL`. Set `RAILWAY_DEPLOYMENT_DRAINING_SECONDS=60` (at least 45).
- *Private network.* The hostname is `<service-name>.railway.internal` (in a variable:
  `RAILWAY_PRIVATE_DOMAIN`). A reference to another service's variable is `${{tools.X}}`:
  `BRISVOX_AGENT_URL=ws://${{tools.RAILWAY_PRIVATE_DOMAIN}}:8765` and
  `BRISVOX_AGENT_TOKEN=${{tools.BRISVOX_AGENT_TOKEN}}`; a database is `${{Postgres.DATABASE_URL}}`.
  Railway's private DNS resolves to IPv4 and IPv6 in environments created after 2025-10-16 and to
  IPv6 only in older ones, so a tools service bound to `0.0.0.0` alone can be unreachable in an old
  environment: bind `::`.
- *Checklist.* Domain target port 8080; health check path `/demo` on brisvox and none on tools; one
  replica; draining seconds set; the agent URL uses `.railway.internal` of the tools service; the
  SDK agent binds `::`.

**Fly.io.** Fly can receive UDP (with a dedicated IPv4 address), so it does not need this section
for media: see "Per-host recipes".

## Verification status

Static checks (no Docker): the agent files load through the server's own validator with
every provider feature on; `cargo check --locked -p brisvox-server --features
all-providers,call-control,smart-turn-ort` passes; the compose file parses; `healthcheck.sh`
passes `bash -n`.

Live pass on a Docker host (2026-10-13, WSL2 Ubuntu, kernel 6.6.87.2, Docker 29.8.0,
Compose 5.5.1, no real provider keys: placeholder values satisfy the "key must be set" check
and are never used for a network call; see "What was not tested"):

| check | result |
|---|---|
| `docker build -t brisvox/server .` | first attempt failed to link (bookworm, see above); fixed by trixie; then OK in 411 s, image 160.7 MB unpacked / 64.5 MB compressed |
| `hadolint/hadolint` on the Dockerfile | clean after the two intended `DL3008` (unpinned apt packages) were annotated with `# hadolint ignore=DL3008` |
| `docker compose -f deploy/compose.yaml config` | valid; both services resolve |
| `up agent-ws` | starts, `(healthy)` after the first probe, `/healthz` 200, `/readyz` 200 `{"status":"ready","accepting":true,...}`, agent `web-agent` loaded with `Vad::Silero` and Smart Turn on; runs read-only, all capabilities dropped, `no-new-privileges`, 53 MB RSS idle, 8 pids |
| websocket handshake (`ws://127.0.0.1:8080/`, `start` message) | `101 Switching Protocols`, then a `started` message; the call then fails at Deepgram (placeholder key) and the STT gives up reconnecting, the call stays open |
| `docker stop -t 45` with one call open | the drain waited for the call: `DrainReport { initial: 1, drained: 1, cancelled: 0, aborted: 0, leaked: 0, forced: false, elapsed: 23.8s }`, `brisvox-server exiting exit_code: 0`, container exit code 0, not OOM-killed, 24.5 s in all (inside the 45 s grace) |
| `docker stop` with no call | 0.67 s, exit code 0 |
| `up agent-twilio` with empty keys | fails fast with `TWILIO_AUTH_TOKEN is required` (the restart policy then retries: set the env file first) |
| `up agent-twilio` with placeholder keys | starts, healthy, Twilio listener on :8082, `CallControls { twilio: true }`, health on host port 8083 |
| `--cpus=4` | the inference pool takes 2 threads (`/readyz` `pressure.threads`), unlimited: 16 on the 32-core host |
| open-file limit | Docker's default inside the container is 1024 (soft), which capped a container at 1017 calls in the capacity run; compose now sets `ulimits.nofile` 65536 (`/proc/<pid>/limits` of the started container reads 65536) |
| no agent file mounted | clean error: `cannot open the file: No such file or directory`, non-zero exit |

Fixes made by this pass: `DEBIAN_RELEASE` bookworm -> trixie (the link failure), compose `ulimits.nofile`, a `BUILD_JOBS`
build argument (a 32-core build host with 12 GB of RAM cannot run 32 rustc jobs), hadolint
annotations, compose project `name: brisvox` (was the directory name `deploy`), and
`docker stop -t` (`--time` is deprecated in Docker 29).

What was not tested: a full call with real providers (needs keys: the live provider passes
need provider keys), the `sip` feature image, Kubernetes, a TLS proxy in front.
