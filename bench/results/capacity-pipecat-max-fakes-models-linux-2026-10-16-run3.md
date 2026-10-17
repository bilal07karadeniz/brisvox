# Capacity: pipecat-max-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 357570 --floor-ms 860 --slo-over-floor-ms 60000 --label pipecat-max-fakes-models (4 CPUs, 4g, Linux) --note run 3 of 3; quiet gate passed after 10s: no cargo/rustc/link.exe on Windows, WSL load1 1.41 (< 1.5); 4 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-pipecat-max-fakes-models-linux-2026-10-16-run3.md --out-json <repo>/bench/results/capacity-pipecat-max-fakes-models-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 10s: no cargo/rustc/link.exe on Windows, WSL load1 1.41 (< 1.5); 4 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 357570 and its descendants every sample period

**Result: max 10 concurrent calls within the SLO**; the next step up did not pass (first failure at 20).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 942 | 961 | 961 | 961 | +101 | 0.09 | 737 / 785 | 596 / 644 | 0.02 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 960 | 969 | 983 | 983 | +109 | 0.37 | 778 / 1219 | 638 / 1075 | 0.03 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 961 | 983 | 1031 | 1031 | +123 | 0.72 | 1219 / 1751 | 1075 / 1604 | 0.04 | 2 | pass |
| 20 | 40 (0) | 78 / 80 | 0 / 2 / 0 | 968 | 1148 | 1225 | 1225 | +288 | 1.09 | 1751 / 3774 | 1604 / 3623 | 0.06 | 2 | FAIL |

Why steps did not pass:

- 20 concurrent: 2 of 80 turns lost or not clean (0 talked over, 2 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
