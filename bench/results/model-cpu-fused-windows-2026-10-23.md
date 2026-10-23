# Model CPU per call

- command: `brisvox-bench model-cpu --models models --configs 1:1:original,1:1:fused,2:1:original,2:1:fused --rounds 30 --batch 20 --out bench/results/model-cpu-fused-windows-2026-10-23.md`
- brisvox 0.1.1 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- single measuring thread; CPU time is the whole process's (ONNX Runtime workers included), configurations interleaved round by round
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- plan: 30 rounds x 20 analyses per configuration and mode (paced: 40 ms sleep after each), Silero 2000 windows per batch

## Smart Turn, back to back (CPU of the work itself)

| intra-op threads | spinning | graph | CPU mean (ms) | CPU p50 / p99 of batch means (ms) | wall p50 / p99 (ms) |
|---:|:---:|:---:|---:|---:|---:|
| 1 | on | original | 28.412 | 28.150 / 32.050 | 27.824 / 33.197 |
| 1 | on | fused | 27.470 | 27.350 / 31.250 | 27.101 / 31.791 |
| 2 | on | original | 32.688 | 32.050 / 37.500 | 15.612 / 20.912 |
| 2 | on | fused | 31.873 | 31.250 / 35.950 | 15.144 / 20.563 |

## Smart Turn, paced (CPU includes what idle workers burn between analyses)

| intra-op threads | spinning | graph | CPU mean (ms) | CPU p50 / p99 of batch means (ms) | wall p50 / p99 (ms) |
|---:|:---:|:---:|---:|---:|---:|
| 1 | on | original | 28.855 | 28.740 / 34.240 | 28.505 / 38.295 |
| 1 | on | fused | 28.068 | 27.940 / 36.590 | 27.300 / 37.431 |
| 2 | on | original | 58.537 | 57.640 / 64.690 | 16.988 / 21.922 |
| 2 | on | fused | 57.395 | 56.840 / 66.990 | 16.343 / 21.261 |

Control, subtracted from every paced batch above: a quiet period of 20 analysis slots with every session loaded (and two CPU readings) costs 3.200 ms (160.0 us per slot).

## Exactness on the 12 golden cases (vs the first configuration)

| intra-op threads | spinning | graph | probabilities differing in bits | worst abs error vs reference | verdicts differing from reference |
|---:|:---:|:---:|---:|---:|---:|
| 1 | on | original | 0 | 5.96e-8 | 0 |
| 1 | on | fused | 3 | 9.63e-2 | 0 |
| 2 | on | original | 0 | 5.96e-8 | 0 |
| 2 | on | fused | 3 | 9.63e-2 | 0 |

## Silero VAD, one 32 ms window

| backend | CPU mean (us) | CPU p50 / p99 of batch means (us) | wall p50 / p99 (us) |
|---|---:|---:|---:|
| tract | 52.6 | 54.5 / 55.0 | 46.3 / 68.8 |
| ONNX Runtime | 87.6 | 86.0 / 94.0 | 80.2 / 124.3 |

Backends on 872 fixture windows: max |p_tract - p_ort| = 1.19e-6, 0 windows where the 0.5 decision differs.
