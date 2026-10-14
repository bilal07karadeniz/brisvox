# Capacity: brisvox-turnexec-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 2223 --floor-ms 950 --label brisvox-turnexec-models (4 CPUs, 4g, Linux) --note run 1 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,25,50,100,200,300,400,500,600 --fixtures-dir /mnt<repo>/fixtures --resolution 25 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-brisvox-turnexec-models-linux-2026-10-13-run1.md --out-json /mnt<repo>/bench/results/capacity-brisvox-turnexec-models-linux-2026-10-13-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 25, 50, 100, 200, 300, 400, 500, 600]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 2223 and its descendants every sample period

**Result: max 300 concurrent calls within the SLO**; the next step up did not pass (first failure at 325).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 805 | 816 | 816 | 816 | -134 | 0.03 | 114 / 115 | 0.01 | 2 | pass |
| 25 | 50 (0) | 100 / 100 | 0 / 0 / 0 | 806 | 824 | 827 | 828 | -126 | 0.33 | 115 / 134 | 0.05 | 3 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 806 | 823 | 825 | 826 | -127 | 0.58 | 134 / 155 | 0.06 | 3 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 807 | 824 | 825 | 827 | -126 | 1.05 | 155 / 192 | 0.10 | 3 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 809 | 826 | 831 | 839 | -124 | 1.56 | 192 / 294 | 0.18 | 2 | pass |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 831 | 1079 | 1146 | 1169 | +129 | 2.52 | 294 / 369 | 0.29 | 3 | pass |
| 325 | 650 (0) | 1300 / 1300 | 0 / 0 / 0 | 995 | 1875 | 3588 | 3597 | +925 | 2.71 | 421 / 421 | 0.33 | 4 | FAIL |
| 350 | 700 (0) | 1400 / 1400 | 0 / 0 / 0 | 1121 | 3570 | 3593 | 3612 | +2620 | 3.05 | 419 / 421 | 0.41 | 3 | FAIL |
| 400 | 800 (0) | 1558 / 1600 | 42 / 0 / 0 | 2022 | 3589 | 3665 | 3926 | +2639 | 3.06 | 369 / 428 | 0.44 | 29 | FAIL |

Why steps did not pass:

- 325 concurrent: p95 V2V 1875 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 350 concurrent: p95 V2V 3570 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 400 concurrent: 42 of 1600 turns lost or not clean (42 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3588 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
