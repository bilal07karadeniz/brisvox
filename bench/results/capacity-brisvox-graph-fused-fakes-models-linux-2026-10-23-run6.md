# Capacity: brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 64654 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux) --note run 6 of 6; quiet gate passed after 20s: no cargo/rustc/link.exe on Windows, WSL load1 1.44 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run6.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run6.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 6 of 6; quiet gate passed after 20s: no cargo/rustc/link.exe on Windows, WSL load1 1.44 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 64654 and its descendants every sample period

**Result: max 320 concurrent calls within the SLO**; the next step up did not pass (first failure at 330).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 867 | 867 | 867 | 867 | +7 | 0.03 | 176 / 187 | 176 / 186 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.10 | 187 / 201 | 186 / 200 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 0.15 | 201 / 209 | 200 / 208 | 0.02 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.25 | 209 / 217 | 208 / 216 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.46 | 217 / 239 | 216 / 238 | 0.04 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 867 | 868 | 871 | +7 | 0.88 | 238 / 288 | 238 / 288 | 0.08 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 868 | 869 | +8 | 1.22 | 270 / 332 | 269 / 332 | 0.13 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 872 | +8 | 1.44 | 276 / 386 | 276 / 386 | 0.16 | 3 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 875 | 876 | 878 | +15 | 1.49 | 311 / 454 | 310 / 454 | 0.13 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 868 | 870 | 874 | +8 | 1.85 | 373 / 495 | 373 / 495 | 0.22 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 873 | 920 | 944 | 956 | +60 | 2.37 | 382 / 542 | 381 / 542 | 0.25 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 882 | 1025 | 1042 | 1063 | +165 | 2.59 | 374 / 600 | 373 / 599 | 0.29 | 2 | pass |
| 330 | 660 (0) | 1320 / 1320 | 0 / 0 / 0 | 882 | 1159 | 1243 | 1296 | +299 | 2.65 | 440 / 599 | 440 / 599 | 0.29 | 2 | FAIL |
| 340 | 680 (0) | 1360 / 1360 | 0 / 0 / 0 | 924 | 1356 | 1426 | 1461 | +496 | 2.72 | 431 / 607 | 430 / 606 | 0.30 | 2 | FAIL |
| 360 | 720 (0) | 1440 / 1440 | 0 / 0 / 0 | 992 | 1819 | 3629 | 3668 | +959 | 2.71 | 423 / 632 | 422 / 631 | 0.30 | 2 | FAIL |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1285 | 3632 | 3643 | 3663 | +2772 | 2.88 | 450 / 681 | 449 / 681 | 0.30 | 36 | FAIL |

Why steps did not pass:

- 330 concurrent: p95 V2V 1158 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 340 concurrent: p95 V2V 1355 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 360 concurrent: p95 V2V 1819 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 400 concurrent: p95 V2V 3631 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
