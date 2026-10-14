# Capacity: pipecat-tuned (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 21369 --floor-ms 950 --label pipecat-tuned (4 CPUs, 4g, Linux) --note run 1 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,20,40,80,120,160,240,320,480 --fixtures-dir /home/user/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-pipecat-tuned-linux-2026-10-13-run1.md --out-json /home/user/brisvox/bench/results/capacity-pipecat-tuned-linux-2026-10-13-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 20, 40, 80, 120, 160, 240, 320, 480]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 21369 and its descendants every sample period

**Result: max 70 concurrent calls within the SLO**; the next step up did not pass (first failure at 80).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1008 | 1027 | 1027 | 1027 | +77 | 0.05 | 550 / 554 | 0.01 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1020 | 1039 | 1042 | 1042 | +89 | 0.67 | 554 / 708 | 0.04 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 1029 | 1105 | 1257 | 1290 | +155 | 1.25 | 706 / 945 | 0.07 | 3 | pass |
| 60 | 120 (0) | 240 / 240 | 0 / 0 / 0 | 1025 | 1049 | 1058 | 1062 | +99 | 1.74 | 1348 / 1432 | 0.10 | 3 | pass |
| 70 | 140 (0) | 280 / 280 | 0 / 0 / 0 | 1025 | 1036 | 1081 | 1124 | +86 | 2.03 | 1431 / 1502 | 0.12 | 3 | pass |
| 80 | 160 (0) | 311 / 320 | 9 / 0 / 0 | 1031 | 2613 | 3650 | 3887 | +1663 | 2.15 | 942 / 1359 | 0.11 | 26 | FAIL |

Why steps did not pass:

- 80 concurrent: 9 of 320 turns lost or not clean (9 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 2613 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
