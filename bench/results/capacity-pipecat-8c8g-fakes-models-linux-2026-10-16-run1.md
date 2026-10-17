# Capacity: pipecat-8c8g-fakes-models (8 CPUs, 8g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 361008 --floor-ms 860 --slo-over-floor-ms 250 --label pipecat-8c8g-fakes-models (8 CPUs, 8g, Linux) --note run 1 of 3; quiet gate passed after 90s: no cargo/rustc/link.exe on Windows, WSL load1 1.42 (< 1.5); 8 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-pipecat-8c8g-fakes-models-linux-2026-10-16-run1.md --out-json <repo>/bench/results/capacity-pipecat-8c8g-fakes-models-linux-2026-10-16-run1.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 1 of 3; quiet gate passed after 90s: no cargo/rustc/link.exe on Windows, WSL load1 1.42 (< 1.5); 8 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 250 ms = **1110 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 361008 and its descendants every sample period

**Result: max 40 concurrent calls within the SLO**; the next step up did not pass (first failure at 50).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 941 | 963 | 963 | 963 | +103 | 0.13 | 1316 / 1411 | 1035 / 1130 | 0.02 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 943 | 966 | 985 | 985 | +106 | 0.54 | 1411 / 1811 | 1130 / 1527 | 0.03 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 961 | 969 | 1036 | 1036 | +109 | 1.03 | 1811 / 2432 | 1527 / 2147 | 0.05 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 962 | 1012 | 1162 | 1162 | +152 | 1.99 | 2432 / 4331 | 2147 / 4043 | 0.07 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 968 | 1072 | 1208 | 1215 | +212 | 3.84 | 4331 / 8045 | 4043 / 7740 | 0.13 | 2 | pass |
| 50 | 100 (14) | 125 / 200 | 38 / 7 / 2 | 2091 | 7019 | 8943 | 9315 | +6159 | 0.38 | 7402 / 8297 | 7241 / 8167 | 0.15 | 2 | FAIL |
| 60 | 120 (27) | 169 / 240 | 5 / 12 / 0 | 1101 | 3773 | 5000 | 5200 | +2913 | 0.02 | 7315 / 8338 | 7164 / 8166 | 0.16 | 2 | FAIL |
| 80 | 160 (41) | 158 / 320 | 41 / 37 / 2 | 1587 | 8932 | 9852 | 9944 | +8072 | 0.00 | 8045 / 8438 | 7739 / 8164 | 0.17 | 9 | FAIL |

Why steps did not pass:

- 50 concurrent: 14 of 100 calls failed; 75 of 200 turns lost or not clean (38 talked over, 7 no response, 2 bot speaking; allowed 0.0 %); p95 V2V 7018 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 50 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 60 concurrent: 27 of 120 calls failed; 71 of 240 turns lost or not clean (5 talked over, 12 no response, 0 bot speaking; allowed 0.0 %); p95 V2V 3772 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 60 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 80 concurrent: 41 of 160 calls failed; 162 of 320 turns lost or not clean (41 talked over, 37 no response, 2 bot speaking; allowed 0.0 %); p95 V2V 8931 ms is above the limit 1110 ms (floor 860 ms + 250 ms)
- 80 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
