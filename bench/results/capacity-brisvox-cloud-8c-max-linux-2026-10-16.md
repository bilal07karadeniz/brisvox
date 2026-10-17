# Capacity summary: brisvox-cloud-8c-max, Linux, 2026-10-16

- brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp
- cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- 3 run(s), per-run files `capacity-brisvox-cloud-8c-max-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 780, range 740-800**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 740 | 750 | 3735 | 5.96 | 8.05 | 1254 | 1245 | 0.66 | 5 |
| 2 | 780 | 790 | 3738 | 5.75 | 7.38 | 1314 | 1305 | 0.77 | 10 |
| 3 | 800 | 806 | 3729 | 5.94 | 7.42 | 1335 | 1325 | 0.72 | 10 |

First failing step of each run:

- run 1: 750 calls (found): 13 of 3000 turns lost or not clean (13 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 790 calls (found): 11 of 3160 turns lost or not clean (11 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 806 calls (found): 18 of 3224 turns lost or not clean (18 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
