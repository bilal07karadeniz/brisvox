# In-process load simulation baseline

- command: `taskset -c 0-3 brisvox-bench sim-load --workers 4 --sessions 500,1000,2000,3000,3500,4000,5000,6000,8000,10000,12000 --out bench/results/sim-saturation-linux-run1.md`
- run confined with `taskset -c 0-3` (4 cores: affinity, so `available_parallelism` = 4) inside WSL2 Ubuntu, Linux 6.6.87.2-microsoft-standard-WSL2, on a 32-core Windows 11 host; the benchmark container was stopped, nothing else of ours running. Saturation sweep, run 1 of 2.
- brisvox 0.1.0 (release build), linux x86_64, 4 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- mock providers in-process (brisvox-sim), real time (SystemClock), one process
- call: 3 turns of 1200 ms speech + 3500 ms silence, answer "Okay, got it.", ramp 1000 ms, linger 1500 ms; mock latency medians STT final 150 / LLM first token 350 / TTS first byte 150 ms (log-normal, sigma 0.3)

Overhead = (FirstAudioOut - TurnEnd) - LLM time to first sentence - TTS time to first byte; wake = delay from waking a task to it running (scheduling pressure on every hop). CPU = cores busy on the plateau (all calls active). Definitions and caveats: `docs/LATENCY.md`. Times in ms; percentiles below 100 samples are coarse (p99 = max).

| calls | answered | failed | barge-ins | overhead n | p50 | p90 | p99 | max | wake p50 | wake p99 | wake max | providers p50 | response p50 | response p99 | CPU cores | peak RSS MiB | RSS/call KiB | tasks peak | leaked tasks | wall s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 500 | 1500/1500 | 0 | 0 | 1500 | 0.037 | 0.118 | 0.170 | 0.277 | 0.056 | 0.114 | 0.338 | 521.959 | 963.140 | 1327.012 | 0.31 | 70.8 | 136 | 6411 | 0 | 16.8 |
| 1000 | 3000/3000 | 0 | 0 | 3000 | 0.030 | 0.162 | 0.276 | 0.493 | 0.056 | 0.121 | 0.265 | 524.767 | 965.095 | 1330.197 | 0.49 | 135.4 | 67 | 12819 | 0 | 16.8 |
| 2000 | 6000/6000 | 0 | 0 | 6000 | 0.037 | 0.220 | 0.442 | 1.359 | 0.054 | 0.148 | 0.598 | 525.812 | 967.233 | 1350.717 | 0.81 | 263.1 | 72 | 25621 | 0 | 16.8 |
| 3000 | 9000/9000 | 0 | 0 | 9000 | 0.046 | 0.336 | 0.703 | 2.826 | 0.052 | 0.421 | 1.657 | 526.433 | 967.198 | 1349.783 | 1.13 | 434.1 | 79 | 38463 | 0 | 16.8 |
| 3500 | 10500/10500 | 0 | 0 | 10500 | 0.046 | 0.358 | 0.771 | 1.693 | 0.053 | 0.526 | 1.970 | 527.123 | 967.777 | 1349.076 | 1.26 | 497.9 | 66 | 44865 | 0 | 16.8 |
| 4000 | 12000/12000 | 0 | 0 | 12000 | 0.042 | 0.314 | 0.665 | 1.987 | 0.052 | 0.536 | 1.506 | 526.631 | 967.736 | 1352.600 | 1.25 | 562.2 | 46 | 51249 | 0 | 16.8 |
| 5000 | 15000/15000 | 0 | 0 | 15000 | 0.047 | 0.402 | 1.123 | 7.938 | 0.049 | 0.812 | 8.691 | 527.379 | 968.630 | 1351.419 | 1.47 | 689.2 | 63 | 64010 | 0 | 16.8 |
| 6000 | 18000/18000 | 0 | 0 | 18000 | 0.057 | 0.454 | 1.040 | 4.693 | 0.049 | 1.326 | 9.366 | 527.284 | 968.382 | 1351.222 | 1.73 | 804.1 | 62 | 76850 | 0 | 16.8 |
| 8000 | 24000/24000 | 0 | 0 | 24000 | 0.093 | 0.576 | 1.300 | 10.141 | 0.050 | 6.235 | 13.363 | 528.972 | 969.546 | 1348.146 | 2.25 | 1230.3 | 86 | 102475 | 0 | 16.8 |
| 10000 | 30000/30000 | 0 | 0 | 30000 | 0.149 | 0.709 | 1.497 | 27.663 | 0.316 | 17.848 | 26.730 | 529.755 | 973.028 | 1354.537 | 2.76 | 1484.8 | 54 | 128048 | 0 | 16.8 |
| 12000 | 36000/36000 | 0 | 0 | 36000 | 0.246 | 0.925 | 1.863 | 33.158 | 0.760 | 29.479 | 38.668 | 535.340 | 984.688 | 1369.912 | 3.10 | 1740.2 | 82 | 153661 | 0 | 16.8 |
