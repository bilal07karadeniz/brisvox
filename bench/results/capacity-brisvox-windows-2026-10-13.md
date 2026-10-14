# Capacity: brisvox

- command: `brisvox-bench capacity --sim --label brisvox --server-workers 4 --pool-threads 2 --steps 1,5,10,20,50,100,200,400,600 --resolution 50 --rounds 2 --client-workers 4 --out-md bench/results/capacity-brisvox-windows-2026-10-13.md --out-json bench/results/capacity-brisvox-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`): 4 tokio workers, 2 inference pool threads, up to 1024 calls; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:52104/...`
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 50, 100, 200, 400, 600]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a step whose frames went out more than 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, energy VAD stop 300 ms, `smart_turn` end of turn, 2 pool threads
- CPU / RSS: read from pid 57372 and its descendants every sample period

**Result: max 400 concurrent calls within the SLO**; the next step up did not pass (first failure at 450).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1011 | 1033 | 1033 | 1033 | +83 | 0.02 | 20 / 20 | 0.05 | 17 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 1007 | 1030 | 1038 | 1038 | +80 | 0.01 | 20 / 23 | 0.06 | 17 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 1008 | 1030 | 1034 | 1034 | +80 | 0.04 | 20 / 27 | 0.08 | 17 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1012 | 1027 | 1030 | 1030 | +77 | 0.07 | 20 / 35 | 0.10 | 17 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 1013 | 1032 | 1037 | 1042 | +82 | 0.07 | 20 / 57 | 0.08 | 17 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 1013 | 1034 | 1044 | 1045 | +84 | 0.28 | 21 / 94 | 0.23 | 17 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 1007 | 1025 | 1037 | 1043 | +75 | 0.48 | 22 / 169 | 0.35 | 17 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 998 | 1015 | 1025 | 1042 | +65 | 0.69 | 23 / 320 | 0.48 | 16 | pass |
| 450 | 900 (0) | 1791 / 1800 | 0 / 9 / 0 | 1001 | 1021 | 1032 | 1043 | +71 | 0.62 | 29 / 362 | 0.42 | 20 | FAIL |
| 500 | 1000 (0) | 1990 / 2000 | 0 / 10 / 0 | 999 | 1019 | 1034 | 3762 | +69 | 0.64 | 28 / 400 | 0.43 | 17 | FAIL |
| 600 | 1200 (0) | 1154 / 2400 | 0 / 1244 / 2 | 1002 | 3555 | 3805 | 3857 | +2605 | 0.82 | 27 / 468 | 0.58 | 18 | FAIL |

Why steps did not pass:

- 450 concurrent: 9 of 1800 turns lost or not clean (0 talked over, 9 no response, 0 bot speaking; allowed 0.0 %)
- 500 concurrent: 10 of 2000 turns lost or not clean (0 talked over, 10 no response, 0 bot speaking; allowed 0.0 %)
- 600 concurrent: 1246 of 2400 turns lost or not clean (0 talked over, 1244 no response, 2 bot speaking; allowed 0.0 %); p95 V2V 3555 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
