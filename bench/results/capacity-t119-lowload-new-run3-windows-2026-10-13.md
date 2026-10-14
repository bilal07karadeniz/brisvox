# Capacity: m3-new-r3

- command: `brisvox-bench capacity --sim --models <repo>/models --pool-threads 2 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label m3-new-r3 --note m3-new-r3; Windows, no CPU limit, provisional --steps 1,5,10 --rounds 2 --fixtures-dir fixtures --keep-going --rounds 6 --out-md bench/results/m3-new-r3.md --out-json bench/results/m3-new-r3.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 2 inference pool threads (queue 8192) plus 2 Smart Turn executor thread(s), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:52323/...`
- server: m3-new-r3; Windows, no CPU limit, provisional
- every call replays `en_two_utterances` (2 utterances) in real time; 6 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 2 pool threads, pool queue 8192
- CPU / RSS: read from pid 16816 and its descendants every sample period

**Result: max 10 concurrent calls within the SLO**; every step passed: a lower bound, the limit is above the ramp.

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 6 (0) | 12 / 12 | 0 / 0 / 0 | 825 | 844 | 844 | 844 | -106 | 0.02 | 101 / 101 | 0.06 | 17 | pass |
| 5 | 30 (0) | 60 / 60 | 0 / 0 / 0 | 823 | 856 | 859 | 859 | -94 | 0.09 | 101 / 105 | 0.08 | 17 | pass |
| 10 | 60 (0) | 120 / 120 | 0 / 0 / 0 | 823 | 849 | 857 | 858 | -101 | 0.17 | 102 / 109 | 0.11 | 17 | pass |

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
