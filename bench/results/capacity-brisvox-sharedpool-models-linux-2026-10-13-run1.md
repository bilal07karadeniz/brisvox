# Capacity: brisvox-sharedpool-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 4334 --floor-ms 950 --label brisvox-sharedpool-models (4 CPUs, 4g, Linux) --note run 1 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,25,50,100,200,300,400,500,600 --fixtures-dir /mnt<repo>/fixtures --resolution 25 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-brisvox-sharedpool-models-linux-2026-10-13-run1.md --out-json /mnt<repo>/bench/results/capacity-brisvox-sharedpool-models-linux-2026-10-13-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 25, 50, 100, 200, 300, 400, 500, 600]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 4334 and its descendants every sample period

**Result: max 250 concurrent calls within the SLO**; the next step up did not pass (first failure at 275).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 806 | 818 | 818 | 818 | -132 | 0.04 | 113 / 115 | 0.01 | 6 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 807 | 823 | 825 | 827 | -127 | 0.40 | 115 / 135 | 0.05 | 3 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 806 | 824 | 825 | 826 | -126 | 0.57 | 135 / 152 | 0.06 | 3 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 807 | 824 | 826 | 828 | -126 | 1.04 | 152 / 215 | 0.09 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 811 | 1077 | 1166 | 1203 | +127 | 1.76 | 215 / 288 | 0.21 | 13 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 810 | 845 | 867 | 894 | -105 | 2.17 | 362 / 362 | 0.28 | 3 | pass |
| 275 | 550 (0) | 1093 / 1100 | 7 / 0 / 0 | 1199 | 3688 | 3806 | 3952 | +2738 | 2.75 | 362 / 364 | 0.34 | 4 | FAIL |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 1035 | 1477 | 1518 | 1560 | +527 | 2.59 | 288 / 362 | 0.28 | 3 | FAIL |

Why steps did not pass:

- 275 concurrent: 7 of 1100 turns lost or not clean (7 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3688 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 300 concurrent: p95 V2V 1477 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
