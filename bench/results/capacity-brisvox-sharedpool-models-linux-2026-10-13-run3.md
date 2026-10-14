# Capacity: brisvox-sharedpool-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 4786 --floor-ms 950 --label brisvox-sharedpool-models (4 CPUs, 4g, Linux) --note run 3 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,25,50,100,200,300,400,500,600 --fixtures-dir /mnt<repo>/fixtures --resolution 25 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-brisvox-sharedpool-models-linux-2026-10-13-run3.md --out-json /mnt<repo>/bench/results/capacity-brisvox-sharedpool-models-linux-2026-10-13-run3.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 25, 50, 100, 200, 300, 400, 500, 600]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 4786 and its descendants every sample period

**Result: max 275 concurrent calls within the SLO**; the next step up did not pass (first failure at 300).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 806 | 815 | 815 | 815 | -135 | 0.03 | 114 / 115 | 0.01 | 6 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 807 | 823 | 825 | 825 | -127 | 0.29 | 115 / 134 | 0.04 | 2 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 806 | 824 | 825 | 826 | -126 | 0.55 | 134 / 152 | 0.06 | 8 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 807 | 824 | 826 | 827 | -126 | 1.02 | 152 / 188 | 0.08 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 808 | 825 | 828 | 836 | -125 | 1.47 | 188 / 291 | 0.15 | 3 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 808 | 826 | 839 | 847 | -124 | 1.93 | 359 / 359 | 0.24 | 3 | pass |
| 275 | 550 (0) | 1100 / 1100 | 0 / 0 / 0 | 818 | 861 | 890 | 914 | -89 | 2.32 | 359 / 359 | 0.26 | 4 | pass |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 832 | 1261 | 1317 | 1361 | +311 | 2.47 | 291 / 359 | 0.26 | 4 | FAIL |

Why steps did not pass:

- 300 concurrent: p95 V2V 1261 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
