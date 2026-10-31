# Interruptions of the bot with the engine's guards

- command: `brisvox-bench denoise calls --set all --models MODELS_DIR --threads 8 --levels --out bench/results/noise-handling-calls.md`
- brisvox 0.1.2 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- set `all`, in process, the real TurnMachine and InterruptionPolicy, oracle STT
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

Cells are calls where the bot is speaking. *Quiet calls interrupted*: calls in which the caller says nothing and the bot was interrupted anyway (false barge-ins, after the guards); the columns split them by what is on the line. *VAD starts*: the same quiet calls counted at the VAD, before the guards. *Missed*: calls in which the caller spoke over the bot and never got through. *Too early*: the bot was interrupted before the caller spoke. *Time to interrupt*: from the caller's first word to the interruption. *Fragments*: extra VAD starts inside the caller's utterance. *Flips*: Smart Turn verdicts at the end of the caller's utterance that differ from the verdict on the caller's audio alone.

## wideband

| configuration | quiet calls interrupted: speech | echo | noise | (VAD starts, all quiet) | real barge-ins missed | interrupted too early | time to interrupt, median | fragments | Smart Turn flips |
|---|---|---|---|---|---|---|---|---|---|
| baseline | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 32 | 392 ms | 2 | 0/48 |
| min_words 1 | 24/32 | 8/16 | 0/27 | 49/75 | 0/48 | 24 | 680 ms | 2 | 0/48 |
| min_words 2 | 24/32 | 8/16 | 0/27 | 49/75 | 0/48 | 24 | 990 ms | 2 | 0/48 |
| noise-adapted guards | 32/32 | 16/16 | 0/27 | 49/75 | 0/48 | 32 | 690 ms | 2 | 0/48 |
| echo text match | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 32 | 392 ms | 2 | 0/48 |
| min_words 1 + echo text match | 24/32 | 0/16 | 0/27 | 49/75 | 0/48 | 18 | 690 ms | 2 | 0/48 |
| min_words 1 + adapted guards | 24/32 | 8/16 | 0/27 | 49/75 | 0/48 | 24 | 680 ms | 2 | 0/48 |
| min_words 1 + adapted + echo match | 24/32 | 0/16 | 0/27 | 49/75 | 0/48 | 18 | 690 ms | 2 | 0/48 |
| level floor -50 dBFS | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 31 | 380 ms | 3 | 0/48 |
| level floor -45 dBFS | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 30 | 380 ms | 12 | 0/48 |
| level floor -40 dBFS | 32/32 | 8/16 | 1/27 | 48/75 | 0/48 | 23 | 368 ms | 24 | 0/48 |
| level floor -35 dBFS | 24/32 | 8/16 | 2/27 | 39/75 | 0/48 | 15 | 368 ms | 52 | 0/48 |
| rolling gate -10 dB | 16/32 | 0/16 | 3/27 | 29/75 | 0/48 | 6 | 378 ms | 159 | 0/48 |
| rolling gate -15 dB | 24/32 | 8/16 | 2/27 | 42/75 | 0/48 | 15 | 368 ms | 73 | 0/48 |
| rolling gate -20 dB | 32/32 | 9/16 | 1/27 | 49/75 | 0/48 | 23 | 368 ms | 26 | 0/48 |
| floor -45 + rolling -15 | 24/32 | 8/16 | 2/27 | 42/75 | 0/48 | 15 | 368 ms | 73 | 0/48 |
| rolling gate -10 dB, onset only | 26/32 | 8/16 | 1/27 | 37/75 | 0/48 | 24 | 378 ms | 2 | 0/48 |
| rolling gate -15 dB, onset only | 32/32 | 9/16 | 1/27 | 43/75 | 0/48 | 26 | 378 ms | 2 | 0/48 |
| rolling gate -20 dB, onset only | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 31 | 380 ms | 2 | 0/48 |
| level floor -40 dBFS, onset only | 32/32 | 15/16 | 1/27 | 49/75 | 0/48 | 31 | 380 ms | 2 | 0/48 |
| level floor -35 dBFS, onset only | 32/32 | 8/16 | 1/27 | 41/75 | 0/48 | 26 | 378 ms | 2 | 0/48 |
| vad threshold 0.6, silence 100 ms | 32/32 | 16/16 | 0/27 | 48/75 | 0/48 | 31 | 392 ms | 2 | 0/48 |
| vad threshold 0.7, silence 100 ms | 32/32 | 16/16 | 0/27 | 48/75 | 0/48 | 31 | 392 ms | 1 | 0/48 |
| vad threshold 0.5, silence 200 ms | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 32 | 492 ms | 0 | 0/48 |
| vad threshold 0.5, silence 300 ms | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 32 | 592 ms | 0 | 0/48 |
| vad threshold 0.6, silence 200 ms | 32/32 | 16/16 | 0/27 | 48/75 | 0/48 | 31 | 492 ms | 0 | 0/48 |
| vad min_speech 128 ms | 32/32 | 16/16 | 0/27 | 48/75 | 0/48 | 31 | 380 ms | 1 | 0/48 |
| rnnoise | 32/32 | 16/16 | 6/27 | 53/75 | 0/48 | 33 | 378 ms | 0 | 1/48 |
| rnnoise, 25 % dry | 32/32 | 16/16 | 2/27 | 50/75 | 0/48 | 31 | 380 ms | 4 | 1/48 |
| hush | 27/32 | 6/16 | 2/27 | 36/75 | 0/48 | 24 | 380 ms | 1 | 0/48 |
| hush, 25 % dry | 32/32 | 15/16 | 2/27 | 49/75 | 0/48 | 28 | 380 ms | 1 | 1/48 |
| hush, 50 % dry | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 31 | 380 ms | 2 | 0/48 |
| hush, forced on 8 kHz audio | 27/32 | 6/16 | 2/27 | 36/75 | 0/48 | 24 | 380 ms | 1 | 0/48 |
| hush + echo text match | 27/32 | 6/16 | 2/27 | 36/75 | 0/48 | 24 | 380 ms | 1 | 0/48 |
| hush + rolling gate -15 dB (onset) + echo match | 26/32 | 6/16 | 2/27 | 34/75 | 0/48 | 24 | 380 ms | 1 | 0/48 |
| rolling gate -15 dB (onset) + echo match | 32/32 | 9/16 | 1/27 | 43/75 | 0/48 | 26 | 378 ms | 2 | 0/48 |
| hush + echo match + min_words 1 | 23/32 | 0/16 | 0/27 | 36/75 | 0/48 | 18 | 710 ms | 1 | 0/48 |

## telephone

| configuration | quiet calls interrupted: speech | echo | noise | (VAD starts, all quiet) | real barge-ins missed | interrupted too early | time to interrupt, median | fragments | Smart Turn flips |
|---|---|---|---|---|---|---|---|---|---|
| baseline | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 33 | 380 ms | 2 | 7/48 |
| min_words 1 | 24/32 | 8/16 | 0/27 | 49/75 | 0/48 | 24 | 670 ms | 2 | 7/48 |
| min_words 2 | 24/32 | 8/16 | 0/27 | 49/75 | 0/48 | 24 | 990 ms | 2 | 7/48 |
| noise-adapted guards | 32/32 | 16/16 | 0/27 | 49/75 | 0/48 | 31 | 670 ms | 2 | 7/48 |
| echo text match | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 33 | 380 ms | 2 | 7/48 |
| min_words 1 + echo text match | 24/32 | 0/16 | 0/27 | 49/75 | 0/48 | 18 | 690 ms | 2 | 7/48 |
| min_words 1 + adapted guards | 24/32 | 8/16 | 0/27 | 49/75 | 0/48 | 24 | 670 ms | 2 | 7/48 |
| min_words 1 + adapted + echo match | 24/32 | 0/16 | 0/27 | 49/75 | 0/48 | 18 | 690 ms | 2 | 7/48 |
| level floor -50 dBFS | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 32 | 380 ms | 3 | 7/48 |
| level floor -45 dBFS | 32/32 | 15/16 | 1/27 | 49/75 | 0/48 | 30 | 378 ms | 12 | 7/48 |
| level floor -40 dBFS | 32/32 | 8/16 | 1/27 | 47/75 | 0/48 | 24 | 368 ms | 28 | 7/48 |
| level floor -35 dBFS | 24/32 | 8/16 | 2/27 | 39/75 | 0/48 | 16 | 368 ms | 63 | 7/48 |
| rolling gate -10 dB | 16/32 | 1/16 | 3/27 | 29/75 | 0/48 | 8 | 378 ms | 167 | 7/48 |
| rolling gate -15 dB | 24/32 | 8/16 | 2/27 | 42/75 | 0/48 | 17 | 368 ms | 77 | 7/48 |
| rolling gate -20 dB | 32/32 | 9/16 | 1/27 | 49/75 | 0/48 | 24 | 368 ms | 34 | 7/48 |
| floor -45 + rolling -15 | 24/32 | 8/16 | 2/27 | 42/75 | 0/48 | 17 | 368 ms | 77 | 7/48 |
| rolling gate -10 dB, onset only | 26/32 | 8/16 | 1/27 | 35/75 | 0/48 | 26 | 378 ms | 2 | 7/48 |
| rolling gate -15 dB, onset only | 32/32 | 11/16 | 1/27 | 44/75 | 0/48 | 26 | 378 ms | 2 | 7/48 |
| rolling gate -20 dB, onset only | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 32 | 380 ms | 2 | 7/48 |
| level floor -40 dBFS, onset only | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 32 | 380 ms | 2 | 7/48 |
| level floor -35 dBFS, onset only | 32/32 | 8/16 | 1/27 | 41/75 | 0/48 | 27 | 378 ms | 2 | 7/48 |
| vad threshold 0.6, silence 100 ms | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 33 | 392 ms | 3 | 7/48 |
| vad threshold 0.7, silence 100 ms | 32/32 | 16/16 | 2/27 | 49/75 | 0/48 | 32 | 424 ms | 3 | 7/48 |
| vad threshold 0.5, silence 200 ms | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 32 | 480 ms | 1 | 7/48 |
| vad threshold 0.5, silence 300 ms | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 31 | 578 ms | 1 | 7/48 |
| vad threshold 0.6, silence 200 ms | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 31 | 480 ms | 0 | 7/48 |
| vad min_speech 128 ms | 32/32 | 16/16 | 2/27 | 49/75 | 0/48 | 33 | 380 ms | 1 | 7/48 |
| rnnoise | 32/32 | 16/16 | 5/27 | 53/75 | 0/48 | 33 | 368 ms | 4 | 8/48 |
| rnnoise, 25 % dry | 32/32 | 16/16 | 3/27 | 51/75 | 0/48 | 32 | 380 ms | 3 | 8/48 |
| hush | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 33 | 380 ms | 2 | 7/48 |
| hush, 25 % dry | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 33 | 380 ms | 2 | 7/48 |
| hush, 50 % dry | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 33 | 380 ms | 2 | 7/48 |
| hush, forced on 8 kHz audio | 25/32 | 4/16 | 2/27 | 36/75 | 0/48 | 25 | 380 ms | 5 | 7/48 |
| hush + echo text match | 32/32 | 16/16 | 1/27 | 49/75 | 0/48 | 33 | 380 ms | 2 | 7/48 |
| hush + rolling gate -15 dB (onset) + echo match | 32/32 | 11/16 | 1/27 | 44/75 | 0/48 | 26 | 378 ms | 2 | 7/48 |
| rolling gate -15 dB (onset) + echo match | 32/32 | 11/16 | 1/27 | 44/75 | 0/48 | 26 | 378 ms | 2 | 7/48 |
| hush + echo match + min_words 1 | 24/32 | 0/16 | 0/27 | 49/75 | 0/48 | 18 | 690 ms | 2 | 7/48 |


## By level

Each cell: quiet calls interrupted/quiet calls · real barge-ins missed/barge-in calls.

### wideband: background speech by level (dB below the caller)

| configuration | -20 dB | -15 dB | -10 dB | -5 dB |
|---|---|---|---|---|
| baseline | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 2 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| noise-adapted guards | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| echo text match | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + echo text match | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + adapted guards | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + adapted + echo match | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -50 dBFS | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -45 dBFS | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -40 dBFS | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -35 dBFS | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -10 dB | 0/8 · 0/6 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -20 dB | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| floor -45 + rolling -15 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -10 dB, onset only | 2/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB, onset only | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -20 dB, onset only | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -40 dBFS, onset only | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -35 dBFS, onset only | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.6, silence 100 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.7, silence 100 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.5, silence 200 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.5, silence 300 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.6, silence 200 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad min_speech 128 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rnnoise | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rnnoise, 25 % dry | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush | 4/8 · 0/6 | 7/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, 25 % dry | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, 50 % dry | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, forced on 8 kHz audio | 4/8 · 0/6 | 7/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush + echo text match | 4/8 · 0/6 | 7/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush + rolling gate -15 dB (onset) + echo match | 3/8 · 0/6 | 7/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB (onset) + echo match | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush + echo match + min_words 1 | 0/8 · 0/6 | 7/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |

### telephone: background speech by level (dB below the caller)

| configuration | -20 dB | -15 dB | -10 dB | -5 dB |
|---|---|---|---|---|
| baseline | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 2 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| noise-adapted guards | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| echo text match | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + echo text match | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + adapted guards | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + adapted + echo match | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -50 dBFS | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -45 dBFS | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -40 dBFS | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -35 dBFS | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -10 dB | 0/8 · 0/6 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -20 dB | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| floor -45 + rolling -15 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -10 dB, onset only | 2/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB, onset only | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -20 dB, onset only | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -40 dBFS, onset only | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -35 dBFS, onset only | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.6, silence 100 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.7, silence 100 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.5, silence 200 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.5, silence 300 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.6, silence 200 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| vad min_speech 128 ms | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rnnoise | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rnnoise, 25 % dry | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, 25 % dry | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, 50 % dry | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, forced on 8 kHz audio | 3/8 · 0/6 | 6/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush + echo text match | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush + rolling gate -15 dB (onset) + echo match | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB (onset) + echo match | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |
| hush + echo match + min_words 1 | 0/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 | 8/8 · 0/6 |

### wideband: noise by level (dB signal-to-noise)

| configuration | +0 dB | +5 dB | +10 dB |
|---|---|---|---|
| baseline | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 1 | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 2 | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| noise-adapted guards | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| echo text match | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 1 + echo text match | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 1 + adapted guards | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 1 + adapted + echo match | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -50 dBFS | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -45 dBFS | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -40 dBFS | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -35 dBFS | 1/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| rolling gate -10 dB | 2/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| rolling gate -15 dB | 1/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| rolling gate -20 dB | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| floor -45 + rolling -15 | 1/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| rolling gate -10 dB, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| rolling gate -15 dB, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| rolling gate -20 dB, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -40 dBFS, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -35 dBFS, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.6, silence 100 ms | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.7, silence 100 ms | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.5, silence 200 ms | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.5, silence 300 ms | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.6, silence 200 ms | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad min_speech 128 ms | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| rnnoise | 2/9 · 0/6 | 3/9 · 0/0 | 1/9 · 0/6 |
| rnnoise, 25 % dry | 1/9 · 0/6 | 1/9 · 0/0 | 0/9 · 0/6 |
| hush | 1/9 · 0/6 | 1/9 · 0/0 | 0/9 · 0/6 |
| hush, 25 % dry | 1/9 · 0/6 | 1/9 · 0/0 | 0/9 · 0/6 |
| hush, 50 % dry | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| hush, forced on 8 kHz audio | 1/9 · 0/6 | 1/9 · 0/0 | 0/9 · 0/6 |
| hush + echo text match | 1/9 · 0/6 | 1/9 · 0/0 | 0/9 · 0/6 |
| hush + rolling gate -15 dB (onset) + echo match | 1/9 · 0/6 | 1/9 · 0/0 | 0/9 · 0/6 |
| rolling gate -15 dB (onset) + echo match | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| hush + echo match + min_words 1 | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |

### telephone: noise by level (dB signal-to-noise)

| configuration | +0 dB | +5 dB | +10 dB |
|---|---|---|---|
| baseline | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 1 | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 2 | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| noise-adapted guards | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| echo text match | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 1 + echo text match | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 1 + adapted guards | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| min_words 1 + adapted + echo match | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -50 dBFS | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -45 dBFS | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -40 dBFS | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -35 dBFS | 1/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| rolling gate -10 dB | 2/9 · 0/6 | 1/9 · 0/0 | 0/9 · 0/6 |
| rolling gate -15 dB | 1/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| rolling gate -20 dB | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| floor -45 + rolling -15 | 1/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| rolling gate -10 dB, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| rolling gate -15 dB, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| rolling gate -20 dB, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -40 dBFS, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| level floor -35 dBFS, onset only | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.6, silence 100 ms | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.7, silence 100 ms | 1/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| vad threshold 0.5, silence 200 ms | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.5, silence 300 ms | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad threshold 0.6, silence 200 ms | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| vad min_speech 128 ms | 1/9 · 0/6 | 0/9 · 0/0 | 1/9 · 0/6 |
| rnnoise | 2/9 · 0/6 | 2/9 · 0/0 | 1/9 · 0/6 |
| rnnoise, 25 % dry | 1/9 · 0/6 | 2/9 · 0/0 | 0/9 · 0/6 |
| hush | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| hush, 25 % dry | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| hush, 50 % dry | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| hush, forced on 8 kHz audio | 1/9 · 0/6 | 1/9 · 0/0 | 0/9 · 0/6 |
| hush + echo text match | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| hush + rolling gate -15 dB (onset) + echo match | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| rolling gate -15 dB (onset) + echo match | 1/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |
| hush + echo match + min_words 1 | 0/9 · 0/6 | 0/9 · 0/0 | 0/9 · 0/6 |

### wideband: bot echo by level (dB below the caller)

| configuration | -25 dB | -15 dB |
|---|---|---|
| baseline | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 | 0/8 · 0/6 | 8/8 · 0/6 |
| min_words 2 | 0/8 · 0/6 | 8/8 · 0/6 |
| noise-adapted guards | 8/8 · 0/6 | 8/8 · 0/6 |
| echo text match | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + echo text match | 0/8 · 0/6 | 0/8 · 0/6 |
| min_words 1 + adapted guards | 0/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + adapted + echo match | 0/8 · 0/6 | 0/8 · 0/6 |
| level floor -50 dBFS | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -45 dBFS | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -40 dBFS | 0/8 · 0/6 | 8/8 · 0/6 |
| level floor -35 dBFS | 0/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -10 dB | 0/8 · 0/6 | 0/8 · 0/6 |
| rolling gate -15 dB | 0/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -20 dB | 1/8 · 0/6 | 8/8 · 0/6 |
| floor -45 + rolling -15 | 0/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -10 dB, onset only | 0/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB, onset only | 1/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -20 dB, onset only | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -40 dBFS, onset only | 7/8 · 0/6 | 8/8 · 0/6 |
| level floor -35 dBFS, onset only | 0/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.6, silence 100 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.7, silence 100 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.5, silence 200 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.5, silence 300 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.6, silence 200 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad min_speech 128 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| rnnoise | 8/8 · 0/6 | 8/8 · 0/6 |
| rnnoise, 25 % dry | 8/8 · 0/6 | 8/8 · 0/6 |
| hush | 0/8 · 0/6 | 6/8 · 0/6 |
| hush, 25 % dry | 7/8 · 0/6 | 8/8 · 0/6 |
| hush, 50 % dry | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, forced on 8 kHz audio | 0/8 · 0/6 | 6/8 · 0/6 |
| hush + echo text match | 0/8 · 0/6 | 6/8 · 0/6 |
| hush + rolling gate -15 dB (onset) + echo match | 0/8 · 0/6 | 6/8 · 0/6 |
| rolling gate -15 dB (onset) + echo match | 1/8 · 0/6 | 8/8 · 0/6 |
| hush + echo match + min_words 1 | 0/8 · 0/6 | 0/8 · 0/6 |

### telephone: bot echo by level (dB below the caller)

| configuration | -25 dB | -15 dB |
|---|---|---|
| baseline | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 | 0/8 · 0/6 | 8/8 · 0/6 |
| min_words 2 | 0/8 · 0/6 | 8/8 · 0/6 |
| noise-adapted guards | 8/8 · 0/6 | 8/8 · 0/6 |
| echo text match | 8/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + echo text match | 0/8 · 0/6 | 0/8 · 0/6 |
| min_words 1 + adapted guards | 0/8 · 0/6 | 8/8 · 0/6 |
| min_words 1 + adapted + echo match | 0/8 · 0/6 | 0/8 · 0/6 |
| level floor -50 dBFS | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -45 dBFS | 7/8 · 0/6 | 8/8 · 0/6 |
| level floor -40 dBFS | 0/8 · 0/6 | 8/8 · 0/6 |
| level floor -35 dBFS | 0/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -10 dB | 0/8 · 0/6 | 1/8 · 0/6 |
| rolling gate -15 dB | 0/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -20 dB | 1/8 · 0/6 | 8/8 · 0/6 |
| floor -45 + rolling -15 | 0/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -10 dB, onset only | 0/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB, onset only | 3/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -20 dB, onset only | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -40 dBFS, onset only | 8/8 · 0/6 | 8/8 · 0/6 |
| level floor -35 dBFS, onset only | 0/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.6, silence 100 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.7, silence 100 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.5, silence 200 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.5, silence 300 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad threshold 0.6, silence 200 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| vad min_speech 128 ms | 8/8 · 0/6 | 8/8 · 0/6 |
| rnnoise | 8/8 · 0/6 | 8/8 · 0/6 |
| rnnoise, 25 % dry | 8/8 · 0/6 | 8/8 · 0/6 |
| hush | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, 25 % dry | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, 50 % dry | 8/8 · 0/6 | 8/8 · 0/6 |
| hush, forced on 8 kHz audio | 0/8 · 0/6 | 4/8 · 0/6 |
| hush + echo text match | 8/8 · 0/6 | 8/8 · 0/6 |
| hush + rolling gate -15 dB (onset) + echo match | 3/8 · 0/6 | 8/8 · 0/6 |
| rolling gate -15 dB (onset) + echo match | 3/8 · 0/6 | 8/8 · 0/6 |
| hush + echo match + min_words 1 | 0/8 · 0/6 | 0/8 · 0/6 |

