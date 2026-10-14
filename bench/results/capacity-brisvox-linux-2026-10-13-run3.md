# Capacity: brisvox (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 13154 --floor-ms 950 --label brisvox (4 CPUs, 4g, Linux) --note run 3 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,50,200,400,800,1200,1600,2000,2400,3000 --fixtures-dir /home/user/brisvox/fixtures --resolution 100 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-brisvox-linux-2026-10-13-run3.md --out-json /home/user/brisvox/bench/results/capacity-brisvox-linux-2026-10-13-run3.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 50, 200, 400, 800, 1200, 1600, 2000, 2400, 3000]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 13154 and its descendants every sample period

**Result: max 2200 concurrent calls within the SLO**; the next step up did not pass (first failure at 2300).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 978 | 991 | 991 | 991 | +41 | 0.01 | 17 / 18 | 0.01 | 2 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 985 | 1002 | 1005 | 1005 | +52 | 0.11 | 18 / 55 | 0.05 | 3 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 984 | 1002 | 1005 | 1008 | +52 | 0.34 | 54 / 172 | 0.13 | 16 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 984 | 1001 | 1004 | 1006 | +51 | 0.65 | 170 / 314 | 0.26 | 3 | pass |
| 800 | 1600 (0) | 3200 / 3200 | 0 / 0 / 0 | 984 | 1002 | 1005 | 1008 | +52 | 1.23 | 310 / 607 | 0.52 | 3 | pass |
| 1200 | 2400 (0) | 4800 / 4800 | 0 / 0 / 0 | 984 | 1004 | 1008 | 1044 | +54 | 1.75 | 591 / 907 | 0.79 | 25 | pass |
| 1600 | 3200 (0) | 6400 / 6400 | 0 / 0 / 0 | 986 | 1014 | 1028 | 1063 | +64 | 2.33 | 878 / 1194 | 1.09 | 4 | pass |
| 2000 | 4000 (0) | 8000 / 8000 | 0 / 0 / 0 | 996 | 1082 | 1116 | 1170 | +132 | 2.80 | 1164 / 1497 | 1.38 | 11 | pass |
| 2200 | 4400 (0) | 8800 / 8800 | 0 / 0 / 0 | 1014 | 1188 | 1240 | 1325 | +238 | 3.02 | 1788 / 1791 | 1.53 | 4 | pass |
| 2300 | 4600 (0) | 9200 / 9200 | 0 / 0 / 0 | 1023 | 1251 | 1312 | 1449 | +301 | 3.11 | 1788 / 1791 | 1.60 | 4 | FAIL |
| 2400 | 4800 (0) | 9600 / 9600 | 0 / 0 / 0 | 1063 | 1273 | 1332 | 1479 | +323 | 3.18 | 1459 / 1806 | 1.65 | 4 | FAIL |

Why steps did not pass:

- 2300 concurrent: p95 V2V 1250 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 2400 concurrent: p95 V2V 1272 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
