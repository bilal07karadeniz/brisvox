# Capacity: brisvox-graph-original-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 21549 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-original-fakes-models (4 CPUs, 4g, Linux) --note run 4 of 4; quiet gate passed after 30s: no cargo/rustc/link.exe on Windows, WSL load1 1.38 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-original-fakes-models-linux-2026-10-23-run4.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-original-fakes-models-linux-2026-10-23-run4.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 4 of 4; quiet gate passed after 30s: no cargo/rustc/link.exe on Windows, WSL load1 1.38 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 21549 and its descendants every sample period

**Result: max 200 concurrent calls within the SLO**; the next step up did not pass (first failure at 210).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 865 | 868 | 868 | 868 | +8 | 0.04 | 170 / 181 | 170 / 180 | 0.01 | 5 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.11 | 181 / 196 | 180 / 196 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.17 | 196 / 196 | 196 / 196 | 0.03 | 4 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 868 | 872 | 872 | +8 | 0.34 | 196 / 209 | 196 / 209 | 0.05 | 5 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 0.56 | 209 / 236 | 208 / 236 | 0.06 | 3 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 869 | 870 | +8 | 1.15 | 236 / 281 | 235 / 281 | 0.12 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 867 | 870 | 874 | 881 | +10 | 1.51 | 276 / 349 | 276 / 349 | 0.18 | 8 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 867 | 873 | 880 | 895 | +13 | 1.84 | 302 / 406 | 302 / 406 | 0.23 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 877 | 891 | 902 | 910 | +31 | 2.38 | 320 / 444 | 319 / 444 | 0.24 | 3 | pass |
| 210 | 420 (0) | 840 / 840 | 0 / 0 / 0 | 964 | 1509 | 1594 | 1717 | +649 | 2.66 | 362 / 454 | 362 / 453 | 0.32 | 18 | FAIL |
| 220 | 440 (0) | 880 / 880 | 0 / 0 / 0 | 955 | 1384 | 1504 | 1539 | +524 | 2.68 | 374 / 469 | 373 / 470 | 0.32 | 14 | FAIL |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 1012 | 2025 | 2239 | 3651 | +1165 | 2.90 | 332 / 500 | 331 / 500 | 0.32 | 3 | FAIL |

Why steps did not pass:

- 210 concurrent: p95 V2V 1509 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 220 concurrent: p95 V2V 1383 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 240 concurrent: p95 V2V 2024 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
