# In-process load simulation baseline

- command: `brisvox-bench sim-load --workers 4 --out bench/results/sim-baseline.md`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- mock providers in-process (brisvox-sim), real time (SystemClock), one process
- call: 3 turns of 1200 ms speech + 3500 ms silence, answer "Okay, got it.", ramp 1000 ms, linger 1500 ms; mock latency medians STT final 150 / LLM first token 350 / TTS first byte 150 ms (log-normal, sigma 0.3)

Overhead = (FirstAudioOut - TurnEnd) - LLM time to first sentence - TTS time to first byte; wake = delay from waking a task to it running (scheduling pressure on every hop). CPU = cores busy on the plateau (all calls active). Definitions and caveats: `docs/LATENCY.md`. Times in ms; percentiles below 100 samples are coarse (p99 = max).

| calls | answered | failed | barge-ins | overhead n | p50 | p90 | p99 | max | wake p50 | wake p99 | wake max | providers p50 | response p50 | response p99 | CPU cores | peak RSS MiB | RSS/call KiB | tasks peak | leaked tasks | wall s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 30/30 | 0 | 0 | 30 | 0.037 | 0.061 | 0.232 | 0.232 | 0.021 | 0.203 | 0.783 | 514.192 | 997.817 | 1369.188 | 0.22 | 9.6 | 230 | 131 | 0 | 16.7 |
| 100 | 300/300 | 0 | 0 | 300 | 0.077 | 0.254 | 0.842 | 1.231 | 0.037 | 0.926 | 8.040 | 544.556 | 999.752 | 1355.595 | 0.28 | 22.4 | 142 | 1291 | 0 | 16.8 |
| 300 | 900/900 | 0 | 0 | 900 | 0.130 | 0.559 | 1.965 | 2.321 | 0.041 | 1.684 | 6.893 | 542.929 | 995.793 | 1353.422 | 0.47 | 48.8 | 133 | 3853 | 0 | 16.8 |
| 500 | 1500/1500 | 0 | 0 | 1500 | 0.173 | 0.646 | 1.675 | 6.038 | 0.037 | 1.750 | 4.766 | 537.163 | 995.581 | 1356.842 | 0.37 | 75.8 | 132 | 6423 | 0 | 16.8 |
