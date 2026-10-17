# Capacity: brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp --note <see server> --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-4c-max-linux-2026-10-16-run2.md --out-json capacity-brisvox-cloud-4c-max-linux-2026-10-16-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 480 concurrent calls within the SLO**; the next step up did not pass (first failure at 490).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 868 | 870 | 870 | 870 | +10 | - | - | - | 0.01 | 4 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 867 | 870 | 871 | 871 | +10 | - | - | - | 0.02 | 6 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 867 | 870 | 871 | 871 | +10 | - | - | - | 0.03 | 4 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 868 | 871 | 871 | +8 | - | - | - | 0.04 | 8 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 869 | 871 | 875 | +9 | - | - | - | 0.07 | 7 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 871 | 872 | +8 | - | - | - | 0.12 | 8 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 923 | 976 | +8 | - | - | - | 0.17 | 6 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 869 | 871 | 877 | +9 | - | - | - | 0.21 | 13 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 867 | 874 | 878 | 885 | +14 | - | - | - | 0.25 | 8 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 872 | 896 | 928 | 936 | +36 | - | - | - | 0.30 | 6 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 889 | 1017 | 1053 | 1073 | +157 | - | - | - | 0.36 | 11 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 1073 | 1820 | 3626 | 3660 | +960 | - | - | - | 0.38 | 7 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1516 | 3645 | 3695 | 3760 | +2785 | - | - | - | 0.43 | 10 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 2124 | 3730 | 3781 | 3836 | +2870 | - | - | - | 0.50 | 7 | pass |
| 490 | 980 (0) | 1826 / 1960 | 134 / 0 / 0 | 2230 | 3742 | 3916 | 3993 | +2882 | - | - | - | 0.51 | 17 | FAIL |
| 500 | 1000 (0) | 1950 / 2000 | 50 / 0 / 0 | 2288 | 3795 | 3926 | 3998 | +2935 | - | - | - | 0.51 | 31 | FAIL |
| 520 | 1040 (0) | 1532 / 2080 | 548 / 0 / 0 | 3280 | 3730 | 3943 | 4000 | +2870 | - | - | - | 0.52 | 13 | FAIL |
| 560 | 1120 (0) | 1444 / 2240 | 787 / 9 / 0 | 3641 | 3927 | 3999 | 4120 | +3067 | - | - | - | 0.52 | 6 | FAIL |
| 640 | 1280 (0) | 1446 / 2560 | 860 / 254 / 0 | 3622 | 3834 | 3908 | 4047 | +2974 | - | - | - | 0.60 | 9 | FAIL |

Why steps did not pass:

- 490 concurrent: 134 of 1960 turns lost or not clean (134 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 500 concurrent: 50 of 2000 turns lost or not clean (50 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 520 concurrent: 548 of 2080 turns lost or not clean (548 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 560 concurrent: 796 of 2240 turns lost or not clean (787 talked over, 9 no response, 0 bot speaking; allowed 0.0 %)
- 640 concurrent: 1114 of 2560 turns lost or not clean (860 talked over, 254 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
