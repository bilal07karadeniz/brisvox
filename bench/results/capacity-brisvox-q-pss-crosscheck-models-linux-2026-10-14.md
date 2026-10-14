# Capacity summary: brisvox-q-pss-crosscheck-models, Linux, 2026-10-14

- brisvox-q-pss-crosscheck-models (4 CPUs, 4g, Linux)
- quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.62 (< 1.0); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 1 run(s), per-run files `capacity-brisvox-q-pss-crosscheck-models-linux-2026-10-14-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 350, range 350-350**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 350 | 375 | 1043 | 2.52 | 7.2 | 432 | 429 | 0.31 | 8 |

First failing step of each run:

- run 1: 375 calls (found): p95 V2V 1405 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
