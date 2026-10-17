# Capacity: brisvox-max-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 354179 --floor-ms 860 --slo-over-floor-ms 60000 --label brisvox-max-fakes-models (4 CPUs, 4g, Linux) --note run 1 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.76 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-max-fakes-models-linux-2026-10-16-run1.md --out-json <repo>/bench/results/capacity-brisvox-max-fakes-models-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.76 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 354179 and its descendants every sample period

**Result: max 430 concurrent calls within the SLO**; the next step up did not pass (first failure at 440).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.03 | 167 / 178 | 167 / 178 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.09 | 178 / 189 | 178 / 189 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.16 | 189 / 199 | 188 / 199 | 0.03 | 4 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.27 | 199 / 206 | 199 / 205 | 0.04 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 868 | 870 | +8 | 0.54 | 204 / 228 | 204 / 228 | 0.06 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 1.03 | 228 / 275 | 227 / 275 | 0.11 | 15 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 1.29 | 253 / 325 | 252 / 325 | 0.14 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 873 | +8 | 1.53 | 270 / 374 | 270 / 373 | 0.18 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 867 | 875 | 878 | 884 | +15 | 1.64 | 288 / 450 | 288 / 450 | 0.18 | 3 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 867 | 880 | 886 | 895 | +20 | 2.19 | 390 / 490 | 390 / 490 | 0.25 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 876 | 1845 | 2026 | 3642 | +985 | 2.62 | 391 / 541 | 391 / 541 | 0.29 | 2 | pass |
| 320 | 640 (0) | 1280 / 1280 | 0 / 0 / 0 | 1055 | 1608 | 1705 | 1735 | +748 | 2.79 | 369 / 583 | 369 / 583 | 0.32 | 2 | pass |
| 400 | 800 (0) | 1600 / 1600 | 0 / 0 / 0 | 1604 | 3636 | 3654 | 3707 | +2776 | 3.05 | 418 / 674 | 418 / 674 | 0.34 | 33 | pass |
| 420 | 840 (0) | 1680 / 1680 | 0 / 0 / 0 | 1594 | 3640 | 3656 | 3778 | +2780 | 3.17 | 459 / 694 | 458 / 694 | 0.39 | 2 | pass |
| 430 | 860 (0) | 1720 / 1720 | 0 / 0 / 0 | 1723 | 3651 | 3687 | 3815 | +2791 | 3.15 | 454 / 699 | 453 / 698 | 0.40 | 2 | pass |
| 440 | 880 (0) | 1756 / 1760 | 4 / 0 / 0 | 2151 | 3659 | 3904 | 3996 | +2799 | 3.17 | 577 / 708 | 576 / 707 | 0.40 | 2 | FAIL |
| 480 | 960 (0) | 1693 / 1920 | 132 / 95 / 0 | 2642 | 3765 | 3928 | 3997 | +2905 | 2.79 | 409 / 737 | 408 / 737 | 0.38 | 2 | FAIL |

Why steps did not pass:

- 440 concurrent: 4 of 1760 turns lost or not clean (4 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 480 concurrent: 227 of 1920 turns lost or not clean (132 talked over, 95 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
