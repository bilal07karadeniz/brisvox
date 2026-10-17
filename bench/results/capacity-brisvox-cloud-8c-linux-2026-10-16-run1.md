# Capacity: brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P)

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 250 --label brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P) --note <see server> --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-8c-linux-2026-10-16-run1.md --out-json capacity-brisvox-cloud-8c-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 480 concurrent calls within the SLO**; the next step up did not pass (first failure at 490).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 869 | 871 | 871 | 871 | +11 | - | - | - | 0.00 | 16 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 867 | 871 | 872 | 872 | +11 | - | - | - | 0.01 | 12 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 868 | 872 | 872 | 872 | +12 | - | - | - | 0.02 | 16 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 871 | 876 | 876 | +11 | - | - | - | 0.03 | 10 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 870 | 873 | 875 | +10 | - | - | - | 0.06 | 13 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 870 | 872 | 874 | +10 | - | - | - | 0.10 | 12 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 870 | 871 | 873 | +10 | - | - | - | 0.14 | 14 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 870 | 872 | 876 | +10 | - | - | - | 0.18 | 10 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 870 | 874 | 882 | +10 | - | - | - | 0.25 | 9 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 871 | 873 | 885 | +11 | - | - | - | 0.33 | 23 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 867 | 872 | 876 | 892 | +12 | - | - | - | 0.40 | 10 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 907 | 961 | 990 | 1043 | +101 | - | - | - | 0.46 | 20 | pass |
| 490 | 980 (0) | 1960 / 1960 | 0 / 0 / 0 | 980 | 1250 | 1314 | 1342 | +390 | - | - | - | 0.48 | 10 | FAIL |
| 500 | 1000 (0) | 2000 / 2000 | 0 / 0 / 0 | 997 | 1364 | 1418 | 1487 | +504 | - | - | - | 0.49 | 15 | FAIL |
| 520 | 1040 (0) | 2080 / 2080 | 0 / 0 / 0 | 1008 | 1435 | 1524 | 1584 | +575 | - | - | - | 0.49 | 11 | FAIL |
| 560 | 1120 (0) | 2240 / 2240 | 0 / 0 / 0 | 1099 | 1749 | 1945 | 3669 | +889 | - | - | - | 0.50 | 19 | FAIL |

Why steps did not pass:

- 490 concurrent: p95 V2V 1250 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 500 concurrent: p95 V2V 1364 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 520 concurrent: p95 V2V 1435 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 560 concurrent: p95 V2V 1748 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
