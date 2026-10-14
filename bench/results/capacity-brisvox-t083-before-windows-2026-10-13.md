# Capacity: base

- command: `brisvox-bench capacity --sim --label base --server-workers 4 --pool-threads 2 --pool-queue 1024 --max-calls 2048 --steps 1200,1500 --rounds 2 --client-workers 4 --stats-every-ms 2000 --out-md bench/results/capacity-brisvox-t083-before-windows-2026-10-13.md --out-json bench/results/capacity-brisvox-t083-before-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`): 4 tokio workers, 2 inference pool threads (queue 1024), up to 2048 calls; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:60343/...`
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1200, 1500]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, energy VAD stop 300 ms, `smart_turn` end of turn, 2 pool threads, pool queue 1024
- CPU / RSS: read from pid 30192 and its descendants every sample period

**Result: max 1200 concurrent calls within the SLO**; the next step up did not pass (first failure at 1500).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1200 | 2400 (0) | 4800 / 4800 | 0 / 0 / 0 | 1006 | 1047 | 1062 | 1097 | +97 | 2.08 | 19 / 911 | 1.24 | 23 | pass |
| 1500 | 3000 (1) | 4923 / 6000 | 0 / 1054 / 21 | 1017 | 2357 | 3758 | 3843 | +1407 | 2.36 | 32 / 1127 | 1.40 | 20 | FAIL |

Why steps did not pass:

- 1500 concurrent: 1 of 3000 calls failed; 1077 of 6000 turns lost or not clean (0 talked over, 1054 no response, 21 bot speaking; allowed 0.0 %); p95 V2V 2357 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 1500 concurrent, failed call: call: connect: IO error: Normal olarak her yuva adresi (iletişim kuralı/ağ adresi/bağlantı noktası) için yalnızca bir kullanıma izin veriliyor. (os error 10048)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
