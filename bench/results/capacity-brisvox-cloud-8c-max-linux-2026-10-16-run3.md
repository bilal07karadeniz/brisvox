# Capacity: brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp --note <see server> --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-8c-max-linux-2026-10-16-run3.md --out-json capacity-brisvox-cloud-8c-max-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 800 concurrent calls within the SLO**; the next step up did not pass (first failure at 806).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 868 | 869 | 869 | 869 | +9 | - | - | - | 0.01 | 5 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 869 | 870 | 870 | +9 | - | - | - | 0.02 | 7 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 867 | 869 | 871 | 871 | +9 | - | - | - | 0.02 | 12 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 869 | 870 | 870 | +9 | - | - | - | 0.04 | 12 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 870 | 883 | 894 | +10 | - | - | - | 0.06 | 8 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 867 | 871 | 874 | 904 | +11 | - | - | - | 0.11 | 9 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 870 | 872 | 873 | +10 | - | - | - | 0.16 | 11 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 869 | 872 | 875 | +9 | - | - | - | 0.21 | 9 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 869 | 871 | 888 | +9 | - | - | - | 0.27 | 14 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 869 | 872 | 875 | +9 | - | - | - | 0.34 | 9 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 866 | 870 | 915 | 963 | +10 | - | - | - | 0.42 | 10 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 881 | 899 | 914 | 937 | +39 | - | - | - | 0.48 | 7 | pass |
| 560 | 1120 (0) | 2240 / 2240 | 0 / 0 / 0 | 1053 | 1572 | 1652 | 1693 | +712 | - | - | - | 0.53 | 13 | pass |
| 640 | 1280 (0) | 2560 / 2560 | 0 / 0 / 0 | 1286 | 3643 | 3674 | 3712 | +2783 | - | - | - | 0.61 | 9 | pass |
| 800 | 1600 (0) | 3200 / 3200 | 0 / 0 / 0 | 2242 | 3729 | 3777 | 3818 | +2869 | - | - | - | 0.72 | 10 | pass |
| 806 | 1612 (0) | 3206 / 3224 | 18 / 0 / 0 | 2345 | 3745 | 3882 | 4000 | +2885 | - | - | - | 0.70 | 8 | FAIL |
| 812 | 1624 (0) | 3192 / 3248 | 56 / 0 / 0 | 2408 | 3784 | 3962 | 3995 | +2924 | - | - | - | 0.75 | 9 | FAIL |
| 825 | 1650 (0) | 3199 / 3300 | 101 / 0 / 0 | 2305 | 3758 | 3877 | 3988 | +2898 | - | - | - | 0.73 | 7 | FAIL |
| 850 | 1700 (0) | 3353 / 3400 | 47 / 0 / 0 | 2594 | 3812 | 3920 | 3998 | +2952 | - | - | - | 0.79 | 12 | FAIL |
| 900 | 1800 (0) | 2583 / 3600 | 1017 / 0 / 0 | 3189 | 3893 | 3963 | 4060 | +3033 | - | - | - | 0.78 | 16 | FAIL |
| 1000 | 2000 (0) | 2127 / 4000 | 1407 / 466 / 0 | 3670 | 4257 | 5668 | 5721 | +3397 | - | - | - | 0.84 | 13 | FAIL |

Why steps did not pass:

- 806 concurrent: 18 of 3224 turns lost or not clean (18 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 812 concurrent: 56 of 3248 turns lost or not clean (56 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 825 concurrent: 101 of 3300 turns lost or not clean (101 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 850 concurrent: 47 of 3400 turns lost or not clean (47 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 900 concurrent: 1017 of 3600 turns lost or not clean (1017 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 1000 concurrent: 1873 of 4000 turns lost or not clean (1407 talked over, 466 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
