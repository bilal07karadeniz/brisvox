# Capacity: livekit-models (4 CPUs, 4g, Linux)

- command: `bench/livekit_baseline/capacity_livekit.sh (VARIANT=tuned STEPS=1,5,10,15,20,25,30,40,50,60,80 RESOLUTION=2 run 2)`
- loadgen.py (Python 3.13.16, livekit rtc SDK), linux x86_64, 8 logical cores available to the generator, 8 worker processes (one asyncio loop each; processes, not a CPU limit: the container's cpuset is the limit)
- live: a LiveKit server at the URL; the load generator is this container (8 worker processes)
- target: `ws://127.0.0.1:7880`
- server: run 2 of 3; LiveKit Agents 1.8.7 (livekit-server livekit/livekit-server:v1.13.9, livekit/livekit-server@sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154) variant tuned: turn detector unlikely_threshold 0.05 and worker load_threshold 1.0; Linux kernel 6.6.87.2-microsoft-standard-WSL2; worker container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --network host, load generator cores 4-11 (8 processes), SFU container on cores 12-15, no CPU limit; host load 12% (Windows total, 12 s mean) before the run; load average 0.16 1.73 1.63 (bench/livekit_baseline/capacity_livekit.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 15, 20, 25, 30, 40, 50, 60, 80]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from the cgroup of the agent container (pid 42330 is its init) every sample period: CPU is the container's `usage_usec` (exited job processes included); the RSS column is **PSS** summed over the container's processes (forked job processes share pages, which a plain sum of `VmRSS` would count once per process: see the second table)

**Result: max 23 concurrent calls within the SLO**; the next step up did not pass (first failure at 25).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1137 | 1163 | 1163 | 1163 | +213 | 0.14 | 527 / 582 | 0.12 | 3 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 1136 | 1160 | 1169 | 1169 | +210 | 0.51 | 572 / 993 | 0.48 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 1135 | 1166 | 1169 | 1169 | +216 | 0.99 | 729 / 1517 | 0.96 | 6 | pass |
| 15 | 30 (0) | 60 / 60 | 0 / 0 / 0 | 1133 | 1155 | 1165 | 1165 | +205 | 1.42 | 623 / 2044 | 1.45 | 18 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1139 | 1185 | 1226 | 1226 | +235 | 1.78 | 460 / 2567 | 1.99 | 54 | pass |
| 22 | 44 (0) | 88 / 88 | 0 / 0 / 0 | 1132 | 1166 | 1179 | 1179 | +216 | 2.00 | 460 / 2778 | 2.21 | 36 | pass |
| 23 | 46 (0) | 92 / 92 | 0 / 0 / 0 | 1139 | 1171 | 1203 | 1203 | +221 | 2.08 | 461 / 2883 | 2.36 | 62 | pass |
| 25 | 50 (9) | 82 / 100 | 0 / 0 / 0 | 1143 | 1222 | 1244 | 1244 | +272 | 1.95 | 460 / 2621 | 2.26 | 105 | FAIL |

The LiveKit server (SFU) and memory by other measures, per step (not counted in the target columns):

| concurrent | SFU CPU (cores) | SFU RSS peak (MB) | target plain VmRSS-sum peak (MB) | target cgroup memory peak (MB) | target processes (peak) |
|---:|---:|---:|---:|---:|---:|
| 1 | 0.03 | 69 | 2392 | 591 | 9 |
| 5 | 0.11 | 97 | 5185 | 1023 | 17 |
| 10 | 0.20 | 145 | 8694 | 1566 | 27 |
| 15 | 0.29 | 181 | 12205 | 2113 | 37 |
| 20 | 0.38 | 226 | 15716 | 2657 | 47 |
| 22 | 0.42 | 263 | 17123 | 2875 | 51 |
| 23 | 0.44 | 265 | 17826 | 2985 | 53 |
| 25 | 0.41 | 251 | 16071 | 2713 | 48 |

Why steps did not pass:

- 25 concurrent: 9 of 50 calls failed; 18 of 100 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1222 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 25 concurrent, failed call: no answer to start: no agent ready after 10 s

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
