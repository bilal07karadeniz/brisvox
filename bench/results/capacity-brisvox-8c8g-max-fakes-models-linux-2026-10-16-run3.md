# Capacity: brisvox-8c8g-max-fakes-models (8 CPUs, 8g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 367291 --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox-8c8g-max-fakes-models (8 CPUs, 8g, Linux) --note run 3 of 3; quiet gate passed after 125s: no cargo/rustc/link.exe on Windows, WSL load1 1.40 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-8c8g-max-fakes-models-linux-2026-10-16-run3.md --out-json <repo>/bench/results/capacity-brisvox-8c8g-max-fakes-models-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 125s: no cargo/rustc/link.exe on Windows, WSL load1 1.40 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 367291 and its descendants every sample period

**Result: max 750 concurrent calls within the SLO**; the next step up did not pass (first failure at 760).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 867 | 867 | 867 | 867 | +7 | 0.03 | 205 / 229 | 206 / 229 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.10 | 229 / 259 | 229 / 259 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.18 | 259 / 279 | 259 / 279 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.30 | 278 / 293 | 278 / 294 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 875 | 878 | +7 | 0.52 | 293 / 318 | 293 / 319 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 1.02 | 317 / 356 | 317 / 356 | 0.09 | 24 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 1.39 | 348 / 405 | 348 / 406 | 0.12 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 867 | 868 | 869 | +7 | 1.67 | 396 / 450 | 395 / 451 | 0.17 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 867 | 868 | 869 | +7 | 2.16 | 399 / 611 | 399 / 612 | 0.24 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 866 | 868 | 870 | 873 | +8 | 3.05 | 521 / 706 | 521 / 706 | 0.30 | 2 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 867 | 871 | 873 | 876 | +11 | 3.45 | 588 / 797 | 588 / 798 | 0.34 | 2 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 974 | 1321 | 1389 | 1433 | +461 | 5.12 | 614 / 888 | 614 / 889 | 0.40 | 3 | pass |
| 560 | 1120 (0) | 2240 / 2240 | 0 / 0 / 0 | 1124 | 2085 | 3637 | 3674 | +1225 | 5.54 | 651 / 985 | 651 / 986 | 0.43 | 4 | pass |
| 640 | 1280 (0) | 2560 / 2560 | 0 / 0 / 0 | 1435 | 3635 | 3648 | 3696 | +2775 | 6.02 | 659 / 1072 | 659 / 1073 | 0.48 | 3 | pass |
| 720 | 1440 (0) | 2880 / 2880 | 0 / 0 / 0 | 2170 | 3654 | 3677 | 3763 | +2794 | 6.20 | 806 / 1160 | 806 / 1161 | 0.54 | 2 | pass |
| 740 | 1480 (0) | 2960 / 2960 | 0 / 0 / 0 | 2240 | 3657 | 3704 | 3786 | +2797 | 6.26 | 826 / 1170 | 826 / 1171 | 0.56 | 3 | pass |
| 750 | 1500 (0) | 3000 / 3000 | 0 / 0 / 0 | 2286 | 3658 | 3686 | 3737 | +2798 | 6.28 | 771 / 1198 | 771 / 1199 | 0.56 | 3 | pass |
| 760 | 1520 (0) | 3006 / 3040 | 34 / 0 / 0 | 2326 | 3665 | 3704 | 3995 | +2805 | 6.28 | 755 / 1198 | 755 / 1199 | 0.56 | 3 | FAIL |
| 800 | 1600 (0) | 3164 / 3200 | 36 / 0 / 0 | 2457 | 3689 | 3784 | 3994 | +2829 | 6.23 | 637 / 1217 | 637 / 1219 | 0.58 | 3 | FAIL |

Why steps did not pass:

- 760 concurrent: 34 of 3040 turns lost or not clean (34 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 800 concurrent: 36 of 3200 turns lost or not clean (36 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
