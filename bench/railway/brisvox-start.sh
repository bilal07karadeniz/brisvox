#!/bin/bash
# The brisvox container of the listening test (bench/railway/README.md): ONE brisvox-server
# process serving two agents, so the owner's real call and the synthetic calls share one
# inference pool, one Smart Turn executor and one admission control:
#   - the public port ($PORT) is the WebRTC signaling listener; the browser's call comes in on
#     its WebSocket fallback (/ws) and runs the main agent with the REAL providers (the PaaS
#     forwards no UDP, so the media socket stays on loopback and only /ws is used);
#   - the private port 9000 is the generic websocket listener; the load generator's synthetic
#     calls run BRISVOX_WS_AGENT_FILE, the agent pointed at the fake providers (its key variable
#     is FAKE_PROVIDERS_API_KEY, which no real provider accepts).
# The process holds the real keys; synthetic calls can only reach the fake-provider agent (their
# listener runs nothing else, and they hold only LOAD_TOKEN).
set -eu
: "${CALL_TOKEN:?CALL_TOKEN must be set}" "${LOAD_TOKEN:?LOAD_TOKEN must be set}"

FAKES=${FAKES_HOST:-fakes.railway.internal}
until curl -fsS "http://$FAKES:8080/ca.pem" -o /tmp/fakes-ca.pem; do
  echo "start: waiting for the fake providers at $FAKES"; sleep 2
done
sed "s/127.0.0.1:8443/$FAKES:8443/g" /etc/bench/agent.fakes.toml > /tmp/agent.fakes.toml

brisvox-bench container-stats --bind '[::]:9100' &

# Optional tuning from Railway variables (empty: the server's defaults).
TUNING=()
[ -n "${LOAD_SMART_TURN_THREADS:-}" ] && TUNING+=("BRISVOX_SMART_TURN_THREADS=$LOAD_SMART_TURN_THREADS")
[ -n "${LOAD_MAX_SESSIONS:-}" ] && TUNING+=("BRISVOX_MAX_SESSIONS=$LOAD_MAX_SESSIONS")
# The control page calls from the load generator's domain.
[ -n "${LOADGEN_PUBLIC:-}" ] && TUNING+=("BRISVOX_WEBRTC_CORS_ORIGIN=https://$LOADGEN_PUBLIC")

env -i PATH="$PATH" HOME=/tmp \
  DEEPGRAM_API_KEY="${DEEPGRAM_API_KEY:-}" \
  OPENAI_COMPATIBLE_API_KEY="${OPENAI_COMPATIBLE_API_KEY:-}" \
  FAKE_PROVIDERS_API_KEY=fake \
  BRISVOX_AGENT_FILE=/etc/bench/agent.real.toml \
  BRISVOX_WS_AGENT_FILE=/tmp/agent.fakes.toml \
  BRISVOX_EXTRA_CA_FILE=/tmp/fakes-ca.pem \
  BRISVOX_MODELS_DIR=/opt/brisvox/models \
  BRISVOX_WS_ADDR='[::]:9000' \
  BRISVOX_WS_TOKEN="$LOAD_TOKEN" \
  BRISVOX_WEBRTC_ADDR="[::]:${PORT:-8080}" \
  BRISVOX_WEBRTC_TOKEN="$CALL_TOKEN" \
  BRISVOX_WEBRTC_UDP_ADDR=127.0.0.1:40000 \
  BRISVOX_HEALTH_ADDR=off \
  BRISVOX_LOG_FORMAT=json \
  RUST_LOG=warn,brisvox::server=info \
  "${TUNING[@]}" \
  brisvox-server &

wait -n
echo "start: a process exited; stopping"
exit 1
