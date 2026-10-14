# Capacity: brisvox, 4 pool threads

- command: `brisvox-bench capacity --sim --label brisvox, 4 pool threads --server-workers 4 --pool-threads 4 --steps 400,600,800,1000 --resolution 100 --rounds 2 --client-workers 4 --out-md bench/results/capacity-brisvox-pool4-windows-2026-10-13.md --out-json bench/results/capacity-brisvox-pool4-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`): 4 tokio workers, 4 inference pool threads, up to 1024 calls; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:52517/...`
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [400, 600, 800, 1000]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a step whose frames went out more than 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, energy VAD stop 300 ms, `smart_turn` end of turn, 4 pool threads
- CPU / RSS: read from pid 24232 and its descendants every sample period

**Result: max 900 concurrent calls within the SLO**; the next step up did not pass (first failure at 1000).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1008 | 1030 | 2358 | 2376 | +80 | 0.46 | 16 / 327 | 0.37 | 23 | pass |
| 600 | 1200 (0) | 2400 / 2400 | 0 / 0 / 0 | 1002 | 1025 | 1031 | 1048 | +75 | 1.21 | 32 / 474 | 0.78 | 17 | pass |
| 800 | 1600 (0) | 3200 / 3200 | 0 / 0 / 0 | 1005 | 1035 | 1050 | 1060 | +85 | 1.49 | 35 / 624 | 1.02 | 19 | pass |
| 900 | 1800 (0) | 3600 / 3600 | 0 / 0 / 0 | 1002 | 1029 | 1043 | 1057 | +79 | 1.91 | 38 / 725 | 1.07 | 26 | pass |
| 1000 | 2000 (0) | 2995 / 4000 | 0 / 997 / 8 | 1010 | 2359 | 3661 | 3857 | +1409 | 1.64 | 37 / 776 | 0.94 | 22 | FAIL |

Why steps did not pass:

- 1000 concurrent: 1005 of 4000 turns lost or not clean (0 talked over, 997 no response, 8 bot speaking; allowed 0.0 %); p95 V2V 2358 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
