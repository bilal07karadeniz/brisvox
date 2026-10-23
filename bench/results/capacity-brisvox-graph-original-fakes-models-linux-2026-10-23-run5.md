# Capacity: brisvox-graph-original-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 50609 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-graph-original-fakes-models (4 CPUs, 4g, Linux) --note run 5 of 5; quiet gate passed after 35s: no cargo/rustc/link.exe on Windows, WSL load1 1.41 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-original-fakes-models-linux-2026-10-23-run5.md --out-json /mnt/c/Projelerim/brisvox/bench/results/capacity-brisvox-graph-original-fakes-models-linux-2026-10-23-run5.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 5 of 5; quiet gate passed after 35s: no cargo/rustc/link.exe on Windows, WSL load1 1.41 (< 1.5); server flags: -e BRISVOX_SMART_TURN_GRAPH=original; real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 50609 and its descendants every sample period

**Result: max 270 concurrent calls within the SLO**; the next step up did not pass (first failure at 280).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 865 | 867 | 867 | 867 | +7 | 0.03 | 170 / 183 | 170 / 182 | 0.01 | 1 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 865 | 867 | 868 | 868 | +7 | 0.09 | 183 / 195 | 182 / 195 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.16 | 195 / 201 | 194 / 200 | 0.02 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.26 | 200 / 209 | 200 / 209 | 0.03 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.46 | 209 / 239 | 208 / 238 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 865 | 867 | 869 | 869 | +7 | 0.91 | 238 / 280 | 237 / 280 | 0.09 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 868 | 868 | 869 | +8 | 1.26 | 248 / 353 | 247 / 353 | 0.13 | 22 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 870 | 873 | +8 | 1.47 | 319 / 401 | 318 / 400 | 0.16 | 2 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 874 | 876 | 882 | +14 | 1.54 | 346 / 443 | 345 / 443 | 0.14 | 2 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 866 | 868 | 872 | 879 | +8 | 1.92 | 357 / 491 | 357 / 490 | 0.22 | 3 | pass |
| 260 | 520 (0) | 1040 / 1040 | 0 / 0 / 0 | 870 | 884 | 889 | 898 | +24 | 2.30 | 417 / 512 | 416 / 512 | 0.26 | 2 | pass |
| 270 | 540 (0) | 1080 / 1080 | 0 / 0 / 0 | 874 | 1108 | 1155 | 1177 | +248 | 2.44 | 363 / 524 | 363 / 524 | 0.27 | 2 | pass |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 874 | 1121 | 1145 | 1169 | +261 | 2.52 | 394 / 535 | 394 / 535 | 0.27 | 2 | FAIL |

Why steps did not pass:

- 280 concurrent: p95 V2V 1121 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
