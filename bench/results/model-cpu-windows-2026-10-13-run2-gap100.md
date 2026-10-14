# Model CPU per call

- command: `brisvox-bench model-cpu --models models --configs 2:1,2:0,1:0 --rounds 30 --batch 20 --gap-ms 100 --silero-batch 2000 --out bench/results/model-cpu-windows-2026-10-13-run2-gap100.md`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- single measuring thread; CPU time is the whole process's (ONNX Runtime workers included), configurations interleaved round by round
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- plan: 30 rounds x 20 analyses per configuration and mode (paced: 100 ms sleep after each), Silero 2000 windows per batch

## Smart Turn, back to back (CPU of the work itself)

| intra-op threads | spinning | CPU mean (ms) | CPU p50 / p99 of batch means (ms) | wall p50 / p99 (ms) |
|---:|:---:|---:|---:|---:|
| 2 | on | 39.482 | 39.100 / 43.000 | 18.661 / 23.828 |
| 2 | off | 38.745 | 39.050 / 42.950 | 19.879 / 24.189 |
| 1 | off | 32.495 | 32.800 / 35.150 | 31.968 / 37.724 |

## Smart Turn, paced (CPU includes what idle workers burn between analyses)

| intra-op threads | spinning | CPU mean (ms) | CPU p50 / p99 of batch means (ms) | wall p50 / p99 (ms) |
|---:|:---:|---:|---:|---:|
| 2 | on | 71.098 | 71.025 / 75.675 | 19.983 / 27.920 |
| 2 | off | 40.632 | 40.575 / 46.775 | 21.092 / 28.565 |
| 1 | off | 32.265 | 32.725 / 34.325 | 32.161 / 40.315 |

Control, subtracted from every paced batch above: a quiet period of 20 analysis slots with every session loaded (and two CPU readings) costs 1.500 ms (75.0 us per slot).

## Exactness on the 12 golden cases (vs the first configuration)

| intra-op threads | spinning | probabilities differing in bits | worst abs error vs reference | verdicts differing from reference |
|---:|:---:|---:|---:|---:|
| 2 | on | 0 | 5.96e-8 | 0 |
| 2 | off | 0 | 5.96e-8 | 0 |
| 1 | off | 0 | 5.96e-8 | 0 |

## Silero VAD, one 32 ms window

| backend | CPU mean (us) | CPU p50 / p99 of batch means (us) | wall p50 / p99 (us) |
|---|---:|---:|---:|
| tract | 59.1 | 62.5 / 62.5 | 52.9 / 81.4 |
| ONNX Runtime | 100.3 | 101.5 / 109.5 | 93.5 / 143.8 |

Backends on 872 fixture windows: max |p_tract - p_ort| = 1.19e-6, 0 windows where the 0.5 decision differs.
