# In-process load simulation baseline

- command: `taskset -c 0-3 brisvox-bench sim-load --workers 4 --sessions 500,1000,2000,3000,4000,6000 --out bench/results/sim-saturation-linux-first.md`
- run confined with `taskset -c 0-3` (4 cores: affinity, so `available_parallelism` = 4) inside WSL2 Ubuntu, Linux 6.6.87.2-microsoft-standard-WSL2, on a 32-core Windows 11 host; the benchmark container was stopped, nothing else of ours running. FIRST sweep, run right after the baseline: it shows wake p99 13.4 ms at 4000 calls, which the two later sweeps (run1, run2, 10000 calls) did not reproduce; treat it as a noisy run of a shared desktop (the Windows host runs other work), kept for honesty.
- brisvox 0.1.0 (release build), linux x86_64, 4 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- mock providers in-process (brisvox-sim), real time (SystemClock), one process
- call: 3 turns of 1200 ms speech + 3500 ms silence, answer "Okay, got it.", ramp 1000 ms, linger 1500 ms; mock latency medians STT final 150 / LLM first token 350 / TTS first byte 150 ms (log-normal, sigma 0.3)

Overhead = (FirstAudioOut - TurnEnd) - LLM time to first sentence - TTS time to first byte; wake = delay from waking a task to it running (scheduling pressure on every hop). CPU = cores busy on the plateau (all calls active). Definitions and caveats: `docs/LATENCY.md`. Times in ms; percentiles below 100 samples are coarse (p99 = max).

| calls | answered | failed | barge-ins | overhead n | p50 | p90 | p99 | max | wake p50 | wake p99 | wake max | providers p50 | response p50 | response p99 | CPU cores | peak RSS MiB | RSS/call KiB | tasks peak | leaked tasks | wall s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 500 | 1500/1500 | 0 | 0 | 1500 | 0.023 | 0.078 | 0.121 | 0.206 | 0.045 | 0.144 | 0.615 | 522.146 | 963.026 | 1326.503 | 0.20 | 71.0 | 137 | 6419 | 0 | 16.8 |
| 1000 | 3000/3000 | 0 | 0 | 3000 | 0.021 | 0.113 | 0.213 | 0.416 | 0.046 | 0.241 | 0.754 | 524.777 | 964.895 | 1329.785 | 0.36 | 135.1 | 67 | 12823 | 0 | 16.8 |
| 2000 | 6000/6000 | 0 | 0 | 6000 | 0.028 | 0.209 | 0.382 | 0.717 | 0.048 | 0.188 | 0.540 | 525.667 | 967.206 | 1349.942 | 0.70 | 292.9 | 93 | 25619 | 0 | 16.8 |
| 3000 | 9000/9000 | 0 | 0 | 9000 | 0.056 | 0.417 | 1.179 | 3.784 | 0.056 | 1.312 | 19.739 | 526.488 | 967.735 | 1348.960 | 1.34 | 420.7 | 63 | 38452 | 0 | 16.8 |
| 4000 | 12000/12000 | 0 | 0 | 12000 | 0.158 | 1.642 | 6.153 | 31.072 | 0.063 | 13.375 | 25.838 | 528.838 | 973.201 | 1358.460 | 2.07 | 548.0 | 61 | 51245 | 0 | 16.8 |
| 6000 | 18000/18000 | 0 | 0 | 18000 | 0.098 | 0.768 | 1.791 | 21.861 | 0.068 | 12.132 | 24.245 | 528.369 | 969.629 | 1354.329 | 2.29 | 824.0 | 77 | 76870 | 0 | 16.8 |
