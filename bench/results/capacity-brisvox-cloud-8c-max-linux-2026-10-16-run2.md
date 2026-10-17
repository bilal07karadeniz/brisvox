# Capacity: brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp --note <see server> --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-8c-max-linux-2026-10-16-run2.md --out-json capacity-brisvox-cloud-8c-max-linux-2026-10-16-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 780 concurrent calls within the SLO**; the next step up did not pass (first failure at 790).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 868 | 869 | 869 | 869 | +9 | - | - | - | 0.01 | 4 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 867 | 870 | 870 | 870 | +10 | - | - | - | 0.01 | 4 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 867 | 871 | 878 | 878 | +11 | - | - | - | 0.03 | 14 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 869 | 870 | 870 | +9 | - | - | - | 0.04 | 6 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 869 | 871 | 872 | +9 | - | - | - | 0.06 | 6 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 869 | 871 | 873 | +9 | - | - | - | 0.12 | 8 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 869 | 872 | 877 | +9 | - | - | - | 0.16 | 7 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 871 | +8 | - | - | - | 0.21 | 8 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 868 | 870 | 890 | +8 | - | - | - | 0.30 | 7 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 870 | 873 | 881 | +10 | - | - | - | 0.37 | 13 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 867 | 871 | 874 | 903 | +11 | - | - | - | 0.44 | 10 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 889 | 999 | 1030 | 1058 | +139 | - | - | - | 0.53 | 8 | pass |
| 560 | 1120 (0) | 2240 / 2240 | 0 / 0 / 0 | 1047 | 1568 | 1625 | 1676 | +708 | - | - | - | 0.60 | 13 | pass |
| 640 | 1280 (0) | 2560 / 2560 | 0 / 0 / 0 | 1207 | 3627 | 3679 | 3749 | +2767 | - | - | - | 0.62 | 8 | pass |
| 720 | 1440 (0) | 2880 / 2880 | 0 / 0 / 0 | 1954 | 3678 | 3735 | 3783 | +2818 | - | - | - | 0.69 | 10 | pass |
| 760 | 1520 (0) | 3040 / 3040 | 0 / 0 / 0 | 2155 | 3712 | 3757 | 3778 | +2852 | - | - | - | 0.75 | 8 | pass |
| 780 | 1560 (0) | 3120 / 3120 | 0 / 0 / 0 | 2265 | 3738 | 3833 | 3957 | +2878 | - | - | - | 0.77 | 10 | pass |
| 790 | 1580 (0) | 3149 / 3160 | 11 / 0 / 0 | 2305 | 3740 | 3808 | 3985 | +2880 | - | - | - | 0.74 | 10 | FAIL |
| 800 | 1600 (0) | 3178 / 3200 | 22 / 0 / 0 | 2204 | 3754 | 3894 | 3994 | +2894 | - | - | - | 0.77 | 10 | FAIL |

Why steps did not pass:

- 790 concurrent: 11 of 3160 turns lost or not clean (11 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 800 concurrent: 22 of 3200 turns lost or not clean (22 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
