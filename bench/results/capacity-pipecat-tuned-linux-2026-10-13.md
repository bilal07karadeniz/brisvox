# Capacity summary: pipecat-tuned, Linux, 2026-10-13

- pipecat-tuned (4 CPUs, 4g, Linux)
- server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-pipecat-tuned-linux-2026-10-13-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 90, range 70-90**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 70 | 80 | 1036 | 2.03 | 28.99 | 1574 | 0.12 | 3 |
| 2 | 90 | 100 | 1060 | 2.45 | 27.27 | 1885 | 0.16 | 4 |
| 3 | 90 | 100 | 1039 | 2.41 | 26.83 | 1862 | 0.15 | 4 |

First failing step of each run:

- run 1: 80 calls (found): 9 of 320 turns lost or not clean (9 talked over, 0 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 2613 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 2: 100 calls (found): 70 of 400 turns lost or not clean (65 talked over, 5 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 9381 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- run 3: 100 calls (found): 163 of 400 turns lost or not clean (0 talked over, 163 no response, 0 bot speaking; allowed 0.0 %)
