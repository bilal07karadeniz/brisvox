# Capacity: brisvox-8c8g-max-fakes-models (8 CPUs, 8g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 365768 --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox-8c8g-max-fakes-models (8 CPUs, 8g, Linux) --note run 1 of 3; quiet gate passed after 15s: no cargo/rustc/link.exe on Windows, WSL load1 1.48 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-8c8g-max-fakes-models-linux-2026-10-16-run1.md --out-json <repo>/bench/results/capacity-brisvox-8c8g-max-fakes-models-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; quiet gate passed after 15s: no cargo/rustc/link.exe on Windows, WSL load1 1.48 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 365768 and its descendants every sample period

**Result: max 740 concurrent calls within the SLO**; the next step up did not pass (first failure at 750).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 865 | 867 | 867 | 867 | +7 | 0.03 | 205 / 240 | 205 / 240 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.10 | 240 / 258 | 240 / 258 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.17 | 258 / 277 | 258 / 277 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.29 | 277 / 298 | 276 / 298 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.52 | 297 / 313 | 296 / 314 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 867 | 868 | 869 | +7 | 1.02 | 313 / 364 | 313 / 364 | 0.09 | 15 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 1.40 | 353 / 394 | 352 / 395 | 0.13 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 868 | 871 | +8 | 1.68 | 386 / 436 | 387 / 436 | 0.17 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 868 | 868 | 869 | +8 | 2.21 | 376 / 591 | 375 / 592 | 0.25 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 868 | 871 | 874 | +8 | 3.16 | 496 / 686 | 496 / 687 | 0.33 | 2 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 867 | 872 | 875 | 880 | +12 | 3.63 | 545 / 797 | 545 / 797 | 0.37 | 3 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 976 | 1458 | 1521 | 1553 | +598 | 5.26 | 625 / 891 | 626 / 891 | 0.43 | 2 | pass |
| 560 | 1120 (0) | 2240 / 2240 | 0 / 0 / 0 | 1190 | 2414 | 3644 | 3683 | +1554 | 5.58 | 607 / 986 | 607 / 987 | 0.45 | 5 | pass |
| 640 | 1280 (0) | 2560 / 2560 | 0 / 0 / 0 | 1545 | 3639 | 3671 | 3732 | +2779 | 6.07 | 657 / 1084 | 657 / 1085 | 0.49 | 3 | pass |
| 720 | 1440 (0) | 2880 / 2880 | 0 / 0 / 0 | 2227 | 3657 | 3699 | 3749 | +2797 | 6.22 | 700 / 1148 | 699 / 1149 | 0.56 | 3 | pass |
| 740 | 1480 (0) | 2960 / 2960 | 0 / 0 / 0 | 2233 | 3660 | 3704 | 3798 | +2800 | 6.26 | 760 / 1184 | 760 / 1186 | 0.57 | 2 | pass |
| 750 | 1500 (0) | 2985 / 3000 | 15 / 0 / 0 | 2283 | 3669 | 3779 | 3992 | +2809 | 6.27 | 807 / 1184 | 808 / 1185 | 0.56 | 5 | FAIL |
| 760 | 1520 (0) | 3029 / 3040 | 11 / 0 / 0 | 2333 | 3668 | 3703 | 3981 | +2808 | 6.33 | 695 / 1193 | 695 / 1194 | 0.57 | 3 | FAIL |
| 800 | 1600 (0) | 3192 / 3200 | 8 / 0 / 0 | 2443 | 3698 | 3801 | 3956 | +2838 | 6.23 | 665 / 1222 | 665 / 1222 | 0.58 | 3 | FAIL |

Why steps did not pass:

- 750 concurrent: 15 of 3000 turns lost or not clean (15 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 760 concurrent: 11 of 3040 turns lost or not clean (11 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 800 concurrent: 8 of 3200 turns lost or not clean (8 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
