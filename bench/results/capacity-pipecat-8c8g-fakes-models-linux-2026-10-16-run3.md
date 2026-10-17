# Capacity: pipecat-8c8g-fakes-models (8 CPUs, 8g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 363607 --floor-ms 860 --slo-over-floor-ms 250 --label pipecat-8c8g-fakes-models (8 CPUs, 8g, Linux) --note run 3 of 3; quiet gate passed after 20s: no cargo/rustc/link.exe on Windows, WSL load1 1.41 (< 1.5); 8 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-pipecat-8c8g-fakes-models-linux-2026-10-16-run3.md --out-json <repo>/bench/results/capacity-pipecat-8c8g-fakes-models-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 20s: no cargo/rustc/link.exe on Windows, WSL load1 1.41 (< 1.5); 8 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 363607 and its descendants every sample period

**Result: max 20 concurrent calls within the SLO**; the next step up did not pass (first failure at 30).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 940 | 963 | 963 | 963 | +103 | 0.12 | 1320 / 1422 | 1037 / 1139 | 0.02 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 945 | 965 | 983 | 983 | +105 | 0.53 | 1422 / 1803 | 1139 / 1517 | 0.03 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 961 | 967 | 1032 | 1032 | +107 | 1.01 | 1803 / 2400 | 1517 / 2115 | 0.05 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 962 | 992 | 1105 | 1105 | +132 | 2.01 | 2400 / 3931 | 2114 / 3635 | 0.07 | 2 | pass |
| 30 | 60 (23) | 69 / 120 | 0 / 5 / 0 | 994 | 1806 | 2531 | 2531 | +946 | 0.00 | 7833 / 8507 | 7523 / 8199 | 0.09 | 2 | FAIL |
| 40 | 80 (0) | 157 / 160 | 0 / 3 / 0 | 970 | 1146 | 1205 | 1212 | +286 | 3.00 | 3931 / 7833 | 3635 / 7524 | 0.11 | 2 | FAIL |

Why steps did not pass:

- 30 concurrent: 23 of 60 calls failed; 51 of 120 turns lost or not clean (0 talked over, 5 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1805 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 30 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 40 concurrent: 3 of 160 turns lost or not clean (0 talked over, 3 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 1146 ms is above the limit 1110 ms (floor 860 ms + 250 ms)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
