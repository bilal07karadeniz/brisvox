# Capacity summary: brisvox-models, Linux, 2026-10-13

- brisvox-models (4 CPUs, 4g, Linux)
- Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-brisvox-models-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 200, range 125-250**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 250 | 275 | 915 | 2.03 | 8.13 | 387 | 0.27 | 3 |
| 2 | 200 | 225 | 833 | 1.61 | 8.04 | 303 | 0.19 | 3 |
| 3 | 125 | 150 | 870 | 1.76 | 14.04 | 265 | 0.28 | 4 |

First failing step of each run:

- run 1: 275 calls (found): p95 V2V 2495 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 225 calls (found): p95 V2V 1239 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 150 calls (found): p95 V2V 1592 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
