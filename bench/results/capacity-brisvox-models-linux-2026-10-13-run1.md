# Capacity: brisvox-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 5573 --floor-ms 950 --label brisvox-models (4 CPUs, 4g, Linux) --note run 1 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,25,50,100,150,200,300,400,600,800 --fixtures-dir /mnt<repo>/fixtures --resolution 25 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-brisvox-models-linux-2026-10-13-run1.md --out-json /mnt<repo>/bench/results/capacity-brisvox-models-linux-2026-10-13-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 25, 50, 100, 150, 200, 300, 400, 600, 800]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 5573 and its descendants every sample period

**Result: max 250 concurrent calls within the SLO**; the next step up did not pass (first failure at 275).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 808 | 816 | 816 | 816 | -134 | 0.03 | 113 / 115 | 0.01 | 3 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 807 | 825 | 826 | 826 | -125 | 0.29 | 115 / 135 | 0.06 | 4 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 807 | 824 | 825 | 825 | -126 | 0.51 | 135 / 153 | 0.08 | 3 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 807 | 824 | 826 | 828 | -126 | 0.89 | 153 / 219 | 0.11 | 3 | pass |
| 150 | 300 (0) | 600 / 600 | 0 / 0 / 0 | 806 | 823 | 826 | 827 | -127 | 1.32 | 219 / 252 | 0.18 | 13 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 811 | 831 | 837 | 841 | -119 | 1.49 | 252 / 294 | 0.17 | 3 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 820 | 915 | 933 | 949 | -35 | 2.03 | 369 / 369 | 0.27 | 3 | pass |
| 275 | 550 (0) | 1100 / 1100 | 0 / 0 / 0 | 1313 | 2496 | 2554 | 2571 | +1546 | 2.42 | 369 / 369 | 0.33 | 4 | FAIL |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 1250 | 2891 | 3170 | 3243 | +1941 | 2.35 | 294 / 369 | 0.30 | 4 | FAIL |

Why steps did not pass:

- 275 concurrent: p95 V2V 2495 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 300 concurrent: p95 V2V 2890 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
