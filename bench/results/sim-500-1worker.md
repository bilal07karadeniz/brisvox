# In-process load simulation baseline

- command: `brisvox-bench sim-load --sessions 500 --workers 1 --out bench/results/sim-500-1worker.md`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 1 tokio workers (threads, not a CPU limit: other threads may use any core)
- mock providers in-process (brisvox-sim), real time (SystemClock), one process
- call: 3 turns of 1200 ms speech + 3500 ms silence, answer "Okay, got it.", ramp 1000 ms, linger 1500 ms; mock latency medians STT final 150 / LLM first token 350 / TTS first byte 150 ms (log-normal, sigma 0.3)

Overhead = (FirstAudioOut - TurnEnd) - LLM time to first sentence - TTS time to first byte; wake = delay from waking a task to it running (scheduling pressure on every hop). CPU = cores busy on the plateau (all calls active). Definitions and caveats: `docs/LATENCY.md`. Times in ms; percentiles below 100 samples are coarse (p99 = max).

| calls | answered | failed | barge-ins | overhead n | p50 | p90 | p99 | max | wake p50 | wake p99 | wake max | providers p50 | response p50 | response p99 | CPU cores | peak RSS MiB | RSS/call KiB | tasks peak | leaked tasks | wall s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 500 | 1500/1500 | 0 | 0 | 1500 | 0.525 | 1.207 | 1.913 | 5.255 | 0.310 | 4.004 | 6.889 | 534.269 | 993.786 | 1359.247 | 0.30 | 71.5 | 132 | 6423 | 0 | 16.8 |
