# Noise suppression: CPU per stream

- command: `brisvox-bench denoise cpu --backends none,rnnoise,hush --seconds 120 --out bench/results/denoise-cpu-hush.md`
- brisvox 0.1.2 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- 120 s of speech fixtures per backend and rate, one thread; the 8 kHz rows carry a telephone call's audio at its own rate (the backend converts to its model's)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

One call, one thread, 20 ms frames of the speech fixtures, steady state (250 frames of warm-up, then the model state reset). `per frame` is the wall time of one `process` call; `CPU` is the process' CPU time over the run per second of audio, in percent of one core (it includes the harness, which does nothing else). `delay` is the algorithmic delay the backend adds to the audio the VAD and the turn detector hear.

| backend | call rate | delay (ms) | build (ms) | per frame p50 (ms) | p95 | p99 | max | CPU, % of one core | 100 calls need |
|---|---|---|---|---|---|---|---|---|---|
| rnnoise | 16000 Hz | 20.000 | 0.184 | 0.056 | 0.081 | 0.087 | 0.129 | 0.29 | 0.3 cores |
| rnnoise | 8000 Hz | 25.250 | 1.629 | 0.065 | 0.091 | 0.103 | 0.205 | 0.35 | 0.4 cores |
| hush | 16000 Hz | 10.000 | 8.494 | 0.202 | 0.245 | 0.276 | 0.525 | 1.03 | 1.0 cores |
| hush | 8000 Hz | 26.000 | 0.711 | 0.210 | 0.258 | 0.284 | 0.682 | 1.09 | 1.1 cores |
