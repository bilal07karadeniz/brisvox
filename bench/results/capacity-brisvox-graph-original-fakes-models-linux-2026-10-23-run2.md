# Capacity: brisvox-graph-original-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 22637 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-original-fakes-models (4 CPUs, 4g, Linux) --note run 2 of 2; quiet gate passed after 55s: no cargo/rustc/link.exe on Windows, WSL load1 1.38 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-original-fakes-models-linux-2026-10-23-run2.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-original-fakes-models-linux-2026-10-23-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 2; quiet gate passed after 55s: no cargo/rustc/link.exe on Windows, WSL load1 1.38 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 22637 and its descendants every sample period

**Result: max 200 concurrent calls within the SLO**; the next step up did not pass (first failure at 210).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.03 | 170 / 182 | 169 / 182 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.11 | 182 / 195 | 182 / 195 | 0.02 | 5 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 0.18 | 194 / 204 | 193 / 203 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.33 | 204 / 211 | 203 / 211 | 0.04 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 868 | 870 | +8 | 0.56 | 211 / 228 | 211 / 228 | 0.06 | 3 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 868 | 870 | +8 | 1.05 | 228 / 278 | 228 / 278 | 0.11 | 3 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 869 | 871 | +8 | 1.44 | 254 / 357 | 253 / 357 | 0.17 | 24 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 869 | 871 | 874 | +9 | 1.76 | 327 / 402 | 326 / 402 | 0.21 | 3 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 878 | 1018 | 1048 | 1067 | +158 | 2.32 | 326 / 447 | 325 / 447 | 0.23 | 3 | pass |
| 210 | 420 (0) | 632 / 840 | 157 / 51 / 0 | 3651 | 3913 | 3999 | 4084 | +3053 | 2.92 | 388 / 463 | 388 / 463 | 0.35 | 18 | FAIL |
| 220 | 440 (0) | 880 / 880 | 0 / 0 / 0 | 965 | 1267 | 1318 | 1340 | +407 | 2.66 | 367 / 464 | 366 / 464 | 0.33 | 3 | FAIL |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 929 | 1831 | 1994 | 2040 | +971 | 2.86 | 336 / 489 | 336 / 489 | 0.33 | 4 | FAIL |

Why steps did not pass:

- 210 concurrent: 208 of 840 turns lost or not clean (157 talked over, 51 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3912 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 220 concurrent: p95 V2V 1266 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 240 concurrent: p95 V2V 1830 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
