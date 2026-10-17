# Capacity summary: pipecat-cloud-8c-max, Linux, 2026-10-16

- pipecat 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp
- cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- 3 run(s), per-run files `capacity-pipecat-cloud-8c-max-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 10, range 10-20**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 10 | 20 | 1126 | 1.95 | 194.64 | 3623 | 3289 | 0.02 | 6 |
| 2 | 20 | 30 | 1268 | 3.55 | 177.54 | 5910 | 5537 | 0.03 | 8 |
| 3 | 10 | 20 | 1109 | 2.16 | 216.28 | 2938 | 2577 | 0.02 | 7 |

First failing step of each run:

- run 1: 20 calls (found): 3 of 80 turns lost or not clean (0 talked over, 3 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 30 calls (found): 2 of 60 calls failed; 14 of 120 turns lost or not clean (7 talked over, 3 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 20 calls (found): 1 of 80 turns lost or not clean (0 talked over, 1 no response, 0 bot speaking; allowed 0.0 %)
