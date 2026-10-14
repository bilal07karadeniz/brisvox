# Capacity: brisvox-q-turnexec-r2-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 10018 --floor-ms 950 --label brisvox-q-turnexec-r2-models (4 CPUs, 4g, Linux) --note run 1 of 1; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,50,100,200,250,300,350,400,500 --fixtures-dir /mnt<repo>/fixtures --resolution 25 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-brisvox-q-turnexec-r2-models-linux-2026-10-13-run1.md --out-json /mnt<repo>/bench/results/capacity-brisvox-q-turnexec-r2-models-linux-2026-10-13-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 1; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 50, 100, 200, 250, 300, 350, 400, 500]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 10018 and its descendants every sample period

**Result: max 325 concurrent calls within the SLO**; the next step up did not pass (first failure at 350).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 805 | 816 | 816 | 816 | -134 | 0.03 | 114 / 115 | 0.01 | 2 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 806 | 824 | 827 | 827 | -126 | 0.58 | 115 / 151 | 0.06 | 3 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 807 | 824 | 825 | 829 | -126 | 1.04 | 151 / 221 | 0.09 | 3 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 808 | 826 | 828 | 835 | -124 | 1.52 | 221 / 291 | 0.15 | 3 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 808 | 825 | 829 | 834 | -125 | 1.95 | 291 / 327 | 0.24 | 2 | pass |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 814 | 935 | 947 | 963 | -15 | 2.44 | 327 / 369 | 0.27 | 3 | pass |
| 325 | 650 (0) | 1300 / 1300 | 0 / 0 / 0 | 893 | 1186 | 1217 | 1242 | +236 | 2.60 | 399 / 402 | 0.31 | 3 | pass |
| 350 | 700 (0) | 1400 / 1400 | 0 / 0 / 0 | 993 | 1376 | 1427 | 1463 | +426 | 2.72 | 369 / 400 | 0.32 | 3 | FAIL |

Why steps did not pass:

- 350 concurrent: p95 V2V 1376 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
