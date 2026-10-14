# Capacity summary: brisvox-turnexec-models, Linux, 2026-10-13

- brisvox-turnexec-models (4 CPUs, 4g, Linux)
- Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-brisvox-turnexec-models-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 250, range 175-300**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 300 | 325 | 1079 | 2.52 | 8.41 | 387 | 0.29 | 3 |
| 2 | 175 | 200 | 824 | 1.57 | 8.99 | 305 | 0.24 | 2 |
| 3 | 250 | 275 | 837 | 2.4 | 9.59 | 385 | 0.3 | 4 |

First failing step of each run:

- run 1: 325 calls (found): p95 V2V 1875 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 200 calls (found): p95 V2V 1510 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 275 calls (found): p95 V2V 1414 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
