# Noise suppression: false barge-ins and turn verdicts

- command: `brisvox-bench denoise quality --models MODELS_DIR --backends none,rnnoise,fastenhancer --noise-alone-s 60 --out bench/results/denoise-quality.md`
- brisvox 0.1.2 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- in process, no pool: the VAD stream and Smart Turn run on this thread
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

Speech: en_two_utterances, tr_two_utterances, en_tr_mixed (6 utterances in all). Noise: 9 recordings (`fixtures/noise`, MUSAN free-sound, public domain). Every speech fixture is mixed with every noise recording at each signal-to-noise ratio: 27 mixes per cell, wideband and through the telephone path (8 kHz mu-law, back to 16 kHz). The VAD is the engine's Silero stream with the production parameters, fed 20 ms frames, with the backend in front of it exactly as the engine's VAD worker runs it; the turn detector is Smart Turn v3.2 on the last 8 s of the enhanced audio, 300 ms after the end of each utterance.

Terms. *False start*: a speech start outside every utterance (from 300 ms before its first sample to 300 ms after its last): while the bot talks, this is a false barge-in. *Missed*: an utterance with no start in that window. *Fragment*: a second start inside one utterance. *Flip*: a Smart Turn verdict that differs from the verdict on the clean audio of the same path; |dp| is the mean change of its probability. *Noise alone*: the recordings alone (60 s of each, looped) at the level they have in the mix, starts per minute.

## wideband

| SNR | backend | false starts in mixes | per quiet minute | missed | fragments | onset error | noise alone: starts/min | flips | mean abs dp |
|---|---|---|---|---|---|---|---|---|---|
| clean | none | 0 | 0.0 | 0/6 | 0 | +17 ms | 0.0 | 0 of 6 | 0.000 |
| clean | rnnoise | 0 | 0.0 | 0/6 | 0 | +7 ms | 0.0 | 0 of 6 | 0.037 |
| clean | fastenhancer-t | 0 | 0.0 | 0/6 | 0 | +1 ms | 0.0 | 0 of 6 | 0.022 |
| 20 dB | none | 0 | 0.0 | 0/54 | 0 | -6 ms | 0.0 | 0 of 54 | 0.050 |
| 20 dB | rnnoise | 0 | 0.0 | 0/54 | 1 | -3 ms | 0.4 | 1 of 54 | 0.039 |
| 20 dB | fastenhancer-t | 0 | 0.0 | 0/54 | 0 | -20 ms | 0.0 | 2 of 54 | 0.052 |
| 10 dB | none | 4 | 4.9 | 0/54 | 0 | -12 ms | 0.0 | 2 of 54 | 0.079 |
| 10 dB | rnnoise | 1 | 1.2 | 0/54 | 3 | -0 ms | 0.1 | 2 of 54 | 0.073 |
| 10 dB | fastenhancer-t | 2 | 2.5 | 0/54 | 1 | -19 ms | 0.0 | 0 of 54 | 0.056 |
| 5 dB | none | 4 | 4.9 | 0/54 | 0 | -4 ms | 0.0 | 3 of 54 | 0.109 |
| 5 dB | rnnoise | 3 | 3.7 | 0/54 | 5 | +4 ms | 0.2 | 3 of 54 | 0.096 |
| 5 dB | fastenhancer-t | 5 | 6.2 | 0/54 | 2 | -9 ms | 1.7 | 1 of 54 | 0.061 |
| 0 dB | none | 4 | 4.9 | 0/54 | 1 | +13 ms | 0.0 | 1 of 54 | 0.082 |
| 0 dB | rnnoise | 2 | 2.5 | 0/54 | 12 | +18 ms | 0.2 | 2 of 54 | 0.082 |
| 0 dB | fastenhancer-t | 5 | 6.2 | 0/54 | 6 | -7 ms | 1.2 | 0 of 54 | 0.060 |

## telephone

| SNR | backend | false starts in mixes | per quiet minute | missed | fragments | onset error | noise alone: starts/min | flips | mean abs dp |
|---|---|---|---|---|---|---|---|---|---|
| clean | none | 0 | 0.0 | 0/6 | 1 | +65 ms | 0.0 | 0 of 6 | 0.000 |
| clean | rnnoise | 0 | 0.0 | 0/6 | 0 | +23 ms | 0.0 | 0 of 6 | 0.002 |
| clean | fastenhancer-t | 0 | 0.0 | 0/6 | 0 | +12 ms | 0.0 | 0 of 6 | 0.008 |
| 20 dB | none | 1 | 1.2 | 0/54 | 1 | +4 ms | 0.0 | 0 of 54 | 0.060 |
| 20 dB | rnnoise | 0 | 0.0 | 0/54 | 6 | +48 ms | 0.2 | 1 of 54 | 0.028 |
| 20 dB | fastenhancer-t | 0 | 0.0 | 0/54 | 12 | +3 ms | 0.0 | 4 of 54 | 0.053 |
| 10 dB | none | 3 | 3.7 | 0/54 | 5 | -13 ms | 0.0 | 0 of 54 | 0.054 |
| 10 dB | rnnoise | 0 | 0.0 | 0/54 | 1 | +35 ms | 0.6 | 4 of 54 | 0.070 |
| 10 dB | fastenhancer-t | 0 | 0.0 | 0/54 | 20 | +10 ms | 0.0 | 7 of 54 | 0.079 |
| 5 dB | none | 4 | 4.9 | 0/54 | 8 | -11 ms | 0.1 | 0 of 54 | 0.049 |
| 5 dB | rnnoise | 0 | 0.0 | 0/54 | 5 | +25 ms | 0.6 | 6 of 54 | 0.083 |
| 5 dB | fastenhancer-t | 0 | 0.0 | 0/54 | 28 | +47 ms | 0.0 | 5 of 54 | 0.070 |
| 0 dB | none | 6 | 7.4 | 1/54 | 8 | +35 ms | 0.1 | 0 of 54 | 0.059 |
| 0 dB | rnnoise | 4 | 4.9 | 0/54 | 25 | +72 ms | 1.9 | 3 of 54 | 0.089 |
| 0 dB | fastenhancer-t | 0 | 0.0 | 0/54 | 69 | +145 ms | 0.0 | 2 of 54 | 0.068 |

