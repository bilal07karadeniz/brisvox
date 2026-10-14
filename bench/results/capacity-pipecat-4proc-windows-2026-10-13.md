# Capacity: pipecat, 4 process(es)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8767 --pid 45176 --floor-ms 950 --label pipecat, 4 process(es) --note Pipecat 1.12.0 bot, Python 3.13.14, Windows: `python bench/pipecat_baseline/bot.py --mock-providers --port 8767 --workers 4 --pid-file ...` (mock STT/LLM/TTS with the brisvox_sim latencies, energy VAD, fixed Smart Turn model; one pipeline pre-warmed per process) --steps 1,40,120,160,200,240,320 --resolution 10 --rounds 2 --client-workers 4 --out-md bench/results/capacity-pipecat-4proc-windows-2026-10-13.md --out-json bench/results/capacity-pipecat-4proc-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:8767`
- server: Pipecat 1.12.0 bot, Python 3.13.14, Windows: `python bench/pipecat_baseline/bot.py --mock-providers --port 8767 --workers 4 --pid-file ...` (mock STT/LLM/TTS with the brisvox_sim latencies, energy VAD, fixed Smart Turn model; one pipeline pre-warmed per process)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 40, 120, 160, 200, 240, 320]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a step whose frames went out more than 100 ms late is generator limited
- CPU / RSS: read from pid 45176 and its descendants every sample period

**Result: max 160 concurrent calls within the SLO**; the next step up did not pass (first failure at 170).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1043 | 1068 | 1068 | 1068 | +118 | 0.05 | 553 / 572 | 0.05 | 17 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 1043 | 1061 | 1073 | 1077 | +111 | 0.78 | 560 / 673 | 0.09 | 18 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 1037 | 1058 | 1063 | 1072 | +108 | 2.63 | 620 / 844 | 0.27 | 17 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 1040 | 1086 | 1293 | 1409 | +136 | 3.49 | 767 / 968 | 0.34 | 18 | pass |
| 170 | 340 (0) | 680 / 680 | 0 / 0 / 0 | 1051 | 1468 | 2208 | 2604 | +518 | 3.79 | 968 / 994 | 0.31 | 18 | FAIL |
| 180 | 360 (46) | 628 / 720 | 0 / 0 / 0 | 1054 | 1148 | 1175 | 1262 | +198 | 2.56 | 819 / 1046 | 0.20 | 17 | FAIL |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 1152 | 1986 | 2916 | 3259 | +1036 | 4.00 | 803 / 1024 | 0.32 | 17 | FAIL |

Why steps did not pass:

- 170 concurrent: p95 V2V 1468 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 180 concurrent: 46 of 360 calls failed; 92 of 720 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 180 concurrent, failed call: a write timed out
- 200 concurrent: p95 V2V 1986 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
