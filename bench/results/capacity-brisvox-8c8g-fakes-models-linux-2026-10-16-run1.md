# Capacity: brisvox-8c8g-fakes-models (8 CPUs, 8g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 359172 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-8c8g-fakes-models (8 CPUs, 8g, Linux) --note run 1 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 1.12 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-8c8g-fakes-models-linux-2026-10-16-run1.md --out-json <repo>/bench/results/capacity-brisvox-8c8g-fakes-models-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 1.12 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 359172 and its descendants every sample period

**Result: max 440 concurrent calls within the SLO**; the next step up did not pass (first failure at 450).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 867 | 868 | 868 | 868 | +8 | 0.02 | 207 / 235 | 207 / 235 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.10 | 235 / 255 | 235 / 255 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.17 | 255 / 269 | 255 / 269 | 0.02 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.29 | 269 / 294 | 269 / 295 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.52 | 294 / 317 | 294 / 318 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 865 | 867 | 868 | 869 | +7 | 1.02 | 315 / 357 | 315 / 358 | 0.09 | 13 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 1.39 | 347 / 396 | 347 / 396 | 0.13 | 3 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 865 | 867 | 868 | 868 | +7 | 1.67 | 360 / 441 | 360 / 442 | 0.17 | 4 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 865 | 867 | 868 | 869 | +7 | 2.20 | 395 / 561 | 395 / 561 | 0.25 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 868 | 869 | 873 | +8 | 3.13 | 480 / 682 | 480 / 682 | 0.32 | 2 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 868 | 889 | 899 | 913 | +29 | 3.95 | 536 / 801 | 536 / 802 | 0.38 | 26 | pass |
| 440 | 880 (0) | 1760 / 1760 | 0 / 0 / 0 | 879 | 1051 | 1102 | 1126 | +191 | 4.64 | 596 / 835 | 596 / 836 | 0.41 | 3 | pass |
| 450 | 900 (0) | 1800 / 1800 | 0 / 0 / 0 | 899 | 1274 | 1359 | 1398 | +414 | 5.00 | 645 / 849 | 645 / 850 | 0.41 | 3 | FAIL |
| 460 | 920 (0) | 1840 / 1840 | 0 / 0 / 0 | 943 | 1282 | 1313 | 1354 | +422 | 5.17 | 656 / 859 | 657 / 860 | 0.42 | 3 | FAIL |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 1058 | 1510 | 1612 | 1649 | +650 | 5.35 | 643 / 890 | 643 / 891 | 0.43 | 2 | FAIL |

Why steps did not pass:

- 450 concurrent: p95 V2V 1273 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 460 concurrent: p95 V2V 1282 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 480 concurrent: p95 V2V 1509 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
