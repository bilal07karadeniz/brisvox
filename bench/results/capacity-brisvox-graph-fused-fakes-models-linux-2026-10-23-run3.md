# Capacity: brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 7489 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux) --note run 3 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 1.29 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run3.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 1.29 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 7489 and its descendants every sample period

**Result: max 190 concurrent calls within the SLO**; the next step up did not pass (first failure at 200).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 867 | 868 | 868 | 868 | +8 | 0.05 | 175 / 188 | 175 / 187 | 0.02 | 3 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 867 | 869 | 870 | 870 | +9 | 0.15 | 188 / 205 | 187 / 205 | 0.03 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 867 | 869 | 869 | 869 | +9 | 0.24 | 205 / 209 | 205 / 209 | 0.04 | 3 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 869 | 870 | 870 | +9 | 0.40 | 209 / 217 | 208 / 217 | 0.06 | 4 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 867 | 869 | 869 | 872 | +9 | 0.72 | 216 / 235 | 216 / 235 | 0.08 | 6 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 867 | 869 | 872 | 877 | +9 | 1.26 | 234 / 306 | 234 / 306 | 0.14 | 12 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 867 | 869 | 872 | 874 | +9 | 1.59 | 289 / 354 | 288 / 354 | 0.20 | 23 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 867 | 876 | 884 | 893 | +16 | 1.96 | 310 / 407 | 309 / 407 | 0.25 | 9 | pass |
| 180 | 360 (0) | 720 / 720 | 0 / 0 / 0 | 872 | 889 | 904 | 974 | +29 | 2.32 | 361 / 429 | 360 / 429 | 0.30 | 8 | pass |
| 190 | 380 (0) | 760 / 760 | 0 / 0 / 0 | 878 | 972 | 981 | 993 | +112 | 2.50 | 340 / 437 | 340 / 437 | 0.32 | 7 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 885 | 1145 | 1253 | 1286 | +285 | 2.48 | 340 / 454 | 340 / 454 | 0.26 | 12 | FAIL |

Why steps did not pass:

- 200 concurrent: p95 V2V 1145 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
