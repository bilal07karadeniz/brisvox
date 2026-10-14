# Capacity: brisvox-q-pss-crosscheck-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 7596 --floor-ms 950 --label brisvox-q-pss-crosscheck-models (4 CPUs, 4g, Linux) --note run 1 of 1; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.62 (< 1.0); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,50,100,200,250,300,350,400,500 --fixtures-dir /home/user/brisvox/fixtures --resolution 25 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-brisvox-q-pss-crosscheck-models-linux-2026-10-14-run1.md --out-json /home/user/brisvox/bench/results/capacity-brisvox-q-pss-crosscheck-models-linux-2026-10-14-run1.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 1; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.62 (< 1.0); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 50, 100, 200, 250, 300, 350, 400, 500]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 7596 and its descendants every sample period

**Result: max 350 concurrent calls within the SLO**; the next step up did not pass (first failure at 375).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 805 | 817 | 817 | 817 | -133 | 0.02 | 118 / 118 | 114 / 115 | 0.01 | 2 | pass |
| 50 | 100 (0) | 200 / 200 | 0 / 0 / 0 | 807 | 823 | 826 | 826 | -127 | 0.52 | 118 / 158 | 115 / 155 | 0.05 | 2 | pass |
| 100 | 200 (0) | 400 / 400 | 0 / 0 / 0 | 807 | 824 | 826 | 828 | -126 | 0.97 | 158 / 195 | 155 / 192 | 0.08 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 808 | 825 | 827 | 829 | -125 | 1.37 | 195 / 293 | 192 / 290 | 0.14 | 13 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 806 | 824 | 826 | 830 | -126 | 1.71 | 293 / 331 | 290 / 328 | 0.22 | 2 | pass |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 814 | 902 | 955 | 970 | -48 | 2.34 | 331 / 368 | 328 / 365 | 0.25 | 2 | pass |
| 350 | 700 (0) | 1400 / 1400 | 0 / 0 / 0 | 825 | 1043 | 1073 | 1087 | +93 | 2.52 | 368 / 412 | 365 / 409 | 0.31 | 8 | pass |
| 375 | 750 (0) | 1500 / 1500 | 0 / 0 / 0 | 938 | 1406 | 1511 | 3586 | +456 | 2.65 | 431 / 432 | 427 / 429 | 0.33 | 2 | FAIL |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1124 | 3566 | 3593 | 3621 | +2616 | 2.63 | 412 / 431 | 409 / 427 | 0.30 | 15 | FAIL |

Why steps did not pass:

- 375 concurrent: p95 V2V 1405 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 400 concurrent: p95 V2V 3566 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
