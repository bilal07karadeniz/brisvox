# Capacity: brisvox with models (Windows, 4 server workers, 2 pool threads)

- command: `brisvox-bench capacity --sim --models models --pool-threads 2 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label brisvox with models (Windows, 4 server workers, 2 pool threads) --note run 1 of 3; Windows x86_64 desktop, NOT a Linux run and NO CPU limit (4 tokio workers, 2 pool threads on a 32-core machine); provisional --steps 1,25,50,100,200,300,400,600,800,1000,1400,1800 --resolution 25 --rounds 2 --fixtures-dir fixtures --out-md bench/results/capacity-brisvox-models-windows-2026-10-13-run1.md --out-json bench/results/capacity-brisvox-models-windows-2026-10-13-run1.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 2 inference pool threads (queue 8192), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:53849/...`
- server: run 1 of 3; Windows x86_64 desktop, NOT a Linux run and NO CPU limit (4 tokio workers, 2 pool threads on a 32-core machine); provisional
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 25, 50, 100, 200, 300, 400, 600, 800, 1000, 1400, 1800]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 2 pool threads, pool queue 8192
- CPU / RSS: read from pid 17916 and its descendants every sample period

**Result: max 200 concurrent calls within the SLO**; the next step up did not pass (first failure at 225).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 818 | 844 | 844 | 844 | -106 | 0.03 | 97 / 101 | 0.08 | 17 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 819 | 841 | 855 | 856 | -109 | 0.32 | 101 / 120 | 0.15 | 17 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 822 | 846 | 862 | 863 | -104 | 0.49 | 105 / 139 | 0.16 | 18 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 823 | 849 | 859 | 874 | -101 | 1.18 | 102 / 205 | 0.39 | 17 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 837 | 1009 | 1078 | 1098 | +59 | 2.40 | 131 / 280 | 0.61 | 17 | pass |
| 225 | 450 (0) | 900 / 900 | 0 / 0 / 0 | 1083 | 1633 | 1683 | 1759 | +683 | 2.64 | 134 / 302 | 0.64 | 18 | FAIL |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 1577 | 2925 | 3250 | 3349 | +1975 | 2.89 | 136 / 320 | 0.65 | 18 | FAIL |
| 300 | 600 (0) | 1191 / 1200 | 9 / 0 / 0 | 2100 | 3723 | 3895 | 3995 | +2773 | 3.13 | 133 / 356 | 0.75 | 18 | FAIL |

Why steps did not pass:

- 225 concurrent: p95 V2V 1633 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 250 concurrent: p95 V2V 2924 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 300 concurrent: 9 of 1200 turns lost or not clean (9 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3723 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
