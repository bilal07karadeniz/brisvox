# Capacity: brisvox-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 349967 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-fakes-models (4 CPUs, 4g, Linux) --note run 2 of 3; quiet gate passed after 65s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-brisvox-fakes-models-linux-2026-10-16-run2.md --out-json <repo>/bench/results/capacity-brisvox-fakes-models-linux-2026-10-16-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 3; quiet gate passed after 65s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 349967 and its descendants every sample period

**Result: max 270 concurrent calls within the SLO**; the next step up did not pass (first failure at 280).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 865 | 867 | 867 | 867 | +7 | 0.03 | 166 / 174 | 166 / 174 | 0.01 | 1 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.09 | 174 / 193 | 174 / 192 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.15 | 189 / 198 | 188 / 198 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.26 | 198 / 209 | 198 / 208 | 0.04 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.47 | 208 / 229 | 207 / 229 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 867 | 868 | 869 | +7 | 0.92 | 229 / 281 | 228 / 281 | 0.09 | 16 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 868 | 871 | +8 | 1.27 | 251 / 316 | 251 / 316 | 0.14 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 873 | +8 | 1.51 | 290 / 367 | 290 / 367 | 0.17 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 876 | 877 | 881 | +16 | 1.56 | 313 / 444 | 313 / 444 | 0.15 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 867 | 873 | 879 | 891 | +13 | 2.05 | 366 / 484 | 365 / 483 | 0.24 | 2 | pass |
| 260 | 520 (0) | 1040 / 1040 | 0 / 0 / 0 | 874 | 889 | 899 | 915 | +29 | 2.46 | 387 / 507 | 387 / 507 | 0.29 | 2 | pass |
| 270 | 540 (0) | 1080 / 1080 | 0 / 0 / 0 | 873 | 885 | 890 | 896 | +25 | 2.50 | 386 / 516 | 385 / 515 | 0.29 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 883 | 1347 | 1441 | 1483 | +487 | 2.64 | 374 / 543 | 374 / 543 | 0.29 | 3 | FAIL |

Why steps did not pass:

- 280 concurrent: p95 V2V 1346 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
