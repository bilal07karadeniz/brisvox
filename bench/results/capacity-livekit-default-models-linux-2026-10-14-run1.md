# Capacity: livekit-default-models (4 CPUs, 4g, Linux)

- command: `bench/livekit_baseline/capacity_livekit.sh (VARIANT=default STEPS=1,5,10,15,20,25,30,40,50,60,80 RESOLUTION=2 run 1)`
- loadgen.py (Python 3.13.16, livekit rtc SDK), linux x86_64, 8 logical cores available to the generator, 8 worker processes (one asyncio loop each; processes, not a CPU limit: the container's cpuset is the limit)
- live: a LiveKit server at the URL; the load generator is this container (8 worker processes)
- target: `ws://127.0.0.1:7880`
- server: run 1 of 3; LiveKit Agents 1.8.7 (livekit-server livekit/livekit-server:v1.13.9, livekit/livekit-server@sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154) variant default: framework defaults (worker and session); Linux kernel 6.6.87.2-microsoft-standard-WSL2; worker container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --network host, load generator cores 4-11 (8 processes), SFU container on cores 12-15, no CPU limit; host load 4% (Windows total, 12 s mean) before the run; load average 0.76 1.95 1.24 (bench/livekit_baseline/capacity_livekit.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 15, 20, 25, 30, 40, 50, 60, 80]; 1 warm-up call(s) first, not counted (the warm-up failed: p95 V2V 2804 ms is above the limit 1200 ms (floor 950 ms + 250 ms))
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from the cgroup of the agent container (pid 24072 is its init) every sample period: CPU is the container's `usage_usec` (exited job processes included); the RSS column is **PSS** summed over the container's processes (forked job processes share pages, which a plain sum of `VmRSS` would count once per process: see the second table)

**Result: no passing step**; even the smallest step did not pass (first failure at 1).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1153 | 2893 | 2893 | 2893 | +1943 | 0.13 | 540 / 595 | 0.12 | 2 | FAIL |

The LiveKit server (SFU) and memory by other measures, per step (not counted in the target columns):

| concurrent | SFU CPU (cores) | SFU RSS peak (MB) | target plain VmRSS-sum peak (MB) | target cgroup memory peak (MB) | target processes (peak) |
|---:|---:|---:|---:|---:|---:|
| 1 | 0.03 | 69 | 2420 | 550 | 9 |

Why steps did not pass:

- 1 concurrent: p95 V2V 2893 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
