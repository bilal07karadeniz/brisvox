# Capacity summary: pipecat-q-tuned-models, Linux, 2026-10-13

- pipecat-q-tuned-models (4 CPUs, 4g, Linux)
- server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-pipecat-q-tuned-models-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 20, range 15-20**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 20 | 25 | 960 | 0.99 | 49.62 | 4153 | 0.05 | 3 |
| 2 | 15 | 20 | 876 | 0.75 | 49.83 | 2966 | 0.04 | 2 |
| 3 | 20 | 25 | 844 | 0.99 | 49.67 | 4279 | 0.05 | 3 |

First failing step of each run:

- run 1: 25 calls (found): 14 of 50 calls failed; 36 of 100 turns lost or not clean (1 talked over, 7 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1902 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 20 calls (found): 4 of 40 calls failed; 8 of 80 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 25 calls (found): 15 of 50 calls failed; 35 of 100 turns lost or not clean (0 talked over, 5 no response, 0 bot speaking; allowed 0.0 %)
