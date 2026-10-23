# Capacity: brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 15553 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux) --note run 1 of 1; quiet gate passed after 30s: no cargo/rustc/link.exe on Windows, WSL load1 1.43 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run1.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 1; quiet gate passed after 30s: no cargo/rustc/link.exe on Windows, WSL load1 1.43 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 15553 and its descendants every sample period

**Result: max 260 concurrent calls within the SLO**; the next step up did not pass (first failure at 270).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.03 | 172 / 181 | 172 / 180 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 867 | 868 | 868 | 868 | +8 | 0.10 | 181 / 197 | 180 / 196 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.18 | 197 / 203 | 196 / 203 | 0.03 | 3 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 0.30 | 203 / 215 | 203 / 215 | 0.04 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 0.51 | 215 / 233 | 214 / 233 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 867 | 869 | 872 | +7 | 1.02 | 233 / 284 | 233 / 284 | 0.11 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 869 | 879 | +8 | 1.38 | 258 / 354 | 257 / 354 | 0.16 | 22 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 870 | 872 | +8 | 1.65 | 312 / 403 | 312 / 403 | 0.20 | 4 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 870 | 879 | 883 | 887 | +19 | 1.77 | 333 / 445 | 333 / 445 | 0.18 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 876 | 929 | 1143 | 1181 | +69 | 2.53 | 340 / 496 | 340 / 497 | 0.28 | 3 | pass |
| 260 | 520 (0) | 1040 / 1040 | 0 / 0 / 0 | 881 | 1034 | 1068 | 1087 | +174 | 2.59 | 406 / 518 | 406 / 519 | 0.30 | 3 | pass |
| 270 | 540 (0) | 1080 / 1080 | 0 / 0 / 0 | 889 | 1737 | 1794 | 3625 | +877 | 2.61 | 406 / 526 | 407 / 526 | 0.28 | 4 | FAIL |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 1450 | 3643 | 3741 | 3983 | +2783 | 2.97 | 388 / 548 | 387 / 548 | 0.32 | 12 | FAIL |

Why steps did not pass:

- 270 concurrent: p95 V2V 1737 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 280 concurrent: p95 V2V 3643 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
