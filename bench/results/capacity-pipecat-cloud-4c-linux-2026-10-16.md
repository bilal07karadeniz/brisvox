# Capacity summary: pipecat-cloud-4c, Linux, 2026-10-16

- pipecat 4 vCPU / 4 GB, cloud (AMD EPYC 9655P)
- cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- 3 run(s), per-run files `capacity-pipecat-cloud-4c-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 1, range 0-1**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 5 | 1075 | 0.2 | 197.06 | 1120 | 937 | 0.01 | 5 |
| 2 | 1 | 5 | 1070 | 0.18 | 176.18 | 1054 | 873 | 0.01 | 10 |
| 3 | None | 1 | - | - | - | - | - | - | - |

First failing step of each run:

- run 1: 5 calls (found): p95 V2V 1130 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 2: 5 calls (found): p95 V2V 1121 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 3: 1 calls (first_step_failed): p95 V2V 1113 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
