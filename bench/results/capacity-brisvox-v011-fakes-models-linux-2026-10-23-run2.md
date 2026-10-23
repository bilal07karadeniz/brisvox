# Capacity: brisvox-v011-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 36503 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-v011-fakes-models (4 CPUs, 4g, Linux) --note run 2 of 2; quiet gate passed after 30s: no cargo/rustc/link.exe on Windows, WSL load1 1.43 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox-regress/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox-regress/bench/results/capacity-brisvox-v011-fakes-models-linux-2026-10-23-run2.md --out-json /mnt/c/Projelerim/brisvox-regress/bench/results/capacity-brisvox-v011-fakes-models-linux-2026-10-23-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 2; quiet gate passed after 30s: no cargo/rustc/link.exe on Windows, WSL load1 1.43 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 36503 and its descendants every sample period

**Result: max 280 concurrent calls within the SLO**; the next step up did not pass (first failure at 290).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.03 | 169 / 176 | 168 / 176 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.09 | 176 / 194 | 176 / 193 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.15 | 194 / 203 | 193 / 203 | 0.02 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.26 | 203 / 213 | 202 / 212 | 0.03 | 7 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.47 | 213 / 232 | 212 / 231 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.90 | 231 / 281 | 230 / 281 | 0.08 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 868 | 869 | +8 | 1.24 | 259 / 324 | 258 / 324 | 0.12 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 872 | +8 | 1.47 | 288 / 405 | 287 / 405 | 0.16 | 14 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 867 | 871 | 874 | 878 | +11 | 1.54 | 318 / 448 | 318 / 447 | 0.14 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 868 | 870 | 875 | +8 | 1.90 | 347 / 485 | 346 / 485 | 0.23 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 873 | 887 | 895 | 905 | +27 | 2.41 | 352 / 537 | 351 / 537 | 0.26 | 2 | pass |
| 290 | 580 (0) | 1160 / 1160 | 0 / 0 / 0 | 879 | 1289 | 1316 | 1328 | +429 | 2.55 | 428 / 544 | 427 / 544 | 0.27 | 2 | FAIL |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 879 | 1180 | 1202 | 1232 | +320 | 2.54 | 380 / 557 | 379 / 557 | 0.26 | 2 | FAIL |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 1042 | 1646 | 1700 | 3657 | +786 | 2.71 | 378 / 583 | 377 / 583 | 0.29 | 2 | FAIL |

Why steps did not pass:

- 290 concurrent: p95 V2V 1289 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 300 concurrent: p95 V2V 1180 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 320 concurrent: p95 V2V 1645 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
