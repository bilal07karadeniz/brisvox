# Capacity: brisvox (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 12534 --floor-ms 950 --label brisvox (4 CPUs, 4g, Linux) --note run 1 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,50,200,400,800,1200,1600,2000,2400,3000 --fixtures-dir /home/user/brisvox/fixtures --resolution 100 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-brisvox-linux-2026-10-13-run1.md --out-json /home/user/brisvox/bench/results/capacity-brisvox-linux-2026-10-13-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 50, 200, 400, 800, 1200, 1600, 2000, 2400, 3000]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 12534 and its descendants every sample period

**Result: max 2100 concurrent calls within the SLO**; the next step up did not pass (first failure at 2200).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 985 | 996 | 996 | 996 | +46 | 0.02 | 17 / 18 | 0.01 | 1 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 984 | 1001 | 1003 | 1006 | +51 | 0.14 | 18 / 57 | 0.07 | 3 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 984 | 1001 | 1005 | 1006 | +51 | 0.38 | 57 / 171 | 0.15 | 3 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 984 | 1001 | 1005 | 1007 | +51 | 0.72 | 169 / 312 | 0.31 | 3 | pass |
| 800 | 1600 (0) | 3200 / 3200 | 0 / 0 / 0 | 984 | 1002 | 1005 | 1009 | +52 | 1.25 | 305 / 606 | 0.53 | 3 | pass |
| 1200 | 2400 (0) | 4800 / 4800 | 0 / 0 / 0 | 984 | 1003 | 1007 | 1019 | +53 | 1.79 | 591 / 908 | 0.79 | 21 | pass |
| 1600 | 3200 (0) | 6400 / 6400 | 0 / 0 / 0 | 986 | 1019 | 1043 | 1098 | +69 | 2.43 | 878 / 1199 | 1.14 | 4 | pass |
| 2000 | 4000 (0) | 8000 / 8000 | 0 / 0 / 0 | 1004 | 1151 | 1196 | 1333 | +201 | 2.94 | 1165 / 1520 | 1.46 | 19 | pass |
| 2100 | 4200 (0) | 8400 / 8400 | 0 / 0 / 0 | 1015 | 1142 | 1187 | 1269 | +192 | 2.98 | 1789 / 1789 | 1.49 | 4 | pass |
| 2200 | 4400 (0) | 8800 / 8800 | 0 / 0 / 0 | 1025 | 1278 | 1366 | 1560 | +328 | 3.13 | 1790 / 1791 | 1.61 | 4 | FAIL |
| 2400 | 4800 (0) | 9506 / 9600 | 0 / 94 / 0 | 1158 | 1800 | 1938 | 2498 | +850 | 2.73 | 1518 / 1808 | 1.49 | 4 | FAIL |

Why steps did not pass:

- 2200 concurrent: p95 V2V 1278 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 2400 concurrent: 94 of 9600 turns lost or not clean (0 talked over, 94 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1799 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
