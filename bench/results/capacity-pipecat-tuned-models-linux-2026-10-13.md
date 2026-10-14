# Capacity summary: pipecat-tuned-models, Linux, 2026-10-13

- pipecat-tuned-models (4 CPUs, 4g, Linux)
- server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-pipecat-tuned-models-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 20, range 5-20**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 20 | 25 | 912 | 1.4 | 69.98 | 3657 | 0.08 | 4 |
| 2 | 20 | 25 | 811 | 1.19 | 59.69 | 3644 | 0.06 | 3 |
| 3 | 5 | 10 | 809 | 0.32 | 63.38 | 1462 | 0.02 | 3 |

First failing step of each run:

- run 1: 25 calls (found): 10 of 50 calls failed; 39 of 100 turns lost or not clean (2 talked over, 17 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 2576 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 25 calls (found): 9 of 50 calls failed; 22 of 100 turns lost or not clean (0 talked over, 4 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3898 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 10 calls (found): 1 of 40 turns lost or not clean (0 talked over, 1 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1641 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
