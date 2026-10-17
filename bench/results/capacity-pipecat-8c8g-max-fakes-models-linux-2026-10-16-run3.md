# Capacity: pipecat-8c8g-max-fakes-models (8 CPUs, 8g, Linux)

- command: `brisvox-bench capacity --url ws://127.0.0.1:8080 --pid 369887 --floor-ms 860 --slo-over-floor-ms 60000 --label pipecat-8c8g-max-fakes-models (8 CPUs, 8g, Linux) --note run 3 of 3; quiet gate passed after 50s: no cargo/rustc/link.exe on Windows, WSL load1 1.42 (< 1.5); 8 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh) --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir <repo>/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md <repo>/bench/results/capacity-pipecat-8c8g-max-fakes-models-linux-2026-10-16-run3.md --out-json <repo>/bench/results/capacity-pipecat-8c8g-max-fakes-models-linux-2026-10-16-run3.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://127.0.0.1:8080`
- server: run 3 of 3; quiet gate passed after 50s: no cargo/rustc/link.exe on Windows, WSL load1 1.42 (< 1.5); 8 Pipecat process(es); real provider clients against bench/fake_providers over TLS (cores 16-19, outside the limits); Linux kernel 6.6.87.2-microsoft-standard-WSL2; container --cpus=8 --cpuset-cpus=0-7 --memory=8g --memory-swap=8g --ulimit nofile=1048576:1048576 --network host, server cores 0-7, generator cores 8-15 (bench/capacity_linux.sh)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / RSS / PSS: read from pid 369887 and its descendants every sample period

**Result: max 40 concurrent calls within the SLO**; the next step up did not pass (first failure at 50).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 941 | 985 | 985 | 985 | +125 | 0.13 | 1318 / 1433 | 1035 / 1147 | 0.02 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 944 | 964 | 964 | 964 | +104 | 0.54 | 1433 / 1843 | 1146 / 1557 | 0.03 | 2 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 961 | 982 | 1058 | 1058 | +122 | 1.03 | 1843 / 2376 | 1557 / 2087 | 0.05 | 2 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 962 | 975 | 1154 | 1154 | +115 | 2.02 | 2376 / 4182 | 2087 / 3891 | 0.08 | 2 | pass |
| 40 | 80 (0) | 160 / 160 | 0 / 0 / 0 | 969 | 1134 | 1265 | 1349 | +274 | 3.72 | 4182 / 7822 | 3891 / 7517 | 0.13 | 2 | pass |
| 50 | 100 (11) | 173 / 200 | 0 / 5 / 0 | 974 | 1218 | 1520 | 1528 | +358 | 0.00 | 6453 / 8485 | 6149 / 8176 | 0.14 | 2 | FAIL |
| 60 | 120 (16) | 197 / 240 | 2 / 9 / 0 | 1021 | 2161 | 5791 | 5967 | +1301 | 0.00 | 6785 / 8422 | 6480 / 8113 | 0.15 | 2 | FAIL |
| 80 | 160 (38) | 223 / 320 | 9 / 12 / 0 | 1039 | 2002 | 2794 | 6076 | +1142 | 0.00 | 7822 / 8506 | 7515 / 8172 | 0.15 | 2 | FAIL |

Why steps did not pass:

- 50 concurrent: 11 of 100 calls failed; 27 of 200 turns lost or not clean (0 talked over, 5 no response, 0 bot speaking; allowed 0.0 %)
- 50 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 60 concurrent: 16 of 120 calls failed; 43 of 240 turns lost or not clean (2 talked over, 9 no response, 0 bot speaking; allowed 0.0 %)
- 60 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 60 concurrent, failed call: a write failed: IO error: Connection reset by peer (os error 104)
- 80 concurrent: 38 of 160 calls failed; 97 of 320 turns lost or not clean (9 talked over, 12 no response, 0 bot speaking; allowed 0.0 %)
- 80 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 80 concurrent, failed call: call: before started: IO error: Connection reset by peer (os error 104)
- 80 concurrent, failed call: call: before started: WebSocket protocol error: Connection reset without closing handshake

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
