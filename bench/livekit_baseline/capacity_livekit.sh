#!/usr/bin/env bash
# The capacity benchmark of LiveKit Agents on Linux: the worker in a container with the same CPU
# and memory limits as the brisvox and Pipecat runs (bench/capacity_linux.sh), mock providers with
# the same latencies, the LiveKit server (SFU) in a container of its own, the load generator in a
# third one.
#
#   bench/livekit_baseline/capacity_livekit.sh                     # variant "default", 3 runs
#   VARIANT=tuned bench/livekit_baseline/capacity_livekit.sh
#   STEPS=1,5,10,20 RUNS=1 bench/livekit_baseline/capacity_livekit.sh
#
# Variants (result files: capacity-livekit-default-models-..., capacity-livekit-threshold-models-...,
# and for "tuned" capacity-livekit-models-linux-<date>*, the one the charts use):
#   default   the framework as shipped: session defaults (Silero VAD, the local audio turn detector
#             v1-mini with its calibrated thresholds), worker defaults (a process per job, idle
#             processes = CPUs, load_threshold 0.7)
#   threshold the same, but the turn detector's unlikely_threshold is lowered to 0.05: the model
#             still runs on every turn, its "probably not finished" verdict no longer holds the turn
#             for max_delay (2.5 s)
#   tuned     threshold + load_threshold 1.0 (the worker takes jobs until the CPU is used up)
#
# Needs Docker on a Linux host (WSL2 is fine) with at least CPUS + 8 + 4 cores: the worker is
# pinned to the first CPUS cores, the load generator to the next 8, the SFU to the 4 after those.
# Results go to bench/results/capacity-livekit<-variant>-models-linux-<date>-run<N>.{md,json}
# (+ .turns.jsonl, .agent.log); bench/capacity_summary.py folds the repeats.
set -euo pipefail

VARIANT=${VARIANT:-default}
CPUS=${CPUS:-4}                       # CPU limit of the worker container (like the others)
MEM=${MEM:-4g}                        # memory limit of the worker container (like the others)
AGENT_CORES=${AGENT_CORES:-0-$((CPUS - 1))}
CLIENT_WORKERS=${CLIENT_WORKERS:-8}   # load generator processes
CLIENT_CORES=${CLIENT_CORES:-$CPUS-$((CPUS + CLIENT_WORKERS - 1))}
SFU_CORES=${SFU_CORES:-$((CPUS + CLIENT_WORKERS))-$((CPUS + CLIENT_WORKERS + 3))}
STEPS=${STEPS:-1,5,10,15,20,25,30,40,50,60,80}
RESOLUTION=${RESOLUTION:-2}           # bisect between the last pass and the first stop
RUNS=${RUNS:-3}
FLOOR_MS=${FLOOR_MS:-950}             # the SLO is p95 V2V <= FLOOR_MS + 250 ms (950: the mocks' nominal floor, like the other systems)
SETTLE_MS=${SETTLE_MS:-20000}         # wait for the worker to be idle between steps (see README)
QUIET_PCT=${QUIET_PCT:-20}            # run only when the Windows host is below this CPU load (needs powershell.exe)
QUIET_WAIT_S=${QUIET_WAIT_S:-1800}    # give up waiting for a quiet host after this long (the run then says so)
LIVEKIT_IMAGE=${LIVEKIT_IMAGE:-livekit/livekit-server:v1.13.9}
CLIENT_IMAGE=${CLIENT_IMAGE:-brisvox-livekit:1.8.7}   # worker and load generator share it
API_KEY=devkey
API_SECRET=devsecret-for-the-brisvox-capacity-benchmark-0123456789   # a throwaway key of the throwaway server
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$ROOT/bench/livekit_baseline
OUT=${OUT:-$ROOT/bench/results}
DATE=${DATE:-$(date +%F)}
NAME_SUFFIX=${NAME_SUFFIX:-}

agent_env=()
case "$VARIANT" in
  default) variant_note="framework defaults (worker and session)" ;;
  threshold)
    agent_env=(-e LK_UNLIKELY_THRESHOLD=0.05)
    variant_note="defaults, turn detector unlikely_threshold 0.05 (model runs, verdict does not hold the turn)" ;;
  tuned)
    agent_env=(-e LK_UNLIKELY_THRESHOLD=0.05 -e LK_LOAD_THRESHOLD=1.0)
    variant_note="turn detector unlikely_threshold 0.05 and worker load_threshold 1.0" ;;
  *) echo "unknown VARIANT $VARIANT" >&2; exit 2 ;;
esac
# File names: the tuned variant is "livekit-models" (the one scripts/gen_charts.py charts and the README
# quotes, like Pipecat's tuned run); the others carry their variant in the name.
case "$VARIANT" in
  tuned) SYSTEM="livekit$NAME_SUFFIX-models" ;;
  *) SYSTEM="livekit-$VARIANT$NAME_SUFFIX-models" ;;
esac

sfu=brisvox-lk-sfu
agent=brisvox-lk-agent
client=brisvox-lk-client
cleanup() { docker rm -f "$client" "$agent" "$sfu" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo ">> image $CLIENT_IMAGE ($(docker image inspect "$CLIENT_IMAGE" >/dev/null 2>&1 && echo present || echo building))"
docker image inspect "$CLIENT_IMAGE" >/dev/null 2>&1 || docker build -q -t "$CLIENT_IMAGE" "$HERE"
docker pull -q "$LIVEKIT_IMAGE" >/dev/null
SFU_DIGEST=$(docker image inspect "$LIVEKIT_IMAGE" --format '{{index .RepoDigests 0}}')
KERNEL=$(uname -r)

host_load() {  # Windows host CPU load in percent (WSL2 interop); empty when unavailable
  command -v powershell.exe >/dev/null 2>&1 || return 0
  powershell.exe -NoProfile -Command "(Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average" 2>/dev/null | tr -d '\r' | head -1
}
wait_quiet() {  # sets QUIET_NOTE
  local waited=0 samples avg
  while :; do
    samples=()
    for _ in 1 2 3 4 5 6; do samples+=("$(host_load)"); sleep 2; done
    avg=$(printf '%s\n' "${samples[@]}" | awk 'NF{s+=$1;n++} END{ if(n) printf "%.0f", s/n; else print "" }')
    if [[ -z $avg ]]; then QUIET_NOTE="host load not measured (no powershell.exe)"; return; fi
    if (( avg <= QUIET_PCT )); then QUIET_NOTE="host load ${avg}% (Windows total, 12 s mean) before the run; load average $(cut -d' ' -f1-3 /proc/loadavg)"; return; fi
    if (( waited >= QUIET_WAIT_S )); then QUIET_NOTE="host NOT quiet: ${avg}% load (Windows total, 12 s mean) before the run, waited ${waited} s; load average $(cut -d' ' -f1-3 /proc/loadavg)"; return; fi
    echo ">> host busy (${avg}% > ${QUIET_PCT}%), waiting"; sleep 20; waited=$((waited + 32))
  done
}

for run in $(seq 1 "$RUNS"); do
  cleanup
  wait_quiet
  echo ">> $SYSTEM run $run/$RUNS: $QUIET_NOTE"
  docker run -d --name "$sfu" --network host --cpuset-cpus="$SFU_CORES" \
    -v "$HERE/livekit.yaml":/etc/livekit.yaml:ro "$LIVEKIT_IMAGE" --config /etc/livekit.yaml >/dev/null
  for _ in $(seq 1 30); do (exec 3<>/dev/tcp/127.0.0.1/7880) 2>/dev/null && break; sleep 1; done
  # The worker: the same limits as the brisvox and Pipecat containers.
  docker run -d --name "$agent" --cpus="$CPUS" --cpuset-cpus="$AGENT_CORES" --memory="$MEM" --memory-swap="$MEM" \
    --ulimit nofile=1048576:1048576 --network host \
    -e LIVEKIT_URL=ws://127.0.0.1:7880 -e LIVEKIT_API_KEY=$API_KEY -e LIVEKIT_API_SECRET=$API_SECRET \
    -e PYTHONPATH=/src/bench/livekit_baseline -e BRISVOX_FIXTURES_DIR=/src/fixtures "${agent_env[@]}" \
    -v "$ROOT":/src:ro "$CLIENT_IMAGE" python /src/bench/livekit_baseline/agent.py start --log-level warn >/dev/null
  # Ready: registered with the server and the idle job processes up (processes stop growing).
  prev=0; stable=0
  for _ in $(seq 1 90); do
    n=$(docker top "$agent" -eo pid 2>/dev/null | tail -n +2 | wc -l)
    if [[ $n -ge 6 && $n -eq $prev ]]; then stable=$((stable + 1)); else stable=0; fi
    prev=$n; [[ $stable -ge 5 ]] && break; sleep 1
  done
  sleep 3
  aid=$(docker inspect -f '{{.Id}}' "$agent"); sid=$(docker inspect -f '{{.Id}}' "$sfu")
  apid=$(docker inspect -f '{{.State.Pid}}' "$agent")
  base="$OUT/capacity-$SYSTEM-linux-$DATE-run$run"
  rm -f "$base.turns.jsonl"
  note="run $run of $RUNS; LiveKit Agents 1.8.7 (livekit-server $LIVEKIT_IMAGE, $SFU_DIGEST) variant $VARIANT: $variant_note; Linux kernel $KERNEL; worker container --cpus=$CPUS --cpuset-cpus=$AGENT_CORES --memory=$MEM --memory-swap=$MEM --network host, load generator cores $CLIENT_CORES ($CLIENT_WORKERS processes), SFU container on cores $SFU_CORES, no CPU limit; $QUIET_NOTE (bench/livekit_baseline/capacity_livekit.sh)"
  docker run --rm --name "$client" --network host --cpuset-cpus="$CLIENT_CORES" --pid=host --cgroupns=host \
    -v /sys/fs/cgroup:/sys/fs/cgroup:ro -v "$ROOT":/src:ro -v "$OUT":/out "$CLIENT_IMAGE" \
    python /src/bench/livekit_baseline/loadgen.py --fixtures-dir /src/fixtures --api-key $API_KEY --api-secret $API_SECRET \
      --url ws://127.0.0.1:7880 --floor-ms "$FLOOR_MS" --steps "$STEPS" --resolution "$RESOLUTION" --rounds 2 --workers "$CLIENT_WORKERS" \
      --settle-ms "$SETTLE_MS" --target-pid "$apid" \
      --target "agent=/sys/fs/cgroup/system.slice/docker-$aid.scope" --target "sfu=/sys/fs/cgroup/system.slice/docker-$sid.scope" \
      --label "$SYSTEM (${CPUS} CPUs, ${MEM}, Linux)" --note "$note" \
      --command "bench/livekit_baseline/capacity_livekit.sh (VARIANT=$VARIANT STEPS=$STEPS RESOLUTION=$RESOLUTION run $run)" \
      --dump-turns "/out/capacity-$SYSTEM-linux-$DATE-run$run.turns.jsonl" \
      --out-md "/out/capacity-$SYSTEM-linux-$DATE-run$run.md" --out-json "/out/capacity-$SYSTEM-linux-$DATE-run$run.json"
  docker logs "$agent" >"$base.agent.log" 2>&1 || true
  docker stats --no-stream "$agent" "$sfu" || true
  cleanup
done
echo ">> done: $OUT/capacity-$SYSTEM-linux-$DATE-run*.{md,json}; fold the repeats with bench/capacity_summary.py"
