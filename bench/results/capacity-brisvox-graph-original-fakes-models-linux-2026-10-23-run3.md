# Capacity: brisvox-graph-original-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 14525 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-original-fakes-models (4 CPUs, 4g, Linux) --note run 3 of 3; quiet gate passed after 40s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-original-fakes-models-linux-2026-10-23-run3.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-original-fakes-models-linux-2026-10-23-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 40s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 14525 and its descendants every sample period

**Result: max 170 concurrent calls within the SLO**; the next step up did not pass (first failure at 180).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 867 | 869 | 869 | 869 | +9 | 0.04 | 170 / 180 | 169 / 179 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.12 | 180 / 192 | 179 / 191 | 0.03 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.21 | 191 / 199 | 191 / 199 | 0.04 | 5 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 868 | 868 | 868 | +8 | 0.35 | 199 / 211 | 199 / 211 | 0.05 | 9 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 869 | 872 | 873 | +9 | 0.61 | 211 / 232 | 210 / 232 | 0.07 | 3 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 869 | 873 | 876 | +9 | 1.21 | 231 / 280 | 230 / 280 | 0.13 | 6 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 867 | 869 | 873 | 882 | +9 | 1.56 | 250 / 342 | 249 / 342 | 0.18 | 17 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 867 | 878 | 891 | 917 | +18 | 1.99 | 288 / 398 | 287 / 398 | 0.25 | 19 | pass |
| 170 | 340 (0) | 680 / 680 | 0 / 0 / 0 | 868 | 886 | 897 | 908 | +26 | 2.23 | 371 / 410 | 371 / 409 | 0.29 | 10 | pass |
| 180 | 360 (0) | 720 / 720 | 0 / 0 / 0 | 879 | 1359 | 1427 | 1451 | +499 | 2.48 | 378 / 420 | 378 / 420 | 0.29 | 15 | FAIL |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 931 | 1488 | 1507 | 1528 | +628 | 2.62 | 314 / 450 | 314 / 449 | 0.27 | 12 | FAIL |

Why steps did not pass:

- 180 concurrent: p95 V2V 1359 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 200 concurrent: p95 V2V 1487 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
