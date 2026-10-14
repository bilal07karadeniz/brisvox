# Model CPU per call

- command: `brisvox-bench model-cpu --models models --configs 2:1,1:1,2:0,3:0,4:0,1:0 --rounds 30 --batch 20 --gap-ms 40 --out bench/results/model-cpu-windows-2026-10-13-run1.md`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- single measuring thread; CPU time is the whole process's (ONNX Runtime workers included), configurations interleaved round by round
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- plan: 30 rounds x 20 analyses per configuration and mode (paced: 40 ms sleep after each), Silero 2000 windows per batch

## Smart Turn, back to back (CPU of the work itself)

| intra-op threads | spinning | CPU mean (ms) | CPU p50 / p99 of batch means (ms) | wall p50 / p99 (ms) |
|---:|:---:|---:|---:|---:|
| 2 | on | 41.253 | 40.650 / 48.400 | 19.266 / 25.562 |
| 1 | on | 34.320 | 33.600 / 40.650 | 33.096 / 46.635 |
| 2 | off | 40.492 | 39.850 / 45.300 | 20.736 / 26.579 |
| 3 | off | 46.907 | 46.100 / 71.900 | 17.728 / 39.305 |
| 4 | off | 52.685 | 51.550 / 82.850 | 16.010 / 28.446 |
| 1 | off | 34.505 | 34.350 / 41.400 | 33.449 / 45.281 |

## Smart Turn, paced (CPU includes what idle workers burn between analyses)

| intra-op threads | spinning | CPU mean (ms) | CPU p50 / p99 of batch means (ms) | wall p50 / p99 (ms) |
|---:|:---:|---:|---:|---:|
| 2 | on | 72.270 | 71.820 / 81.170 | 20.292 / 31.952 |
| 1 | on | 35.105 | 34.270 / 46.020 | 33.596 / 53.208 |
| 2 | off | 43.357 | 42.070 / 72.570 | 22.187 / 42.575 |
| 3 | off | 49.145 | 47.570 / 78.020 | 18.753 / 35.442 |
| 4 | off | 53.855 | 52.220 / 72.570 | 16.882 / 31.205 |
| 1 | off | 34.188 | 33.520 / 43.670 | 33.455 / 44.141 |

Control, subtracted from every paced batch above: a quiet period of 20 analysis slots with every session loaded (and two CPU readings) costs 1.600 ms (80.0 us per slot).

## Exactness on the 12 golden cases (vs the first configuration)

| intra-op threads | spinning | probabilities differing in bits | worst abs error vs reference | verdicts differing from reference |
|---:|:---:|---:|---:|---:|
| 2 | on | 0 | 5.96e-8 | 0 |
| 1 | on | 0 | 5.96e-8 | 0 |
| 2 | off | 0 | 5.96e-8 | 0 |
| 3 | off | 0 | 5.96e-8 | 0 |
| 4 | off | 0 | 5.96e-8 | 0 |
| 1 | off | 0 | 5.96e-8 | 0 |

## Silero VAD, one 32 ms window

| backend | CPU mean (us) | CPU p50 / p99 of batch means (us) | wall p50 / p99 (us) |
|---|---:|---:|---:|
| tract | 61.2 | 62.5 / 70.5 | 53.2 / 84.8 |
| ONNX Runtime | 105.5 | 101.5 / 125.0 | 94.2 / 172.0 |

Backends on 872 fixture windows: max |p_tract - p_ort| = 1.19e-6, 0 windows where the 0.5 decision differs.
