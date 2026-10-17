# Capacity: pipecat-max-fakes-models (4 CPUs, 4g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 356034 --floor-ms 860 --slo-over-floor-ms 60000 --label pipecat-max-fakes-models (4 CPUs, 4g, Linux) --note run 1 of 3; quiet gate passed after 80s: no cargo/rustc/link.exe on Windows, WSL load1 1.48 (< 1.5); 4 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-pipecat-max-fakes-models-linux-2026-10-16-run1.md --out-json <repo>/bench/results/capacity-pipecat-max-fakes-models-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; quiet gate passed after 80s: no cargo/rustc/link.exe on Windows, WSL load1 1.48 (< 1.5); 4 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 12-15, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 --network host, server cores 0-3, generator cores 4-11 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 356034 and its descendants every sample period

**Result: max 20 concurrent calls within the SLO**; the next step up did not pass (first failure at 30).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 942 | 961 | 961 | 961 | +101 | 0.09 | 741 / 803 | 603 / 664 | 0.01 | 1 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 955 | 964 | 983 | 983 | +104 | 0.38 | 803 / 1200 | 664 / 1058 | 0.03 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 961 | 1056 | 1150 | 1150 | +196 | 0.70 | 1200 / 2040 | 1058 / 1899 | 0.04 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 963 | 1113 | 1215 | 1215 | +253 | 1.39 | 2040 / 3736 | 1899 / 3587 | 0.07 | 2 | pass |
| 30 | 60 (11) | 73 / 120 | 21 / 3 / 1 | 1012 | 7552 | 8282 | 8282 | +6692 | 0.20 | 3399 / 4225 | 3271 / 4099 | 0.08 | 2 | FAIL |
| 40 | 80 (17) | 109 / 160 | 12 / 5 / 0 | 1134 | 5758 | 6069 | 6226 | +4898 | 0.00 | 3736 / 4214 | 3587 / 4097 | 0.08 | 2 | FAIL |

Why steps did not pass:

- 30 concurrent: 11 of 60 calls failed; 47 of 120 turns lost or not clean (21 talked over, 3 no response, 1 bot speaking; allowed 0.0 %)
- 30 concurrent, failed call: call: before started: WebSocket protocol error: Connection reset without closing handshake
- 30 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 40 concurrent: 17 of 80 calls failed; 51 of 160 turns lost or not clean (12 talked over, 5 no response, 0 bot speaking; allowed 0.0 %)
- 40 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
