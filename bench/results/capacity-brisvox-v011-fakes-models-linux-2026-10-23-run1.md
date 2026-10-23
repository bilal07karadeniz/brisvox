# Capacity: brisvox-v011-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 13962 --floor-ms 860 --slo-over-floor-ms 250 --label brisvox-v011-fakes-models (4 CPUs, 4g, Linux) --note run 1 of 1; quiet gate passed after 35s: no cargo/rustc/link.exe on Windows, WSL load1 1.48 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /mnt/c/Projelerim/brisvox-regress/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md /mnt/c/Projelerim/brisvox-regress/bench/results/capacity-brisvox-v011-fakes-models-linux-2026-10-23-run1.md --out-json /mnt/c/Projelerim/brisvox-regress/bench/results/capacity-brisvox-v011-fakes-models-linux-2026-10-23-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 1; quiet gate passed after 35s: no cargo/rustc/link.exe on Windows, WSL load1 1.48 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 13962 and its descendants every sample period

**Result: max 250 concurrent calls within the SLO**; the next step up did not pass (first failure at 260).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 865 | 867 | 867 | 867 | +7 | 0.03 | 166 / 175 | 166 / 174 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 866 | 868 | 869 | 869 | +8 | 0.09 | 175 / 191 | 174 / 191 | 0.02 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 866 | 867 | 868 | 868 | +7 | 0.16 | 191 / 199 | 190 / 198 | 0.03 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 866 | 867 | 867 | 867 | +7 | 0.27 | 198 / 208 | 197 / 208 | 0.04 | 3 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 866 | 868 | 868 | 868 | +8 | 0.48 | 207 / 227 | 208 / 227 | 0.05 | 2 | pass |
| 80 | 160 (0) | 320 / 320 | 0 / 0 / 0 | 866 | 868 | 869 | 870 | +8 | 0.98 | 227 / 274 | 226 / 273 | 0.10 | 2 | pass |
| 120 | 240 (0) | 480 / 480 | 0 / 0 / 0 | 866 | 867 | 868 | 869 | +7 | 1.24 | 249 / 327 | 248 / 327 | 0.12 | 2 | pass |
| 160 | 320 (0) | 640 / 640 | 0 / 0 / 0 | 866 | 868 | 869 | 873 | +8 | 1.55 | 279 / 370 | 279 / 370 | 0.18 | 9 | pass |
| 200 | 400 (0) | 800 / 800 | 0 / 0 / 0 | 868 | 875 | 878 | 882 | +15 | 1.65 | 286 / 443 | 286 / 443 | 0.18 | 3 | pass |
| 240 | 480 (0) | 960 / 960 | 0 / 0 / 0 | 871 | 932 | 963 | 993 | +72 | 2.32 | 340 / 486 | 340 / 486 | 0.27 | 2 | pass |
| 250 | 500 (0) | 1000 / 1000 | 0 / 0 / 0 | 874 | 1009 | 1054 | 1070 | +149 | 2.38 | 370 / 506 | 370 / 506 | 0.27 | 3 | pass |
| 260 | 520 (0) | 1040 / 1040 | 0 / 0 / 0 | 884 | 1175 | 1217 | 1232 | +315 | 2.53 | 367 / 512 | 367 / 512 | 0.31 | 2 | FAIL |
| 280 | 560 (0) | 1120 / 1120 | 0 / 0 / 0 | 883 | 1707 | 1770 | 1805 | +847 | 2.68 | 381 / 539 | 381 / 539 | 0.30 | 2 | FAIL |

Why steps did not pass:

- 260 concurrent: p95 V2V 1175 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 280 concurrent: p95 V2V 1707 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
