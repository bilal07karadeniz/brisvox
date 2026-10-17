# Capacity summary: pipecat-cloud-8c, Linux, 2026-10-16

- pipecat 8 vCPU / 8 GB, cloud (AMD EPYC 9655P)
- cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- 3 run(s), per-run files `capacity-pipecat-cloud-8c-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 10, range 5-10**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 10 | 20 | 1046 | 1.98 | 198.27 | 3345 | 2974 | 0.02 | 16 |
| 2 | 5 | 10 | 1051 | 1.01 | 202.32 | 2349 | 1991 | 0.01 | 5 |
| 3 | 10 | 20 | 1052 | 2.1 | 210.34 | 3867 | 3493 | 0.02 | 5 |

First failing step of each run:

- run 1: 20 calls (found): p95 V2V 1443 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 2: 10 calls (found): p95 V2V 1111 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 3: 20 calls (found): 1 of 80 turns lost or not clean (0 talked over, 1 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1673 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
