# Noise suppression: false barge-ins and turn verdicts

- command: `brisvox-bench denoise quality --models MODELS_DIR --backends none,rnnoise,fastenhancer --noise-dir MUSAN_BG_DIR --noise-alone-s 30 --out bench/results/denoise-quality-musan-background.md`
- brisvox 0.1.2 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- in process, no pool: the VAD stream and Smart Turn run on this thread
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

Speech: en_two_utterances, tr_two_utterances, en_tr_mixed (6 utterances in all). Noise: 34 recordings from the 37 the corpus lists as background noises (musan_bg, MUSAN free-sound, public domain; not committed, the list is in docs/FIXTURES_SOURCES.md). Every speech fixture is mixed with every noise recording at each signal-to-noise ratio: 102 mixes per cell, wideband and through the telephone path (8 kHz mu-law, back to 16 kHz). The VAD is the engine's Silero stream with the production parameters, fed 20 ms frames, with the backend in front of it exactly as the engine's VAD worker runs it; the turn detector is Smart Turn v3.2 on the last 8 s of the enhanced audio, 300 ms after the end of each utterance.

Terms. *False start*: a speech start outside every utterance (from 300 ms before its first sample to 300 ms after its last): while the bot talks, this is a false barge-in. *Missed*: an utterance with no start in that window. *Fragment*: a second start inside one utterance. *Flip*: a Smart Turn verdict that differs from the verdict on the clean audio of the same path; |dp| is the mean change of its probability. *Noise alone*: the recordings alone (30 s of each, looped) at the level they have in the mix, starts per minute.

## wideband

| SNR | backend | false starts in mixes | per quiet minute | missed | fragments | onset error | noise alone: starts/min | flips | mean abs dp |
|---|---|---|---|---|---|---|---|---|---|
| clean | none | 0 | 0.0 | 0/6 | 0 | +17 ms | 0.0 | 0 of 6 | 0.000 |
| clean | rnnoise | 0 | 0.0 | 0/6 | 0 | +7 ms | 0.0 | 0 of 6 | 0.037 |
| clean | fastenhancer-t | 0 | 0.0 | 0/6 | 0 | +1 ms | 0.0 | 0 of 6 | 0.022 |
| 20 dB | none | 1 | 0.3 | 0/204 | 0 | -1 ms | 0.1 | 1 of 204 | 0.046 |
| 20 dB | rnnoise | 0 | 0.0 | 0/204 | 1 | -1 ms | 0.2 | 3 of 204 | 0.038 |
| 20 dB | fastenhancer-t | 0 | 0.0 | 0/204 | 1 | -12 ms | 0.0 | 3 of 204 | 0.051 |
| 10 dB | none | 0 | 0.0 | 0/204 | 1 | -8 ms | 0.1 | 3 of 204 | 0.066 |
| 10 dB | rnnoise | 6 | 2.0 | 0/204 | 11 | -3 ms | 0.4 | 4 of 204 | 0.054 |
| 10 dB | fastenhancer-t | 3 | 1.0 | 0/204 | 3 | -15 ms | 0.2 | 6 of 204 | 0.074 |
| 5 dB | none | 4 | 1.3 | 0/204 | 5 | -5 ms | 0.2 | 8 of 204 | 0.089 |
| 5 dB | rnnoise | 5 | 1.6 | 0/204 | 21 | +3 ms | 0.5 | 10 of 204 | 0.084 |
| 5 dB | fastenhancer-t | 6 | 2.0 | 0/204 | 13 | -12 ms | 0.5 | 4 of 204 | 0.066 |
| 0 dB | none | 4 | 1.3 | 0/204 | 9 | +22 ms | 0.2 | 6 of 204 | 0.084 |
| 0 dB | rnnoise | 10 | 3.3 | 0/204 | 56 | +19 ms | 0.5 | 7 of 204 | 0.086 |
| 0 dB | fastenhancer-t | 9 | 2.9 | 1/204 | 40 | +15 ms | 0.6 | 6 of 204 | 0.080 |

## telephone

| SNR | backend | false starts in mixes | per quiet minute | missed | fragments | onset error | noise alone: starts/min | flips | mean abs dp |
|---|---|---|---|---|---|---|---|---|---|
| clean | none | 0 | 0.0 | 0/6 | 1 | +65 ms | 0.0 | 0 of 6 | 0.000 |
| clean | rnnoise | 0 | 0.0 | 0/6 | 0 | +23 ms | 0.0 | 0 of 6 | 0.002 |
| clean | fastenhancer-t | 0 | 0.0 | 0/6 | 0 | +12 ms | 0.0 | 0 of 6 | 0.008 |
| 20 dB | none | 1 | 0.3 | 0/204 | 5 | +9 ms | 0.1 | 0 of 204 | 0.049 |
| 20 dB | rnnoise | 1 | 0.3 | 0/204 | 18 | +43 ms | 0.2 | 1 of 204 | 0.033 |
| 20 dB | fastenhancer-t | 0 | 0.0 | 0/204 | 45 | +7 ms | 0.0 | 16 of 204 | 0.051 |
| 10 dB | none | 5 | 1.6 | 0/204 | 18 | -8 ms | 0.1 | 2 of 204 | 0.062 |
| 10 dB | rnnoise | 3 | 1.0 | 0/204 | 8 | +37 ms | 0.3 | 7 of 204 | 0.048 |
| 10 dB | fastenhancer-t | 0 | 0.0 | 0/204 | 73 | +17 ms | 0.1 | 22 of 204 | 0.073 |
| 5 dB | none | 6 | 2.0 | 0/204 | 26 | -3 ms | 0.1 | 2 of 204 | 0.070 |
| 5 dB | rnnoise | 4 | 1.3 | 0/204 | 21 | +53 ms | 0.2 | 11 of 204 | 0.057 |
| 5 dB | fastenhancer-t | 1 | 0.3 | 0/204 | 115 | +41 ms | 0.1 | 17 of 204 | 0.077 |
| 0 dB | none | 7 | 2.3 | 2/204 | 32 | +11 ms | 0.2 | 3 of 204 | 0.072 |
| 0 dB | rnnoise | 3 | 1.0 | 0/204 | 75 | +85 ms | 0.4 | 8 of 204 | 0.074 |
| 0 dB | fastenhancer-t | 0 | 0.0 | 7/204 | 203 | +116 ms | 0.1 | 10 of 204 | 0.074 |

