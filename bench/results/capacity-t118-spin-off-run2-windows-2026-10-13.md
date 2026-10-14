# Capacity: brisvox with models, spin-off-run2 (Windows)

- command: `brisvox-bench capacity --sim --models models --pool-threads 2 --server-workers 4 --client-workers 8 --max-calls 4096 --floor-ms 950 --stats-every-ms 10000 --label brisvox with models, spin-off-run2 (Windows) --note A/B spin-off-run2; Windows x86_64 desktop, NOT a Linux run, no CPU limit; a WSL benchmark ran alongside --steps 150,200,250 --rounds 2 --fixtures-dir fixtures --out-md bench/results/capacity-t118-spin-off-run2-windows-2026-10-13.md --out-json bench/results/capacity-t118-spin-off-run2-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`), with the real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime) instead of the energy VAD and the Smart Turn stub: 4 tokio workers, 2 inference pool threads (queue 8192), up to 4096 calls; the load generator is this process (8 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:53108/...`
- server: A/B spin-off-run2; Windows x86_64 desktop, NOT a Linux run, no CPU limit; a WSL benchmark ran alongside
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [150, 200, 250]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, real models (Silero VAD on tract, Smart Turn v3.2 on ONNX Runtime), `smart_turn` end of turn, 2 pool threads, pool queue 8192
- CPU / RSS: read from pid 8824 and its descendants every sample period

**Result: max 200 concurrent calls within the SLO**; the next step up did not pass (first failure at 250).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 150 | 300 (0) | 600 / 600 | 0 / 0 / 0 | 819 | 846 | 853 | 866 | -104 | 1.37 | 101 / 241 | 0.39 | 17 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 826 | 864 | 880 | 886 | -86 | 2.00 | 132 / 279 | 0.49 | 19 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 997 | 1751 | 1951 | 1983 | +801 | 2.79 | 133 / 318 | 0.77 | 18 | FAIL |

Why steps did not pass:

- 250 concurrent: p95 V2V 1751 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
