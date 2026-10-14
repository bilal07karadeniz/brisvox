# LiveKit server (SFU) cost in the LiveKit capacity runs, 2026-10-14

The SFU is `livekit/livekit-server:v1.13.9` in a container of its own (cores 12-15, no CPU or memory
limit) and is not counted in the 4-CPU / 4 GiB budget of the agent container. CPU is the container
cgroup's `usage_usec` over the step; RSS is the sum of `VmRSS` of its processes (one process).
Source: the `sfu` block of every step in `capacity-livekit-models-linux-2026-10-14-run<N>.json` (tuned variant).

At the step each run reports as its capacity:

| run | calls | SFU CPU (cores) | SFU CPU per call (mcores) | SFU RSS peak (MB) | agent CPU (cores) |
|---:|---:|---:|---:|---:|---:|
| 1 | 22 | 0.44 | 20 | 276 | 2.09 |
| 2 | 23 | 0.44 | 19 | 277 | 2.08 |
| 3 | 25 | 0.47 | 19 | 294 | 2.22 |

Over the steps of run 1 (the SFU scales linearly with the calls):

| calls | SFU CPU (cores) | SFU RSS peak (MB) |
|---:|---:|---:|
| 1 | 0.03 | 70 |
| 5 | 0.11 | 101 |
| 10 | 0.20 | 153 |
| 15 | 0.29 | 193 |
| 20 | 0.38 | 245 |
| 22 | 0.44 | 276 |
| 25 | 0.46 | 276 |

The SFU forwards one audio track each way per call (Opus, 2 participants per room) and is about a
fifth of the agent's CPU per call. The server logged one warning in every run: the kernel's UDP
receive buffer is below its recommended size (not changed, see `bench/livekit_baseline/README.md`).
