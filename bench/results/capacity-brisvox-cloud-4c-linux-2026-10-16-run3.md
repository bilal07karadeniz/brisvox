# Capacity: brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P)

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 250 --label brisvox 4 vCPU / 4 GB, cloud (AMD EPYC 9655P) --note <see server> --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-brisvox-cloud-4c-linux-2026-10-16-run3.md --out-json capacity-brisvox-cloud-4c-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 220 concurrent calls within the SLO**; the next step up did not pass (first failure at 230).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 868 | 870 | 870 | 870 | +10 | - | - | - | 0.01 | 4 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 868 | 870 | 870 | 870 | +10 | - | - | - | 0.02 | 12 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 868 | 873 | 874 | 874 | +13 | - | - | - | 0.03 | 7 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 872 | 876 | 876 | +12 | - | - | - | 0.04 | 6 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 872 | 875 | 881 | +12 | - | - | - | 0.07 | 8 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 867 | 872 | 873 | 876 | +12 | - | - | - | 0.11 | 7 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 867 | 872 | 876 | 893 | +12 | - | - | - | 0.16 | 18 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 867 | 872 | 876 | 899 | +12 | - | - | - | 0.20 | 11 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 880 | 883 | 891 | +20 | - | - | - | 0.24 | 11 | pass |
| 220 | 440 (0) | 880 / 880 | 0 / 0 / 0 | 879 | 898 | 914 | 926 | +38 | - | - | - | 0.26 | 9 | pass |
| 230 | 460 (0) | 920 / 920 | 0 / 0 / 0 | 887 | 1175 | 1202 | 1219 | +315 | - | - | - | 0.26 | 7 | FAIL |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 902 | 1300 | 1380 | 1418 | +440 | - | - | - | 0.29 | 16 | FAIL |

Why steps did not pass:

- 230 concurrent: p95 V2V 1175 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 240 concurrent: p95 V2V 1300 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
