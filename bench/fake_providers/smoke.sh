#!/usr/bin/env bash
# Smoke test of the fake providers: one call of brisvox-server and one call of the Pipecat baseline,
# both through the fakes over TLS, with `brisvox-bench v2v` (fixture en_two_utterances, two turns).
#
#   bench/fake_providers/smoke.sh
#
# Needs: brisvox-fake-providers, brisvox-server (features all-providers,smart-turn-ort), brisvox-bench
# (cargo build -p brisvox-fake-providers -p brisvox-server -p brisvox-bench ...), models/ (scripts/models/fetch_models.py),
# and the Pipecat venv (bench/pipecat_baseline/README.md "Setup"). Environment overrides:
#   BIN_DIR   directory of the three binaries (default: target/release, else target/debug)
#   PCPY      Python of the Pipecat venv (default: ../pcvenv/bin/python, or ../pcvenv/Scripts/python.exe)
#   WORK      scratch directory (default: a new temp directory)
#   SKIP_PIPECAT=1  only the brisvox call
# Ports: fakes 8080 (plain) and 8443 (TLS), brisvox-server 8090 (health 8091), the Pipecat bot 8765.
set -u
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
BIN_DIR=${BIN_DIR:-}
if [ -z "$BIN_DIR" ]; then
  if [ -x target/release/brisvox-server ] || [ -x target/release/brisvox-server.exe ]; then BIN_DIR=target/release; else BIN_DIR=target/debug; fi
fi
EXE=""; [ -x "$BIN_DIR/brisvox-server.exe" ] && EXE=.exe
PCPY=${PCPY:-}
if [ -z "$PCPY" ]; then
  if [ -x ../pcvenv/bin/python ]; then PCPY=../pcvenv/bin/python; else PCPY=../pcvenv/Scripts/python.exe; fi
fi
WORK=${WORK:-$(mktemp -d)}
MODELS=${BRISVOX_MODELS_DIR:-$ROOT/models}
mkdir -p "$WORK"
PIDS=""
cleanup() {
  for p in $PIDS; do
    kill "$p" 2>/dev/null
    # Git Bash on Windows: a native process outlives `kill` of its MSYS pid
    if [ -r "/proc/$p/winpid" ]; then taskkill //F //PID "$(cat "/proc/$p/winpid")" >/dev/null 2>&1; fi
  done
}
trap cleanup EXIT
fail=0

echo "== fakes (TLS 8443, plain 8080), work dir $WORK"
"$BIN_DIR/brisvox-fake-providers$EXE" --fixtures-dir fixtures --ca-dir "$WORK/pki" --plain-addr 127.0.0.1:8080 --tls-addr 127.0.0.1:8443 >"$WORK/fakes.log" 2>&1 &
PIDS="$PIDS $!"
for _ in $(seq 1 50); do curl -sf http://127.0.0.1:8080/healthz >/dev/null 2>&1 && break; sleep 0.2; done
curl -sf http://127.0.0.1:8080/healthz >/dev/null || { echo "fakes did not start:"; cat "$WORK/fakes.log"; exit 1; }
CA="$WORK/pki/ca.pem"

echo "== wire check (plain Python clients)"
"$PCPY" bench/fake_providers/check_wire.py --url https://127.0.0.1:8443 --ca "$CA" || fail=1

echo "== brisvox-server through the fakes"
BRISVOX_AGENT_FILE="$ROOT/bench/fake_providers/agent.fakes.toml" BRISVOX_WS_ADDR=127.0.0.1:8090 BRISVOX_HEALTH_ADDR=127.0.0.1:8091 \
  BRISVOX_MODELS_DIR="$MODELS" BRISVOX_EXTRA_CA_FILE="$CA" FAKE_PROVIDERS_API_KEY=fake-key \
  "$BIN_DIR/brisvox-server$EXE" >"$WORK/brisvox-server.log" 2>&1 &
BPID=$!; PIDS="$PIDS $BPID"
for _ in $(seq 1 100); do curl -sf http://127.0.0.1:8091/readyz >/dev/null 2>&1 && break; sleep 0.2; done
"$BIN_DIR/brisvox-bench$EXE" v2v --url ws://127.0.0.1:8090 --fixtures en_two_utterances --runs 1 --min-gap-ms 5000 \
  --out-md "$WORK/v2v-brisvox.md" --out-json "$WORK/v2v-brisvox.json" || fail=1
echo "brisvox: turns/answered/.../p50/p90 ms: $(grep -m1 '^| all ' "$WORK/v2v-brisvox.md")"
grep -q '^| all | 2 | 2 | ' "$WORK/v2v-brisvox.md" || { echo "brisvox did not answer both turns"; fail=1; }
kill "$BPID" 2>/dev/null

if [ "${SKIP_PIPECAT:-0}" != 1 ]; then
  echo "== Pipecat baseline through the fakes"
  (cd bench/pipecat_baseline && BRISVOX_MODELS_DIR="$MODELS" SSL_CERT_FILE="$CA" FAKE_PROVIDERS_API_KEY=fake-key \
    exec "$PCPY" bot.py --agent ../fake_providers/agent.fakes.toml --port 8765 >"$WORK/pipecat.log" 2>&1) &
  PPID_=$!; PIDS="$PIDS $PPID_"
  for _ in $(seq 1 150); do grep -q "pipeline ok" "$WORK/pipecat.log" 2>/dev/null && break; sleep 0.2; done
  "$BIN_DIR/brisvox-bench$EXE" v2v --url ws://127.0.0.1:8765 --fixtures en_two_utterances --runs 1 --min-gap-ms 5000 \
    --out-md "$WORK/v2v-pipecat.md" --out-json "$WORK/v2v-pipecat.json" || fail=1
  echo "pipecat: turns/answered/.../p50/p90 ms: $(grep -m1 '^| all ' "$WORK/v2v-pipecat.md")"
  grep -q '^| all | 2 | 2 | ' "$WORK/v2v-pipecat.md" || { echo "pipecat did not answer both turns"; fail=1; }
fi

echo "== what the fakes saw"
curl -s http://127.0.0.1:8080/metrics | grep -E "^fake_(stt_streams|tts_streams|llm_requests)_total|^fake_tls_(handshakes|failures)_total|^fake_bad"
echo "results and logs in $WORK"
if [ "$fail" = 0 ]; then echo "SMOKE OK"; else echo "SMOKE FAILED"; fi
exit "$fail"
