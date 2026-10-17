# Capacity: brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp --note <see server> --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-8c-max-linux-2026-10-16-run1.md --out-json capacity-brisvox-cloud-8c-max-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 740 concurrent calls within the SLO**; the next step up did not pass (first failure at 750).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 868 | 870 | 870 | 870 | +10 | - | - | - | 0.00 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 867 | 869 | 871 | 871 | +9 | - | - | - | 0.01 | 4 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 867 | 870 | 886 | 886 | +10 | - | - | - | 0.02 | 3 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 869 | 873 | 873 | +9 | - | - | - | 0.03 | 4 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 869 | 871 | 878 | +9 | - | - | - | 0.05 | 4 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 870 | 873 | 883 | +10 | - | - | - | 0.09 | 5 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 867 | 869 | 872 | 880 | +9 | - | - | - | 0.13 | 6 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 867 | 870 | 872 | 876 | +10 | - | - | - | 0.18 | 5 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 870 | 874 | 878 | +10 | - | - | - | 0.26 | 5 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 870 | 876 | 892 | +10 | - | - | - | 0.31 | 7 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 868 | 879 | 892 | 903 | +19 | - | - | - | 0.38 | 3 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 984 | 1418 | 1477 | 1522 | +558 | - | - | - | 0.46 | 8 | pass |
| 560 | 1120 (0) | 2240 / 2240 | 0 / 0 / 0 | 1313 | 2138 | 3651 | 3718 | +1278 | - | - | - | 0.49 | 6 | pass |
| 640 | 1280 (0) | 2560 / 2560 | 0 / 0 / 0 | 1645 | 3651 | 3696 | 3765 | +2791 | - | - | - | 0.57 | 4 | pass |
| 720 | 1440 (0) | 2880 / 2880 | 0 / 0 / 0 | 2402 | 3729 | 3789 | 3921 | +2869 | - | - | - | 0.65 | 5 | pass |
| 740 | 1480 (0) | 2960 / 2960 | 0 / 0 / 0 | 2406 | 3735 | 3792 | 3831 | +2875 | - | - | - | 0.66 | 5 | pass |
| 750 | 1500 (0) | 2987 / 3000 | 13 / 0 / 0 | 2421 | 3729 | 3789 | 3831 | +2869 | - | - | - | 0.65 | 4 | FAIL |
| 760 | 1520 (0) | 3035 / 3040 | 5 / 0 / 0 | 2513 | 3762 | 3878 | 3986 | +2902 | - | - | - | 0.67 | 5 | FAIL |
| 800 | 1600 (0) | 2710 / 3200 | 490 / 0 / 0 | 2667 | 3769 | 3930 | 3997 | +2909 | - | - | - | 0.71 | 5 | FAIL |

Why steps did not pass:

- 750 concurrent: 13 of 3000 turns lost or not clean (13 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 760 concurrent: 5 of 3040 turns lost or not clean (5 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 800 concurrent: 490 of 3200 turns lost or not clean (490 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
