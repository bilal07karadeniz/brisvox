# Capacity: brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 29703 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-fused-fakes-models (4 CPUs, 4g, Linux) --note run 2 of 2; quiet gate passed after 35s: no cargo/rustc/link.exe on Windows, WSL load1 1.42 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run2.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-fused-fakes-models-linux-2026-10-23-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 2; quiet gate passed after 35s: no cargo/rustc/link.exe on Windows, WSL load1 1.42 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=fused; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 29703 and its descendants every sample period

**Result: max 240 concurrent calls within the SLO**; the next step up did not pass (first failure at 250).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 868 | 869 | 869 | 869 | +9 | 0.06 | 176 / 184 | 176 / 184 | 0.02 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 868 | 870 | 870 | 870 | +10 | 0.20 | 184 / 204 | 183 / 204 | 0.04 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 868 | 869 | 870 | 870 | +9 | 0.33 | 204 / 206 | 203 / 206 | 0.06 | 3 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 867 | 869 | 870 | 870 | +9 | 0.52 | 206 / 216 | 206 / 216 | 0.06 | 3 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 0.62 | 216 / 236 | 215 / 236 | 0.06 | 4 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 869 | 877 | 879 | +9 | 1.17 | 236 / 310 | 236 / 310 | 0.12 | 3 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 869 | 873 | +8 | 1.46 | 284 / 351 | 283 / 351 | 0.17 | 3 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 869 | 871 | 881 | +9 | 1.75 | 317 / 403 | 316 / 403 | 0.21 | 3 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 876 | 894 | 905 | 933 | +34 | 2.11 | 353 / 450 | 352 / 450 | 0.22 | 3 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 878 | 969 | 993 | 1009 | +109 | 2.57 | 346 / 494 | 346 / 494 | 0.29 | 3 | pass |
| 250 | 500 (0) | 985 / 1000 | 5 / 10 / 0 | 1940 | 3650 | 3748 | 3942 | +2790 | 2.55 | 358 / 530 | 357 / 529 | 0.30 | 6 | FAIL |
| 260 | 520 (0) | 1040 / 1040 | 0 / 0 / 0 | 1026 | 1902 | 2222 | 2293 | +1042 | 2.86 | 357 / 513 | 356 / 512 | 0.33 | 3 | FAIL |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 1142 | 2005 | 3625 | 3637 | +1145 | 2.85 | 386 / 544 | 385 / 543 | 0.33 | 3 | FAIL |

Why steps did not pass:

- 250 concurrent: 15 of 1000 turns lost or not clean (5 talked over, 10 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3650 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 260 concurrent: p95 V2V 1901 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 280 concurrent: p95 V2V 2005 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
