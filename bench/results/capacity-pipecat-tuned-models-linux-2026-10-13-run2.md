# Capacity: pipecat-tuned-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 3985 --floor-ms 950 --label pipecat-tuned-models (4 CPUs, 4g, Linux) --note run 2 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,30,40,50,60,70,80,100 --fixtures-dir /mnt<repo>/fixtures --resolution 5 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-pipecat-tuned-models-linux-2026-10-13-run2.md --out-json /mnt<repo>/bench/results/capacity-pipecat-tuned-models-linux-2026-10-13-run2.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 30, 40, 50, 60, 70, 80, 100]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 3985 and its descendants every sample period

**Result: max 20 concurrent calls within the SLO**; the next step up did not pass (first failure at 25).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 789 | 813 | 813 | 813 | -137 | 0.12 | 737 / 839 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 795 | 809 | 809 | 809 | -141 | 0.40 | 839 / 1389 | 0.03 | 4 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 806 | 913 | 967 | 967 | -37 | 0.59 | 1389 / 1953 | 0.04 | 4 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 806 | 811 | 943 | 943 | -139 | 1.19 | 1953 / 3475 | 0.06 | 3 | pass |
| 25 | 50 (9) | 78 / 100 | 0 / 4 / 0 | 809 | 3898 | 4460 | 4460 | +2948 | 0.00 | 3715 / 4244 | 0.08 | 5 | FAIL |
| 30 | 60 (11) | 90 / 120 | 0 / 8 / 0 | 807 | 933 | 1795 | 1795 | -17 | 0.02 | 3475 / 4260 | 0.08 | 4 | FAIL |

Why steps did not pass:

- 25 concurrent: 9 of 50 calls failed; 22 of 100 turns lost or not clean (0 talked over, 4 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3898 ms is above the limit 1200 ms (floor 950 ms + 250 ms)
- 25 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 30 concurrent: 11 of 60 calls failed; 30 of 120 turns lost or not clean (0 talked over, 8 no response, 0 bot speaking; allowed 0.0 %)
- 30 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
