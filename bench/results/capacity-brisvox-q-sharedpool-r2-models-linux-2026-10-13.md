# Capacity summary: brisvox-q-sharedpool-r2-models, Linux, 2026-10-13

- brisvox-q-sharedpool-r2-models (4 CPUs, 4g, Linux)
- Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 1 run(s), per-run files `capacity-brisvox-q-sharedpool-r2-models-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 275, range 275-275**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 275 | 300 | 986 | 2.32 | 8.42 | 380 | 0.28 | 2 |

First failing step of each run:

- run 1: 300 calls (found): p95 V2V 1336 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
