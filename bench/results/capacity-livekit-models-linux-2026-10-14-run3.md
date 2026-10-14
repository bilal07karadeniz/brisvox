# Capacity: livekit-models (4 CPUs, 4g, Linux)

- command: `bench/livekit_baseline/capacity_livekit.sh (VARIANT=tuned STEPS=1,5,10,15,20,25,30,40,50,60,80 RESOLUTION=2 run 3)`
- loadgen.py (Python 3.13.16, livekit rtc SDK), linux x86_64, 8 logical cores available to the generator, 8 worker processes (one asyncio loop each; processes, not a CPU limit: the container's cpuset is the limit)
- live: a LiveKit server at the URL; the load generator is this container (8 worker processes)
- target: `ws://127.0.0.1:7880`
- server: run 3 of 3; LiveKit Agents 1.8.7 (livekit-server livekit/livekit-server:v1.13.9, livekit/livekit-server@sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154) variant tuned: turn detector unlikely_threshold 0.05 and worker load_threshold 1.0; Linux kernel 6.6.87.2-microsoft-standard-WSL2; worker container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --network host, load generator cores 4-11 (8 processes), SFU container on cores 12-15, no CPU limit; host load 3% (Windows total, 12 s mean) before the run; load average 3.87 3.27 2.31 (bench/livekit_baseline/capacity_livekit.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 15, 20, 25, 30, 40, 50, 60, 80]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from the cgroup of the agent container (pid 56853 is its init) every sample period: CPU is the container's `usage_usec` (exited job processes included); the RSS column is **PSS** summed over the container's processes (forked job processes share pages, which a plain sum of `VmRSS` would count once per process: see the second table)

**Result: max 25 concurrent calls within the SLO**; the next step up did not pass (first failure at 27).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1129 | 1134 | 1134 | 1134 | +184 | 0.13 | 541 / 596 | 0.12 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 1143 | 1163 | 1163 | 1163 | +213 | 0.50 | 595 / 1016 | 0.48 | 3 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 1132 | 1167 | 1182 | 1182 | +217 | 0.99 | 752 / 1540 | 0.98 | 6 | pass |
| 15 | 30 (0) | 60 / 60 | 0 / 0 / 0 | 1128 | 1147 | 1166 | 1166 | +197 | 1.40 | 645 / 2068 | 1.48 | 24 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1132 | 1167 | 1193 | 1193 | +217 | 1.84 | 538 / 2591 | 2.03 | 37 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 1138 | 1192 | 1210 | 1212 | +242 | 2.22 | 484 / 3117 | 2.55 | 53 | pass |
| 27 | 54 (11) | 86 / 108 | 0 / 0 / 0 | 1136 | 1187 | 1194 | 1194 | +237 | 2.04 | 485 / 2752 | 2.43 | 49 | FAIL |
| 30 | 60 (5) | 110 / 120 | 0 / 0 / 0 | 1142 | 1209 | 1294 | 1306 | +259 | 2.47 | 484 / 3376 | 2.89 | 51 | FAIL |

The LiveKit server (SFU) and memory by other measures, per step (not counted in the target columns):

| concurrent | SFU CPU (cores) | SFU RSS peak (MB) | target plain VmRSS-sum peak (MB) | target cgroup memory peak (MB) | target processes (peak) |
|---:|---:|---:|---:|---:|---:|
| 1 | 0.03 | 67 | 2419 | 584 | 9 |
| 5 | 0.11 | 98 | 5230 | 1018 | 17 |
| 10 | 0.20 | 142 | 8744 | 1561 | 27 |
| 15 | 0.29 | 179 | 12262 | 2109 | 37 |
| 20 | 0.38 | 226 | 15775 | 2651 | 47 |
| 25 | 0.47 | 281 | 19295 | 3198 | 57 |
| 27 | 0.44 | 306 | 16834 | 2818 | 50 |
| 30 | 0.52 | 305 | 21048 | 3468 | 62 |

Why steps did not pass:

- 27 concurrent: 11 of 54 calls failed; 22 of 108 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 27 concurrent, failed call: no answer to start: no agent ready after 10 s
- 30 concurrent: 5 of 60 calls failed; 10 of 120 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1209 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 30 concurrent, failed call: no answer to start: no agent ready after 10 s

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
