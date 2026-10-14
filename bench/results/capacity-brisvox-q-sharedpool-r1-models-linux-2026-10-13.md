# Capacity summary: brisvox-q-sharedpool-r1-models, Linux, 2026-10-13

- brisvox-q-sharedpool-r1-models (4 CPUs, 4g, Linux)
- Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 1 run(s), per-run files `capacity-brisvox-q-sharedpool-r1-models-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 300, range 300-300**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 300 | 325 | 1115 | 2.47 | 8.24 | 376 | 0.25 | 4 |

First failing step of each run:

- run 1: 325 calls (found): p95 V2V 1808 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
