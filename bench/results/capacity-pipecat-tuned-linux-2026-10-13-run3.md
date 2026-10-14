# Capacity: pipecat-tuned (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 24767 --floor-ms 950 --label pipecat-tuned (4 CPUs, 4g, Linux) --note run 3 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,20,40,80,120,160,240,320,480 --fixtures-dir /home/user/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-pipecat-tuned-linux-2026-10-13-run3.md --out-json /home/user/brisvox/bench/results/capacity-pipecat-tuned-linux-2026-10-13-run3.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 20, 40, 80, 120, 160, 240, 320, 480]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 24767 and its descendants every sample period

**Result: max 90 concurrent calls within the SLO**; the next step up did not pass (first failure at 100).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 1008 | 1029 | 1029 | 1029 | +79 | 0.06 | 549 / 553 | 0.01 | 3 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1022 | 1038 | 1043 | 1043 | +88 | 0.62 | 553 / 694 | 0.04 | 3 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 1030 | 1080 | 1096 | 1098 | +130 | 1.16 | 691 / 933 | 0.06 | 3 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 1027 | 1107 | 1184 | 1188 | +157 | 2.16 | 929 / 1286 | 0.13 | 18 | pass |
| 90 | 180 (0) | 360 / 360 | 0 / 0 / 0 | 1025 | 1039 | 1043 | 1044 | +89 | 2.41 | 1725 / 1776 | 0.15 | 4 | pass |
| 100 | 200 (0) | 237 / 400 | 0 / 163 / 0 | 1029 | 1142 | 1267 | 1350 | +192 | 2.44 | 1578 / 1734 | 0.11 | 2 | FAIL |
| 120 | 240 (0) | 457 / 480 | 23 / 0 / 0 | 1037 | 2587 | 3489 | 3979 | +1637 | 2.74 | 1274 / 1587 | 0.14 | 3 | FAIL |

Why steps did not pass:

- 100 concurrent: 163 of 400 turns lost or not clean (0 talked over, 163 no response, 0 bot speaking; allowed 0.0 %)
- 120 concurrent: 23 of 480 turns lost or not clean (23 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 2587 ms is above the limit 1200 ms (floor 950 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
