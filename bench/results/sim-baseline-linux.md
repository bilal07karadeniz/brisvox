# In-process load simulation baseline

- command: `taskset -c 0-3 brisvox-bench sim-load --workers 4 --out bench/results/sim-baseline-linux.md`
- run confined with `taskset -c 0-3` (4 cores: affinity, so `available_parallelism` = 4) inside WSL2 Ubuntu, Linux 6.6.87.2-microsoft-standard-WSL2, on a 32-core Windows 11 host; the benchmark container was stopped, nothing else of ours running. Same command as the Windows baseline (bench/results/sim-baseline.md) plus taskset.
- brisvox 0.1.0 (release build), linux x86_64, 4 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- mock providers in-process (brisvox-sim), real time (SystemClock), one process
- call: 3 turns of 1200 ms speech + 3500 ms silence, answer "Okay, got it.", ramp 1000 ms, linger 1500 ms; mock latency medians STT final 150 / LLM first token 350 / TTS first byte 150 ms (log-normal, sigma 0.3)

Overhead = (FirstAudioOut - TurnEnd) - LLM time to first sentence - TTS time to first byte; wake = delay from waking a task to it running (scheduling pressure on every hop). CPU = cores busy on the plateau (all calls active). Definitions and caveats: `docs/LATENCY.md`. Times in ms; percentiles below 100 samples are coarse (p99 = max).

| calls | answered | failed | barge-ins | overhead n | p50 | p90 | p99 | max | wake p50 | wake p99 | wake max | providers p50 | response p50 | response p99 | CPU cores | peak RSS MiB | RSS/call KiB | tasks peak | leaked tasks | wall s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 30/30 | 0 | 0 | 30 | 0.033 | 0.065 | 0.105 | 0.105 | 0.047 | 0.128 | 0.377 | 507.070 | 962.724 | 1334.789 | 0.02 | 8.0 | 384 | 131 | 0 | 16.7 |
| 100 | 300/300 | 0 | 0 | 300 | 0.025 | 0.048 | 0.075 | 0.081 | 0.046 | 0.117 | 0.189 | 526.437 | 964.621 | 1323.657 | 0.06 | 20.2 | 123 | 1291 | 0 | 16.8 |
| 300 | 900/900 | 0 | 0 | 900 | 0.023 | 0.074 | 0.116 | 0.142 | 0.046 | 0.100 | 0.154 | 524.027 | 963.884 | 1317.869 | 0.14 | 49.3 | 108 | 3849 | 0 | 16.8 |
| 500 | 1500/1500 | 0 | 0 | 1500 | 0.029 | 0.104 | 0.185 | 0.707 | 0.050 | 0.133 | 1.709 | 521.774 | 962.464 | 1326.363 | 0.27 | 75.1 | 63 | 6415 | 0 | 16.8 |
