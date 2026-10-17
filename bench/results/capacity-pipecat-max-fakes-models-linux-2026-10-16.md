# Capacity summary: pipecat-max-fakes-models, Linux, 2026-10-16

- pipecat-max-fakes-models (4 CPUs, 4g, Linux)
- quiet gate passed after 80s: no cargo/rustc/link.exe on Windows, WSL load1 1.48 (< 1.5); 4 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-pipecat-max-fakes-models-linux-2026-10-16-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 20, range 10-20**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 20 | 30 | 1113 | 1.39 | 69.28 | 3918 | 3761 | 0.07 | 2 |
| 2 | 20 | 30 | 990 | 1.37 | 68.33 | 3943 | 3793 | 0.06 | 2 |
| 3 | 10 | 20 | 983 | 0.72 | 72.46 | 1837 | 1682 | 0.04 | 2 |

First failing step of each run:

- run 1: 30 calls (found): 11 of 60 calls failed; 47 of 120 turns lost or not clean (21 talked over, 3 no response, 1 bot speaking; allowed 0.0 %)
- run 2: 30 calls (found): 10 of 60 calls failed; 21 of 120 turns lost or not clean (0 talked over, 1 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 20 calls (found): 2 of 80 turns lost or not clean (0 talked over, 2 no response, 0 bot speaking; allowed 0.0 %)
