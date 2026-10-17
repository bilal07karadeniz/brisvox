# Capacity: pipecat 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 60000 --label pipecat 8 vCPU / 8 GB, cloud (AMD EPYC 9655P), max ramp --note <see server> --steps 1,5,10,20,40,80,120,160,240,320,400,480,560,640,800,1000,1200 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-pipecat-cloud-8c-max-linux-2026-10-16-run2.md --out-json capacity-pipecat-cloud-8c-max-linux-2026-10-16-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 8 vCPU / 8 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 240, 320, 400, 480, 560, 640, 800, 1000, 1200]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 20 concurrent calls within the SLO**; the next step up did not pass (first failure at 30).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 993 | 1001 | 1001 | 1001 | +141 | - | - | - | 0.00 | 2 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 985 | 1046 | 1111 | 1111 | +186 | - | - | - | 0.01 | 6 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 1006 | 1085 | 1122 | 1122 | +225 | - | - | - | 0.02 | 4 | pass |
| 20 | 40 (0) | 80 / 80 | 0 / 0 / 0 | 1068 | 1268 | 1439 | 1439 | +408 | - | - | - | 0.03 | 8 | pass |
| 30 | 60 (2) | 106 / 120 | 7 / 3 / 0 | 2157 | 5179 | 5883 | 6283 | +4319 | - | - | - | 0.04 | 6 | FAIL |
| 40 | 80 (6) | 122 / 160 | 19 / 5 / 2 | 1743 | 3889 | 5800 | 6753 | +3029 | - | - | - | 0.04 | 4 | FAIL |

Why steps did not pass:

- 30 concurrent: 2 of 60 calls failed; 14 of 120 turns lost or not clean (7 talked over, 3 no response, 0 bot speaking; allowed 0.0 %)
- 30 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)
- 40 concurrent: 6 of 80 calls failed; 38 of 160 turns lost or not clean (19 talked over, 5 no response, 2 bot speaking; allowed 0.0 %)
- 40 concurrent, failed call: the connection broke: IO error: Connection reset by peer (os error 104)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
