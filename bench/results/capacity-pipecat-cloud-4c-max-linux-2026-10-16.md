# Capacity summary: pipecat-cloud-4c-max, Linux, 2026-10-16

- pipecat 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp
- cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- 3 run(s), per-run files `capacity-pipecat-cloud-4c-max-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 10, range 10-10**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 10 | 20 | 1146 | 1.42 | 141.85 | 2590 | 2399 | 0.02 | 4 |
| 2 | 10 | 20 | 1133 | 1.37 | 137.32 | 2573 | 2383 | 0.02 | 14 |
| 3 | 10 | 20 | 1104 | 1.35 | 135.02 | 2745 | 2555 | 0.02 | 6 |

First failing step of each run:

- run 1: 20 calls (found): 1 of 40 calls failed; 21 of 80 turns lost or not clean (19 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 20 calls (found): 7 of 80 turns lost or not clean (5 talked over, 2 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 20 calls (found): 3 of 40 calls failed; 8 of 80 turns lost or not clean (0 talked over, 2 no response, 0 bot speaking; allowed 0.0 %)
