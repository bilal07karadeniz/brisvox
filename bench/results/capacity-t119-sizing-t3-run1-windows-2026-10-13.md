# Capacity: m2-t3-r1

- command: `brisvox-bench capacity --sim --models <repo>/models --pool-threads 2 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label m2-t3-r1 --note m2-t3-r1; Windows, no CPU limit, provisional --steps 300,350,400 --rounds 2 --fixtures-dir fixtures --keep-going --turn-threads 3 --out-md bench/results/m2-t3-r1.md --out-json bench/results/m2-t3-r1.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 2 inference pool threads (queue 8192) plus 3 Smart Turn executor thread(s), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:59691/...`
- server: m2-t3-r1; Windows, no CPU limit, provisional
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [300, 350, 400]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 2 pool threads, pool queue 8192
- CPU / RSS: read from pid 31672 and its descendants every sample period

**Result: no passing step**; even the smallest step did not pass (first failure at 300).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 850 | 1222 | 1257 | 1278 | +272 | 4.39 | 115 / 398 | 0.94 | 18 | FAIL |
| 350 | 700 (0) | 1400 / 1400 | 0 / 0 / 0 | 923 | 1734 | 1872 | 1901 | +784 | 5.29 | 179 / 437 | 1.01 | 20 | FAIL |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 839 | 1422 | 1482 | 1519 | +472 | 5.07 | 180 / 475 | 1.10 | 19 | FAIL |

Why steps did not pass:

- 300 concurrent: p95 V2V 1222 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 350 concurrent: p95 V2V 1734 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 400 concurrent: p95 V2V 1422 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
