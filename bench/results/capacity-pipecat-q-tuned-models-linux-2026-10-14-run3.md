# Capacity: pipecat-q-tuned-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 6081 --floor-ms 950 --label pipecat-q-tuned-models (4 CPUs, 4g, Linux) --note run 3 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.82 (< 1.0); server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,15,20,25,30,40 --fixtures-dir /home/user/brisvox/fixtures --resolution 5 --rounds 2 --client-workers 8 --out-md /home/user/brisvox/bench/results/capacity-pipecat-q-tuned-models-linux-2026-10-14-run3.md --out-json /home/user/brisvox/bench/results/capacity-pipecat-q-tuned-models-linux-2026-10-14-run3.json`
- brisvox 0.1.0 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 0s: no cargo/rustc/link.exe on Windows, WSL load1 0.82 (< 1.0); server flags: -e OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1; 4 Pipecat process(es); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 15, 20, 25, 30, 40]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 950 ms + 250 ms = **1200 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 6081 and its descendants every sample period

**Result: max 15 concurrent calls within the SLO**; the next step up did not pass (first failure at 20).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 788 | 807 | 807 | 807 | -143 | 0.06 | 739 / 841 | 596 / 698 | 0.01 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 790 | 811 | 823 | 823 | -139 | 0.24 | 841 / 1183 | 698 / 1040 | 0.03 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 805 | 813 | 877 | 877 | -137 | 0.48 | 1144 / 1792 | 1001 / 1646 | 0.04 | 2 | pass |
| 15 | 30 (0) | 60 / 60 | 0 / 0 / 0 | 806 | 809 | 885 | 885 | -141 | 0.71 | 1792 / 3080 | 1645 / 2931 | 0.06 | 7 | pass |
| 20 | 40 (5) | 70 / 80 | 0 / 0 / 0 | 805 | 817 | 907 | 907 | -133 | 0.00 | 3080 / 4245 | 2931 / 4096 | 0.07 | 2 | FAIL |

Why steps did not pass:

- 20 concurrent: 5 of 40 calls failed; 10 of 80 turns lost or not clean (0 talked over, 0 no response, 0 bot speaking; allowed 0.0 %)
- 20 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 20 concurrent, failed call: a write failed: IO error: Connection reset by peer (os error 104)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
