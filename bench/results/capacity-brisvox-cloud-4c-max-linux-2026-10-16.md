# Capacity summary: brisvox-cloud-4c-max, Linux, 2026-10-16

- brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp
- cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- 3 run(s), per-run files `capacity-brisvox-cloud-4c-max-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 430, range 400-480**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 430 | 440 | 3713 | 3.07 | 7.14 | 762 | 753 | 0.4 | 5 |
| 2 | 480 | 490 | 3730 | 2.95 | 6.14 | 806 | 797 | 0.5 | 7 |
| 3 | 400 | 410 | 3742 | 3.01 | 7.52 | 710 | 713 | 0.35 | 23 |

First failing step of each run:

- run 1: 440 calls (found): 2 of 1760 turns lost or not clean (2 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 490 calls (found): 134 of 1960 turns lost or not clean (134 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 410 calls (found): 1 of 1640 turns lost or not clean (1 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
