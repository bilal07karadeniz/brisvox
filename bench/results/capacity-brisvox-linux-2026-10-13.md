# Capacity summary: brisvox, Linux, 2026-10-13

- brisvox (4 CPUs, 4g, Linux)
- Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-brisvox-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 2100, range 2100-2200**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2100 | 2200 | 1142 | 2.98 | 1.42 | 1876 | 1.49 | 4 |
| 2 | 2100 | 2200 | 1163 | 2.9 | 1.38 | 1840 | 1.42 | 4 |
| 3 | 2200 | 2300 | 1188 | 3.02 | 1.37 | 1878 | 1.53 | 4 |

First failing step of each run:

- run 1: 2200 calls (found): p95 V2V 1278 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 2200 calls (found): p95 V2V 1255 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 2300 calls (found): p95 V2V 1250 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
