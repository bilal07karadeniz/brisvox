# Noise suppression: CPU per stream

- command: `brisvox-bench denoise cpu --backends none,rnnoise,fastenhancer --fastenhancer-model MODELS_DIR/fastenhancer_t.onnx --seconds 120 --out bench/results/denoise-cpu.md`
- brisvox 0.1.2 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- 120 s of speech fixtures per backend and rate, one thread; the 8 kHz rows carry a telephone call's audio at its own rate (the backend converts to its model's)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

One call, one thread, 20 ms frames of the speech fixtures, steady state (250 frames of warm-up, then the model state reset). `per frame` is the wall time of one `process` call; `CPU` is the process' CPU time over the run per second of audio, in percent of one core (it includes the harness, which does nothing else). `delay` is the algorithmic delay the backend adds to the audio the VAD and the turn detector hear.

| backend | call rate | delay (ms) | build (ms) | per frame p50 (ms) | p95 | p99 | max | CPU, % of one core | 100 calls need |
|---|---|---|---|---|---|---|---|---|---|
| rnnoise | 16000 Hz | 20.000 | 0.193 | 0.067 | 0.147 | 0.311 | 1.110 | 0.38 | 0.4 cores |
| rnnoise | 8000 Hz | 25.250 | 3.332 | 0.085 | 0.195 | 0.419 | 1.308 | 0.39 | 0.4 cores |
| fastenhancer-t | 16000 Hz | 16.000 | 148.361 | 0.272 | 0.746 | 1.166 | 2.911 | 1.56 | 1.6 cores |
| fastenhancer-t | 8000 Hz | 32.000 | 27.141 | 0.310 | 0.796 | 1.208 | 1.910 | 1.76 | 1.8 cores |
