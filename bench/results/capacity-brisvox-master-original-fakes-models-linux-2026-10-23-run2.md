# Capacity: brisvox-master-original-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 28100 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-master-original-fakes-models (4 CPUs, 4g, Linux) --note run 2 of 2; quiet gate passed after 45s: no cargo/rustc/link.exe on Windows, WSL load1 1.45 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-master-original-fakes-models-linux-2026-10-23-run2.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-master-original-fakes-models-linux-2026-10-23-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 2; quiet gate passed after 45s: no cargo/rustc/link.exe on Windows, WSL load1 1.45 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 28100 and its descendants every sample period

**Result: max 270 concurrent calls within the SLO**; the next step up did not pass (first failure at 280).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.03 | 170 / 178 | 170 / 178 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.09 | 178 / 195 | 178 / 195 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.16 | 195 / 200 | 195 / 200 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.25 | 200 / 212 | 200 / 212 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 867 | 868 | 869 | +7 | 0.46 | 212 / 231 | 212 / 231 | 0.04 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 868 | 869 | +8 | 0.94 | 230 / 279 | 230 / 279 | 0.09 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 1.28 | 251 / 342 | 251 / 342 | 0.13 | 22 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 872 | +8 | 1.51 | 288 / 398 | 287 / 399 | 0.17 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 875 | 878 | 883 | +15 | 1.57 | 339 / 447 | 339 / 447 | 0.15 | 3 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 870 | 874 | 882 | +10 | 1.99 | 377 / 490 | 377 / 490 | 0.24 | 2 | pass |
| 260 | 520 (0) | 1040 / 1040 | 0 / 0 / 0 | 871 | 895 | 922 | 939 | +35 | 2.32 | 410 / 504 | 410 / 504 | 0.26 | 2 | pass |
| 270 | 540 (0) | 1080 / 1080 | 0 / 0 / 0 | 872 | 887 | 895 | 912 | +27 | 2.41 | 355 / 524 | 355 / 524 | 0.27 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 888 | 1296 | 1389 | 1426 | +436 | 2.59 | 401 / 541 | 400 / 541 | 0.27 | 2 | FAIL |

Why steps did not pass:

- 280 concurrent: p95 V2V 1295 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
