# Capacity: livekit-models (4 CPUs, 4g, Linux)

- command: `bench/livekit_baseline/capacity_livekit.sh (VARIANT=tuned STEPS=1,5,10,15,20,25,30,40,50,60,80 RESOLUTION=2 run 1)`
- loadgen.py (Python 3.13.16, livekit rtc SDK), linux x86_64, 8 logical cores available to the generator, 8 worker processes (one asyncio loop each; processes, not a CPU limit: the container's cpuset is the limit)
- live: a LiveKit server at the URL; the load generator is this container (8 worker processes)
- target: `ws://127.0.0.1:7880`
- server: run 1 of 3; LiveKit Agents 1.8.7 (livekit-server livekit/livekit-server:v1.13.9, livekit/livekit-server@sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154) variant tuned: turn detector unlikely_threshold 0.05 and worker load_threshold 1.0; Linux kernel 6.6.87.2-microsoft-standard-WSL2; worker container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --network host, load generator cores 4-11 (8 processes), SFU container on cores 12-15, no CPU limit; host load 8% (Windows total, 12 s mean) before the run; load average 0.28 0.93 0.99 (bench/livekit_baseline/capacity_livekit.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 15, 20, 25, 30, 40, 50, 60, 80]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from the cgroup of the agent container (pid 27380 is its init) every sample period: CPU is the container's `usage_usec` (exited job processes included); the RSS column is **PSS** summed over the container's processes (forked job processes share pages, which a plain sum of `VmRSS` would count once per process: see the second table)

**Result: max 22 concurrent calls within the SLO**; the next step up did not pass (first failure at 23).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1137 | 1159 | 1159 | 1159 | +209 | 0.14 | 541 / 624 | 0.12 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 1131 | 1147 | 1148 | 1148 | +197 | 0.52 | 596 / 1015 | 0.49 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 1136 | 1159 | 1173 | 1173 | +209 | 1.01 | 752 / 1540 | 0.98 | 17 | pass |
| 15 | 30 (0) | 60 / 60 | 0 / 0 / 0 | 1132 | 1156 | 1165 | 1165 | +206 | 1.41 | 646 / 2065 | 1.43 | 15 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1139 | 1181 | 1211 | 1211 | +231 | 1.82 | 483 / 2587 | 2.02 | 57 | pass |
| 22 | 44 (0) | 88 / 88 | 0 / 0 / 0 | 1140 | 1171 | 1190 | 1190 | +221 | 2.09 | 484 / 2799 | 2.33 | 29 | pass |
| 23 | 46 (6) | 80 / 92 | 0 / 0 / 0 | 1138 | 1165 | 1185 | 1185 | +215 | 1.99 | 484 / 2588 | 2.27 | 38 | FAIL |
| 25 | 50 (4) | 92 / 100 | 0 / 0 / 0 | 1140 | 1182 | 1203 | 1203 | +232 | 2.19 | 484 / 2904 | 2.52 | 53 | FAIL |

The LiveKit server (SFU) and memory by other measures, per step (not counted in the target columns):

| concurrent | SFU CPU (cores) | SFU RSS peak (MB) | target plain VmRSS-sum peak (MB) | target cgroup memory peak (MB) | target processes (peak) |
|---:|---:|---:|---:|---:|---:|
| 1 | 0.03 | 67 | 2741 | 629 | 10 |
| 5 | 0.11 | 96 | 5226 | 1034 | 17 |
| 10 | 0.20 | 146 | 8740 | 1578 | 27 |
| 15 | 0.29 | 184 | 12255 | 2123 | 37 |
| 20 | 0.38 | 234 | 15772 | 2664 | 47 |
| 22 | 0.44 | 264 | 17178 | 2885 | 51 |
| 23 | 0.42 | 265 | 15773 | 2666 | 47 |
| 25 | 0.46 | 263 | 17882 | 2994 | 53 |

Why steps did not pass:

- 23 concurrent: 6 of 46 calls failed; 12 of 92 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 23 concurrent, failed call: no answer to start: no agent ready after 10 s
- 25 concurrent: 4 of 50 calls failed; 8 of 100 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 25 concurrent, failed call: no answer to start: no agent ready after 10 s

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
