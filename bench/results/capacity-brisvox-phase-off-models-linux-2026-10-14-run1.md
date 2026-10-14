# Capacity: brisvox-phase-off-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 1679 --floor-ms 950 --label brisvox-phase-off-models (4 CPUs, 4g, Linux) --note run 1 of 1; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.56 (< 1.0); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,50,100,200,250,300,350,400,500 --fixtures-dir /home/user/brisvox/fixtures --resolution 25 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-brisvox-phase-off-models-linux-2026-10-14-run1.md --out-json /home/user/brisvox/bench/results/capacity-brisvox-phase-off-models-linux-2026-10-14-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 1; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.56 (< 1.0); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 50, 100, 200, 250, 300, 350, 400, 500]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 1679 and its descendants every sample period

**Result: max 350 concurrent calls within the SLO**; the next step up did not pass (first failure at 375).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 805 | 808 | 808 | 808 | -142 | 0.02 | 117 / 118 | 113 / 115 | 0.01 | 2 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 805 | 806 | 807 | 808 | -144 | 0.51 | 118 / 156 | 115 / 153 | 0.05 | 2 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 806 | 807 | 808 | 810 | -143 | 0.94 | 156 / 194 | 153 / 191 | 0.07 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 806 | 809 | 811 | 816 | -141 | 1.37 | 194 / 293 | 191 / 290 | 0.12 | 16 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 805 | 807 | 808 | 813 | -143 | 1.71 | 293 / 333 | 290 / 330 | 0.22 | 2 | pass |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 808 | 818 | 825 | 840 | -132 | 2.26 | 333 / 372 | 330 / 368 | 0.24 | 2 | pass |
| 350 | 700 (0) | 1400 / 1400 | 0 / 0 / 0 | 817 | 925 | 953 | 973 | -25 | 2.50 | 372 / 412 | 368 / 409 | 0.29 | 2 | pass |
| 375 | 750 (0) | 1500 / 1500 | 0 / 0 / 0 | 880 | 1317 | 1376 | 1395 | +367 | 2.63 | 431 / 434 | 428 / 430 | 0.32 | 2 | FAIL |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1045 | 1515 | 3574 | 3584 | +565 | 2.59 | 412 / 441 | 409 / 438 | 0.29 | 13 | FAIL |

Why steps did not pass:

- 375 concurrent: p95 V2V 1316 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 400 concurrent: p95 V2V 1515 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
