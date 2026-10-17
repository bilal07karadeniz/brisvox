# Capacity summary: brisvox-cloud-4c, Linux, 2026-10-16

- brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P)
- cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- 3 run(s), per-run files `capacity-brisvox-cloud-4c-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 250, range 220-260**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 260 | 270 | 1035 | 2.44 | 9.38 | 549 | 540 | 0.27 | 10 |
| 2 | 250 | 260 | 972 | 2.37 | 9.47 | 545 | 535 | 0.31 | 13 |
| 3 | 220 | 230 | 898 | 2.35 | 10.7 | 504 | 495 | 0.26 | 9 |

First failing step of each run:

- run 1: 270 calls (found): p95 V2V 1250 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 2: 260 calls (found): p95 V2V 1121 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 3: 230 calls (found): p95 V2V 1175 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
