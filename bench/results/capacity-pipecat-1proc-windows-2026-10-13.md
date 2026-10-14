# Capacity: pipecat, 1 process(es)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8765 --pid 10628 --floor-ms 950 --label pipecat, 1 process(es) --note Pipecat 1.12.0 bot, Python 3.13.14, Windows: `python bench/pipecat_baseline/bot.py --mock-providers --port 8765 --workers 1 --pid-file ...` (mock STT/LLM/TTS with the brisvox_sim latencies, energy VAD, fixed Smart Turn model; one pipeline pre-warmed per process) --steps 1,5,10,20,30,40,60 --resolution 5 --rounds 2 --client-workers 4 --out-md bench/results/capacity-pipecat-1proc-windows-2026-10-13.md --out-json bench/results/capacity-pipecat-1proc-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:8765`
- server: Pipecat 1.12.0 bot, Python 3.13.14, Windows: `python bench/pipecat_baseline/bot.py --mock-providers --port 8765 --workers 1 --pid-file ...` (mock STT/LLM/TTS with the brisvox_sim latencies, energy VAD, fixed Smart Turn model; one pipeline pre-warmed per process)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 30, 40, 60]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a step whose frames went out more than 100 ms late is generator limited
- CPU / RSS: read from pid 10628 and its descendants every sample period

**Result: max 50 concurrent calls within the SLO**; the next step up did not pass (first failure at 55).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1045 | 1066 | 1066 | 1066 | +116 | 0.05 | 128 / 141 | 0.05 | 17 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 1043 | 1069 | 1070 | 1070 | +119 | 0.09 | 130 / 151 | 0.06 | 18 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 1048 | 1064 | 1065 | 1065 | +114 | 0.19 | 138 / 157 | 0.07 | 17 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1041 | 1056 | 1060 | 1060 | +106 | 0.38 | 143 / 186 | 0.10 | 18 | pass |
| 30 | 60 (0) | 120 / 120 | 0 / 0 / 0 | 1040 | 1058 | 1079 | 1082 | +108 | 0.52 | 170 / 230 | 0.10 | 17 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 1043 | 1064 | 1067 | 1068 | +114 | 0.71 | 212 / 218 | 0.12 | 17 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 1048 | 1090 | 1100 | 1127 | +140 | 0.93 | 193 / 248 | 0.14 | 17 | pass |
| 55 | 110 (0) | 220 / 220 | 0 / 0 / 0 | 1078 | 1211 | 1266 | 1347 | +261 | 1.00 | 194 / 252 | 0.15 | 17 | FAIL |
| 60 | 120 (0) | 235 / 240 | 5 / 0 / 0 | 1205 | 2863 | 3846 | 3938 | +1913 | 1.03 | 197 / 255 | 0.11 | 17 | FAIL |

Why steps did not pass:

- 55 concurrent: p95 V2V 1211 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 60 concurrent: 5 of 240 turns lost or not clean (5 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 2863 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
