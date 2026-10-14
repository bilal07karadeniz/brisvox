#!/usr/bin/env bash
# The in-process load simulation (`brisvox-bench sim-load`) on Linux confined to 4 cores,
# plus a saturation sweep. Results: bench/results/sim-baseline-linux.md and
# sim-saturation-linux-run<N>.md (docs/LATENCY.md, "Linux, confined to 4 cores").
#
#   bench/sim_load_linux.sh                    # baseline + 2 sweeps, cores 0-3
#   CORES=0-1 WORKERS=2 bench/sim_load_linux.sh
#   BENCH=target/linux/release/brisvox-bench bench/sim_load_linux.sh   # a prebuilt binary
#
# Needs a Linux host (WSL2 is fine) with `taskset` and the cores free of other work. The sweep
# raises the number of simultaneous calls until the overhead or wake p99 passes 10 ms (the
# acceptance criterion); a run that is not valid (a failed call, a dropped probe) writes no file.
set -euo pipefail

CORES=${CORES:-0-3}                    # cores the process may run on (taskset -c)
WORKERS=${WORKERS:-4}                  # tokio workers = the number of cores in CORES
SWEEP=${SWEEP:-500,1000,2000,3000,3500,4000,5000,6000,8000,10000,12000}
SWEEPS=${SWEEPS:-2}                    # repeats of the sweep (the tail moves about 2x between runs)
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$ROOT/bench/results}
BENCH=${BENCH:-$ROOT/target/release/brisvox-bench}

if [[ ! -x $BENCH ]]; then
  echo ">> building brisvox-bench (release)"
  (cd "$ROOT" && cargo build --release -p brisvox-bench)
fi

# Insert one line after the generated "command:" line saying how the run was confined.
annotate() {
  local file=$1 command=$2
  sed -i "/^- command:/c\\- command: \`$command\`\n- run confined with \`taskset -c $CORES\` (affinity, so \`available_parallelism\` = $WORKERS) on Linux $(uname -r); nothing else of ours running." "$file"
}

echo ">> baseline: sim-load on cores $CORES"
taskset -c "$CORES" "$BENCH" sim-load --workers "$WORKERS" --out "$OUT/sim-baseline-linux.md"
annotate "$OUT/sim-baseline-linux.md" \
  "taskset -c $CORES brisvox-bench sim-load --workers $WORKERS --out bench/results/sim-baseline-linux.md"

for run in $(seq 1 "$SWEEPS"); do
  echo ">> saturation sweep $run/$SWEEPS: $SWEEP"
  taskset -c "$CORES" "$BENCH" sim-load --workers "$WORKERS" --sessions "$SWEEP" \
    --out "$OUT/sim-saturation-linux-run$run.md"
  annotate "$OUT/sim-saturation-linux-run$run.md" \
    "taskset -c $CORES brisvox-bench sim-load --workers $WORKERS --sessions $SWEEP --out bench/results/sim-saturation-linux-run$run.md"
done
echo ">> done: look for the first row whose overhead p99 or wake p99 is above 10 ms"
