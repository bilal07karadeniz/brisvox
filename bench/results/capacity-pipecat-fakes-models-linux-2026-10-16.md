# Capacity summary: pipecat-fakes-models, Linux, 2026-10-16

- pipecat-fakes-models (4 CPUs, 4g, Linux)
- quiet gate passed after 70s: no cargo/rustc/link.exe on Windows, WSL load1 1.46 (< 1.5); 4 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-pipecat-fakes-models-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 20, range 5-20**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 20 | 30 | 1003 | 1.43 | 71.43 | 3741 | 3588 | 0.07 | 2 |
| 2 | 20 | 30 | 1048 | 1.4 | 70.1 | 3974 | 3819 | 0.07 | 2 |
| 3 | 5 | 10 | 985 | 0.4 | 79.19 | 1348 | 1195 | 0.03 | 2 |

First failing step of each run:

- run 1: 30 calls (found): 11 of 60 calls failed; 33 of 120 turns lost or not clean (4 talked over, 7 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 6113 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 2: 30 calls (found): 14 of 60 calls failed; 31 of 120 turns lost or not clean (1 talked over, 2 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1994 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- run 3: 10 calls (found): p95 V2V 1144 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
