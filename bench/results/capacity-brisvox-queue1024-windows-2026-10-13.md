# Capacity: brisvox, 2 pool threads, queue 1024

- command: `brisvox-bench capacity --sim --label brisvox, 2 pool threads, queue 1024 --server-workers 4 --pool-threads 2 --pool-queue 1024 --steps 50,200,400,600,800,1000,1200 --resolution 100 --rounds 2 --client-workers 4 --out-md bench/results/capacity-brisvox-queue1024-windows-2026-10-13.md --out-json bench/results/capacity-brisvox-queue1024-windows-2026-10-13.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the production engine over `brisvox_sim` mock providers behind the real websocket transport, as a child process (`brisvox-bench engine-server`): 4 tokio workers, 2 inference pool threads (queue 1024), up to 1024 calls; the load generator is this process (4 tokio workers, the worker count in the line above)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:65399/...`
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [50, 200, 400, 600, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- engine target: mock providers STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms, energy VAD stop 300 ms, `smart_turn` end of turn, 2 pool threads, pool queue 1024
- CPU / RSS: read from pid 55700 and its descendants every sample period

**Result: max 1000 concurrent calls within the SLO**; the next step up did not pass (first failure at 1100).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 1010 | 1035 | 1043 | 1048 | +85 | 0.08 | 19 / 57 | 0.10 | 18 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 1004 | 1026 | 1039 | 1043 | +76 | 0.36 | 21 / 171 | 0.29 | 21 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1000 | 1017 | 1026 | 1040 | +67 | 0.68 | 23 / 319 | 0.47 | 20 | pass |
| 600 | 1200 (0) | 2400 / 2400 | 0 / 0 / 0 | 1002 | 1022 | 1028 | 1040 | +72 | 1.14 | 26 / 468 | 0.75 | 17 | pass |
| 800 | 1600 (0) | 3200 / 3200 | 0 / 0 / 0 | 1003 | 1032 | 1039 | 1055 | +82 | 1.50 | 28 / 619 | 0.91 | 19 | pass |
| 1000 | 2000 (0) | 4000 / 4000 | 0 / 0 / 0 | 1003 | 1032 | 1044 | 1065 | +82 | 1.88 | 31 / 768 | 1.20 | 31 | pass |
| 1100 | 2200 (152) | 4096 / 4400 | 0 / 0 / 0 | 1001 | 1033 | 1044 | 1069 | +83 | 1.93 | 35 / 818 | 1.09 | 38 | FAIL |
| 1200 | 2400 (354) | 4092 / 4800 | 0 / 0 / 0 | 1003 | 1034 | 1048 | 1066 | +84 | 1.84 | 34 / 813 | 1.19 | 41 | FAIL |

Why steps did not pass:

- 1100 concurrent: 152 of 2200 calls failed; 304 of 4400 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 1100 concurrent, failed call: call: connect: HTTP error: 503 Service Unavailable
- 1200 concurrent: 354 of 2400 calls failed; 708 of 4800 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 1200 concurrent, failed call: call: connect: HTTP error: 503 Service Unavailable
- 1200 concurrent, failed call: call: connect: IO error: Normal olarak her yuva adresi (iletişim kuralı/ağ adresi/bağlantı noktası) için yalnızca bir kullanıma izin veriliyor. (os error 10048)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
