# Capacity: brisvox-max-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 355378 --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox-max-fakes-models (4 CPUs, 4g, Linux) --note run 3 of 3; quiet gate passed after 70s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-max-fakes-models-linux-2026-10-16-run3.md --out-json <repo>/bench/results/capacity-brisvox-max-fakes-models-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 70s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 355378 and its descendants every sample period

**Result: max 500 concurrent calls within the SLO**; the next step up did not pass (first failure at 510).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.03 | 171 / 185 | 171 / 184 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 865 | 868 | 868 | 868 | +8 | 0.08 | 185 / 193 | 184 / 192 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.16 | 193 / 198 | 192 / 197 | 0.03 | 5 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.27 | 197 / 207 | 197 / 206 | 0.04 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 868 | 869 | +7 | 0.48 | 206 / 229 | 205 / 229 | 0.05 | 4 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 868 | 870 | +8 | 0.95 | 229 / 280 | 228 / 280 | 0.10 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 1.30 | 253 / 330 | 253 / 329 | 0.14 | 12 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 868 | 872 | +8 | 1.50 | 275 / 377 | 274 / 377 | 0.17 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 875 | 877 | 884 | +15 | 1.55 | 299 / 450 | 298 / 450 | 0.15 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 869 | 870 | 873 | +9 | 1.94 | 309 / 491 | 308 / 491 | 0.24 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 874 | 1280 | 1304 | 1329 | +420 | 2.52 | 371 / 540 | 370 / 539 | 0.28 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 955 | 1320 | 1379 | 1426 | +460 | 2.73 | 383 / 586 | 382 / 586 | 0.31 | 2 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1507 | 3634 | 3643 | 3661 | +2774 | 3.01 | 442 / 675 | 441 / 675 | 0.33 | 15 | pass |
| 480 | 960 (0) | 1920 / 1920 | 0 / 0 / 0 | 2021 | 3650 | 3663 | 3694 | +2790 | 3.20 | 464 / 759 | 463 / 758 | 0.41 | 2 | pass |
| 500 | 1000 (0) | 2000 / 2000 | 0 / 0 / 0 | 2142 | 3656 | 3688 | 3804 | +2796 | 3.23 | 536 / 774 | 535 / 774 | 0.41 | 2 | pass |
| 510 | 1020 (0) | 2009 / 2040 | 31 / 0 / 0 | 2109 | 3661 | 3734 | 3997 | +2801 | 3.18 | 530 / 791 | 529 / 791 | 0.42 | 3 | FAIL |
| 520 | 1040 (0) | 2045 / 2080 | 35 / 0 / 0 | 2182 | 3674 | 3797 | 3904 | +2814 | 3.15 | 504 / 804 | 504 / 804 | 0.42 | 2 | FAIL |
| 560 | 1120 (0) | 2075 / 2240 | 165 / 0 / 0 | 2993 | 3750 | 3925 | 3998 | +2890 | 3.21 | 551 / 847 | 551 / 847 | 0.44 | 2 | FAIL |
| 640 | 1280 (0) | 1631 / 2560 | 667 / 262 / 0 | 3629 | 4008 | 4192 | 4453 | +3148 | 2.97 | 537 / 919 | 537 / 919 | 0.44 | 2 | FAIL |

Why steps did not pass:

- 510 concurrent: 31 of 2040 turns lost or not clean (31 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 520 concurrent: 35 of 2080 turns lost or not clean (35 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 560 concurrent: 165 of 2240 turns lost or not clean (165 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 640 concurrent: 929 of 2560 turns lost or not clean (667 talked over, 262 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
