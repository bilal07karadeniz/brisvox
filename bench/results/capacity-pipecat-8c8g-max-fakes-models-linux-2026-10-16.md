# Capacity summary: pipecat-8c8g-max-fakes-models, Linux, 2026-10-16

- pipecat-8c8g-max-fakes-models (8 CPUs, 8g, Linux)
- quiet gate passed after 115s: no cargo/rustc/link.exe on Windows, WSL load1 1.47 (< 1.5); 8 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-pipecat-8c8g-max-fakes-models-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 20, range 20-40**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 20 | 30 | 1036 | 1.99 | 99.7 | 4721 | 4414 | 0.07 | 2 |
| 2 | 20 | 30 | 1127 | 1.97 | 98.53 | 4478 | 4173 | 0.07 | 2 |
| 3 | 40 | 50 | 1134 | 3.72 | 92.97 | 8202 | 7882 | 0.13 | 2 |

First failing step of each run:

- run 1: 30 calls (found): 7 of 60 calls failed; 19 of 120 turns lost or not clean (0 talked over, 5 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 30 calls (found): 16 of 60 calls failed; 35 of 120 turns lost or not clean (0 talked over, 3 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 50 calls (found): 11 of 100 calls failed; 27 of 200 turns lost or not clean (0 talked over, 5 no response, 0 bot speaking; allowed 0.0 %)
