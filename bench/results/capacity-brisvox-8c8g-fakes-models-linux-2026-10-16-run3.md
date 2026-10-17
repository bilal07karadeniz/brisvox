# Capacity: brisvox-8c8g-fakes-models (8 CPUs, 8g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 360408 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-8c8g-fakes-models (8 CPUs, 8g, Linux) --note run 3 of 3; quiet gate passed after 95s: no cargo/rustc/link.exe on Windows, WSL load1 1.39 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-8c8g-fakes-models-linux-2026-10-16-run3.md --out-json <repo>/bench/results/capacity-brisvox-8c8g-fakes-models-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 95s: no cargo/rustc/link.exe on Windows, WSL load1 1.39 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 360408 and its descendants every sample period

**Result: max 430 concurrent calls within the SLO**; the next step up did not pass (first failure at 440).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.02 | 204 / 235 | 205 / 235 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.10 | 235 / 254 | 235 / 254 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 865 | 867 | 867 | 867 | +7 | 0.17 | 254 / 262 | 254 / 263 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.29 | 262 / 287 | 263 / 288 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.52 | 287 / 315 | 287 / 316 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 865 | 867 | 868 | 868 | +7 | 1.04 | 315 / 358 | 315 / 359 | 0.10 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 865 | 867 | 868 | 868 | +7 | 1.41 | 350 / 398 | 350 / 399 | 0.13 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 865 | 867 | 868 | 868 | +7 | 1.68 | 393 / 444 | 393 / 445 | 0.17 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 865 | 867 | 868 | 870 | +7 | 2.20 | 405 / 615 | 406 / 616 | 0.25 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 868 | 869 | 874 | +8 | 3.13 | 513 / 706 | 513 / 707 | 0.32 | 2 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 868 | 883 | 893 | 912 | +23 | 3.83 | 552 / 804 | 553 / 805 | 0.37 | 19 | pass |
| 420 | 840 (0) | 1680 / 1680 | 0 / 0 / 0 | 871 | 989 | 1032 | 1054 | +129 | 4.45 | 665 / 815 | 666 / 816 | 0.42 | 2 | pass |
| 430 | 860 (0) | 1720 / 1720 | 0 / 0 / 0 | 879 | 1001 | 1038 | 1064 | +141 | 4.68 | 666 / 821 | 667 / 822 | 0.42 | 3 | pass |
| 440 | 880 (0) | 1760 / 1760 | 0 / 0 / 0 | 888 | 1151 | 1229 | 1253 | +291 | 4.95 | 590 / 844 | 590 / 846 | 0.42 | 3 | FAIL |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 1053 | 1559 | 1712 | 1761 | +699 | 5.34 | 617 / 903 | 617 / 905 | 0.43 | 3 | FAIL |

Why steps did not pass:

- 440 concurrent: p95 V2V 1151 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 480 concurrent: p95 V2V 1559 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
