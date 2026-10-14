# Capacity summary: brisvox-sharedpool-models, Linux, 2026-10-13

- brisvox-sharedpool-models (4 CPUs, 4g, Linux)
- Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-brisvox-sharedpool-models-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 250, range 175-275**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 250 | 275 | 845 | 2.17 | 8.67 | 379 | 0.28 | 3 |
| 2 | 175 | 200 | 824 | 1.56 | 8.94 | 305 | 0.22 | 4 |
| 3 | 275 | 300 | 861 | 2.32 | 8.44 | 377 | 0.26 | 4 |

First failing step of each run:

- run 1: 275 calls (found): 7 of 1100 turns lost or not clean (7 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3688 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 200 calls (found): p95 V2V 1539 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 300 calls (found): p95 V2V 1261 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
