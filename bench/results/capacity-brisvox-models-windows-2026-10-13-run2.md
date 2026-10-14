# Capacity: brisvox with models (Windows, 4 server workers, 2 pool threads)

- command: `brisvox-bench capacity --sim --models models --pool-threads 2 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label brisvox with models (Windows, 4 server workers, 2 pool threads) --note run 2 of 3; Windows x86_64 desktop, NOT a Linux run and NO CPU limit (4 tokio workers, 2 pool threads on a 32-core machine); provisional --steps 1,25,50,100,200,300,400,600,800,1000,1400,1800 --resolution 25 --rounds 2 --fixtures-dir fixtures --out-md bench/results/capacity-brisvox-models-windows-2026-10-13-run2.md --out-json bench/results/capacity-brisvox-models-windows-2026-10-13-run2.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 2 inference pool threads (queue 8192), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:58958/...`
- server: run 2 of 3; Windows x86_64 desktop, NOT a Linux run and NO CPU limit (4 tokio workers, 2 pool threads on a 32-core machine); provisional
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 25, 50, 100, 200, 300, 400, 600, 800, 1000, 1400, 1800]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 2 pool threads, pool queue 8192
- CPU / RSS: read from pid 47768 and its descendants every sample period

**Result: max 200 concurrent calls within the SLO**; the next step up did not pass (first failure at 225).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 816 | 842 | 842 | 842 | -108 | 0.05 | 101 / 101 | 0.12 | 17 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 824 | 847 | 852 | 855 | -103 | 0.31 | 101 / 120 | 0.16 | 17 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 826 | 844 | 855 | 860 | -106 | 0.55 | 102 / 138 | 0.24 | 17 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 817 | 841 | 849 | 865 | -109 | 1.06 | 102 / 204 | 0.29 | 18 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 827 | 885 | 924 | 947 | -65 | 2.26 | 131 / 280 | 0.73 | 18 | pass |
| 225 | 450 (0) | 900 / 900 | 0 / 0 / 0 | 971 | 1457 | 1606 | 1652 | +507 | 2.64 | 133 / 300 | 0.80 | 18 | FAIL |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 1593 | 2623 | 2823 | 2881 | +1673 | 2.93 | 133 / 320 | 0.68 | 19 | FAIL |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 1620 | 2959 | 3282 | 3357 | +2009 | 3.01 | 133 / 354 | 0.79 | 18 | FAIL |

Why steps did not pass:

- 225 concurrent: p95 V2V 1457 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 250 concurrent: p95 V2V 2623 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 300 concurrent: p95 V2V 2958 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
