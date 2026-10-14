# Capacity: pipecat (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 15863 --floor-ms 950 --label pipecat (4 CPUs, 4g, Linux) --note run 2 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,20,40,80,120,160,240,320,480 --fixtures-dir /home/user/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-pipecat-linux-2026-10-13-run2.md --out-json /home/user/brisvox/bench/results/capacity-pipecat-linux-2026-10-13-run2.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 3; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 20, 40, 80, 120, 160, 240, 320, 480]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 15863 and its descendants every sample period

**Result: max 70 concurrent calls within the SLO**; the next step up did not pass (first failure at 80).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1008 | 1028 | 1028 | 1028 | +78 | 0.08 | 550 / 560 | 0.01 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1022 | 1040 | 1048 | 1048 | +90 | 1.01 | 559 / 688 | 0.03 | 3 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 1028 | 1071 | 1087 | 1088 | +121 | 1.90 | 687 / 935 | 0.06 | 3 | pass |
| 60 | 120 (0) | 240 / 240 | 0 / 0 / 0 | 1028 | 1066 | 1103 | 1106 | +116 | 2.65 | 1314 / 1413 | 0.10 | 3 | pass |
| 70 | 140 (0) | 280 / 280 | 0 / 0 / 0 | 1032 | 1089 | 1169 | 1269 | +139 | 2.86 | 1413 / 1530 | 0.13 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 1042 | 1256 | 1691 | 3007 | +306 | 2.94 | 931 / 1328 | 0.11 | 3 | FAIL |

Why steps did not pass:

- 80 concurrent: p95 V2V 1256 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
