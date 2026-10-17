# Capacity: brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P)

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 250 --label brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P) --note <see server> --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-8c-linux-2026-10-16-run3.md --out-json capacity-brisvox-cloud-8c-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 470 concurrent calls within the SLO**; the next step up did not pass (first failure at 480).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 869 | 869 | 869 | 869 | +9 | - | - | - | 0.01 | 4 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 868 | 870 | 871 | 871 | +10 | - | - | - | 0.01 | 7 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 868 | 870 | 872 | 872 | +10 | - | - | - | 0.02 | 5 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 871 | 878 | 878 | +11 | - | - | - | 0.04 | 5 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 870 | 873 | 874 | +10 | - | - | - | 0.06 | 12 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 867 | 870 | 872 | 882 | +10 | - | - | - | 0.11 | 8 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 869 | 872 | 874 | +9 | - | - | - | 0.15 | 9 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 870 | 872 | 881 | +10 | - | - | - | 0.19 | 5 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 867 | 872 | 878 | 904 | +12 | - | - | - | 0.28 | 15 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 869 | 873 | 879 | +9 | - | - | - | 0.36 | 7 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 872 | 896 | 907 | 936 | +36 | - | - | - | 0.42 | 9 | pass |
| 440 | 880 (0) | 1760 / 1760 | 0 / 0 / 0 | 890 | 944 | 981 | 1039 | +84 | - | - | - | 0.45 | 10 | pass |
| 460 | 920 (0) | 1840 / 1840 | 0 / 0 / 0 | 909 | 1050 | 1092 | 1120 | +190 | - | - | - | 0.46 | 6 | pass |
| 470 | 940 (0) | 1880 / 1880 | 0 / 0 / 0 | 930 | 1074 | 1131 | 1180 | +214 | - | - | - | 0.46 | 10 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 936 | 1313 | 1416 | 1473 | +453 | - | - | - | 0.49 | 10 | FAIL |

Why steps did not pass:

- 480 concurrent: p95 V2V 1312 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
