# In-process load simulation baseline

- command: `taskset -c 0-3 brisvox-bench sim-load --workers 4 --sessions 500,1000,2000,3000,3500,4000,5000,6000,8000,10000,12000 --out bench/results/sim-saturation-linux-run2.md`
- run confined with `taskset -c 0-3` (4 cores: affinity, so `available_parallelism` = 4) inside WSL2 Ubuntu, Linux 6.6.87.2-microsoft-standard-WSL2, on a 32-core Windows 11 host; the benchmark container was stopped, nothing else of ours running. Saturation sweep, run 2 of 2.
- brisvox 0.1.0 (release build), linux x86_64, 4 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- mock providers in-process (brisvox-sim), real time (SystemClock), one process
- call: 3 turns of 1200 ms speech + 3500 ms silence, answer "Okay, got it.", ramp 1000 ms, linger 1500 ms; mock latency medians STT final 150 / LLM first token 350 / TTS first byte 150 ms (log-normal, sigma 0.3)

Overhead = (FirstAudioOut - TurnEnd) - LLM time to first sentence - TTS time to first byte; wake = delay from waking a task to it running (scheduling pressure on every hop). CPU = cores busy on the plateau (all calls active). Definitions and caveats: `docs/LATENCY.md`. Times in ms; percentiles below 100 samples are coarse (p99 = max).

| calls | answered | failed | barge-ins | overhead n | p50 | p90 | p99 | max | wake p50 | wake p99 | wake max | providers p50 | response p50 | response p99 | CPU cores | peak RSS MiB | RSS/call KiB | tasks peak | leaked tasks | wall s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 500 | 1500/1500 | 0 | 0 | 1500 | 0.029 | 0.102 | 0.158 | 0.229 | 0.053 | 0.120 | 0.589 | 522.411 | 962.678 | 1325.924 | 0.27 | 70.8 | 137 | 6413 | 0 | 16.8 |
| 1000 | 3000/3000 | 0 | 0 | 3000 | 0.026 | 0.135 | 0.235 | 0.346 | 0.055 | 0.104 | 0.631 | 524.936 | 965.272 | 1331.120 | 0.44 | 134.9 | 69 | 12821 | 0 | 16.8 |
| 2000 | 6000/6000 | 0 | 0 | 6000 | 0.028 | 0.181 | 0.348 | 6.788 | 0.048 | 0.204 | 5.980 | 525.458 | 967.419 | 1349.883 | 0.66 | 262.9 | 85 | 25615 | 0 | 16.8 |
| 3000 | 9000/9000 | 0 | 0 | 9000 | 0.033 | 0.235 | 0.495 | 1.338 | 0.047 | 0.290 | 1.009 | 526.327 | 967.146 | 1348.853 | 0.91 | 390.7 | 68 | 38468 | 0 | 16.8 |
| 3500 | 10500/10500 | 0 | 0 | 10500 | 0.034 | 0.277 | 0.580 | 1.341 | 0.048 | 0.379 | 0.788 | 526.853 | 967.714 | 1348.618 | 1.04 | 454.9 | 54 | 44855 | 0 | 16.8 |
| 4000 | 12000/12000 | 0 | 0 | 12000 | 0.039 | 0.314 | 0.724 | 2.142 | 0.048 | 0.574 | 15.255 | 526.631 | 967.579 | 1352.544 | 1.22 | 599.0 | 88 | 51246 | 0 | 16.8 |
| 5000 | 15000/15000 | 0 | 0 | 15000 | 0.048 | 0.378 | 0.959 | 3.423 | 0.048 | 0.947 | 2.745 | 527.399 | 968.393 | 1352.334 | 1.52 | 726.1 | 74 | 64007 | 0 | 16.8 |
| 6000 | 18000/18000 | 0 | 0 | 18000 | 0.052 | 0.392 | 0.910 | 2.609 | 0.048 | 1.002 | 4.586 | 527.332 | 968.027 | 1349.934 | 1.64 | 823.3 | 47 | 76850 | 0 | 16.8 |
| 8000 | 24000/24000 | 0 | 0 | 24000 | 0.088 | 0.541 | 1.119 | 2.921 | 0.050 | 2.008 | 4.612 | 528.357 | 968.978 | 1347.990 | 2.17 | 1042.8 | 66 | 102467 | 0 | 16.8 |
| 10000 | 30000/30000 | 0 | 0 | 30000 | 0.150 | 0.724 | 1.492 | 18.116 | 0.274 | 19.551 | 36.428 | 530.010 | 973.267 | 1354.571 | 2.75 | 1297.8 | 48 | 128028 | 0 | 16.8 |
| 12000 | 36000/36000 | 0 | 0 | 36000 | 0.234 | 0.808 | 1.592 | 25.275 | 0.631 | 24.387 | 31.204 | 533.261 | 982.124 | 1366.826 | 3.09 | 1552.9 | 58 | 153635 | 0 | 16.8 |
