# Capacity summary: brisvox-8c8g-max-fakes-models, Linux, 2026-10-16

- brisvox-8c8g-max-fakes-models (8 CPUs, 8g, Linux)
- quiet gate passed after 15s: no cargo/rustc/link.exe on Windows, WSL load1 1.48 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-brisvox-8c8g-max-fakes-models-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 750, range 740-812**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 740 | 750 | 3660 | 6.26 | 8.45 | 1242 | 1243 | 0.57 | 2 |
| 2 | 812 | 818 | 3705 | 6.25 | 7.69 | 1314 | 1315 | 0.59 | 3 |
| 3 | 750 | 760 | 3658 | 6.28 | 8.37 | 1256 | 1257 | 0.56 | 3 |

First failing step of each run:

- run 1: 750 calls (found): 15 of 3000 turns lost or not clean (15 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 818 calls (found): 2 of 3272 turns lost or not clean (2 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 760 calls (found): 34 of 3040 turns lost or not clean (34 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
