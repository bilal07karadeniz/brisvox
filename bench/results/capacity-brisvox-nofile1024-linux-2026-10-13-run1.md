# Capacity: brisvox-nofile1024 (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 27761 --floor-ms 950 --label brisvox-nofile1024 (4 CPUs, 4g, Linux) --note run 1 of 1; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1024:1024 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,800,1000,1100,1200 --fixtures-dir /home/user/brisvox/fixtures --resolution 0 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-brisvox-nofile1024-linux-2026-10-13-run1.md --out-json /home/user/brisvox/bench/results/capacity-brisvox-nofile1024-linux-2026-10-13-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 1; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1024:1024 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 800, 1000, 1100, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 27761 and its descendants every sample period

**Result: max 1000 concurrent calls within the SLO**; the next step up did not pass (first failure at 1100).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 965 | 998 | 998 | 998 | +48 | 0.01 | 14 / 18 | 0.01 | 4 | pass |
| 800 | 1600 (0) | 3200 / 3200 | 0 / 0 / 0 | 984 | 1001 | 1005 | 1010 | +51 | 1.25 | 18 / 603 | 0.53 | 23 | pass |
| 1000 | 2000 (0) | 4000 / 4000 | 0 / 0 / 0 | 984 | 1003 | 1006 | 1015 | +53 | 1.59 | 590 / 747 | 0.70 | 8 | pass |
| 1100 | 2200 (166) | 4068 / 4400 | 0 / 0 / 0 | 984 | 1003 | 1007 | 1016 | +53 | 1.74 | 735 / 770 | 0.78 | 4 | FAIL |

Why steps did not pass:

- 1100 concurrent: 166 of 2200 calls failed; 332 of 4400 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 1100 concurrent, failed call: call: connect timed out

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
