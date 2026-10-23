# Capacity: brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 43554 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux) --note run 5 of 5; quiet gate passed after 40s: no cargo/rustc/link.exe on Windows, WSL load1 1.44 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run5.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run5.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 5 of 5; quiet gate passed after 40s: no cargo/rustc/link.exe on Windows, WSL load1 1.44 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 43554 and its descendants every sample period

**Result: max 290 concurrent calls within the SLO**; the next step up did not pass (first failure at 300).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 867 | 868 | 868 | 868 | +8 | 0.03 | 177 / 191 | 177 / 191 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.09 | 191 / 203 | 191 / 202 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.15 | 203 / 208 | 202 / 208 | 0.03 | 5 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.26 | 208 / 215 | 208 / 214 | 0.03 | 5 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.46 | 213 / 239 | 213 / 239 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.89 | 239 / 287 | 238 / 286 | 0.08 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 867 | 868 | 870 | +7 | 1.24 | 258 / 338 | 258 / 338 | 0.13 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 872 | +8 | 1.46 | 293 / 387 | 292 / 387 | 0.16 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 875 | 876 | 881 | +15 | 1.52 | 329 / 456 | 328 / 455 | 0.14 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 868 | 870 | 876 | +8 | 1.88 | 354 / 494 | 354 / 493 | 0.22 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 872 | 887 | 898 | 916 | +27 | 2.41 | 383 / 540 | 383 / 540 | 0.26 | 2 | pass |
| 290 | 580 (0) | 1160 / 1160 | 0 / 0 / 0 | 875 | 1003 | 1045 | 1065 | +143 | 2.48 | 399 / 548 | 398 / 548 | 0.27 | 2 | pass |
| 300 | 600 (0) | 1200 / 1200 | 0 / 0 / 0 | 878 | 1469 | 1699 | 1734 | +609 | 2.61 | 428 / 578 | 427 / 578 | 0.25 | 2 | FAIL |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 930 | 1645 | 3634 | 3641 | +785 | 2.69 | 394 / 588 | 393 / 588 | 0.28 | 2 | FAIL |

Why steps did not pass:

- 300 concurrent: p95 V2V 1468 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 320 concurrent: p95 V2V 1644 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
