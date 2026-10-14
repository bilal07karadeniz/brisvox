# Docker and compose

The repository has a multi-stage `Dockerfile` and `deploy/compose.yaml`, which runs two services
from one image:

- `agent-ws`, a websocket agent for browsers and apps (the
  [websocket protocol](../reference/websocket-protocol.md));
- `agent-twilio`, a phone agent behind [Twilio Media Streams](../telephony/twilio.md) with
  Twilio call control (transfer, DTMF, hang-up).

```sh
cp deploy/ws.env.example deploy/ws.env              # fill in DEEPGRAM_API_KEY, OPENAI_API_KEY, CARTESIA_API_KEY
cp deploy/twilio.env.example deploy/twilio.env      # keys, Twilio settings and BRISVOX_TWILIO_STREAM_SECRET (required)
docker compose -f deploy/compose.yaml up --build
# or just one service:
docker compose -f deploy/compose.yaml up --build agent-ws
```

Every release also publishes the image, so nothing has to be built: set `BRISVOX_IMAGE` to
`ghcr.io/bilal07karadeniz/brisvox-server:vX.Y.Z` (or `:latest`) and start with `up --no-build`:

```sh
BRISVOX_IMAGE=ghcr.io/bilal07karadeniz/brisvox-server:latest \
  docker compose -f deploy/compose.yaml up --no-build agent-ws
```

`agent-twilio` does not start until `BRISVOX_TWILIO_STREAM_SECRET` (at least 32 random bytes, for
example `openssl rand -base64 48`) has a value in `deploy/twilio.env`; the same secret goes to your
voice webhook, which answers each call with a signed per-call stream URL
(`examples/phone-receptionist/twiml_server.py` is one; see [Twilio Media Streams](../telephony/twilio.md)),
and the proxy in front must route the per-call path `/twilio/stream*`
([Deploying in a container](../operations/deploy.md)).

Check that the websocket agent is up:

```sh
curl -i http://127.0.0.1:8081/readyz
```

As written, both services publish their ports on `127.0.0.1` only. The generic websocket
listener has **no authentication of its own** (compose runs it with `BRISVOX_INSECURE_LISTENERS=1`
because its port is loopback-only; set `BRISVOX_SESSION_SECRET` before publishing it) and Twilio needs a public `wss://` URL, so for
anything beyond your own machine put a TLS-terminating reverse proxy in front. The container
runs as a non-root user with a read-only root filesystem, no capabilities and a tmpfs `/tmp`;
it drains on `SIGTERM` (the compose file's `stop_grace_period: 45s` is longer than the drain).

The image bakes in the Smart Turn model and two reference agent definitions
(`deploy/agents/web.toml`, `deploy/agents/phone.toml`); mount your own over
`/etc/brisvox/agent.toml` to change the agent.

Everything about the image (build arguments and feature sets, size, health checks, TLS and
reverse-proxy notes, Kubernetes probes) is in [Deploying in a container](../operations/deploy.md);
every setting is in [Operations](../operations.md).

To add your own tools, run an [SDK agent](sdk-agent.md) next to the container and set
`BRISVOX_AGENT_URL` (and `BRISVOX_AGENT_TOKEN`) on the service.
