# Capacity: brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P)

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 250 --label brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P) --note <see server> --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-4c-linux-2026-10-16-run2.md --out-json capacity-brisvox-cloud-4c-linux-2026-10-16-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 250 concurrent calls within the SLO**; the next step up did not pass (first failure at 260).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 869 | 871 | 871 | 871 | +11 | - | - | - | 0.01 | 5 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 867 | 870 | 871 | 871 | +10 | - | - | - | 0.02 | 4 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 867 | 870 | 871 | 871 | +10 | - | - | - | 0.03 | 5 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 870 | 876 | 876 | +10 | - | - | - | 0.04 | 5 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 871 | 874 | 875 | +11 | - | - | - | 0.07 | 7 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 867 | 870 | 872 | 883 | +10 | - | - | - | 0.13 | 7 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 867 | 870 | 874 | 878 | +10 | - | - | - | 0.18 | 5 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 870 | 873 | 890 | +10 | - | - | - | 0.22 | 6 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 867 | 872 | 875 | 877 | +12 | - | - | - | 0.26 | 7 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 883 | 937 | 973 | 982 | +77 | - | - | - | 0.31 | 16 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 907 | 972 | 982 | 1029 | +112 | - | - | - | 0.31 | 13 | pass |
| 260 | 520 (0) | 1040 / 1040 | 0 / 0 / 0 | 930 | 1121 | 1168 | 1226 | +261 | - | - | - | 0.33 | 7 | FAIL |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 986 | 1445 | 1488 | 1518 | +585 | - | - | - | 0.34 | 11 | FAIL |

Why steps did not pass:

- 260 concurrent: p95 V2V 1121 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 280 concurrent: p95 V2V 1444 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
