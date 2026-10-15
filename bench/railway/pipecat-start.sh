#!/bin/bash
# The Pipecat container of the listening test (bench/railway/README.md): two bot processes and
# the container stats, sharing this container's CPU and memory.
#   load: synthetic calls from the load generator, private network [::]:9001, fake providers only
#         (a clean environment: it never sees a real key), 4 uvicorn workers (one event loop per
#         vCPU, as in the capacity benchmark);
#   real: the owner's call from the browser, public $PORT, real providers, one worker. It binds
#         0.0.0.0: Python binds "::" as IPv6 only, and the platform's public proxy connects over
#         IPv4 (the private network is IPv6, so the load process binds "::").
set -eu

FAKES=${FAKES_HOST:-fakes.railway.internal}
until curl -fsS "http://$FAKES:8080/ca.pem" -o /tmp/fakes-ca.pem; do
  echo "start: waiting for the fake providers at $FAKES"; sleep 2
done
sed "s/127.0.0.1:8443/$FAKES:8443/g" /app/bench/fake_providers/agent.fakes.toml > /tmp/agent.fakes.toml

brisvox-bench container-stats --bind '[::]:9100' &

cd /app/bench/pipecat_baseline
env -i PATH="$PATH" HOME=/tmp \
  FAKE_PROVIDERS_API_KEY=fake \
  SSL_CERT_FILE=/tmp/fakes-ca.pem \
  BRISVOX_MODELS_DIR=/opt/models \
  python bot.py --agent /tmp/agent.fakes.toml --env-file /dev/null \
    --host '::' --port 9001 --workers "${LOAD_WORKERS:-4}" --log-level WARNING &

BOT_ACCESS_TOKEN="$CALL_TOKEN" BRISVOX_MODELS_DIR=/opt/models \
  python bot.py --agent agent_live_en_aura.toml --env-file /dev/null \
    --host 0.0.0.0 --port "${PORT:-8080}" --workers 1 &

wait -n
echo "start: a process exited; stopping"
exit 1
