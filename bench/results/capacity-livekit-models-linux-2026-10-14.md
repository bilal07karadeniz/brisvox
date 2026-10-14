# Capacity summary: livekit-models, Linux, 2026-10-14

- livekit-models (4 CPUs, 4g, Linux)
- LiveKit Agents 1.8.7 (livekit-server livekit/livekit-server:v1.13.9, livekit/livekit-server@sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154) variant tuned: turn detector unlikely_threshold 0.05 and worker load_threshold 1.0; Linux kernel 6.6.87.2-microsoft-standard-WSL2; worker container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --network host, load generator cores 4-11 (8 processes), SFU container on cores 12-15, no CPU limit; host load 8% (Windows total, 12 s mean) before the run; load average 0.28 0.93 0.99 (bench/livekit_baseline/capacity_livekit.sh)
- 3 run(s), per-run files `capacity-livekit-models-linux-2026-10-14-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 23, range 22-25**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 22 | 23 | 1171 | 2.09 | 94.91 | 18012 | 2935 | 2.33 | 29 |
| 2 | 23 | 25 | 1171 | 2.08 | 90.34 | 18692 | 3023 | 2.36 | 62 |
| 3 | 25 | 27 | 1192 | 2.22 | 88.71 | 20232 | 3269 | 2.55 | 53 |

First failing step of each run:

- run 1: 23 calls (found): 6 of 46 calls failed; 12 of 92 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 25 calls (found): 9 of 50 calls failed; 18 of 100 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1222 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 27 calls (found): 11 of 54 calls failed; 22 of 108 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
