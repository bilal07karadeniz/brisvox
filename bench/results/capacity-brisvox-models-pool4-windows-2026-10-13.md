# Capacity: brisvox with models (Windows, 4 server workers, 4 pool threads)

- command: `brisvox-bench capacity --sim --models models --pool-threads 4 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label brisvox with models (Windows, 4 server workers, 4 pool threads) --note ablation: 4 pool threads; Windows x86_64 desktop, NOT a Linux run and NO CPU limit; provisional --steps 1,100,200,300,400,500,600,800,1000 --resolution 25 --rounds 2 --fixtures-dir fixtures --out-md bench/results/capacity-brisvox-models-pool4-windows-2026-10-13.md --out-json bench/results/capacity-brisvox-models-pool4-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 4 inference pool threads (queue 8192), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:63814/...`
- server: ablation: 4 pool threads; Windows x86_64 desktop, NOT a Linux run and NO CPU limit; provisional
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 100, 200, 300, 400, 500, 600, 800, 1000]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 4 pool threads, pool queue 8192
- CPU / RSS: read from pid 15060 and its descendants every sample period

**Result: max 175 concurrent calls within the SLO**; the next step up did not pass (first failure at 200).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 786 | 835 | 835 | 835 | -115 | 0.04 | 126 / 129 | 0.11 | 19 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 825 | 843 | 853 | 860 | -107 | 1.81 | 129 / 290 | 0.52 | 19 | pass |
| 150 | 300 (0) | 600 / 600 | 0 / 0 / 0 | 818 | 842 | 856 | 894 | -108 | 2.20 | 218 / 328 | 0.63 | 18 | pass |
| 175 | 350 (0) | 700 / 700 | 0 / 0 / 0 | 824 | 889 | 920 | 964 | -61 | 3.21 | 220 / 348 | 0.75 | 20 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 889 | 1764 | 1844 | 1892 | +814 | 3.97 | 217 / 365 | 0.65 | 19 | FAIL |

Why steps did not pass:

- 200 concurrent: p95 V2V 1763 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
