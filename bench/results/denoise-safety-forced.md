# Noise suppression: clean speech must survive

- command: `brisvox-bench denoise safety --force`
- brisvox 0.1.2 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- backends measured on every path, the engine's refusal below their minimum rate ignored
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

Speech fixtures with no noise, on both paths, as recorded and 10 and 20 dB quieter (a soft-spoken caller). *Gain*: median level change of the frames inside utterances; fails below -10 dB (a warning from -6 dB). *VAD recall*: share of the utterances' duration the VAD marks as speech, with the filter against without; fails on a loss of more than 5 points. *Missed*: utterances with no speech start, with the filter against without. *refused*: the engine does not use the backend on this path (below its minimum sample rate), nothing measured.

| backend | path | caller level | gain | VAD recall (without) | missed (without) | result |
|---|---|---|---|---|---|---|
| rnnoise | wideband | +0 dB | -0.1 dB | 1.00 (1.00) | 0/6 (0) | ok |
| hush | wideband | +0 dB | -8.6 dB | 1.00 (1.00) | 0/6 (0) | attenuates |
| rnnoise | wideband | -10 dB | -0.0 dB | 1.00 (1.00) | 0/6 (0) | ok |
| hush | wideband | -10 dB | -2.8 dB | 1.00 (1.00) | 0/6 (0) | ok |
| rnnoise | wideband | -20 dB | -0.0 dB | 0.99 (0.99) | 0/6 (0) | ok |
| hush | wideband | -20 dB | -28.2 dB | 0.46 (0.99) | 1/6 (0) | FAILS |
| rnnoise | telephone | +0 dB | -0.1 dB | 0.99 (0.98) | 0/6 (0) | ok |
| hush | telephone | +0 dB | -12.0 dB | 0.98 (0.98) | 0/6 (0) | FAILS |
| rnnoise | telephone | -10 dB | -0.0 dB | 0.97 (0.97) | 0/6 (0) | ok |
| hush | telephone | -10 dB | -6.5 dB | 0.98 (0.97) | 0/6 (0) | attenuates |
| rnnoise | telephone | -20 dB | -0.0 dB | 0.99 (0.98) | 0/6 (0) | ok |
| hush | telephone | -20 dB | -30.3 dB | 0.34 (0.98) | 2/6 (0) | FAILS |

