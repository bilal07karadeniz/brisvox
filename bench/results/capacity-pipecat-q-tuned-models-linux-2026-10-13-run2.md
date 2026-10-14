# Capacity: pipecat-q-tuned-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 2248 --floor-ms 950 --label pipecat-q-tuned-models (4 CPUs, 4g, Linux) --note run 2 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,15,20,25,30,40 --fixtures-dir /mnt<repo>/fixtures --resolution 5 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-pipecat-q-tuned-models-linux-2026-10-13-run2.md --out-json /mnt<repo>/bench/results/capacity-pipecat-q-tuned-models-linux-2026-10-13-run2.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 2 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 15, 20, 25, 30, 40]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 2248 and its descendants every sample period

**Result: max 15 concurrent calls within the SLO**; the next step up did not pass (first failure at 20).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 788 | 809 | 809 | 809 | -141 | 0.07 | 737 / 838 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 790 | 808 | 809 | 809 | -142 | 0.25 | 838 / 1192 | 0.02 | 3 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 805 | 810 | 870 | 870 | -140 | 0.52 | 1175 / 1793 | 0.03 | 3 | pass |
| 15 | 30 (0) | 60 / 60 | 0 / 0 / 0 | 806 | 876 | 997 | 997 | -74 | 0.75 | 1793 / 2828 | 0.04 | 2 | pass |
| 20 | 40 (4) | 72 / 80 | 0 / 0 / 0 | 805 | 984 | 997 | 997 | +34 | 0.00 | 2828 / 4243 | 0.05 | 3 | FAIL |

Why steps did not pass:

- 20 concurrent: 4 of 40 calls failed; 8 of 80 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 20 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
