# Capacity: brisvox with models (Windows, 4 server workers, 2 pool threads)

- command: `brisvox-bench capacity --sim --models models --pool-threads 2 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label brisvox with models (Windows, 4 server workers, 2 pool threads) --note run 3 of 3; Windows x86_64 desktop, NOT a Linux run and NO CPU limit (4 tokio workers, 2 pool threads on a 32-core machine); provisional --steps 1,25,50,100,200,300,400,600,800,1000,1400,1800 --resolution 25 --rounds 2 --fixtures-dir fixtures --out-md bench/results/capacity-brisvox-models-windows-2026-10-13-run3.md --out-json bench/results/capacity-brisvox-models-windows-2026-10-13-run3.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 2 inference pool threads (queue 8192), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:54471/...`
- server: run 3 of 3; Windows x86_64 desktop, NOT a Linux run and NO CPU limit (4 tokio workers, 2 pool threads on a 32-core machine); provisional
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 25, 50, 100, 200, 300, 400, 600, 800, 1000, 1400, 1800]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 2 pool threads, pool queue 8192
- CPU / RSS: read from pid 660 and its descendants every sample period

**Result: max 200 concurrent calls within the SLO**; the next step up did not pass (first failure at 225).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 822 | 842 | 842 | 842 | -108 | 0.03 | 101 / 101 | 0.11 | 17 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 824 | 844 | 856 | 857 | -106 | 0.36 | 101 / 120 | 0.22 | 18 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 823 | 844 | 854 | 860 | -106 | 0.55 | 102 / 138 | 0.25 | 18 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 821 | 843 | 855 | 858 | -107 | 0.91 | 102 / 203 | 0.27 | 17 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 823 | 885 | 929 | 945 | -65 | 1.99 | 131 / 279 | 0.50 | 18 | pass |
| 225 | 450 (0) | 900 / 900 | 0 / 0 / 0 | 1102 | 1800 | 2045 | 2138 | +850 | 2.74 | 135 / 299 | 0.75 | 18 | FAIL |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 1352 | 2205 | 2268 | 2317 | +1255 | 2.79 | 133 / 318 | 0.62 | 18 | FAIL |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 1144 | 3389 | 3582 | 3634 | +2439 | 2.98 | 133 / 354 | 0.81 | 18 | FAIL |

Why steps did not pass:

- 225 concurrent: p95 V2V 1800 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 250 concurrent: p95 V2V 2204 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 300 concurrent: p95 V2V 3388 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
