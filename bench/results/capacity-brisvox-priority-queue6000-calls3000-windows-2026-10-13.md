# Capacity: brisvox, 2 pool threads, priority lines, queue 6000, 3000 calls allowed

- command: `brisvox-bench capacity --sim --label brisvox, 2 pool threads, priority lines, queue 6000, 3000 calls allowed --note extended ramp after the high-priority VAD line; the pool queue is sized 2 x max calls as brisvox-server does --server-workers 4 --pool-threads 2 --pool-queue 6000 --max-calls 3000 --steps 1500,1800,2000,2200,2400 --keep-going --rounds 2 --client-workers 4 --out-md bench/results/capacity-brisvox-priority-queue6000-calls3000-windows-2026-10-13.md --out-json bench/results/capacity-brisvox-priority-queue6000-calls3000-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`): 4 tokio workers, 2 inference pool threads (queue 6000), up to 3000 calls; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:56370/...`
- server: extended ramp after the high-priority VAD line; the pool queue is sized 2 x max calls as brisvox-server does
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1500, 1800, 2000, 2200, 2400]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, energy VAD stop 300 ms, `smart_turn` end of turn, 2 pool threads, pool queue 6000
- CPU / RSS: read from pid 57852 and its descendants every sample period

**Result: max 1800 concurrent calls within the SLO**; the next step up did not pass (first failure at 2000).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1500 | 3000 (0) | 6000 / 6000 | 0 / 0 / 0 | 1014 | 1072 | 1106 | 1193 | +122 | 2.74 | 16 / 1132 | 1.50 | 57 | pass |
| 1800 | 3600 (0) | 7200 / 7200 | 0 / 0 / 0 | 1043 | 1168 | 1212 | 1319 | +218 | 3.12 | 31 / 1368 | 1.87 | 49 | pass |
| 2000 | 4000 (0) | 8000 / 8000 | 0 / 0 / 0 | 1084 | 1224 | 1282 | 1399 | +274 | 3.15 | 36 / 1515 | 2.06 | 55 | FAIL |
| 2200 | 4400 (0) | 8798 / 8800 | 0 / 2 / 0 | 1128 | 1324 | 1402 | 2547 | +374 | 2.53 | 42 / 1720 | 1.78 | 36 | FAIL |
| 2400 | 4800 (2) | 9370 / 9600 | 0 / 226 / 0 | 1163 | 1398 | 1515 | 2724 | +448 | 2.52 | 41 / 1882 | 1.92 | 32 | FAIL |

Why steps did not pass:

- 2000 concurrent: p95 V2V 1224 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 2200 concurrent: 2 of 8800 turns lost or not clean (0 talked over, 2 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1323 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 2400 concurrent: 2 of 4800 calls failed; 230 of 9600 turns lost or not clean (0 talked over, 226 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1397 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 2400 concurrent, failed call: call: connect: IO error: Normal olarak her yuva adresi (iletişim kuralı/ağ adresi/bağlantı noktası) için yalnızca bir kullanıma izin veriliyor. (os error 10048)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
