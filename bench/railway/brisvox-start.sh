#!/bin/bash
# The brisvox container of the listening test (bench/railway/README.md): two brisvox-server
# processes and the container stats, sharing this container's CPU and memory.
#   load: synthetic calls from the load generator, private network [::]:9000, fake providers only
#         (a clean environment: it never sees a real key);
#   real: the owner's call from the browser, public $PORT, real providers.
set -eu

FAKES=${FAKES_HOST:-fakes.railway.internal}
until curl -fsS "http://$FAKES:8080/ca.pem" -o /tmp/fakes-ca.pem; do
  echo "start: waiting for the fake providers at $FAKES"; sleep 2
done
sed "s/127.0.0.1:8443/$FAKES:8443/g" /etc/bench/agent.fakes.toml > /tmp/agent.fakes.toml

brisvox-bench container-stats --bind '[::]:9100' &

env -i PATH="$PATH" HOME=/tmp \
  BRISVOX_AGENT_FILE=/tmp/agent.fakes.toml \
  FAKE_PROVIDERS_API_KEY=fake \
  BRISVOX_EXTRA_CA_FILE=/tmp/fakes-ca.pem \
  BRISVOX_MODELS_DIR=/opt/brisvox/models \
  BRISVOX_WS_ADDR='[::]:9000' \
  BRISVOX_WS_TOKEN="$LOAD_TOKEN" \
  BRISVOX_HEALTH_ADDR=off \
  BRISVOX_MAX_SESSIONS=1000 \
  BRISVOX_LOG_FORMAT=json \
  RUST_LOG=warn \
  brisvox-server &

BRISVOX_AGENT_FILE=/etc/bench/agent.real.toml \
  BRISVOX_MODELS_DIR=/opt/brisvox/models \
  BRISVOX_WS_ADDR="[::]:${PORT:-8080}" \
  BRISVOX_WS_TOKEN="$CALL_TOKEN" \
  BRISVOX_HEALTH_ADDR=off \
  BRISVOX_MAX_SESSIONS=4 \
  brisvox-server &

# One process gone: stop the container (Railway restarts it).
wait -n
echo "start: a process exited; stopping"
exit 1
