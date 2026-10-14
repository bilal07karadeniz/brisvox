# Capacity: m2-old-r1

- command: `brisvox-bench capacity --sim --models <repo>/models --pool-threads 2 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label m2-old-r1 --note m2-old-r1; Windows, no CPU limit, provisional --steps 200,250,300,350 --rounds 2 --fixtures-dir fixtures --keep-going --turn-threads 0 --out-md bench/results/m2-old-r1.md --out-json bench/results/m2-old-r1.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 2 inference pool threads (queue 8192), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:55927/...`
- server: m2-old-r1; Windows, no CPU limit, provisional
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [200, 250, 300, 350]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 2 pool threads, pool queue 8192
- CPU / RSS: read from pid 56940 and its descendants every sample period

**Result: no passing step**; even the smallest step did not pass (first failure at 200).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 824 | 1390 | 1442 | 1485 | +440 | 2.36 | 98 / 280 | 0.49 | 17 | FAIL |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 1031 | 2057 | 2233 | 2344 | +1107 | 2.96 | 133 / 318 | 0.57 | 18 | FAIL |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 1693 | 3656 | 3720 | 3766 | +2706 | 3.64 | 133 / 358 | 0.77 | 18 | FAIL |
| 350 | 700 (0) | 1303 / 1400 | 97 / 0 / 0 | 2189 | 3762 | 3929 | 3998 | +2812 | 3.97 | 134 / 395 | 0.86 | 18 | FAIL |

Why steps did not pass:

- 200 concurrent: p95 V2V 1389 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 250 concurrent: p95 V2V 2056 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 300 concurrent: p95 V2V 3655 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 350 concurrent: 97 of 1400 turns lost or not clean (97 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3761 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
