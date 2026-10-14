# Streaming resampler

- command: `brisvox-bench resample --out bench/results/resampler.md`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- single thread, steady state (fresh instance after a warm-up run)

`brisvox_audio::StreamResampler` (rubato `Fft`, one FFT block = 160 samples of the lower rate; windowed sinc for pairs that output 8 kHz), mono PCM16, fed 20 ms frames. Cost = mean time in `process` per 20 ms input frame (release, one warm instance: a best case; with 500 instances cycling through the cache the perf audit measured ~1.5-2x). Identity rows time only the clock reads and a copy. SNR against the best-fit sine after the start-up transient; edge = 0.8 x the lower Nyquist. Alias rejection (48 kHz -> 8 kHz, full-scale 6 kHz tone): nothing left above the 16-bit quantisation floor (< -96 dB).

| from Hz | to Hz | cost µs/frame | filter delay ms | SNR 1 kHz dB | SNR edge dB |
|---:|---:|---:|---:|---:|---:|
| 8000 | 8000 | 0.03 | 0.00 | 99.8 | 100.4 |
| 8000 | 16000 | 0.69 | 10.00 | 97.6 | 100.4 |
| 8000 | 22050 | 1.19 | 9.98 | 90.6 | 90.5 |
| 8000 | 24000 | 0.96 | 10.00 | 93.3 | 91.3 |
| 8000 | 44100 | 2.28 | 10.00 | 90.6 | 90.5 |
| 8000 | 48000 | 1.65 | 10.00 | 92.9 | 92.3 |
| 16000 | 8000 | 5.50 | 6.00 | 90.6 | 97.0 |
| 16000 | 16000 | 0.03 | 0.00 | 97.6 | 100.4 |
| 16000 | 22050 | 1.32 | 9.98 | 90.5 | 90.5 |
| 16000 | 24000 | 1.15 | 5.00 | 93.4 | 91.3 |
| 16000 | 44100 | 2.16 | 4.99 | 90.6 | 90.5 |
| 16000 | 48000 | 1.91 | 5.00 | 93.5 | 91.3 |
| 22050 | 8000 | 7.16 | 5.00 | 90.4 | 91.3 |
| 22050 | 16000 | 1.31 | 10.00 | 91.7 | 89.3 |
| 22050 | 22050 | 0.03 | 0.00 | 92.0 | 100.4 |
| 22050 | 24000 | 1.67 | 6.67 | 90.4 | 91.5 |
| 22050 | 44100 | 1.91 | 3.63 | 90.3 | 100.4 |
| 22050 | 48000 | 2.35 | 6.67 | 90.4 | 91.8 |
| 24000 | 8000 | 6.19 | 4.62 | 102.6 | 96.8 |
| 24000 | 16000 | 1.13 | 5.00 | 97.6 | 100.4 |
| 24000 | 22050 | 1.65 | 6.67 | 89.2 | 90.8 |
| 24000 | 24000 | 0.02 | 0.00 | 93.3 | 100.4 |
| 24000 | 44100 | 2.53 | 3.33 | 89.2 | 91.1 |
| 24000 | 48000 | 2.05 | 3.33 | 92.9 | 100.4 |
| 44100 | 8000 | 10.19 | 4.25 | 91.9 | 94.7 |
| 44100 | 16000 | 2.05 | 5.00 | 89.7 | 89.9 |
| 44100 | 22050 | 1.83 | 3.63 | 91.8 | 100.4 |
| 44100 | 24000 | 2.49 | 3.33 | 92.3 | 91.9 |
| 44100 | 44100 | 0.09 | 0.00 | 92.0 | 100.4 |
| 44100 | 48000 | 3.34 | 3.33 | 89.8 | 91.5 |
| 48000 | 8000 | 12.38 | 5.25 | 100.5 | 98.4 |
| 48000 | 16000 | 1.83 | 5.00 | 97.6 | 100.4 |
| 48000 | 22050 | 2.25 | 6.67 | 91.1 | 95.1 |
| 48000 | 24000 | 1.98 | 3.33 | 93.3 | 100.4 |
| 48000 | 44100 | 3.26 | 3.33 | 89.3 | 90.8 |
| 48000 | 48000 | 0.17 | 0.00 | 93.4 | 100.4 |
