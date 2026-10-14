# Capacity: pipecat-q-tuned-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 2662 --floor-ms 950 --label pipecat-q-tuned-models (4 CPUs, 4g, Linux) --note run 3 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,15,20,25,30,40 --fixtures-dir /mnt<repo>/fixtures --resolution 5 --rounds 2 --client-workers 8 --out-md /mnt<repo>/bench/results/capacity-pipecat-q-tuned-models-linux-2026-10-13-run3.md --out-json /mnt<repo>/bench/results/capacity-pipecat-q-tuned-models-linux-2026-10-13-run3.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 15, 20, 25, 30, 40]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS: read from pid 2662 and its descendants every sample period

**Result: max 20 concurrent calls within the SLO**; the next step up did not pass (first failure at 25).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 788 | 807 | 807 | 807 | -143 | 0.06 | 737 / 830 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 788 | 807 | 808 | 808 | -143 | 0.25 | 830 / 1166 | 0.02 | 4 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 806 | 872 | 926 | 926 | -78 | 0.49 | 1132 / 2010 | 0.03 | 2 | pass |
| 15 | 30 (0) | 60 / 60 | 0 / 0 / 0 | 805 | 809 | 839 | 839 | -141 | 0.72 | 2010 / 2405 | 0.04 | 4 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 806 | 844 | 918 | 918 | -106 | 0.99 | 2405 / 4080 | 0.05 | 3 | pass |
| 25 | 50 (15) | 65 / 100 | 0 / 5 / 0 | 804 | 857 | 858 | 858 | -93 | 0.00 | 4080 / 4231 | 0.05 | 4 | FAIL |

Why steps did not pass:

- 25 concurrent: 15 of 50 calls failed; 35 of 100 turns lost or not clean (0 talked over, 5 no response, 0 bot speaking; allowed 0.0 %)
- 25 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 25 concurrent, failed call: call: before started: WebSocket protocol error: Connection reset without closing handshake

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
