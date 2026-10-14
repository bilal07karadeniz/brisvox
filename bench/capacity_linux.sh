#!/usr/bin/env bash
# The capacity benchmark of brisvox and Pipecat on Linux, each in a container with the same
# CPU and memory limits, both over key-free mock providers with the same latencies.
#
#   bench/capacity_linux.sh                 # 4 CPUs, 4 GiB, both systems
#   CPUS=2 MEM=2g STEPS=1,5,10,20,40 bench/capacity_linux.sh
#   SYSTEMS=brisvox bench/capacity_linux.sh # one system only
#   MODELS=1 bench/capacity_linux.sh        # the real Silero VAD + Smart Turn models
#                                           # (needs models/ from scripts/models/fetch_models.py;
#                                           # result files get the suffix -models)
#
# Needs Docker on a Linux host with at least CPUS + 4 cores (the load generator runs on the host,
# pinned away from the server's cores). Results go to bench/results/capacity-<system>-linux-
# <date>-run<N>.{md,json} (one pair per repeat; RUNS=3 repeats the ramp) and bench/capacity_summary.py
# folds the repeats into capacity-<system>-linux-<date>.{md,json}. First run: WSL2
# (kernel 6.6), Docker 29, mock providers (docs/LATENCY.md, "Capacity benchmark, Linux").
#
# What each side runs (docs/LATENCY.md, "Capacity benchmark"):
#   brisvox: `brisvox-bench engine-server`, the production engine over brisvox_sim mock providers
#   pipecat: `bot.py --mock-providers --workers $CPUS`, Pipecat over mocks with the same numbers
# Same fixture, same client, same SLO (p95 V2V <= floor 950 ms + 250 ms, no failed call).
set -euo pipefail

CPUS=${CPUS:-4}                       # CPU limit of each server container
MEM=${MEM:-4g}                        # memory limit of each server container
SERVER_CORES=${SERVER_CORES:-0-$((CPUS - 1))}   # cores the server container may use
CLIENT_WORKERS=${CLIENT_WORKERS:-8}   # tokio workers of the load generator
CLIENT_CORES=${CLIENT_CORES:-$CPUS-$((CPUS + CLIENT_WORKERS - 1))} # cores of the load generator (host side)
STEPS=${STEPS:-1,5,10,20,40,80,120,160,240,320,480,640}
MAX_CALLS=${MAX_CALLS:-4096}          # brisvox sim server's call limit; above every step of the ramp
RUNS=${RUNS:-1}                       # repeats of the whole ramp per system (use >= 3 for medians)
RUN_FROM=${RUN_FROM:-1}               # first repeat to play (resume a series: RUN_FROM=2 RUNS=3)
SERVER_ENV=${SERVER_ENV:-}            # extra `docker run` flags for the server, e.g. "-e OMP_NUM_THREADS=1"
ENGINE_ARGS=${ENGINE_ARGS:-}          # extra `brisvox-bench engine-server` flags, e.g. "--turn-threads 0" (Smart Turn shares the VAD pool, the old layout)
NAME_SUFFIX=${NAME_SUFFIX:-}          # result files: capacity-<system><suffix>-linux-..., e.g. -tuned
PIPECAT_WORKERS=${PIPECAT_WORKERS:-$CPUS} # Pipecat processes (one event loop each); default: one per CPU
BUILD_JOBS=${BUILD_JOBS:-4}           # cargo jobs of the bench build (RAM: about 1 GB per job)
QUIET_GATE=${QUIET_GATE:-0}           # 1 = before each run wait for a quiet host (see quiet_gate below)
QUIET_LOAD=${QUIET_LOAD:-1.0}         # 1-minute load average the gate wants to see below
QUIET_WAIT_S=${QUIET_WAIT_S:-1800}    # give up (and fail) after this many seconds
NOFILE=${NOFILE:-1048576}             # RLIMIT_NOFILE of the server container (see below)
RESOLUTION=${RESOLUTION:-10}          # bisect between the last pass and the first stop
SYSTEMS=${SYSTEMS:-"brisvox pipecat"}
PORT=${PORT:-8080}
# MODELS=1: the real models. brisvox runs `engine-server --models` (Silero VAD on tract and
# Smart Turn v3.2 on ONNX Runtime, the production server's models and pool); Pipecat runs
# `bot.py --mock-models real` (its Silero VAD, brisvox's pinned silero_vad.onnx when present in
# the models dir, and its Smart Turn v3 analyzer). The prebuilt static ONNX Runtime needs glibc
# >= 2.38 (the Dockerfile builds on trixie for the same reason), so the Rust and run images
# default to trixie. The models directory (scripts/models/fetch_models.py) is mounted read-only.
MODELS=${MODELS:-0}
MODELS_DIR=${MODELS_DIR:-$(cd "$(dirname "$0")/.." && pwd)/models}
if [[ $MODELS == 1 ]]; then
  [[ -f $MODELS_DIR/smart-turn-v3.2-cpu.onnx ]] || {
    echo "MODELS=1 needs $MODELS_DIR/smart-turn-v3.2-cpu.onnx (scripts/models/fetch_models.py)" >&2
    exit 2
  }
  NAME_SUFFIX="${NAME_SUFFIX}-models"
  RUST_IMAGE=${RUST_IMAGE:-rust:1.99.0-slim-trixie}
  RUN_IMAGE=${RUN_IMAGE:-debian:trixie-slim}
  BENCH_FEATURES="--features models"
  TARGET_SUBDIR=linux-models
else
  BENCH_FEATURES=""
  TARGET_SUBDIR=linux
fi
RUST_IMAGE=${RUST_IMAGE:-rust:1.99.0-slim-bookworm}   # = rust-toolchain.toml and the Dockerfile
PYTHON_IMAGE=${PYTHON_IMAGE:-python:3.13-slim-bookworm}
RUN_IMAGE=${RUN_IMAGE:-debian:bookworm-slim}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$ROOT/bench/results}
DATE=${DATE:-$(date +%F)}
# nofile: Docker's default soft limit inside a container is 1024, which is one socket per call
# for the server: a first run without it stopped at exactly 1017 open calls (every slot above
# that failed "connect timed out" while accept() returned EMFILE), whatever the CPU left.
LIMITS=(--cpus="$CPUS" --cpuset-cpus="$SERVER_CORES" --memory="$MEM" --memory-swap="$MEM"
  --ulimit nofile="$NOFILE:$NOFILE")
BENCH="$ROOT/target/$TARGET_SUBDIR/release/brisvox-bench"

echo ">> building brisvox-bench $BENCH_FEATURES for Linux ($RUST_IMAGE)"
docker run --rm -v "$ROOT":/src -w /src -e CARGO_TARGET_DIR=/src/target/$TARGET_SUBDIR \
  -e CARGO_BUILD_JOBS="$BUILD_JOBS" -v brisvox-cargo-registry:/usr/local/cargo/registry \
  "$RUST_IMAGE" sh -c \
  "apt-get update -qq && apt-get install -y -qq --no-install-recommends build-essential cmake pkg-config ca-certificates >/dev/null && cargo build --release -p brisvox-bench $BENCH_FEATURES"

if [[ " $SYSTEMS " == *" pipecat "* ]]; then
  echo ">> installing the pinned Pipecat lock into the volume brisvox-pcvenv ($PYTHON_IMAGE)"
  docker run --rm -v "$ROOT":/src -v brisvox-pcvenv:/venv "$PYTHON_IMAGE" sh -c \
    'python -m venv /venv && /venv/bin/pip install -q -r /src/bench/pipecat_baseline/requirements.lock.txt'
fi

name=brisvox-capacity-server
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT
KERNEL=$(uname -r)
# One socket per open call on the generator side: raise the soft limit to the hard one.
ulimit -n "$(ulimit -Hn)" 2>/dev/null || true
# --network host: the generator reaches the server without docker's NAT / userland proxy, which
# would burn CPU outside the container's limits and differ with the port-publishing mode. The
# CPU and memory limits are cgroup limits and apply the same way.
NET=(--network host)

# The quiet-host gate. On a WSL2 host the Windows side shares the CPUs, so before a measured
# run no cargo.exe / rustc.exe / link.exe may be running there and the VM's 1-minute load average
# must be below QUIET_LOAD (the build above and the previous run's own load decay first). The
# result is printed and goes into the run's note ("quiet gate: ...").
GATE_NOTE=""
quiet_gate() {
  GATE_NOTE=""
  [[ $QUIET_GATE == 1 ]] || return 0
  local waited=0 busy load
  while :; do
    busy=""
    if command -v tasklist.exe >/dev/null 2>&1; then
      busy=$(tasklist.exe 2>/dev/null | tr -d '\r' | grep -i -E '^(cargo|rustc|link)\.exe' | awk '{print $1}' | sort -u | tr '\n' ' ' || true)
    fi
    load=$(cut -d' ' -f1 /proc/loadavg)
    if [[ -z $busy ]] && awk -v l="$load" -v m="$QUIET_LOAD" 'BEGIN { exit !(l < m) }'; then
      GATE_NOTE="quiet gate passed after ${waited}s: no cargo/rustc/link.exe on Windows, WSL load1 $load (< $QUIET_LOAD)"
      echo ">> $GATE_NOTE"
      return 0
    fi
    if (( waited >= QUIET_WAIT_S )); then
      echo "quiet gate not reached in ${QUIET_WAIT_S}s (busy: ${busy:-none}, load1 $load)" >&2
      exit 3
    fi
    sleep 5
    waited=$((waited + 5))
  done
}

start_server() {
  # shellcheck disable=SC2086 # SERVER_ENV is a list of docker flags
  case "$1" in
    brisvox)
      local models_args=() models_mount=()
      if [[ $MODELS == 1 ]]; then
        # --stats-every-ms: the pool's busy share and the Smart Turn / VAD split, one line per
        # period, kept from `docker logs` after the run.
        models_args=(--models /models --stats-every-ms 10000)
        models_mount=(-v "$MODELS_DIR":/models:ro)
      fi
      docker run -d --name "$name" "${LIMITS[@]}" "${NET[@]}" $SERVER_ENV -v "$ROOT":/src:ro \
        "${models_mount[@]}" \
        "$RUN_IMAGE" /src/target/$TARGET_SUBDIR/release/brisvox-bench engine-server \
        --bind "0.0.0.0:$PORT" --fixtures-dir /src/fixtures --max-calls "$MAX_CALLS" \
        --pool-threads $((CPUS / 2 > 0 ? CPUS / 2 : 1)) "${models_args[@]}" $ENGINE_ARGS >/dev/null
      ;;
    pipecat)
      local models_args=() models_env=() models_mount=()
      if [[ $MODELS == 1 ]]; then
        models_args=(--mock-models real)
        models_env=(-e BRISVOX_MODELS_DIR=/models)
        models_mount=(-v "$MODELS_DIR":/models:ro)
      fi
      docker run -d --name "$name" "${LIMITS[@]}" "${NET[@]}" $SERVER_ENV "${models_env[@]}" \
        -v "$ROOT":/src:ro -v brisvox-pcvenv:/venv:ro "${models_mount[@]}" "$PYTHON_IMAGE" \
        /venv/bin/python /src/bench/pipecat_baseline/bot.py --mock-providers --host 0.0.0.0 \
        --port "$PORT" --workers "$PIPECAT_WORKERS" --log-level WARNING "${models_args[@]}" >/dev/null
      ;;
    *) echo "unknown system $1" >&2; exit 2 ;;
  esac
}

for system in $SYSTEMS; do
  for run in $(seq "$RUN_FROM" "$RUNS"); do
    cleanup
    quiet_gate
    start_server "$system"   # a fresh container per run: no state carried over between repeats
    echo ">> $system$NAME_SUFFIX run $run/$RUNS: waiting for the server"
    ready=0
    for _ in $(seq 1 120); do
      if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then ready=1; break; fi
      sleep 1
    done
    if [[ $ready != 1 ]]; then
      echo "$system did not start listening on :$PORT" >&2
      docker logs --tail 40 "$name" >&2 || true
      exit 1
    fi
    # The container's init process is the server: its tree is everything the container runs.
    pid=$(docker inspect -f '{{.State.Pid}}' "$name")
    # The PSS column reads /proc/<pid>/smaps_rollup of every process of the container, which
    # only root may do for another user's processes; without it the PSS columns stay empty ("-").
    [[ -r /proc/$pid/smaps_rollup ]] || {
      echo "smaps_rollup of pid $pid is not readable: run this script as root (wsl -u root) for PSS" >&2
      exit 4
    }
    echo ">> $system$NAME_SUFFIX run $run/$RUNS: capacity run against pid $pid (limits: ${LIMITS[*]})"
    extra="run $run of $RUNS"
    [[ -n $GATE_NOTE ]] && extra+="; $GATE_NOTE"
    [[ -n $SERVER_ENV ]] && extra+="; server flags: $SERVER_ENV"
    [[ $system == pipecat ]] && extra+="; $PIPECAT_WORKERS Pipecat process(es)"
    taskset -c "$CLIENT_CORES" "$BENCH" capacity --url "ws://127.0.0.1:$PORT/" --pid "$pid" \
      --floor-ms 950 --label "$system$NAME_SUFFIX (${CPUS} CPUs, ${MEM}, Linux)" \
      --note "$extra; Linux kernel $KERNEL; container ${LIMITS[*]} --network host, server cores $SERVER_CORES, generator cores $CLIENT_CORES (bench/capacity_linux.sh)" \
      --steps "$STEPS" --fixtures-dir "$ROOT/fixtures" \
      --resolution "$RESOLUTION" --rounds 2 --client-workers "$CLIENT_WORKERS" \
      --out-md "$OUT/capacity-$system$NAME_SUFFIX-linux-$DATE-run$run.md" \
      --out-json "$OUT/capacity-$system$NAME_SUFFIX-linux-$DATE-run$run.json"
    docker stats --no-stream "$name" || true
    if [[ $MODELS == 1 ]]; then
      # The server's own record of the run (brisvox: pool stats and the CPU split; Pipecat: its
      # warnings), next to the ramp's files.
      docker logs "$name" >"$OUT/capacity-$system$NAME_SUFFIX-linux-$DATE-run$run.server.log" 2>&1 || true
    fi
    cleanup
  done
done
echo ">> done: $OUT/capacity-*-linux-$DATE-run*.{md,json}; fold the repeats with bench/capacity_summary.py"
