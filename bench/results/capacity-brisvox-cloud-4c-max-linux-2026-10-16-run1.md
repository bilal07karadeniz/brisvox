# Capacity: brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp --note <see server> --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-4c-max-linux-2026-10-16-run1.md --out-json capacity-brisvox-cloud-4c-max-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 430 concurrent calls within the SLO**; the next step up did not pass (first failure at 440).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 867 | 869 | 869 | 869 | +9 | - | - | - | 0.00 | 3 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 867 | 869 | 870 | 870 | +9 | - | - | - | 0.01 | 4 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 867 | 868 | 870 | 870 | +8 | - | - | - | 0.02 | 4 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 869 | 872 | 872 | +9 | - | - | - | 0.03 | 5 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 869 | 871 | 872 | +9 | - | - | - | 0.05 | 4 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 867 | 869 | 871 | 874 | +9 | - | - | - | 0.09 | 9 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 867 | 869 | 871 | 874 | +9 | - | - | - | 0.13 | 9 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 870 | 871 | 884 | +10 | - | - | - | 0.17 | 5 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 867 | 870 | 874 | 877 | +10 | - | - | - | 0.19 | 4 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 875 | 896 | 915 | 932 | +36 | - | - | - | 0.24 | 6 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 927 | 1317 | 1416 | 1461 | +457 | - | - | - | 0.26 | 5 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 1213 | 1855 | 3643 | 3684 | +995 | - | - | - | 0.30 | 7 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1835 | 3677 | 3730 | 3769 | +2817 | - | - | - | 0.36 | 4 | pass |
| 420 | 840 (0) | 1680 / 1680 | 0 / 0 / 0 | 1896 | 3688 | 3750 | 3774 | +2828 | - | - | - | 0.37 | 5 | pass |
| 430 | 860 (0) | 1720 / 1720 | 0 / 0 / 0 | 1997 | 3713 | 3771 | 3800 | +2853 | - | - | - | 0.40 | 5 | pass |
| 440 | 880 (0) | 1758 / 1760 | 2 / 0 / 0 | 2111 | 3712 | 3783 | 3963 | +2852 | - | - | - | 0.38 | 5 | FAIL |
| 480 | 960 (0) | 1888 / 1920 | 32 / 0 / 0 | 2282 | 3768 | 3865 | 3998 | +2908 | - | - | - | 0.41 | 5 | FAIL |

Why steps did not pass:

- 440 concurrent: 2 of 1760 turns lost or not clean (2 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 480 concurrent: 32 of 1920 turns lost or not clean (32 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
