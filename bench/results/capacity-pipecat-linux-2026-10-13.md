# Capacity summary: pipecat, Linux, 2026-10-13

- pipecat (4 CPUs, 4g, Linux)
- Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-pipecat-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 70, range 70-70**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 70 | 80 | 1097 | 2.88 | 41.2 | 1604 | 0.13 | 4 |
| 2 | 70 | 80 | 1089 | 2.86 | 40.86 | 1604 | 0.13 | 2 |
| 3 | 70 | 80 | 1064 | 2.88 | 41.12 | 1584 | 0.12 | 3 |

First failing step of each run:

- run 1: 80 calls (found): p95 V2V 1376 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 80 calls (found): p95 V2V 1256 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 80 calls (found): 32 of 320 turns lost or not clean (32 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1305 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
