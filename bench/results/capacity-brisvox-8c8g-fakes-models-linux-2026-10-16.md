# Capacity summary: brisvox-8c8g-fakes-models, Linux, 2026-10-16

- brisvox-8c8g-fakes-models (8 CPUs, 8g, Linux)
- quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 1.12 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-brisvox-8c8g-fakes-models-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 440, range 430-440**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 440 | 450 | 1051 | 4.64 | 10.53 | 875 | 876 | 0.41 | 3 |
| 2 | 440 | 450 | 1101 | 4.8 | 10.91 | 874 | 874 | 0.41 | 3 |
| 3 | 430 | 440 | 1001 | 4.68 | 10.89 | 860 | 861 | 0.42 | 3 |

First failing step of each run:

- run 1: 450 calls (found): p95 V2V 1273 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 2: 450 calls (found): p95 V2V 1199 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 3: 440 calls (found): p95 V2V 1151 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
