# Capacity: brisvox, 2 pool threads, queue 1024, 2048 calls allowed

- command: `brisvox-bench capacity --sim --label brisvox, 2 pool threads, queue 1024, 2048 calls allowed --server-workers 4 --pool-threads 2 --pool-queue 1024 --max-calls 2048 --steps 1000,1200,1500 --rounds 2 --client-workers 4 --out-md bench/results/capacity-brisvox-queue1024-calls2048-windows-2026-10-13.md --out-json bench/results/capacity-brisvox-queue1024-calls2048-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`): 4 tokio workers, 2 inference pool threads (queue 1024), up to 2048 calls; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:59251/...`
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1000, 1200, 1500]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, energy VAD stop 300 ms, `smart_turn` end of turn, 2 pool threads, pool queue 1024
- CPU / RSS: read from pid 55844 and its descendants every sample period

**Result: max 1200 concurrent calls within the SLO**; the next step up did not pass (first failure at 1500).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1000 | 2000 (0) | 4000 / 4000 | 0 / 0 / 0 | 1003 | 1032 | 1042 | 1054 | +82 | 1.84 | 19 / 762 | 1.15 | 32 | pass |
| 1200 | 2400 (0) | 4800 / 4800 | 0 / 0 / 0 | 1004 | 1041 | 1053 | 1079 | +91 | 2.08 | 30 / 912 | 1.26 | 21 | pass |
| 1500 | 3000 (0) | 4904 / 6000 | 0 / 1093 / 3 | 1007 | 1060 | 3626 | 3863 | +110 | 2.25 | 32 / 1159 | 1.45 | 27 | FAIL |

Why steps did not pass:

- 1500 concurrent: 1096 of 6000 turns lost or not clean (0 talked over, 1093 no response, 3 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
