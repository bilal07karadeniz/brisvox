# Capacity summary: brisvox-cloud-8c, Linux, 2026-10-16

- brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P)
- cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- 3 run(s), per-run files `capacity-brisvox-cloud-8c-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 470, range 460-480**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 480 | 490 | 961 | 4.51 | 9.4 | 951 | 942 | 0.46 | 20 |
| 2 | 460 | 470 | 938 | 4.62 | 10.05 | 930 | 921 | 0.46 | 7 |
| 3 | 470 | 480 | 1074 | 4.69 | 9.97 | 939 | 930 | 0.46 | 10 |

First failing step of each run:

- run 1: 490 calls (found): p95 V2V 1250 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 2: 470 calls (found): p95 V2V 1181 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 3: 480 calls (found): p95 V2V 1312 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
