# Capacity: brisvox-8c8g-max-fakes-models (8 CPUs, 8g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 366504 --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox-8c8g-max-fakes-models (8 CPUs, 8g, Linux) --note run 2 of 3; quiet gate passed after 120s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-8c8g-max-fakes-models-linux-2026-10-16-run2.md --out-json <repo>/bench/results/capacity-brisvox-8c8g-max-fakes-models-linux-2026-10-16-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 3; quiet gate passed after 120s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 366504 and its descendants every sample period

**Result: max 812 concurrent calls within the SLO**; the next step up did not pass (first failure at 818).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.03 | 203 / 230 | 203 / 230 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.10 | 229 / 255 | 229 / 255 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.17 | 255 / 276 | 255 / 276 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.29 | 276 / 286 | 275 / 286 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 884 | 895 | +8 | 0.52 | 285 / 311 | 285 / 311 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 865 | 867 | 868 | 908 | +7 | 1.04 | 309 / 358 | 309 / 357 | 0.10 | 13 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 865 | 867 | 868 | 868 | +7 | 1.41 | 338 / 401 | 339 / 401 | 0.13 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 865 | 867 | 868 | 869 | +7 | 1.66 | 373 / 439 | 373 / 439 | 0.17 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 865 | 867 | 868 | 873 | +7 | 2.17 | 380 / 605 | 380 / 606 | 0.25 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 865 | 867 | 868 | 872 | +7 | 3.07 | 499 / 710 | 498 / 710 | 0.32 | 2 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 867 | 872 | 877 | 894 | +12 | 3.55 | 568 / 797 | 569 / 797 | 0.35 | 27 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 958 | 1275 | 1337 | 1381 | +415 | 5.09 | 567 / 894 | 567 / 894 | 0.40 | 2 | pass |
| 560 | 1120 (0) | 2240 / 2240 | 0 / 0 / 0 | 1248 | 2123 | 3642 | 3671 | +1263 | 5.53 | 578 / 988 | 578 / 989 | 0.43 | 3 | pass |
| 640 | 1280 (0) | 2560 / 2560 | 0 / 0 / 0 | 1398 | 3634 | 3656 | 3694 | +2774 | 6.01 | 660 / 1083 | 660 / 1084 | 0.48 | 3 | pass |
| 800 | 1600 (0) | 3200 / 3200 | 0 / 0 / 0 | 2424 | 3706 | 3822 | 3948 | +2846 | 6.21 | 654 / 1236 | 654 / 1237 | 0.57 | 3 | pass |
| 812 | 1624 (0) | 3248 / 3248 | 0 / 0 / 0 | 2467 | 3705 | 3779 | 3901 | +2845 | 6.25 | 738 / 1253 | 738 / 1254 | 0.59 | 3 | pass |
| 818 | 1636 (0) | 3270 / 3272 | 2 / 0 / 0 | 2449 | 3724 | 3849 | 3995 | +2864 | 6.26 | 759 / 1252 | 759 / 1253 | 0.60 | 3 | FAIL |
| 825 | 1650 (0) | 3292 / 3300 | 8 / 0 / 0 | 2501 | 3709 | 3752 | 3989 | +2849 | 6.26 | 745 / 1265 | 745 / 1266 | 0.59 | 3 | FAIL |
| 850 | 1700 (0) | 3396 / 3400 | 4 / 0 / 0 | 2593 | 3773 | 3885 | 3998 | +2913 | 6.31 | 868 / 1286 | 868 / 1287 | 0.62 | 3 | FAIL |
| 900 | 1800 (0) | 3421 / 3600 | 179 / 0 / 0 | 2722 | 3856 | 3942 | 3999 | +2996 | 6.36 | 808 / 1337 | 807 / 1338 | 0.64 | 3 | FAIL |
| 1000 | 2000 (0) | 3053 / 4000 | 947 / 0 / 0 | 3634 | 3990 | 4123 | 4188 | +3130 | 6.49 | 838 / 1421 | 838 / 1422 | 0.69 | 3 | FAIL |

Why steps did not pass:

- 818 concurrent: 2 of 3272 turns lost or not clean (2 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 825 concurrent: 8 of 3300 turns lost or not clean (8 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 850 concurrent: 4 of 3400 turns lost or not clean (4 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 900 concurrent: 179 of 3600 turns lost or not clean (179 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 1000 concurrent: 947 of 4000 turns lost or not clean (947 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
