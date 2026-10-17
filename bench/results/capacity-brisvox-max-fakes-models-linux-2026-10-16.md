# Capacity summary: brisvox-max-fakes-models, Linux, 2026-10-16

- brisvox-max-fakes-models (4 CPUs, 4g, Linux)
- quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.76 (< 1.5); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); brisvox-server, admission control off; Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-brisvox-max-fakes-models-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 430, range 380-500**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 430 | 440 | 3651 | 3.15 | 7.33 | 733 | 732 | 0.4 | 2 |
| 2 | 380 | 390 | 3639 | 3.14 | 8.27 | 661 | 661 | 0.38 | 2 |
| 3 | 500 | 510 | 3656 | 3.23 | 6.46 | 812 | 812 | 0.41 | 2 |

First failing step of each run:

- run 1: 440 calls (found): 4 of 1760 turns lost or not clean (4 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 390 calls (found): 14 of 1560 turns lost or not clean (14 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 510 calls (found): 31 of 2040 turns lost or not clean (31 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
