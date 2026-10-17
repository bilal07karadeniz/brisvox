# Capacity: brisvox-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 349450 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-fakes-models (4 CPUs, 4g, Linux) --note run 1 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.73 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-fakes-models-linux-2026-10-16-run1.md --out-json <repo>/bench/results/capacity-brisvox-fakes-models-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.73 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 349450 and its descendants every sample period

**Result: max 300 concurrent calls within the SLO**; the next step up did not pass (first failure at 310).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 867 | 868 | 868 | 868 | +8 | 0.03 | 167 / 180 | 167 / 179 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.09 | 180 / 192 | 179 / 191 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.17 | 191 / 191 | 190 / 191 | 0.03 | 3 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.27 | 190 / 203 | 190 / 203 | 0.04 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.50 | 203 / 226 | 203 / 226 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 869 | 872 | +8 | 0.99 | 226 / 274 | 226 / 274 | 0.10 | 10 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 867 | 868 | 871 | +7 | 1.27 | 259 / 349 | 258 / 349 | 0.14 | 21 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 872 | +8 | 1.50 | 316 / 401 | 315 / 401 | 0.17 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 874 | 877 | 884 | +14 | 1.58 | 340 / 441 | 339 / 441 | 0.16 | 3 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 867 | 882 | 891 | 901 | +22 | 2.09 | 357 / 486 | 356 / 486 | 0.24 | 9 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 873 | 887 | 894 | 913 | +27 | 2.44 | 398 / 535 | 397 / 535 | 0.27 | 2 | pass |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 881 | 1105 | 1125 | 1141 | +245 | 2.54 | 423 / 550 | 422 / 550 | 0.26 | 2 | pass |
| 310 | 620 (0) | 1240 / 1240 | 0 / 0 / 0 | 880 | 1578 | 1599 | 3625 | +718 | 2.55 | 433 / 564 | 432 / 564 | 0.28 | 2 | FAIL |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 937 | 1270 | 1401 | 1465 | +410 | 2.67 | 358 / 583 | 358 / 582 | 0.30 | 2 | FAIL |

Why steps did not pass:

- 310 concurrent: p95 V2V 1577 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 320 concurrent: p95 V2V 1269 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
