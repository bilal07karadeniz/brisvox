# Capacity: pipecat 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp

- command: `brisvox-bench capacity --url ws://server/... --floor-ms 860 --slo-over-floor-ms 60000 --label pipecat 4 vCPU / 4 GB, cloud (AMD EPYC 9655P), max ramp --note <see server> --steps 1,5,10,20,40,80,120,160,200,240,280,320,400,480,640 --fixtures-dir /opt/fixtures --resolution 10 --rounds 2 --client-workers 8 --out-md capacity-pipecat-cloud-4c-max-linux-2026-10-16-run2.md --out-json capacity-pipecat-cloud-4c-max-linux-2026-10-16-run2.json`
- brisvox 0.1.1 (release build), linux x86_64, 8 logical cores, 8 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: a server at the URL; the load generator is this process (8 tokio workers, the worker count in the line above)
- target: `ws://server/...`
- server: cloud containers (AMD EPYC 9655P, Linux), server limits 4 vCPU / 4 GB (cgroup cpu.max and memory.max, checked from the samples); the fake providers and this load generator ran in containers of their own; server CPU and memory per step from the server container's cgroup samples every 2 s (steps[].target)
- every call replays `en_two_utterances` (2 utterances) in real time; 2 call(s) per slot, slots start spread over 4000 ms; steps [1, 5, 10, 20, 40, 80, 120, 160, 200, 240, 280, 320, 400, 480, 640]; 1 warm-up call(s) first, not counted
- SLO: p95 V2V at most floor 860 ms + 60000 ms = **60860 ms**, no failed call, at most 0.0 % of turns lost or not clean; a healthy step in which more than 1.0 % of the frames went out over 100 ms late is generator limited
- CPU / memory of the server: not read by the generator (no pid); per step in the .json (steps[].target), from the server container's cgroup samples

**Result: max 10 concurrent calls within the SLO**; the next step up did not pass (first failure at 20).

| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) | p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2 (0) | 4 / 4 | 0 / 0 / 0 | 978 | 1059 | 1059 | 1059 | +199 | - | - | - | 0.01 | 8 | pass |
| 5 | 10 (0) | 20 / 20 | 0 / 0 / 0 | 1037 | 1143 | 1147 | 1147 | +283 | - | - | - | 0.01 | 18 | pass |
| 10 | 20 (0) | 40 / 40 | 0 / 0 / 0 | 1073 | 1133 | 1170 | 1170 | +273 | - | - | - | 0.02 | 14 | pass |
| 20 | 40 (0) | 73 / 80 | 5 / 2 / 0 | 2117 | 3479 | 5873 | 5873 | +2619 | - | - | - | 0.03 | 40 | FAIL |

Why steps did not pass:

- 20 concurrent: 7 of 80 turns lost or not clean (5 talked over, 2 no response, 0 bot speaking; allowed 0.0 %)

Percentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).
