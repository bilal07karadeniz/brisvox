# Capacity: brisvox-turnexec-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 2451 --floor-ms 950 --label brisvox-turnexec-models (4 CPUs, 4g, Linux) --note run 2 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,25,50,100,200,300,400,500,600 --fixtures-dir /mnt<repo>/fixtures --resolution 25 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-brisvox-turnexec-models-linux-2026-10-13-run2.md --out-json /mnt<repo>/bench/results/capacity-brisvox-turnexec-models-linux-2026-10-13-run2.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 25, 50, 100, 200, 300, 400, 500, 600]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 2451 and its descendants every sample period

**Result: max 175 concurrent calls within the SLO**; the next step up did not pass (first failure at 200).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 806 | 815 | 815 | 815 | -135 | 0.03 | 114 / 115 | 0.01 | 1 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 806 | 824 | 826 | 826 | -126 | 0.33 | 115 / 133 | 0.05 | 2 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 807 | 823 | 825 | 826 | -127 | 0.74 | 133 / 153 | 0.10 | 3 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 807 | 825 | 826 | 829 | -125 | 1.16 | 153 / 208 | 0.13 | 3 | pass |
| 150 | 300 (0) | 600 / 600 | 0 / 0 / 0 | 806 | 824 | 826 | 829 | -126 | 1.44 | 291 / 291 | 0.20 | 2 | pass |
| 175 | 350 (0) | 700 / 700 | 0 / 0 / 0 | 806 | 824 | 826 | 828 | -126 | 1.57 | 291 / 291 | 0.24 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 821 | 1511 | 1568 | 1589 | +561 | 1.97 | 208 / 291 | 0.24 | 3 | FAIL |

Why steps did not pass:

- 200 concurrent: p95 V2V 1510 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
