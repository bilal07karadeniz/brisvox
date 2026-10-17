# Capacity summary: brisvox-fakes-models, Linux, 2026-10-16

- brisvox-fakes-models (4 CPUs, 4g, Linux)
- quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.73 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-brisvox-fakes-models-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 290, range 270-300**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 300 | 310 | 1105 | 2.54 | 8.48 | 577 | 577 | 0.26 | 2 |
| 2 | 270 | 280 | 885 | 2.5 | 9.26 | 541 | 540 | 0.29 | 2 |
| 3 | 290 | 300 | 1097 | 2.57 | 8.88 | 565 | 565 | 0.3 | 2 |

First failing step of each run:

- run 1: 310 calls (found): p95 V2V 1577 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 2: 280 calls (found): p95 V2V 1346 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 3: 300 calls (found): p95 V2V 1193 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
