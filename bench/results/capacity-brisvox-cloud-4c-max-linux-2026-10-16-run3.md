# Capacity: brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp --note <see server> --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-4c-max-linux-2026-10-16-run3.md --out-json capacity-brisvox-cloud-4c-max-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 400 concurrent calls within the SLO**; the next step up did not pass (first failure at 410).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 873 | 874 | 874 | 874 | +14 | - | - | - | 0.00 | 4 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 872 | 875 | 876 | 876 | +15 | - | - | - | 0.01 | 4 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 872 | 874 | 877 | 877 | +14 | - | - | - | 0.02 | 4 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 872 | 875 | 876 | 876 | +15 | - | - | - | 0.03 | 6 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 871 | 874 | 876 | 876 | +14 | - | - | - | 0.05 | 5 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 871 | 874 | 883 | 911 | +14 | - | - | - | 0.09 | 7 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 871 | 874 | 880 | 887 | +14 | - | - | - | 0.12 | 7 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 870 | 873 | 876 | 885 | +13 | - | - | - | 0.18 | 7 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 879 | 901 | 909 | 924 | +41 | - | - | - | 0.20 | 13 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 932 | 1100 | 1139 | 1183 | +240 | - | - | - | 0.24 | 8 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 1284 | 1995 | 3661 | 3711 | +1135 | - | - | - | 0.24 | 10 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 1250 | 3637 | 3692 | 3736 | +2777 | - | - | - | 0.29 | 6 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 2275 | 3742 | 3783 | 3845 | +2882 | - | - | - | 0.35 | 23 | pass |
| 410 | 820 (0) | 1639 / 1640 | 1 / 0 / 0 | 2274 | 3760 | 3856 | 3976 | +2900 | - | - | - | 0.36 | 6 | FAIL |
| 420 | 840 (0) | 1675 / 1680 | 5 / 0 / 0 | 2299 | 3767 | 3883 | 3986 | +2907 | - | - | - | 0.37 | 13 | FAIL |
| 440 | 880 (0) | 1695 / 1760 | 65 / 0 / 0 | 2384 | 3818 | 3931 | 3997 | +2958 | - | - | - | 0.38 | 7 | FAIL |
| 480 | 960 (0) | 1092 / 1920 | 762 / 66 / 0 | 3656 | 3943 | 4041 | 4128 | +3083 | - | - | - | 0.36 | 7 | FAIL |

Why steps did not pass:

- 410 concurrent: 1 of 1640 turns lost or not clean (1 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 420 concurrent: 5 of 1680 turns lost or not clean (5 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 440 concurrent: 65 of 1760 turns lost or not clean (65 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 480 concurrent: 828 of 1920 turns lost or not clean (762 talked over, 66 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
