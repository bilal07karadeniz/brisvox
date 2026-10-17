# Capacity: brisvox-max-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 354806 --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox-max-fakes-models (4 CPUs, 4g, Linux) --note run 2 of 3; quiet gate passed after 85s: no cargo/rustc/link.exe on Windows, WSL load1 1.39 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-max-fakes-models-linux-2026-10-16-run2.md --out-json <repo>/bench/results/capacity-brisvox-max-fakes-models-linux-2026-10-16-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 3; quiet gate passed after 85s: no cargo/rustc/link.exe on Windows, WSL load1 1.39 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 354806 and its descendants every sample period

**Result: max 380 concurrent calls within the SLO**; the next step up did not pass (first failure at 390).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 865 | 866 | 866 | 866 | +6 | 0.03 | 165 / 174 | 164 / 173 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.09 | 174 / 189 | 173 / 189 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.16 | 189 / 197 | 189 / 196 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 865 | 867 | 868 | 868 | +7 | 0.26 | 197 / 206 | 196 / 205 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.47 | 205 / 228 | 204 / 228 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 868 | 869 | +8 | 0.93 | 227 / 275 | 226 / 274 | 0.09 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 1.28 | 248 / 344 | 247 / 343 | 0.14 | 24 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 873 | +8 | 1.55 | 296 / 390 | 295 / 390 | 0.19 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 875 | 877 | 882 | +15 | 1.58 | 335 / 439 | 334 / 438 | 0.16 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 870 | 874 | 879 | +10 | 2.03 | 378 / 478 | 377 / 478 | 0.25 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 874 | 979 | 995 | 1015 | +119 | 2.55 | 374 / 534 | 374 / 533 | 0.29 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 1094 | 1459 | 1523 | 1548 | +599 | 2.82 | 357 / 576 | 356 / 575 | 0.32 | 2 | pass |
| 360 | 720 (0) | 1440 / 1440 | 0 / 0 / 0 | 1320 | 3633 | 3643 | 3653 | +2773 | 3.07 | 452 / 619 | 451 / 619 | 0.36 | 2 | pass |
| 380 | 760 (0) | 1520 / 1520 | 0 / 0 / 0 | 1659 | 3639 | 3684 | 3828 | +2779 | 3.14 | 429 / 630 | 428 / 630 | 0.38 | 2 | pass |
| 390 | 780 (0) | 1546 / 1560 | 14 / 0 / 0 | 1457 | 3635 | 3654 | 3998 | +2775 | 3.06 | 442 / 662 | 441 / 661 | 0.35 | 2 | FAIL |
| 400 | 800 (0) | 1596 / 1600 | 4 / 0 / 0 | 1729 | 3636 | 3755 | 3980 | +2776 | 3.05 | 411 / 669 | 410 / 668 | 0.35 | 3 | FAIL |

Why steps did not pass:

- 390 concurrent: 14 of 1560 turns lost or not clean (14 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 400 concurrent: 4 of 1600 turns lost or not clean (4 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
