# Capacity: brisvox, 8 tokio workers, 2 pool threads, priority lines, queue 6000

- command: `brisvox-bench capacity --sim --label brisvox, 8 tokio workers, 2 pool threads, priority lines, queue 6000 --note ablation, the same server with 8 tokio workers: the limit above 1800 calls is the 4 workers --server-workers 8 --pool-threads 2 --pool-queue 6000 --max-calls 3000 --steps 2000,2200 --keep-going --rounds 2 --client-workers 4 --out-md bench/results/capacity-brisvox-priority-workers8-windows-2026-10-13.md --out-json bench/results/capacity-brisvox-priority-workers8-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`): 8 tokio workers, 2 inference pool threads (queue 6000), up to 3000 calls; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:51260/...`
- server: ablation, the same server with 8 tokio workers: the limit above 1800 calls is the 4 workers
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [2000, 2200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, energy VAD stop 300 ms, `smart_turn` end of turn, 2 pool threads, pool queue 6000
- CPU / RSS: read from pid 28036 and its descendants every sample period

**Result: max 2200 concurrent calls within the SLO**; every step passed: a lower bound, the limit is above the ramp.

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 2000 | 4000 (0) | 8000 / 8000 | 0 / 0 / 0 | 1014 | 1068 | 1091 | 1189 | +118 | 4.13 | 16 / 1521 | 2.20 | 84 | pass |
| 2200 | 4400 (0) | 8800 / 8800 | 0 / 0 / 0 | 1033 | 1103 | 1124 | 1194 | +153 | 4.41 | 36 / 1671 | 2.27 | 45 | pass |

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
