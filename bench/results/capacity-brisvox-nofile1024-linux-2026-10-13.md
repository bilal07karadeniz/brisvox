# Capacity summary: brisvox-nofile1024, Linux, 2026-10-13

- brisvox-nofile1024 (4 CPUs, 4g, Linux)
- Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1024:1024 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 1 run(s), per-run files `capacity-brisvox-nofile1024-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 1000, range 1000-1000**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1000 | 1100 | 1003 | 1.59 | 1.59 | 784 | 0.7 | 8 |

First failing step of each run:

- run 1: 1100 calls (found): 166 of 2200 calls failed; 332 of 4400 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
