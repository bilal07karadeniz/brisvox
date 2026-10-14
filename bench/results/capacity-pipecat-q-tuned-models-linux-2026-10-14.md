# Capacity summary: pipecat-q-tuned-models, Linux, 2026-10-14

- pipecat-q-tuned-models (4 CPUs, 4g, Linux)
- quiet gate passed after 80s: no cargo/rustc/link.exe on Windows, WSL load1 0.15 (< 1.0); server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- 3 run(s), per-run files `capacity-pipecat-q-tuned-models-linux-2026-10-14-run<N>.{md,json}`; SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn
- **capacity (max concurrent calls within the SLO): median 20, range 15-20**

| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 20 | 25 | 940 | 0.92 | 45.94 | 4344 | 4190 | 0.07 | 5 |
| 2 | 20 | 25 | 817 | 0.91 | 45.64 | 4136 | 3981 | 0.07 | 2 |
| 3 | 15 | 20 | 809 | 0.71 | 47.15 | 3230 | 3073 | 0.06 | 7 |

First failing step of each run:

- run 1: 25 calls (found): 8 of 50 calls failed; 17 of 100 turns lost or not clean (0 talked over, 1 no response, 0 bot speaking; allowed 0.0 %)
- run 2: 25 calls (found): 16 of 50 calls failed; 34 of 100 turns lost or not clean (0 talked over, 2 no response, 0 bot speaking; allowed 0.0 %)
- run 3: 20 calls (found): 5 of 40 calls failed; 10 of 80 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
