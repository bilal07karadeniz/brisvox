# Capacity summary: livekit-default-models, Linux, 2026-10-14

- livekit-default-models (4 CPUs, 4g, Linux)
- LiveKit Agents 1.8.7 (livekit-server livekit/livekit-server:v1.13.9, livekit/livekit-server@sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154) variant default: framework defaults (worker and session); Linux kernel 6.6.87.2-microsoft-standard-WSL2; worker container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --network host, load generator cores 4-11 (8 processes), SFU container on cores 12-15, no CPU limit; host load 4% (Windows total, 12 s mean) before the run; load average 0.76 1.95 1.24 (bench/livekit_baseline/capacity_livekit.sh)
- 3 run(s), per-run files `capacity-livekit-default-models-linux-2026-10-14-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 0, range 0-0**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | None | 1 | - | - | - | - | - | - | - |
| 2 | None | 1 | - | - | - | - | - | - | - |
| 3 | None | 1 | - | - | - | - | - | - | - |

First failing step of each run:

- run 1: 1 calls (first_step_failed): p95 V2V 2893 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 1 calls (first_step_failed): p95 V2V 2846 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 1 calls (first_step_failed): p95 V2V 2805 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
