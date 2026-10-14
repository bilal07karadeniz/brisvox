# Capacity: m2-new-r1

- command: `brisvox-bench capacity --sim --models <repo>/models --pool-threads 2 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label m2-new-r1 --note m2-new-r1; Windows, no CPU limit, provisional --steps 200,250,300,350 --rounds 2 --fixtures-dir fixtures --keep-going --out-md bench/results/m2-new-r1.md --out-json bench/results/m2-new-r1.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 2 inference pool threads (queue 8192) plus 2 Smart Turn executor thread(s), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:51214/...`
- server: m2-new-r1; Windows, no CPU limit, provisional
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [200, 250, 300, 350]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 2 pool threads, pool queue 8192
- CPU / RSS: read from pid 26664 and its descendants every sample period

**Result: max 300 concurrent calls within the SLO**; the next step up did not pass (first failure at 350).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 817 | 837 | 847 | 862 | -113 | 2.35 | 101 / 278 | 0.45 | 34 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 814 | 840 | 844 | 859 | -110 | 2.60 | 132 / 315 | 0.56 | 19 | pass |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 816 | 838 | 847 | 863 | -112 | 2.97 | 132 / 354 | 0.64 | 19 | pass |
| 350 | 700 (0) | 1400 / 1400 | 0 / 0 / 0 | 932 | 1341 | 1408 | 1442 | +391 | 3.81 | 133 / 391 | 0.81 | 18 | FAIL |

Why steps did not pass:

- 350 concurrent: p95 V2V 1341 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
