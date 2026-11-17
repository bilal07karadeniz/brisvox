# Barge-ins and background noise: pause, then decide

What happens to a bot that is speaking when something else makes sound on the caller's line: a cough, a
keyboard, a door, dishes, music, the street, babble, a television, a second person. The questions: how
often does that cut the bot off for good, how often does a pause and resume (`[interruption]
pause_then_decide`) save the response, and what does it cost the caller who really does interrupt,
compared with today's guards, with a LiveKit-like and with a Pipecat-like setup.

Command: `brisvox-bench denoise realistic --set fair --threads 12 --paths wideband,telephone --recognizers
oracle,model,model+short-x:1,model+short-x:3,model+noise-words:0.1` (feature `denoise`, release build). The
sections "Reading" and "Summary" are written from the program's output; everything under "Full output" is the program's own output.

## Method

- **The engine part is real.** The real `TurnMachine` with the real `InterruptionPolicy` (the agent file's
  defaults: `min_speech` 300 ms, echo window 400 ms, backchannel lists, `min_words` 0) and, in the pause
  modes, the real `PausePolicy` with the `words` rule, run on the events of the real Silero VAD (production
  parameters). Nothing of the transport, the TTS or a recognizer runs: a pause is the machine's `PausePlayout`
  action, a resume its `ResumePlayout`.
- **Calls.** Per path (wideband 16 kHz, telephone 8 kHz mu-law round trip) and recognizer, 5 187 calls: 7 callers (six
  LibriSpeech voices and one Turkish voice, two utterances each), 3 first-turn variants (clean, short "Alo", a television behind it), then the bot answers and
  - *quiet calls*: the caller says nothing; a second voice at 3 levels or noise at 5 levels (30 dB under the
    caller's speech up to the same level), 4 draws per kind and level for noise;
  - *barge-in calls*: the caller speaks over the bot, 525 of each kind per path: a whole sentence, a 0.9 s command and a 0.5 s command word
    ("stop", "wait", "dur", "bekle", "hold on", "yeter"), on a clean line, with a second voice at 2 levels, or with noise at 2 levels. The two short kinds
    are cut from the utterance **starting at its first speech frame**, so a 0.5 s command is 0.5 s of speech (an earlier version cut slices that carried lead-in silence and
    made today's guards look worse than they are). The transcript of a command word is the word.
- **What counts for how much.** A real caller says a sentence or phrase more often than "stop". The weighted summary counts
  60 % sentences, 25 % 0.9 s commands and 15 % 0.5 s command words. That share is an assumption about callers, not a measurement; the per-kind
  rows are always shown, and a reader with other traffic re-weights them. Every mode has the same calls, so the denominators are equal.
- **Noise.** Real: the nine MUSAN `free-sound` excerpts of `fixtures/noise` (public domain). Synthetic, from a
  seeded generator (`crates/brisvox-bench/src/noise_synth.rs`): cough (one to three bursts, a voiced part
  from none to speech-like), keyboard (two bursts of clicks), door (thump, latch, rumble), dishes (ringing
  clinks), music (chords and a melody), street (rumble and a passing vehicle). Babble: four LibriSpeech clips at once. The
  level is the noise's RMS while it sounds (events: over their active frames) against the RMS of the caller's first
  utterance.
- **Second voices**: LibriSpeech clips through a loudspeaker model and a measured room (OpenSLR 28 impulse
  responses), far-field loudspeaker recordings (VOiCES), Turkish broadcast speech (MediaSpeech), and a dry
  close-microphone clip. 27 voices, 8 rooms, 185 distinct caller x speaker pairs, same-sex and other-sex balanced.
- **Recognizer.** All rates are **assumptions**, in the range a small streaming recognizer shows on audio like this; they are not a measurement of any provider and not
  reproducible from this repository. *Oracle*: the words of a source that is within 15 dB of the caller are heard 300 ms late with an
  interim every 250 ms; noise has no words. *Modelled*: windows of 1.8 s of a source are left unheard (a real sentence 2.6 % wideband and 9.7 % telephone; a
  television through a loudspeaker and a room 33 %, far-field 68 %, Turkish broadcast 62 %), and an utterance of at most 1 s is missed **x2** as
  often as a sentence (5.2 % and 19.4 %), with x1 and x3 as sensitivity runs. *Modelled with noise words*: 10 % of the 1.8 s windows of noise make the
  recognizer write one word ("the").
- **Modes.** *baseline (today)*: the agent file's guards. *Pipecat-like*: the same with one recognised word required before an interruption (`MinWordsInterruptionStrategy`'s idea, `min_words = 1`).
  *LiveKit-like, no resume*: interrupt after 500 ms of speech (`min_interruption_duration`), no resume. *LiveKit-like*: pause after 500 ms of speech, resume if no word came by 2.0 s
  (`resume_false_interruption`; the engine's own window is limited to 1.75 s, the harness is not). *pause(words) after N ms, W s*: our rule with the pause onset delay `pause_after_ms`
  = N and the decision window W.
- **Counts.** A *candidate* is a VAD start while the bot speaks. Noise-only candidates are those of quiet calls with noise that has no speech in it. *Cut off*: interrupted for good.
  *Pauses/100*: pauses of the bot per 100 such calls. *Held*: a barge-in call in which the VAD started on the caller and the call never got a cut
  (calls where the VAD never started on the caller are the same in every mode and are not counted as held).

## Caveats

- **The noise headline is driven by synthetic voiced coughs.** Of the noise-only candidates today's guards cut off, almost all are generated coughs with a voiced part; the real
  recordings (MUSAN) produced only 4 (wideband) and 8 (telephone) candidates in 2 940 calls, of which 2 and 3 cut the bot off. Keyboard, dishes, music and street
  almost never start the VAD at these levels. The numbers say what the machine does with a candidate a noise made, not how often real rooms make one; the real-recording evidence is too thin to rate a default on.
- **The oracle's zero is by construction** (a recognizer that hears every word loses nothing), and the 1.7 % to 2.1 % of noise cut-offs with noise words rests on an assumed 10 %.
- **Today's guards lose nothing here by construction**: with `min_words = 0` they decide on duration alone, so a real utterance of at least 300 ms always passes. Any rule that waits
  for a recognised word can only equal that with a recognizer that never misses; its loss is the recognizer's miss rate, which is why the recognizer is varied.
- **Recognizer rates are assumptions** (above); a provider that is deafer on short words, or hears words in noise, moves the `words` rule's numbers directly.
- **No live transport.** The pause is the machine's action. A real transport adds the clear and the replay of the unplayed tail (tens of milliseconds, and over WebRTC and SIP possibly some
  audio already sent), and a real recognizer adds its own latency. The run is on a desktop PC and shows trends.
- **Short commands** are the first half second (or 0.9 s) of a longer utterance, with the word as the transcript: they test the command-word path and the recognizer miss, not the acoustics of the word "stop".


## Reading

1. **Real barge-ins, fair comparison.** Today's guards lose no real barge-in here (by construction, see the caveats). Every rule that waits for a recognised word loses what the recognizer misses: with the
   oracle recognizer nothing; with the default assumption (short utterances missed x2) 1.2 % (wideband) and 5.1 % (telephone) of the weighted barge-ins, 0.7 % and 2.7 % at x1, 1.8 % and 7.8 % at x3 (the loss sits in
   the 0.9 s commands and the 0.5 s command words, never in sentences). The pause with the `words` rule is within 0.1 percentage points of the LiveKit-like configuration (pause after 500 ms, resume by 2.0 s) on
   every recognizer, and far better than the Pipecat-like `min_words = 1` (7.3 % and 10.7 % at x2, 37 % to 47 % of the 0.5 s command words across the oracle and the x1 to x3 runs, both paths). The LiveKit-like variant that only interrupts after 500 ms and never resumes loses 0.1 %.
2. **Noise without speech.** Today's guards cut the bot off for good on 13.1 % (wideband) and 18.9 % (telephone) of the 312 and 302 candidates such noise makes: voiced synthetic coughs (20 % and 27 % of them), some
   synthetic doors (7 % and 10 %) and 2 and 3 of the 4 and 8 candidates of the real recordings. With the `words` rule none cuts the bot off (oracle and the default modelled recognizer), 1.7 % and 2.1 % when a tenth of the noise windows make the recognizer
   write a word. Interrupting only after 500 ms (no resume) leaves 3.2 % and 4.9 %; `min_words = 1` removes them too, at the price in 1.
3. **Pauses and their length** (per 100 quiet noise calls, median / p90 ms quiet, wideband; telephone alike): pause at the first moment of speech 9.9 (380 / 680), after 150 ms 9.9 (340 / 624), after 250 ms 8.7 (248 / 532),
   **after 400 ms 2.0 (378 / 598)**, LiveKit-like (500 ms) 1.3 (310 / 546). Only a delay of 400 ms removes most pauses (short clicks, door bangs and single coughs end sooner); 250 ms removes few. The delay
   costs the caller who really interrupts that much silence later (silence after the onset: 102, 150, 250, 400 ms against 400 ms with today's guards), so 400 ms is the longest delay that is no later than today. The real-barge-in loss is the same for every delay
   (it is the recognizer's). The decision window (1.0 or 1.5 s) changes almost nothing; 1.0 s shortens the longest pause (906 against 1 060 ms).
4. **The cut.** The bot is silent at the delay (102 to 400 ms) after the onset, and the final cut comes with the first word: median 698 ms, p90 878 ms wideband (oracle), against 400 ms with today's guards, because the words rule needs the word.
5. **Second voices and babble** (no goal here; full tables below): the `words` rule resumes only what the recognizer does not hear, continuous speech commits at the end of the window, and a second person who talks still interrupts the bot.

## Summary

Per path, the columns of the program's *Summary* table: noise pauses per 100 quiet noise calls, how long the bot was quiet before it resumed (ms, median / p90), the share of noise candidates that cut the bot off for good, real barge-ins held (calls in which the VAD started on the caller and the call never got a cut) as a share of all barge-in calls of the kind, the weighted share (60 / 25 / 15 %), silence after a real onset (ms, median) and the cut (ms, median / p90).

### Modelled, short utterances missed x2 (the default assumption), wideband

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 13.1% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 29/525 (5.5%) | 208/525 (39.6%) | **7.3%** | 694 | 694 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 3.2% | 0/525 (0.0%) | 0/525 (0.0%) | 2/525 (0.4%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.3 | 310 / 546 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 500 | 698 / 882 |
| pause(words) after 0 ms, 1.5 s | 9.9 | 380 / 680 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 102 | 698 / 882 |
| pause(words) after 150 ms, 1.5 s | 9.9 | 340 / 624 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 150 | 698 / 882 |
| pause(words) after 250 ms, 1.5 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 250 | 698 / 882 |
| pause(words) after 400 ms, 1.5 s | 2.0 | 378 / 598 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 400 | 698 / 882 |
| pause(words) after 250 ms, 1.0 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 14/525 (2.7%) | 18/525 (3.4%) | **1.2%** | 250 | 698 / 882 |

### Modelled, short utterances missed x2 (the default assumption), telephone

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 18.9% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 86/525 (16.4%) | 230/525 (43.8%) | **10.7%** | 460 | 460 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 4.9% | 0/525 (0.0%) | 0/525 (0.0%) | 3/525 (0.6%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.9 | 290 / 578 | 0.0% | 0/525 (0.0%) | 72/525 (13.7%) | 61/525 (11.6%) | **5.2%** | 500 | 698 / 906 |
| pause(words) after 0 ms, 1.5 s | 9.5 | 400 / 680 | 0.0% | 0/525 (0.0%) | 71/525 (13.5%) | 61/525 (11.6%) | **5.1%** | 102 | 698 / 906 |
| pause(words) after 150 ms, 1.5 s | 9.5 | 348 / 640 | 0.0% | 0/525 (0.0%) | 71/525 (13.5%) | 62/525 (11.8%) | **5.2%** | 150 | 698 / 906 |
| pause(words) after 250 ms, 1.5 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 71/525 (13.5%) | 61/525 (11.6%) | **5.1%** | 250 | 698 / 906 |
| pause(words) after 400 ms, 1.5 s | 2.5 | 374 / 666 | 0.0% | 0/525 (0.0%) | 71/525 (13.5%) | 61/525 (11.6%) | **5.1%** | 400 | 698 / 906 |
| pause(words) after 250 ms, 1.0 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 69/525 (13.1%) | 60/525 (11.4%) | **5.0%** | 250 | 698 / 906 |

### Sensitivity: weighted held share of real barge-ins by recognizer

| path | mode | Oracle | Modelled, short utterances missed x1 | Modelled, short utterances missed x2 (the default assumption) | Modelled, short utterances missed x3 | Modelled x2, a word in 10 % of noise windows |
|---|---|---|---|---|---|---|
| W | baseline (today) | 0.0% | 0.0% | 0.0% | 0.0% | 0.0% |
| W | Pipecat-like: min_words = 1 | 6.3% | 6.9% | 7.3% | 7.8% | 7.2% |
| W | LiveKit-like, no resume: interrupt after 500 ms | 0.1% | 0.1% | 0.1% | 0.1% | 0.1% |
| W | LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% | 0.7% | 1.2% | 1.8% | 1.2% |
| W | pause(words) after 400 ms, 1.5 s | 0.0% | 0.7% | 1.2% | 1.8% | 1.2% |
| T | baseline (today) | 0.0% | 0.0% | 0.0% | 0.0% | 0.0% |
| T | Pipecat-like: min_words = 1 | 6.3% | 8.6% | 10.7% | 13.0% | 10.3% |
| T | LiveKit-like, no resume: interrupt after 500 ms | 0.1% | 0.1% | 0.1% | 0.1% | 0.1% |
| T | LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% | 2.7% | 5.2% | 7.9% | 4.8% |
| T | pause(words) after 400 ms, 1.5 s | 0.0% | 2.7% | 5.1% | 7.8% | 4.7% |

## Full output

- command: `brisvox-bench denoise realistic --set fair --threads 12 --paths wideband,telephone --recognizers oracle,model,model+short-x:1,model+short-x:3,model+noise-words:0.1 --out FILE`
- brisvox 0.1.3 (release build), windows x86_64, 32 logical cores, 0 tokio workers (threads, not a CPU limit: other threads may use any core)
- set `fair`, in process, the real TurnMachine and InterruptionPolicy on the events of the real Silero VAD, seed 151
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

### What the calls are made of

- 5187 calls per path and recognizer, of which 4368 have background noise on the line (8 kinds) and 756 a second voice: 7 callers, 27 second voices; **185 distinct caller x speaker pairs** (72 of the same sex, 72 of different sexes, 41 where a sex is not known) and 626 distinct conditions (pair, recording and room).
- Second voices: 27; real noise recordings: 9; rooms: RVB2014_smallroom1 (0.20 s), RVB2014_mediumroom1 (0.45 s), RVB2014_largeroom1 (0.61 s), RVB2014_mediumroom2 (0.68 s), air_binaural_office_0 (0.62 s), air_binaural_meeting_0 (0.30 s), RWCP_cirline_jr1 (0.61 s), air_binaural_lecture_0 (0.86 s).


### oracle recognizer, wideband

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 13.1% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 196/525 (37.3%) | **6.3%** | 694 | 694 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 3.2% | 0/525 (0.0%) | 0/525 (0.0%) | 3/525 (0.6%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.3 | 310 / 546 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 500 | 698 / 878 |
| pause(words) after 0 ms, 1.5 s | 9.9 | 380 / 680 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 102 | 698 / 878 |
| pause(words) after 150 ms, 1.5 s | 9.9 | 340 / 624 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 150 | 698 / 878 |
| pause(words) after 250 ms, 1.5 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 250 | 698 / 878 |
| pause(words) after 400 ms, 1.5 s | 2.0 | 378 / 598 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 698 / 878 |
| pause(words) after 250 ms, 1.0 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 250 | 698 / 878 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 312 | 13.1% | 0.0% | 86.9% | - | - | - | 41/2940 |
| Pipecat-like: min_words = 1 | 314 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 314 | 3.2% | 0.0% | 96.8% | - | - | - | 10/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 291 | 0.0% | 13.1% | 86.9% | 310 | 546 | 666 | 0/2940 |
| pause(words) after 0 ms, 1.5 s | 291 | 0.0% | 100.0% | 0.0% | 380 | 680 | 1060 | 0/2940 |
| pause(words) after 150 ms, 1.5 s | 291 | 0.0% | 100.0% | 0.0% | 340 | 624 | 1016 | 0/2940 |
| pause(words) after 250 ms, 1.5 s | 291 | 0.0% | 88.0% | 12.0% | 248 | 532 | 916 | 0/2940 |
| pause(words) after 400 ms, 1.5 s | 291 | 0.0% | 19.9% | 80.1% | 378 | 598 | 766 | 0/2940 |
| pause(words) after 250 ms, 1.0 s | 291 | 0.0% | 88.0% | 12.0% | 248 | 532 | 916 | 0/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 50.0% (0.0%) n=4 | 19.9% (0.0%) n=166 | 0.0% (0.0%) n=1 | 7.1% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=4 | 0.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=4 | 6.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=4 | 0.0% (26.2%) n=145 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=4 | 0.0% (100.0%) n=145 | 0.0% (100.0%) n=1 | 0.0% (100.0%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=4 | 0.0% (100.0%) n=145 | 0.0% (100.0%) n=1 | 0.0% (100.0%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (75.0%) n=4 | 0.0% (90.3%) n=145 | 0.0% (100.0%) n=1 | 0.0% (87.1%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (50.0%) n=4 | 0.0% (34.5%) n=145 | 0.0% (0.0%) n=1 | 0.0% (7.1%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (75.0%) n=4 | 0.0% (90.3%) n=145 | 0.0% (100.0%) n=1 | 0.0% (87.1%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=3 | 7.6% (0.0%) n=66 | 10.1% (0.0%) n=109 | 17.0% (0.0%) n=88 | 21.7% (0.0%) n=46 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 0.0% (0.0%) n=89 | 0.0% (0.0%) n=46 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 6.7% (0.0%) n=89 | 8.7% (0.0%) n=46 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=3 | 0.0% (8.1%) n=62 | 0.0% (7.8%) n=102 | 0.0% (17.1%) n=82 | 0.0% (26.2%) n=42 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=3 | 0.0% (100.0%) n=62 | 0.0% (100.0%) n=102 | 0.0% (100.0%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=3 | 0.0% (100.0%) n=62 | 0.0% (100.0%) n=102 | 0.0% (100.0%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 250 ms, 1.5 s | 0.0% (33.3%) n=3 | 0.0% (83.9%) n=62 | 0.0% (90.2%) n=102 | 0.0% (90.2%) n=82 | 0.0% (88.1%) n=42 |
| pause(words) after 400 ms, 1.5 s | 0.0% (0.0%) n=3 | 0.0% (14.5%) n=62 | 0.0% (16.7%) n=102 | 0.0% (22.0%) n=82 | 0.0% (33.3%) n=42 |
| pause(words) after 250 ms, 1.0 s | 0.0% (33.3%) n=3 | 0.0% (83.9%) n=62 | 0.0% (90.2%) n=102 | 0.0% (90.2%) n=82 | 0.0% (88.1%) n=42 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 573 | 1070 | 3 | 400 | 400 | 400 | 400 | 390 | 882 |
| Pipecat-like: min_words = 1 | 1575 | 218 | 7 | 211 | 373 | 1127 | 229 | 694 | 710 | 694 | 710 | 695 | 990 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 3 | 0 | 3 | 534 | 1078 | 6 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 6 | 6 | 0 | 388 | 1084 | 0 | 500 | 500 | 698 | 878 | 710 | 930 |
| pause(words) after 0 ms, 1.5 s | 1575 | 0 | 0 | 0 | 450 | 1082 | 0 | 102 | 110 | 698 | 878 | 700 | 940 |
| pause(words) after 150 ms, 1.5 s | 1575 | 0 | 0 | 0 | 388 | 1082 | 0 | 150 | 150 | 698 | 878 | 700 | 930 |
| pause(words) after 250 ms, 1.5 s | 1575 | 0 | 0 | 0 | 388 | 1082 | 0 | 250 | 250 | 698 | 878 | 700 | 930 |
| pause(words) after 400 ms, 1.5 s | 1575 | 0 | 0 | 0 | 388 | 1085 | 0 | 400 | 400 | 698 | 878 | 700 | 930 |
| pause(words) after 250 ms, 1.0 s | 1575 | 2 | 2 | 0 | 522 | 1067 | 0 | 250 | 250 | 698 | 878 | 720 | 940 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 15/525 (0) | 196/525 (7) | 19/63 (0) | 184/1008 (1) | 8/504 (6) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 3/525 (0) | 0/63 (0) | 2/1008 (0) | 1/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 0/525 (0) | 0/525 (6) | 0/63 (0) | 0/1008 (0) | 0/504 (6) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 0/525 (0) | 0/525 (2) | 0/63 (0) | 0/1008 (2) | 0/504 (0) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 100.0% (0.0%) n=63 | 92.5% (0.0%) n=67 | 85.7% (0.0%) n=112 | 91.2% (0.0%) n=57 | 97.3% (0.0%) n=451 | - | - | - |
| Pipecat-like: min_words = 1 | 55.3% (0.0%) n=76 | 51.2% (0.0%) n=80 | 0.0% (0.0%) n=151 | 47.4% (0.0%) n=76 | 51.3% (0.0%) n=491 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 100.0% (0.0%) n=63 | 88.4% (0.0%) n=69 | 79.5% (0.0%) n=112 | 78.7% (0.0%) n=61 | 93.4% (0.0%) n=456 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 86.1% (13.9%) n=72 | 73.6% (20.8%) n=72 | 18.4% (66.7%) n=114 | 75.0% (23.3%) n=60 | 84.5% (10.8%) n=464 | 990 | 1875 | 2000 |
| pause(words) after 0 ms, 1.5 s | 94.0% (6.0%) n=67 | 84.1% (15.9%) n=69 | 47.3% (42.0%) n=112 | 81.4% (16.9%) n=59 | 86.6% (12.1%) n=462 | 880 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 91.3% (8.7%) n=69 | 79.2% (20.8%) n=72 | 44.5% (45.4%) n=119 | 78.3% (21.7%) n=60 | 86.4% (12.1%) n=462 | 924 | 1500 | 1500 |
| pause(words) after 250 ms, 1.5 s | 91.3% (8.7%) n=69 | 79.2% (20.8%) n=72 | 43.4% (45.1%) n=113 | 78.3% (21.7%) n=60 | 85.9% (12.1%) n=462 | 824 | 1424 | 1500 |
| pause(words) after 400 ms, 1.5 s | 88.7% (11.3%) n=71 | 76.7% (19.2%) n=73 | 37.2% (50.4%) n=113 | 76.7% (21.7%) n=60 | 85.9% (10.6%) n=462 | 870 | 1500 | 1500 |
| pause(words) after 250 ms, 1.0 s | 100.0% (0.0%) n=63 | 88.1% (11.9%) n=67 | 56.8% (32.4%) n=111 | 87.7% (12.3%) n=57 | 88.9% (10.5%) n=458 | 600 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 59/63 | 63/63 | 48/63 | 2/420 | 33/420 | 0/420 | 6/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| Pipecat-like: min_words = 1 | 42/63 | 41/63 | 0/63 | 36/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 252/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 59/63 | 60/63 | 47/63 | 0/420 | 10/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 416/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 62/63 | 53/63 | 21/63 | 42/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 392/420 |
| pause(words) after 0 ms, 1.5 s | 63/63 | 58/63 | 43/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 400/420 |
| pause(words) after 150 ms, 1.5 s | 63/63 | 57/63 | 43/63 | 44/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 399/420 |
| pause(words) after 250 ms, 1.5 s | 63/63 | 57/63 | 42/63 | 44/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 397/420 |
| pause(words) after 400 ms, 1.5 s | 63/63 | 56/63 | 42/63 | 43/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 397/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 59/63 | 53/63 | 47/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 405/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 255 | far-field recording | -10 dB | 3.75 s | 6.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 2699 | far-field recording | -6 dB | 3.75 s | 6.16 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 4701 | far-field recording | -10 dB | 4.74 s | 7.14 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 475 | far-field recording | -14 dB | 3.75 s | 6.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1958 | far-field recording | -6 dB | 4.74 s | 7.18 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 3860 | babble | -30 dB | 10.95 s | 12.30 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 1239 | loudspeaker in a room | -18 dB | 7.75 s | 7.98 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2480 | Turkish broadcast | -18 dB | 4.87 s | 6.28 s | 1406 | resumed |
| pause(words) after 150 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2965 | dry second voice | -18 dB | 7.84 s | 9.36 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 745 | loudspeaker in a room | -18 dB | 7.71 s | 9.22 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 742 | dry second voice | -18 dB | 7.81 s | 9.28 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2230 | far-field recording | -18 dB | 7.97 s | 9.00 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 4355 | babble | -30 dB | 9.38 s | 10.54 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2626 | babble | -30 dB | 8.13 s | 9.80 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 426 | far-field recording | -6 dB | 4.99 s | 6.56 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2724 | far-field recording | -18 dB | 7.97 s | 9.70 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 749 | far-field recording | -10 dB | 7.71 s | 9.48 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3390 | far-field recording | -6 dB | 4.23 s | 6.08 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 11.08 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4355 | babble | -30 dB | 9.38 s | 10.54 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3466 | far-field recording | -10 dB | 7.68 s | 9.54 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 255 | far-field recording | -10 dB | 7.55 s | 8.64 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 402 | babble | -30 dB | 4.77 s | 5.96 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2624 | babble | -30 dB | 9.41 s | 10.64 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 3199 | cough | -14 dB | 8.99 s | 9.44 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2870 | babble | -30 dB | 12.87 s | 13.98 s | 1000 | resumed |


### oracle recognizer, telephone

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 18.9% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 14/525 (2.7%) | 198/525 (37.7%) | **6.3%** | 460 | 460 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 4.9% | 0/525 (0.0%) | 0/525 (0.0%) | 4/525 (0.8%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.9 | 290 / 578 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 500 | 698 / 878 |
| pause(words) after 0 ms, 1.5 s | 9.5 | 400 / 680 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 102 | 698 / 878 |
| pause(words) after 150 ms, 1.5 s | 9.5 | 348 / 640 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 150 | 698 / 878 |
| pause(words) after 250 ms, 1.5 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 250 | 698 / 878 |
| pause(words) after 400 ms, 1.5 s | 2.5 | 374 / 666 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 698 / 878 |
| pause(words) after 250 ms, 1.0 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 250 | 698 / 878 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 302 | 18.9% | 0.0% | 81.1% | - | - | - | 57/2940 |
| Pipecat-like: min_words = 1 | 307 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 305 | 4.9% | 0.0% | 95.1% | - | - | - | 15/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 280 | 0.0% | 20.4% | 79.6% | 290 | 578 | 666 | 0/2940 |
| pause(words) after 0 ms, 1.5 s | 280 | 0.0% | 100.0% | 0.0% | 400 | 680 | 1060 | 0/2940 |
| pause(words) after 150 ms, 1.5 s | 280 | 0.0% | 100.0% | 0.0% | 348 | 640 | 1016 | 0/2940 |
| pause(words) after 250 ms, 1.5 s | 280 | 0.0% | 86.1% | 13.9% | 276 | 560 | 916 | 0/2940 |
| pause(words) after 400 ms, 1.5 s | 280 | 0.0% | 26.4% | 73.6% | 374 | 666 | 766 | 0/2940 |
| pause(words) after 250 ms, 1.0 s | 280 | 0.0% | 86.1% | 13.9% | 276 | 560 | 916 | 0/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 37.5% (0.0%) n=8 | 27.4% (0.0%) n=179 | 0.0% (0.0%) n=2 | 10.2% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=184 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 8.2% (0.0%) n=182 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (12.5%) n=8 | 0.0% (35.7%) n=157 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=8 | 0.0% (100.0%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=8 | 0.0% (100.0%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (87.5%) n=8 | 0.0% (91.7%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (37.5%) n=8 | 0.0% (42.0%) n=157 | 0.0% (0.0%) n=2 | 0.0% (10.2%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (87.5%) n=8 | 0.0% (91.7%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=8 | 10.7% (0.0%) n=75 | 16.0% (0.0%) n=100 | 23.6% (0.0%) n=72 | 34.0% (0.0%) n=47 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=77 | 0.0% (0.0%) n=100 | 0.0% (0.0%) n=74 | 0.0% (0.0%) n=48 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 1.3% (0.0%) n=77 | 0.0% (0.0%) n=100 | 12.3% (0.0%) n=73 | 10.6% (0.0%) n=47 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (14.3%) n=7 | 0.0% (14.7%) n=68 | 0.0% (17.6%) n=91 | 0.0% (26.9%) n=67 | 0.0% (25.5%) n=47 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=7 | 0.0% (100.0%) n=68 | 0.0% (100.0%) n=91 | 0.0% (100.0%) n=67 | 0.0% (100.0%) n=47 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=7 | 0.0% (100.0%) n=68 | 0.0% (100.0%) n=91 | 0.0% (100.0%) n=67 | 0.0% (100.0%) n=47 |
| pause(words) after 250 ms, 1.5 s | 0.0% (57.1%) n=7 | 0.0% (79.4%) n=68 | 0.0% (91.2%) n=91 | 0.0% (83.6%) n=67 | 0.0% (93.6%) n=47 |
| pause(words) after 400 ms, 1.5 s | 0.0% (14.3%) n=7 | 0.0% (20.6%) n=68 | 0.0% (26.4%) n=91 | 0.0% (28.4%) n=67 | 0.0% (34.0%) n=47 |
| pause(words) after 250 ms, 1.0 s | 0.0% (57.1%) n=7 | 0.0% (79.4%) n=68 | 0.0% (91.2%) n=91 | 0.0% (83.6%) n=67 | 0.0% (93.6%) n=47 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 524 | 1112 | 7 | 400 | 400 | 400 | 400 | 390 | 880 |
| Pipecat-like: min_words = 1 | 1575 | 219 | 7 | 212 | 352 | 1175 | 234 | 460 | 710 | 460 | 710 | 695 | 990 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 4 | 0 | 4 | 468 | 1119 | 11 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 3 | 3 | 0 | 375 | 1127 | 0 | 500 | 500 | 698 | 878 | 710 | 940 |
| pause(words) after 0 ms, 1.5 s | 1575 | 0 | 0 | 0 | 411 | 1123 | 0 | 102 | 110 | 698 | 878 | 700 | 940 |
| pause(words) after 150 ms, 1.5 s | 1575 | 0 | 0 | 0 | 375 | 1124 | 0 | 150 | 150 | 698 | 878 | 700 | 940 |
| pause(words) after 250 ms, 1.5 s | 1575 | 1 | 1 | 0 | 375 | 1126 | 0 | 250 | 250 | 698 | 878 | 700 | 940 |
| pause(words) after 400 ms, 1.5 s | 1575 | 0 | 0 | 0 | 375 | 1127 | 0 | 400 | 400 | 698 | 878 | 700 | 940 |
| pause(words) after 250 ms, 1.0 s | 1575 | 3 | 3 | 0 | 455 | 1117 | 0 | 250 | 250 | 698 | 878 | 710 | 950 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 14/525 (0) | 198/525 (7) | 20/63 (0) | 179/1008 (2) | 13/504 (5) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 4/525 (0) | 0/63 (0) | 2/1008 (0) | 2/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 0/525 (0) | 0/525 (3) | 0/63 (0) | 0/1008 (0) | 0/504 (3) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 0/525 (0) | 0/525 (1) | 0/63 (0) | 0/1008 (0) | 0/504 (1) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 0/525 (0) | 0/525 (3) | 0/63 (0) | 0/1008 (2) | 0/504 (1) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 96.9% (0.0%) n=65 | 85.1% (0.0%) n=74 | 76.7% (0.0%) n=116 | 86.2% (0.0%) n=65 | 97.8% (0.0%) n=445 | - | - | - |
| Pipecat-like: min_words = 1 | 53.8% (0.0%) n=78 | 48.2% (0.0%) n=85 | 0.0% (0.0%) n=150 | 45.2% (0.0%) n=84 | 52.8% (0.0%) n=477 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 96.9% (0.0%) n=65 | 78.9% (0.0%) n=76 | 60.0% (0.0%) n=120 | 68.5% (0.0%) n=73 | 95.5% (0.0%) n=447 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 86.1% (13.9%) n=72 | 69.3% (21.3%) n=75 | 6.7% (59.7%) n=134 | 76.5% (14.7%) n=68 | 86.6% (10.5%) n=449 | 782 | 1666 | 2000 |
| pause(words) after 0 ms, 1.5 s | 94.0% (6.0%) n=67 | 73.0% (27.0%) n=74 | 24.1% (57.1%) n=133 | 74.3% (21.4%) n=70 | 89.0% (10.8%) n=446 | 780 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 91.3% (8.7%) n=69 | 73.0% (27.0%) n=74 | 24.1% (57.1%) n=133 | 75.4% (20.3%) n=69 | 87.5% (12.1%) n=448 | 788 | 1456 | 1500 |
| pause(words) after 250 ms, 1.5 s | 91.3% (8.7%) n=69 | 73.0% (25.7%) n=74 | 20.3% (59.4%) n=133 | 75.4% (20.3%) n=69 | 87.5% (11.6%) n=448 | 792 | 1420 | 1500 |
| pause(words) after 400 ms, 1.5 s | 88.7% (11.3%) n=71 | 72.0% (21.3%) n=75 | 15.8% (54.9%) n=133 | 77.9% (17.6%) n=68 | 87.3% (10.7%) n=448 | 762 | 1366 | 1500 |
| pause(words) after 250 ms, 1.0 s | 100.0% (0.0%) n=63 | 79.2% (19.4%) n=72 | 38.0% (51.2%) n=129 | 82.1% (16.4%) n=67 | 90.4% (9.2%) n=446 | 560 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 58/63 | 56/63 | 49/63 | 3/420 | 49/420 | 0/420 | 5/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| Pipecat-like: min_words = 1 | 42/63 | 40/63 | 0/63 | 37/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 251/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 57/63 | 51/63 | 46/63 | 0/420 | 15/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 411/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 62/63 | 50/63 | 9/63 | 42/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 387/420 |
| pause(words) after 0 ms, 1.5 s | 63/63 | 52/63 | 24/63 | 42/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 394/420 |
| pause(words) after 150 ms, 1.5 s | 63/63 | 52/63 | 24/63 | 42/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 390/420 |
| pause(words) after 250 ms, 1.5 s | 63/63 | 52/63 | 24/63 | 42/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 390/420 |
| pause(words) after 400 ms, 1.5 s | 63/63 | 52/63 | 21/63 | 43/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 389/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 55/63 | 40/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 399/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 969 | far-field recording | -14 dB | 7.68 s | 10.02 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 2871 | babble | -30 dB | 8.64 s | 11.04 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 4702 | far-field recording | -4 dB | 4.23 s | 4.74 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 722 | far-field recording | -14 dB | 8.23 s | 10.54 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3192 | far-field recording | -14 dB | 7.71 s | 10.12 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 1980 | loudspeaker in a room | -18 dB | 8.07 s | 9.48 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 1486 | loudspeaker in a room | -18 dB | 11.11 s | 12.52 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 1192 | far-field recording | -6 dB | 4.80 s | 6.18 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3959 | far-field recording | -18 dB | 4.80 s | 6.16 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3415 | far-field recording | -6 dB | 4.80 s | 6.22 s | 1402 | resumed |
| pause(words) after 150 ms, 1.5 s | 3459 | dry second voice | -18 dB | 7.84 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 745 | loudspeaker in a room | -18 dB | 7.71 s | 9.22 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 1236 | dry second voice | -18 dB | 7.81 s | 9.28 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 742 | dry second voice | -18 dB | 7.81 s | 9.28 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2965 | dry second voice | -18 dB | 7.84 s | 9.36 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2972 | far-field recording | -10 dB | 7.71 s | 9.44 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 4207 | far-field recording | -10 dB | 9.25 s | 10.98 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 10.96 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2129 | babble | -30 dB | 13.06 s | 14.62 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 723 | far-field recording | -6 dB | 7.75 s | 9.44 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 2972 | far-field recording | -10 dB | 7.71 s | 9.44 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4207 | far-field recording | -10 dB | 9.25 s | 10.98 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 749 | far-field recording | -10 dB | 7.65 s | 9.42 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 2224 | dry second voice | -18 dB | 9.67 s | 11.50 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 10.96 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 251 | loudspeaker in a room | -18 dB | 7.65 s | 8.10 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 4848 | babble | -30 dB | 6.69 s | 7.76 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 996 | far-field recording | -10 dB | 8.07 s | 9.12 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 1439 | far-field recording | -6 dB | 7.84 s | 8.10 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 3933 | far-field recording | -14 dB | 9.28 s | 10.34 s | 1000 | resumed |


### modelled recognizer (short utterances missed x2), wideband

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 13.1% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 29/525 (5.5%) | 208/525 (39.6%) | **7.3%** | 694 | 694 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 3.2% | 0/525 (0.0%) | 0/525 (0.0%) | 2/525 (0.4%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.3 | 310 / 546 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 500 | 698 / 882 |
| pause(words) after 0 ms, 1.5 s | 9.9 | 380 / 680 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 102 | 698 / 882 |
| pause(words) after 150 ms, 1.5 s | 9.9 | 340 / 624 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 150 | 698 / 882 |
| pause(words) after 250 ms, 1.5 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 250 | 698 / 882 |
| pause(words) after 400 ms, 1.5 s | 2.0 | 378 / 598 | 0.0% | 0/525 (0.0%) | 15/525 (2.9%) | 18/525 (3.4%) | **1.2%** | 400 | 698 / 882 |
| pause(words) after 250 ms, 1.0 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 14/525 (2.7%) | 18/525 (3.4%) | **1.2%** | 250 | 698 / 882 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 312 | 13.1% | 0.0% | 86.9% | - | - | - | 41/2940 |
| Pipecat-like: min_words = 1 | 314 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 314 | 3.2% | 0.0% | 96.8% | - | - | - | 10/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 291 | 0.0% | 13.1% | 86.9% | 310 | 546 | 666 | 0/2940 |
| pause(words) after 0 ms, 1.5 s | 291 | 0.0% | 100.0% | 0.0% | 380 | 680 | 1060 | 0/2940 |
| pause(words) after 150 ms, 1.5 s | 291 | 0.0% | 100.0% | 0.0% | 340 | 624 | 1016 | 0/2940 |
| pause(words) after 250 ms, 1.5 s | 291 | 0.0% | 88.0% | 12.0% | 248 | 532 | 916 | 0/2940 |
| pause(words) after 400 ms, 1.5 s | 291 | 0.0% | 19.9% | 80.1% | 378 | 598 | 766 | 0/2940 |
| pause(words) after 250 ms, 1.0 s | 291 | 0.0% | 88.0% | 12.0% | 248 | 532 | 916 | 0/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 50.0% (0.0%) n=4 | 19.9% (0.0%) n=166 | 0.0% (0.0%) n=1 | 7.1% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=4 | 0.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=4 | 6.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=4 | 0.0% (26.2%) n=145 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=4 | 0.0% (100.0%) n=145 | 0.0% (100.0%) n=1 | 0.0% (100.0%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=4 | 0.0% (100.0%) n=145 | 0.0% (100.0%) n=1 | 0.0% (100.0%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (75.0%) n=4 | 0.0% (90.3%) n=145 | 0.0% (100.0%) n=1 | 0.0% (87.1%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (50.0%) n=4 | 0.0% (34.5%) n=145 | 0.0% (0.0%) n=1 | 0.0% (7.1%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (75.0%) n=4 | 0.0% (90.3%) n=145 | 0.0% (100.0%) n=1 | 0.0% (87.1%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=3 | 7.6% (0.0%) n=66 | 10.1% (0.0%) n=109 | 17.0% (0.0%) n=88 | 21.7% (0.0%) n=46 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 0.0% (0.0%) n=89 | 0.0% (0.0%) n=46 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 6.7% (0.0%) n=89 | 8.7% (0.0%) n=46 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=3 | 0.0% (8.1%) n=62 | 0.0% (7.8%) n=102 | 0.0% (17.1%) n=82 | 0.0% (26.2%) n=42 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=3 | 0.0% (100.0%) n=62 | 0.0% (100.0%) n=102 | 0.0% (100.0%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=3 | 0.0% (100.0%) n=62 | 0.0% (100.0%) n=102 | 0.0% (100.0%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 250 ms, 1.5 s | 0.0% (33.3%) n=3 | 0.0% (83.9%) n=62 | 0.0% (90.2%) n=102 | 0.0% (90.2%) n=82 | 0.0% (88.1%) n=42 |
| pause(words) after 400 ms, 1.5 s | 0.0% (0.0%) n=3 | 0.0% (14.5%) n=62 | 0.0% (16.7%) n=102 | 0.0% (22.0%) n=82 | 0.0% (33.3%) n=42 |
| pause(words) after 250 ms, 1.0 s | 0.0% (33.3%) n=3 | 0.0% (83.9%) n=62 | 0.0% (90.2%) n=102 | 0.0% (90.2%) n=82 | 0.0% (88.1%) n=42 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 573 | 1070 | 3 | 400 | 400 | 400 | 400 | 390 | 882 |
| Pipecat-like: min_words = 1 | 1575 | 250 | 13 | 237 | 326 | 1149 | 258 | 694 | 710 | 694 | 710 | 690 | 1010 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 2 | 0 | 2 | 534 | 1078 | 5 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 39 | 6 | 33 | 330 | 1100 | 33 | 500 | 500 | 698 | 882 | 700 | 940 |
| pause(words) after 0 ms, 1.5 s | 1575 | 33 | 0 | 33 | 414 | 1098 | 33 | 102 | 110 | 698 | 882 | 700 | 950 |
| pause(words) after 150 ms, 1.5 s | 1575 | 33 | 0 | 33 | 346 | 1101 | 35 | 150 | 150 | 698 | 882 | 700 | 940 |
| pause(words) after 250 ms, 1.5 s | 1575 | 33 | 0 | 33 | 344 | 1098 | 33 | 250 | 250 | 698 | 882 | 700 | 940 |
| pause(words) after 400 ms, 1.5 s | 1575 | 33 | 0 | 33 | 330 | 1101 | 33 | 400 | 400 | 698 | 882 | 700 | 940 |
| pause(words) after 250 ms, 1.0 s | 1575 | 34 | 2 | 32 | 484 | 1075 | 32 | 250 | 250 | 698 | 882 | 720 | 980 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 29/525 (3) | 208/525 (10) | 21/63 (0) | 203/1008 (0) | 13/504 (13) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 2/525 (0) | 0/63 (0) | 2/1008 (0) | 0/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 15/525 (0) | 18/525 (6) | 6/63 (0) | 26/1008 (0) | 1/504 (6) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 15/525 (0) | 18/525 (0) | 6/63 (0) | 26/1008 (0) | 1/504 (0) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 15/525 (0) | 18/525 (0) | 6/63 (0) | 26/1008 (0) | 1/504 (0) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 15/525 (0) | 18/525 (0) | 6/63 (0) | 26/1008 (0) | 1/504 (0) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 15/525 (0) | 18/525 (0) | 6/63 (0) | 26/1008 (0) | 1/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 14/525 (0) | 18/525 (2) | 6/63 (0) | 26/1008 (2) | 0/504 (0) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 100.0% (0.0%) n=63 | 92.5% (0.0%) n=67 | 85.7% (0.0%) n=112 | 91.2% (0.0%) n=57 | 97.3% (0.0%) n=451 | - | - | - |
| Pipecat-like: min_words = 1 | 85.7% (0.0%) n=70 | 72.7% (0.0%) n=77 | 0.0% (0.0%) n=151 | 41.9% (0.0%) n=86 | 96.5% (0.0%) n=452 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 100.0% (0.0%) n=63 | 91.2% (0.0%) n=68 | 79.5% (0.0%) n=112 | 80.3% (0.0%) n=61 | 96.5% (0.0%) n=452 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 91.0% (7.5%) n=67 | 83.3% (11.1%) n=72 | 18.4% (66.7%) n=114 | 67.2% (26.9%) n=67 | 100.0% (0.0%) n=448 | 894 | 1822 | 2000 |
| pause(words) after 0 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 47.3% (42.0%) n=112 | 73.8% (26.2%) n=65 | 100.0% (0.0%) n=448 | 1040 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 44.5% (45.4%) n=119 | 71.2% (28.8%) n=66 | 100.0% (0.0%) n=448 | 1120 | 1500 | 1500 |
| pause(words) after 250 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 43.4% (45.1%) n=113 | 71.2% (28.8%) n=66 | 100.0% (0.0%) n=448 | 1012 | 1364 | 1500 |
| pause(words) after 400 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (11.1%) n=72 | 37.2% (50.4%) n=113 | 69.7% (28.8%) n=66 | 100.0% (0.0%) n=448 | 886 | 1500 | 1500 |
| pause(words) after 250 ms, 1.0 s | 96.9% (3.1%) n=65 | 89.9% (10.1%) n=69 | 56.8% (32.4%) n=111 | 75.0% (25.0%) n=64 | 100.0% (0.0%) n=448 | 724 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 59/63 | 63/63 | 48/63 | 2/420 | 33/420 | 0/420 | 6/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| Pipecat-like: min_words = 1 | 60/63 | 54/63 | 0/63 | 36/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 59/63 | 60/63 | 48/63 | 0/420 | 10/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 61/63 | 57/63 | 21/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 0 ms, 1.5 s | 62/63 | 58/63 | 43/63 | 48/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 150 ms, 1.5 s | 62/63 | 58/63 | 43/63 | 47/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 250 ms, 1.5 s | 62/63 | 58/63 | 42/63 | 47/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 400 ms, 1.5 s | 62/63 | 58/63 | 42/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 59/63 | 53/63 | 48/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 475 | far-field recording | -14 dB | 3.75 s | 6.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1738 | far-field recording | -4 dB | 4.74 s | 7.14 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3219 | far-field recording | -10 dB | 4.71 s | 7.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 4702 | far-field recording | -4 dB | 4.16 s | 6.60 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 255 | far-field recording | -10 dB | 3.75 s | 6.12 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 1239 | loudspeaker in a room | -18 dB | 7.75 s | 7.98 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2480 | Turkish broadcast | -18 dB | 4.87 s | 6.28 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 918 | loudspeaker in a room | -6 dB | 8.00 s | 8.52 s | 1402 | resumed |
| pause(words) after 150 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3961 | far-field recording | -4 dB | 5.44 s | 7.02 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 426 | far-field recording | -6 dB | 4.99 s | 6.56 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3891 | keyboard | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1736 | far-field recording | -18 dB | 5.67 s | 7.34 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2230 | far-field recording | -18 dB | 7.97 s | 9.00 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 426 | far-field recording | -6 dB | 4.99 s | 6.56 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.75 s | 9.48 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2724 | far-field recording | -18 dB | 7.97 s | 9.70 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 749 | far-field recording | -10 dB | 7.71 s | 9.48 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 502 | far-field recording | -10 dB | 8.23 s | 10.06 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 2972 | far-field recording | -10 dB | 7.68 s | 9.54 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4131 | far-field recording | -6 dB | 4.71 s | 6.56 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 1167 | far-field recording | -6 dB | 4.71 s | 6.56 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 428 | Turkish broadcast | -6 dB | 3.65 s | 4.84 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 206 | Turkish broadcast | -6 dB | 8.10 s | 9.32 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2971 | far-field recording | -18 dB | 7.91 s | 8.26 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 3199 | cough | -14 dB | 8.99 s | 9.44 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 959 | music | -14 dB | 9.31 s | 10.02 s | 1000 | resumed |


### modelled recognizer (short utterances missed x2), telephone

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 18.9% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 86/525 (16.4%) | 230/525 (43.8%) | **10.7%** | 460 | 460 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 4.9% | 0/525 (0.0%) | 0/525 (0.0%) | 3/525 (0.6%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.9 | 290 / 578 | 0.0% | 0/525 (0.0%) | 72/525 (13.7%) | 61/525 (11.6%) | **5.2%** | 500 | 698 / 906 |
| pause(words) after 0 ms, 1.5 s | 9.5 | 400 / 680 | 0.0% | 0/525 (0.0%) | 71/525 (13.5%) | 61/525 (11.6%) | **5.1%** | 102 | 698 / 906 |
| pause(words) after 150 ms, 1.5 s | 9.5 | 348 / 640 | 0.0% | 0/525 (0.0%) | 71/525 (13.5%) | 62/525 (11.8%) | **5.2%** | 150 | 698 / 906 |
| pause(words) after 250 ms, 1.5 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 71/525 (13.5%) | 61/525 (11.6%) | **5.1%** | 250 | 698 / 906 |
| pause(words) after 400 ms, 1.5 s | 2.5 | 374 / 666 | 0.0% | 0/525 (0.0%) | 71/525 (13.5%) | 61/525 (11.6%) | **5.1%** | 400 | 698 / 906 |
| pause(words) after 250 ms, 1.0 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 69/525 (13.1%) | 60/525 (11.4%) | **5.0%** | 250 | 698 / 906 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 302 | 18.9% | 0.0% | 81.1% | - | - | - | 57/2940 |
| Pipecat-like: min_words = 1 | 307 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 305 | 4.9% | 0.0% | 95.1% | - | - | - | 15/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 280 | 0.0% | 20.4% | 79.6% | 290 | 578 | 666 | 0/2940 |
| pause(words) after 0 ms, 1.5 s | 280 | 0.0% | 100.0% | 0.0% | 400 | 680 | 1060 | 0/2940 |
| pause(words) after 150 ms, 1.5 s | 280 | 0.0% | 100.0% | 0.0% | 348 | 640 | 1016 | 0/2940 |
| pause(words) after 250 ms, 1.5 s | 280 | 0.0% | 86.1% | 13.9% | 276 | 560 | 916 | 0/2940 |
| pause(words) after 400 ms, 1.5 s | 280 | 0.0% | 26.4% | 73.6% | 374 | 666 | 766 | 0/2940 |
| pause(words) after 250 ms, 1.0 s | 280 | 0.0% | 86.1% | 13.9% | 276 | 560 | 916 | 0/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 37.5% (0.0%) n=8 | 27.4% (0.0%) n=179 | 0.0% (0.0%) n=2 | 10.2% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=184 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 8.2% (0.0%) n=182 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (12.5%) n=8 | 0.0% (35.7%) n=157 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=8 | 0.0% (100.0%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=8 | 0.0% (100.0%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (87.5%) n=8 | 0.0% (91.7%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (37.5%) n=8 | 0.0% (42.0%) n=157 | 0.0% (0.0%) n=2 | 0.0% (10.2%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (87.5%) n=8 | 0.0% (91.7%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=8 | 10.7% (0.0%) n=75 | 16.0% (0.0%) n=100 | 23.6% (0.0%) n=72 | 34.0% (0.0%) n=47 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=77 | 0.0% (0.0%) n=100 | 0.0% (0.0%) n=74 | 0.0% (0.0%) n=48 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 1.3% (0.0%) n=77 | 0.0% (0.0%) n=100 | 12.3% (0.0%) n=73 | 10.6% (0.0%) n=47 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (14.3%) n=7 | 0.0% (14.7%) n=68 | 0.0% (17.6%) n=91 | 0.0% (26.9%) n=67 | 0.0% (25.5%) n=47 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=7 | 0.0% (100.0%) n=68 | 0.0% (100.0%) n=91 | 0.0% (100.0%) n=67 | 0.0% (100.0%) n=47 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=7 | 0.0% (100.0%) n=68 | 0.0% (100.0%) n=91 | 0.0% (100.0%) n=67 | 0.0% (100.0%) n=47 |
| pause(words) after 250 ms, 1.5 s | 0.0% (57.1%) n=7 | 0.0% (79.4%) n=68 | 0.0% (91.2%) n=91 | 0.0% (83.6%) n=67 | 0.0% (93.6%) n=47 |
| pause(words) after 400 ms, 1.5 s | 0.0% (14.3%) n=7 | 0.0% (20.6%) n=68 | 0.0% (26.4%) n=91 | 0.0% (28.4%) n=67 | 0.0% (34.0%) n=47 |
| pause(words) after 250 ms, 1.0 s | 0.0% (57.1%) n=7 | 0.0% (79.4%) n=68 | 0.0% (91.2%) n=91 | 0.0% (83.6%) n=67 | 0.0% (93.6%) n=47 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 524 | 1112 | 7 | 400 | 400 | 400 | 400 | 390 | 880 |
| Pipecat-like: min_words = 1 | 1575 | 329 | 13 | 316 | 311 | 1206 | 353 | 460 | 710 | 460 | 710 | 695 | 1030 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 3 | 0 | 3 | 464 | 1119 | 10 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 140 | 7 | 133 | 318 | 1149 | 139 | 500 | 500 | 698 | 906 | 715 | 1030 |
| pause(words) after 0 ms, 1.5 s | 1575 | 134 | 2 | 132 | 366 | 1142 | 136 | 102 | 110 | 698 | 906 | 720 | 1030 |
| pause(words) after 150 ms, 1.5 s | 1575 | 135 | 2 | 133 | 329 | 1150 | 143 | 150 | 150 | 698 | 906 | 705 | 1030 |
| pause(words) after 250 ms, 1.5 s | 1575 | 135 | 3 | 132 | 329 | 1149 | 140 | 250 | 250 | 698 | 906 | 710 | 1030 |
| pause(words) after 400 ms, 1.5 s | 1575 | 134 | 2 | 132 | 318 | 1149 | 139 | 400 | 400 | 698 | 906 | 705 | 1030 |
| pause(words) after 250 ms, 1.0 s | 1575 | 132 | 3 | 129 | 403 | 1132 | 133 | 250 | 250 | 698 | 906 | 720 | 1030 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 86/525 (3) | 230/525 (10) | 26/63 (0) | 262/1008 (0) | 28/504 (13) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 3/525 (0) | 0/63 (0) | 2/1008 (0) | 1/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 72/525 (1) | 61/525 (6) | 14/63 (0) | 107/1008 (2) | 12/504 (5) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 71/525 (0) | 61/525 (2) | 14/63 (0) | 107/1008 (1) | 11/504 (1) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 71/525 (0) | 62/525 (2) | 14/63 (0) | 107/1008 (1) | 12/504 (1) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 71/525 (1) | 61/525 (2) | 14/63 (0) | 107/1008 (2) | 11/504 (1) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 71/525 (1) | 61/525 (1) | 14/63 (0) | 107/1008 (2) | 11/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 69/525 (0) | 60/525 (3) | 14/63 (0) | 107/1008 (2) | 8/504 (1) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 96.9% (0.0%) n=65 | 85.1% (0.0%) n=74 | 76.7% (0.0%) n=116 | 86.2% (0.0%) n=65 | 97.8% (0.0%) n=445 | - | - | - |
| Pipecat-like: min_words = 1 | 83.3% (0.0%) n=72 | 67.1% (0.0%) n=85 | 0.0% (0.0%) n=150 | 37.6% (0.0%) n=93 | 97.1% (0.0%) n=446 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 96.9% (0.0%) n=65 | 79.2% (0.0%) n=77 | 60.0% (0.0%) n=120 | 69.4% (0.0%) n=72 | 97.1% (0.0%) n=446 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 91.0% (7.5%) n=67 | 80.3% (9.2%) n=76 | 6.7% (59.7%) n=134 | 65.4% (19.2%) n=78 | 99.8% (0.0%) n=442 | 762 | 1822 | 2000 |
| pause(words) after 0 ms, 1.5 s | 92.5% (7.5%) n=67 | 80.5% (18.2%) n=77 | 24.1% (57.1%) n=133 | 65.8% (32.9%) n=79 | 99.8% (0.2%) n=442 | 700 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (17.1%) n=76 | 24.1% (57.1%) n=133 | 68.8% (31.2%) n=77 | 99.8% (0.2%) n=442 | 632 | 1332 | 1460 |
| pause(words) after 250 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (15.8%) n=76 | 20.3% (59.4%) n=133 | 66.7% (33.3%) n=78 | 99.8% (0.2%) n=442 | 560 | 1304 | 1500 |
| pause(words) after 400 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (10.5%) n=76 | 15.8% (54.9%) n=133 | 68.8% (26.0%) n=77 | 99.8% (0.2%) n=442 | 710 | 1222 | 1500 |
| pause(words) after 250 ms, 1.0 s | 96.9% (3.1%) n=65 | 82.7% (14.7%) n=75 | 38.0% (51.2%) n=129 | 68.8% (31.2%) n=77 | 99.8% (0.2%) n=442 | 500 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 58/63 | 56/63 | 49/63 | 3/420 | 49/420 | 0/420 | 5/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| Pipecat-like: min_words = 1 | 60/63 | 54/63 | 0/63 | 35/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 57/63 | 51/63 | 46/63 | 0/420 | 15/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 61/63 | 56/63 | 9/63 | 44/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 0 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 150 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 250 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 400 ms, 1.5 s | 62/63 | 57/63 | 21/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 57/63 | 40/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 969 | far-field recording | -14 dB | 7.68 s | 10.02 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3686 | far-field recording | -14 dB | 7.75 s | 10.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 4702 | far-field recording | -4 dB | 4.23 s | 4.74 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1741 | Turkish broadcast | -4 dB | 6.85 s | 7.52 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 722 | far-field recording | -14 dB | 8.23 s | 10.54 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 249 | dry second voice | -10 dB | 5.03 s | 6.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 3169 | Turkish broadcast | -14 dB | 9.44 s | 10.80 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3415 | far-field recording | -6 dB | 4.80 s | 6.22 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 1192 | far-field recording | -6 dB | 4.80 s | 6.18 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3959 | far-field recording | -18 dB | 4.80 s | 6.16 s | 1402 | resumed |
| pause(words) after 150 ms, 1.5 s | 4178 | loudspeaker in a room | -14 dB | 5.06 s | 5.80 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3893 | door | -22 dB | 11.49 s | 13.04 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 200 | dry second voice | -6 dB | 8.29 s | 9.80 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4427 | far-field recording | -14 dB | 9.28 s | 9.68 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3897 | music | -22 dB | 11.46 s | 13.04 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 4131 | far-field recording | -6 dB | 4.74 s | 6.48 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 10.96 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1167 | far-field recording | -6 dB | 4.74 s | 6.48 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.68 s | 9.42 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 4146 | street | -22 dB | 8.45 s | 10.16 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4131 | far-field recording | -6 dB | 4.74 s | 6.48 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 749 | far-field recording | -10 dB | 7.65 s | 9.42 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 10.96 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 1167 | far-field recording | -6 dB | 4.74 s | 6.48 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.68 s | 9.42 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 428 | Turkish broadcast | -6 dB | 3.65 s | 4.84 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 959 | music | -14 dB | 9.31 s | 10.02 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 1191 | far-field recording | -14 dB | 5.38 s | 6.60 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 1201 | door | -22 dB | 5.38 s | 6.08 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 1947 | music | -14 dB | 5.38 s | 6.44 s | 1000 | resumed |


### modelled recognizer (short utterances missed x1), wideband

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 13.1% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 23/525 (4.4%) | 204/525 (38.9%) | **6.9%** | 694 | 694 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 3.2% | 0/525 (0.0%) | 0/525 (0.0%) | 2/525 (0.4%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.3 | 310 / 546 | 0.0% | 0/525 (0.0%) | 9/525 (1.7%) | 8/525 (1.5%) | **0.7%** | 500 | 698 / 882 |
| pause(words) after 0 ms, 1.5 s | 9.9 | 380 / 680 | 0.0% | 0/525 (0.0%) | 9/525 (1.7%) | 8/525 (1.5%) | **0.7%** | 102 | 698 / 882 |
| pause(words) after 150 ms, 1.5 s | 9.9 | 340 / 624 | 0.0% | 0/525 (0.0%) | 9/525 (1.7%) | 8/525 (1.5%) | **0.7%** | 150 | 698 / 882 |
| pause(words) after 250 ms, 1.5 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 9/525 (1.7%) | 8/525 (1.5%) | **0.7%** | 250 | 698 / 882 |
| pause(words) after 400 ms, 1.5 s | 2.0 | 378 / 598 | 0.0% | 0/525 (0.0%) | 9/525 (1.7%) | 8/525 (1.5%) | **0.7%** | 400 | 698 / 882 |
| pause(words) after 250 ms, 1.0 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 8/525 (1.5%) | 8/525 (1.5%) | **0.6%** | 250 | 698 / 882 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 312 | 13.1% | 0.0% | 86.9% | - | - | - | 41/2940 |
| Pipecat-like: min_words = 1 | 314 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 314 | 3.2% | 0.0% | 96.8% | - | - | - | 10/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 291 | 0.0% | 13.1% | 86.9% | 310 | 546 | 666 | 0/2940 |
| pause(words) after 0 ms, 1.5 s | 291 | 0.0% | 100.0% | 0.0% | 380 | 680 | 1060 | 0/2940 |
| pause(words) after 150 ms, 1.5 s | 291 | 0.0% | 100.0% | 0.0% | 340 | 624 | 1016 | 0/2940 |
| pause(words) after 250 ms, 1.5 s | 291 | 0.0% | 88.0% | 12.0% | 248 | 532 | 916 | 0/2940 |
| pause(words) after 400 ms, 1.5 s | 291 | 0.0% | 19.9% | 80.1% | 378 | 598 | 766 | 0/2940 |
| pause(words) after 250 ms, 1.0 s | 291 | 0.0% | 88.0% | 12.0% | 248 | 532 | 916 | 0/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 50.0% (0.0%) n=4 | 19.9% (0.0%) n=166 | 0.0% (0.0%) n=1 | 7.1% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=4 | 0.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=4 | 6.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=4 | 0.0% (26.2%) n=145 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=4 | 0.0% (100.0%) n=145 | 0.0% (100.0%) n=1 | 0.0% (100.0%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=4 | 0.0% (100.0%) n=145 | 0.0% (100.0%) n=1 | 0.0% (100.0%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (75.0%) n=4 | 0.0% (90.3%) n=145 | 0.0% (100.0%) n=1 | 0.0% (87.1%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (50.0%) n=4 | 0.0% (34.5%) n=145 | 0.0% (0.0%) n=1 | 0.0% (7.1%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (75.0%) n=4 | 0.0% (90.3%) n=145 | 0.0% (100.0%) n=1 | 0.0% (87.1%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=3 | 7.6% (0.0%) n=66 | 10.1% (0.0%) n=109 | 17.0% (0.0%) n=88 | 21.7% (0.0%) n=46 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 0.0% (0.0%) n=89 | 0.0% (0.0%) n=46 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 6.7% (0.0%) n=89 | 8.7% (0.0%) n=46 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=3 | 0.0% (8.1%) n=62 | 0.0% (7.8%) n=102 | 0.0% (17.1%) n=82 | 0.0% (26.2%) n=42 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=3 | 0.0% (100.0%) n=62 | 0.0% (100.0%) n=102 | 0.0% (100.0%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=3 | 0.0% (100.0%) n=62 | 0.0% (100.0%) n=102 | 0.0% (100.0%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 250 ms, 1.5 s | 0.0% (33.3%) n=3 | 0.0% (83.9%) n=62 | 0.0% (90.2%) n=102 | 0.0% (90.2%) n=82 | 0.0% (88.1%) n=42 |
| pause(words) after 400 ms, 1.5 s | 0.0% (0.0%) n=3 | 0.0% (14.5%) n=62 | 0.0% (16.7%) n=102 | 0.0% (22.0%) n=82 | 0.0% (33.3%) n=42 |
| pause(words) after 250 ms, 1.0 s | 0.0% (33.3%) n=3 | 0.0% (83.9%) n=62 | 0.0% (90.2%) n=102 | 0.0% (90.2%) n=82 | 0.0% (88.1%) n=42 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 573 | 1070 | 3 | 400 | 400 | 400 | 400 | 390 | 882 |
| Pipecat-like: min_words = 1 | 1575 | 236 | 9 | 227 | 326 | 1149 | 248 | 694 | 710 | 694 | 710 | 690 | 990 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 2 | 0 | 2 | 534 | 1078 | 5 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 23 | 6 | 17 | 330 | 1100 | 17 | 500 | 500 | 698 | 882 | 700 | 940 |
| pause(words) after 0 ms, 1.5 s | 1575 | 17 | 0 | 17 | 414 | 1098 | 17 | 102 | 110 | 698 | 882 | 700 | 940 |
| pause(words) after 150 ms, 1.5 s | 1575 | 17 | 0 | 17 | 346 | 1101 | 19 | 150 | 150 | 698 | 882 | 700 | 940 |
| pause(words) after 250 ms, 1.5 s | 1575 | 17 | 0 | 17 | 344 | 1098 | 17 | 250 | 250 | 698 | 882 | 700 | 940 |
| pause(words) after 400 ms, 1.5 s | 1575 | 17 | 0 | 17 | 330 | 1101 | 17 | 400 | 400 | 698 | 882 | 700 | 940 |
| pause(words) after 250 ms, 1.0 s | 1575 | 18 | 2 | 16 | 484 | 1075 | 16 | 250 | 250 | 698 | 882 | 720 | 970 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 23/525 (1) | 204/525 (8) | 21/63 (0) | 193/1008 (0) | 13/504 (9) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 2/525 (0) | 0/63 (0) | 2/1008 (0) | 0/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 9/525 (0) | 8/525 (6) | 4/63 (0) | 12/1008 (0) | 1/504 (6) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 9/525 (0) | 8/525 (0) | 4/63 (0) | 12/1008 (0) | 1/504 (0) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 9/525 (0) | 8/525 (0) | 4/63 (0) | 12/1008 (0) | 1/504 (0) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 9/525 (0) | 8/525 (0) | 4/63 (0) | 12/1008 (0) | 1/504 (0) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 9/525 (0) | 8/525 (0) | 4/63 (0) | 12/1008 (0) | 1/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 8/525 (0) | 8/525 (2) | 4/63 (0) | 12/1008 (2) | 0/504 (0) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 100.0% (0.0%) n=63 | 92.5% (0.0%) n=67 | 85.7% (0.0%) n=112 | 91.2% (0.0%) n=57 | 97.3% (0.0%) n=451 | - | - | - |
| Pipecat-like: min_words = 1 | 85.7% (0.0%) n=70 | 72.7% (0.0%) n=77 | 0.0% (0.0%) n=151 | 41.9% (0.0%) n=86 | 96.5% (0.0%) n=452 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 100.0% (0.0%) n=63 | 91.2% (0.0%) n=68 | 79.5% (0.0%) n=112 | 80.3% (0.0%) n=61 | 96.5% (0.0%) n=452 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 91.0% (7.5%) n=67 | 83.3% (11.1%) n=72 | 18.4% (66.7%) n=114 | 67.2% (26.9%) n=67 | 100.0% (0.0%) n=448 | 894 | 1822 | 2000 |
| pause(words) after 0 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 47.3% (42.0%) n=112 | 73.8% (26.2%) n=65 | 100.0% (0.0%) n=448 | 1040 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 44.5% (45.4%) n=119 | 71.2% (28.8%) n=66 | 100.0% (0.0%) n=448 | 1120 | 1500 | 1500 |
| pause(words) after 250 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 43.4% (45.1%) n=113 | 71.2% (28.8%) n=66 | 100.0% (0.0%) n=448 | 1012 | 1364 | 1500 |
| pause(words) after 400 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (11.1%) n=72 | 37.2% (50.4%) n=113 | 69.7% (28.8%) n=66 | 100.0% (0.0%) n=448 | 886 | 1500 | 1500 |
| pause(words) after 250 ms, 1.0 s | 96.9% (3.1%) n=65 | 89.9% (10.1%) n=69 | 56.8% (32.4%) n=111 | 75.0% (25.0%) n=64 | 100.0% (0.0%) n=448 | 724 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 59/63 | 63/63 | 48/63 | 2/420 | 33/420 | 0/420 | 6/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| Pipecat-like: min_words = 1 | 60/63 | 54/63 | 0/63 | 36/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 59/63 | 60/63 | 48/63 | 0/420 | 10/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 61/63 | 57/63 | 21/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 0 ms, 1.5 s | 62/63 | 58/63 | 43/63 | 48/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 150 ms, 1.5 s | 62/63 | 58/63 | 43/63 | 47/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 250 ms, 1.5 s | 62/63 | 58/63 | 42/63 | 47/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 400 ms, 1.5 s | 62/63 | 58/63 | 42/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 59/63 | 53/63 | 48/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3686 | far-field recording | -14 dB | 7.71 s | 10.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 255 | far-field recording | -10 dB | 3.75 s | 6.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1738 | far-field recording | -4 dB | 4.74 s | 7.14 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 2699 | far-field recording | -6 dB | 3.75 s | 6.16 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3192 | far-field recording | -14 dB | 7.71 s | 10.12 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 1239 | loudspeaker in a room | -18 dB | 7.75 s | 7.98 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2480 | Turkish broadcast | -18 dB | 4.87 s | 6.28 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 918 | loudspeaker in a room | -6 dB | 8.00 s | 8.52 s | 1402 | resumed |
| pause(words) after 150 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4393 | street | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 1737 | far-field recording | -10 dB | 5.47 s | 6.98 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2478 | far-field recording | -10 dB | 5.44 s | 6.98 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.75 s | 9.48 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2724 | far-field recording | -18 dB | 7.97 s | 9.70 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2230 | far-field recording | -18 dB | 7.97 s | 9.00 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 426 | far-field recording | -6 dB | 4.99 s | 6.56 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1736 | far-field recording | -18 dB | 5.67 s | 7.34 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.75 s | 9.48 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 2724 | far-field recording | -18 dB | 7.97 s | 9.70 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 502 | far-field recording | -10 dB | 8.23 s | 10.06 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 11.08 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3390 | far-field recording | -6 dB | 4.23 s | 6.08 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 428 | Turkish broadcast | -6 dB | 3.65 s | 4.84 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2971 | far-field recording | -18 dB | 7.91 s | 8.26 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 255 | far-field recording | -10 dB | 7.55 s | 8.64 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 959 | music | -14 dB | 9.31 s | 10.02 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 1738 | far-field recording | -4 dB | 8.55 s | 9.64 s | 1000 | resumed |


### modelled recognizer (short utterances missed x1), telephone

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 18.9% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 50/525 (9.5%) | 219/525 (41.7%) | **8.6%** | 694 | 694 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 4.9% | 0/525 (0.0%) | 0/525 (0.0%) | 3/525 (0.6%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.9 | 290 / 578 | 0.0% | 0/525 (0.0%) | 36/525 (6.9%) | 35/525 (6.7%) | **2.7%** | 500 | 698 / 902 |
| pause(words) after 0 ms, 1.5 s | 9.5 | 400 / 680 | 0.0% | 0/525 (0.0%) | 35/525 (6.7%) | 35/525 (6.7%) | **2.7%** | 102 | 698 / 906 |
| pause(words) after 150 ms, 1.5 s | 9.5 | 348 / 640 | 0.0% | 0/525 (0.0%) | 35/525 (6.7%) | 35/525 (6.7%) | **2.7%** | 150 | 698 / 902 |
| pause(words) after 250 ms, 1.5 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 35/525 (6.7%) | 35/525 (6.7%) | **2.7%** | 250 | 698 / 902 |
| pause(words) after 400 ms, 1.5 s | 2.5 | 374 / 666 | 0.0% | 0/525 (0.0%) | 35/525 (6.7%) | 35/525 (6.7%) | **2.7%** | 400 | 698 / 902 |
| pause(words) after 250 ms, 1.0 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 34/525 (6.5%) | 35/525 (6.7%) | **2.6%** | 250 | 698 / 906 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 302 | 18.9% | 0.0% | 81.1% | - | - | - | 57/2940 |
| Pipecat-like: min_words = 1 | 307 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 305 | 4.9% | 0.0% | 95.1% | - | - | - | 15/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 280 | 0.0% | 20.4% | 79.6% | 290 | 578 | 666 | 0/2940 |
| pause(words) after 0 ms, 1.5 s | 280 | 0.0% | 100.0% | 0.0% | 400 | 680 | 1060 | 0/2940 |
| pause(words) after 150 ms, 1.5 s | 280 | 0.0% | 100.0% | 0.0% | 348 | 640 | 1016 | 0/2940 |
| pause(words) after 250 ms, 1.5 s | 280 | 0.0% | 86.1% | 13.9% | 276 | 560 | 916 | 0/2940 |
| pause(words) after 400 ms, 1.5 s | 280 | 0.0% | 26.4% | 73.6% | 374 | 666 | 766 | 0/2940 |
| pause(words) after 250 ms, 1.0 s | 280 | 0.0% | 86.1% | 13.9% | 276 | 560 | 916 | 0/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 37.5% (0.0%) n=8 | 27.4% (0.0%) n=179 | 0.0% (0.0%) n=2 | 10.2% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=184 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 8.2% (0.0%) n=182 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (12.5%) n=8 | 0.0% (35.7%) n=157 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=8 | 0.0% (100.0%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=8 | 0.0% (100.0%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (87.5%) n=8 | 0.0% (91.7%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (37.5%) n=8 | 0.0% (42.0%) n=157 | 0.0% (0.0%) n=2 | 0.0% (10.2%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (87.5%) n=8 | 0.0% (91.7%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=8 | 10.7% (0.0%) n=75 | 16.0% (0.0%) n=100 | 23.6% (0.0%) n=72 | 34.0% (0.0%) n=47 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=77 | 0.0% (0.0%) n=100 | 0.0% (0.0%) n=74 | 0.0% (0.0%) n=48 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 1.3% (0.0%) n=77 | 0.0% (0.0%) n=100 | 12.3% (0.0%) n=73 | 10.6% (0.0%) n=47 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (14.3%) n=7 | 0.0% (14.7%) n=68 | 0.0% (17.6%) n=91 | 0.0% (26.9%) n=67 | 0.0% (25.5%) n=47 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=7 | 0.0% (100.0%) n=68 | 0.0% (100.0%) n=91 | 0.0% (100.0%) n=67 | 0.0% (100.0%) n=47 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=7 | 0.0% (100.0%) n=68 | 0.0% (100.0%) n=91 | 0.0% (100.0%) n=67 | 0.0% (100.0%) n=47 |
| pause(words) after 250 ms, 1.5 s | 0.0% (57.1%) n=7 | 0.0% (79.4%) n=68 | 0.0% (91.2%) n=91 | 0.0% (83.6%) n=67 | 0.0% (93.6%) n=47 |
| pause(words) after 400 ms, 1.5 s | 0.0% (14.3%) n=7 | 0.0% (20.6%) n=68 | 0.0% (26.4%) n=91 | 0.0% (28.4%) n=67 | 0.0% (34.0%) n=47 |
| pause(words) after 250 ms, 1.0 s | 0.0% (57.1%) n=7 | 0.0% (79.4%) n=68 | 0.0% (91.2%) n=91 | 0.0% (83.6%) n=67 | 0.0% (93.6%) n=47 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 524 | 1112 | 7 | 400 | 400 | 400 | 400 | 390 | 880 |
| Pipecat-like: min_words = 1 | 1575 | 280 | 11 | 269 | 311 | 1204 | 304 | 694 | 710 | 694 | 710 | 690 | 1030 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 3 | 0 | 3 | 464 | 1119 | 10 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 75 | 4 | 71 | 318 | 1149 | 76 | 500 | 500 | 698 | 902 | 705 | 1010 |
| pause(words) after 0 ms, 1.5 s | 1575 | 71 | 1 | 70 | 366 | 1142 | 73 | 102 | 110 | 698 | 906 | 710 | 1010 |
| pause(words) after 150 ms, 1.5 s | 1575 | 71 | 1 | 70 | 329 | 1150 | 79 | 150 | 150 | 698 | 902 | 700 | 1010 |
| pause(words) after 250 ms, 1.5 s | 1575 | 72 | 2 | 70 | 329 | 1149 | 77 | 250 | 250 | 698 | 902 | 700 | 1010 |
| pause(words) after 400 ms, 1.5 s | 1575 | 71 | 1 | 70 | 318 | 1149 | 76 | 400 | 400 | 698 | 902 | 700 | 1010 |
| pause(words) after 250 ms, 1.0 s | 1575 | 72 | 3 | 69 | 403 | 1132 | 72 | 250 | 250 | 698 | 906 | 710 | 1010 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 50/525 (2) | 219/525 (9) | 22/63 (0) | 225/1008 (0) | 22/504 (11) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 3/525 (0) | 0/63 (0) | 2/1008 (0) | 1/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 36/525 (1) | 35/525 (3) | 7/63 (0) | 58/1008 (1) | 6/504 (3) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 35/525 (0) | 35/525 (1) | 7/63 (0) | 58/1008 (0) | 5/504 (1) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 35/525 (0) | 35/525 (1) | 7/63 (0) | 58/1008 (0) | 5/504 (1) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 35/525 (1) | 35/525 (1) | 7/63 (0) | 58/1008 (1) | 5/504 (1) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 35/525 (1) | 35/525 (0) | 7/63 (0) | 58/1008 (1) | 5/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 34/525 (0) | 35/525 (3) | 7/63 (0) | 58/1008 (2) | 4/504 (1) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 96.9% (0.0%) n=65 | 85.1% (0.0%) n=74 | 76.7% (0.0%) n=116 | 86.2% (0.0%) n=65 | 97.8% (0.0%) n=445 | - | - | - |
| Pipecat-like: min_words = 1 | 83.3% (0.0%) n=72 | 67.1% (0.0%) n=85 | 0.0% (0.0%) n=150 | 37.6% (0.0%) n=93 | 97.1% (0.0%) n=446 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 96.9% (0.0%) n=65 | 79.2% (0.0%) n=77 | 60.0% (0.0%) n=120 | 69.4% (0.0%) n=72 | 97.1% (0.0%) n=446 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 91.0% (7.5%) n=67 | 80.3% (9.2%) n=76 | 6.7% (59.7%) n=134 | 65.4% (19.2%) n=78 | 99.8% (0.0%) n=442 | 762 | 1822 | 2000 |
| pause(words) after 0 ms, 1.5 s | 92.5% (7.5%) n=67 | 80.5% (18.2%) n=77 | 24.1% (57.1%) n=133 | 65.8% (32.9%) n=79 | 99.8% (0.2%) n=442 | 700 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (17.1%) n=76 | 24.1% (57.1%) n=133 | 68.8% (31.2%) n=77 | 99.8% (0.2%) n=442 | 632 | 1332 | 1460 |
| pause(words) after 250 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (15.8%) n=76 | 20.3% (59.4%) n=133 | 66.7% (33.3%) n=78 | 99.8% (0.2%) n=442 | 560 | 1304 | 1500 |
| pause(words) after 400 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (10.5%) n=76 | 15.8% (54.9%) n=133 | 68.8% (26.0%) n=77 | 99.8% (0.2%) n=442 | 710 | 1222 | 1500 |
| pause(words) after 250 ms, 1.0 s | 96.9% (3.1%) n=65 | 82.7% (14.7%) n=75 | 38.0% (51.2%) n=129 | 68.8% (31.2%) n=77 | 99.8% (0.2%) n=442 | 500 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 58/63 | 56/63 | 49/63 | 3/420 | 49/420 | 0/420 | 5/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| Pipecat-like: min_words = 1 | 60/63 | 54/63 | 0/63 | 35/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 57/63 | 51/63 | 46/63 | 0/420 | 15/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 61/63 | 56/63 | 9/63 | 44/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 0 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 150 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 250 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 400 ms, 1.5 s | 62/63 | 57/63 | 21/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 57/63 | 40/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1741 | Turkish broadcast | -4 dB | 6.85 s | 7.52 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3192 | far-field recording | -14 dB | 7.71 s | 10.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 722 | far-field recording | -14 dB | 8.23 s | 10.54 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 4702 | far-field recording | -4 dB | 4.23 s | 4.74 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 969 | far-field recording | -14 dB | 7.68 s | 10.02 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 249 | dry second voice | -10 dB | 5.03 s | 6.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 1192 | far-field recording | -6 dB | 4.80 s | 6.18 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3415 | far-field recording | -6 dB | 4.80 s | 6.22 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3959 | far-field recording | -18 dB | 4.80 s | 6.16 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 2724 | far-field recording | -18 dB | 8.10 s | 9.44 s | 1398 | resumed |
| pause(words) after 150 ms, 1.5 s | 3893 | door | -22 dB | 11.49 s | 13.04 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4393 | street | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 200 | dry second voice | -6 dB | 8.29 s | 9.80 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4178 | loudspeaker in a room | -14 dB | 5.06 s | 5.80 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3897 | music | -22 dB | 11.46 s | 13.04 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 10.96 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.68 s | 9.42 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 4146 | street | -22 dB | 8.45 s | 10.16 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2972 | far-field recording | -10 dB | 7.71 s | 9.44 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 3466 | far-field recording | -10 dB | 7.71 s | 9.44 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 502 | far-field recording | -10 dB | 8.23 s | 10.00 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 10.96 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 749 | far-field recording | -10 dB | 7.65 s | 9.42 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.68 s | 9.42 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4409 | cough | -14 dB | 10.27 s | 10.64 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 428 | Turkish broadcast | -6 dB | 3.65 s | 4.84 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2927 | cough | -14 dB | 9.92 s | 10.98 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 3179 | dishes | -22 dB | 9.44 s | 10.50 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 3659 | loudspeaker in a room | -14 dB | 7.81 s | 8.94 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 5156 | dishes | -14 dB | 11.17 s | 12.24 s | 1000 | resumed |


### modelled recognizer (short utterances missed x3), wideband

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 13.1% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 36/525 (6.9%) | 212/525 (40.4%) | **7.8%** | 694 | 694 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 3.2% | 0/525 (0.0%) | 0/525 (0.0%) | 2/525 (0.4%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.3 | 310 / 546 | 0.0% | 0/525 (0.0%) | 22/525 (4.2%) | 28/525 (5.3%) | **1.8%** | 500 | 698 / 882 |
| pause(words) after 0 ms, 1.5 s | 9.9 | 380 / 680 | 0.0% | 0/525 (0.0%) | 22/525 (4.2%) | 28/525 (5.3%) | **1.8%** | 102 | 698 / 882 |
| pause(words) after 150 ms, 1.5 s | 9.9 | 340 / 624 | 0.0% | 0/525 (0.0%) | 22/525 (4.2%) | 28/525 (5.3%) | **1.8%** | 150 | 698 / 882 |
| pause(words) after 250 ms, 1.5 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 22/525 (4.2%) | 28/525 (5.3%) | **1.8%** | 250 | 698 / 882 |
| pause(words) after 400 ms, 1.5 s | 2.0 | 378 / 598 | 0.0% | 0/525 (0.0%) | 22/525 (4.2%) | 28/525 (5.3%) | **1.8%** | 400 | 698 / 882 |
| pause(words) after 250 ms, 1.0 s | 8.7 | 248 / 532 | 0.0% | 0/525 (0.0%) | 20/525 (3.8%) | 28/525 (5.3%) | **1.8%** | 250 | 698 / 882 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 312 | 13.1% | 0.0% | 86.9% | - | - | - | 41/2940 |
| Pipecat-like: min_words = 1 | 314 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 314 | 3.2% | 0.0% | 96.8% | - | - | - | 10/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 291 | 0.0% | 13.1% | 86.9% | 310 | 546 | 666 | 0/2940 |
| pause(words) after 0 ms, 1.5 s | 291 | 0.0% | 100.0% | 0.0% | 380 | 680 | 1060 | 0/2940 |
| pause(words) after 150 ms, 1.5 s | 291 | 0.0% | 100.0% | 0.0% | 340 | 624 | 1016 | 0/2940 |
| pause(words) after 250 ms, 1.5 s | 291 | 0.0% | 88.0% | 12.0% | 248 | 532 | 916 | 0/2940 |
| pause(words) after 400 ms, 1.5 s | 291 | 0.0% | 19.9% | 80.1% | 378 | 598 | 766 | 0/2940 |
| pause(words) after 250 ms, 1.0 s | 291 | 0.0% | 88.0% | 12.0% | 248 | 532 | 916 | 0/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 50.0% (0.0%) n=4 | 19.9% (0.0%) n=166 | 0.0% (0.0%) n=1 | 7.1% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=4 | 0.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=4 | 6.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=4 | 0.0% (26.2%) n=145 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=4 | 0.0% (100.0%) n=145 | 0.0% (100.0%) n=1 | 0.0% (100.0%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=4 | 0.0% (100.0%) n=145 | 0.0% (100.0%) n=1 | 0.0% (100.0%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (75.0%) n=4 | 0.0% (90.3%) n=145 | 0.0% (100.0%) n=1 | 0.0% (87.1%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (50.0%) n=4 | 0.0% (34.5%) n=145 | 0.0% (0.0%) n=1 | 0.0% (7.1%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (75.0%) n=4 | 0.0% (90.3%) n=145 | 0.0% (100.0%) n=1 | 0.0% (87.1%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=3 | 7.6% (0.0%) n=66 | 10.1% (0.0%) n=109 | 17.0% (0.0%) n=88 | 21.7% (0.0%) n=46 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 0.0% (0.0%) n=89 | 0.0% (0.0%) n=46 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 6.7% (0.0%) n=89 | 8.7% (0.0%) n=46 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=3 | 0.0% (8.1%) n=62 | 0.0% (7.8%) n=102 | 0.0% (17.1%) n=82 | 0.0% (26.2%) n=42 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=3 | 0.0% (100.0%) n=62 | 0.0% (100.0%) n=102 | 0.0% (100.0%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=3 | 0.0% (100.0%) n=62 | 0.0% (100.0%) n=102 | 0.0% (100.0%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 250 ms, 1.5 s | 0.0% (33.3%) n=3 | 0.0% (83.9%) n=62 | 0.0% (90.2%) n=102 | 0.0% (90.2%) n=82 | 0.0% (88.1%) n=42 |
| pause(words) after 400 ms, 1.5 s | 0.0% (0.0%) n=3 | 0.0% (14.5%) n=62 | 0.0% (16.7%) n=102 | 0.0% (22.0%) n=82 | 0.0% (33.3%) n=42 |
| pause(words) after 250 ms, 1.0 s | 0.0% (33.3%) n=3 | 0.0% (83.9%) n=62 | 0.0% (90.2%) n=102 | 0.0% (90.2%) n=82 | 0.0% (88.1%) n=42 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 573 | 1070 | 3 | 400 | 400 | 400 | 400 | 390 | 882 |
| Pipecat-like: min_words = 1 | 1575 | 262 | 14 | 248 | 326 | 1149 | 269 | 694 | 710 | 694 | 710 | 690 | 1010 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 2 | 0 | 2 | 534 | 1078 | 5 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 56 | 6 | 50 | 330 | 1100 | 50 | 500 | 500 | 698 | 882 | 700 | 940 |
| pause(words) after 0 ms, 1.5 s | 1575 | 50 | 0 | 50 | 414 | 1098 | 50 | 102 | 110 | 698 | 882 | 700 | 950 |
| pause(words) after 150 ms, 1.5 s | 1575 | 50 | 0 | 50 | 346 | 1101 | 52 | 150 | 150 | 698 | 882 | 700 | 940 |
| pause(words) after 250 ms, 1.5 s | 1575 | 50 | 0 | 50 | 344 | 1098 | 50 | 250 | 250 | 698 | 882 | 700 | 940 |
| pause(words) after 400 ms, 1.5 s | 1575 | 50 | 0 | 50 | 330 | 1101 | 50 | 400 | 400 | 698 | 882 | 700 | 940 |
| pause(words) after 250 ms, 1.0 s | 1575 | 50 | 2 | 48 | 484 | 1075 | 48 | 250 | 250 | 698 | 882 | 720 | 990 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 36/525 (3) | 212/525 (11) | 21/63 (0) | 212/1008 (0) | 15/504 (14) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 2/525 (0) | 0/63 (0) | 2/1008 (0) | 0/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 22/525 (0) | 28/525 (6) | 6/63 (0) | 41/1008 (0) | 3/504 (6) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 22/525 (0) | 28/525 (0) | 6/63 (0) | 41/1008 (0) | 3/504 (0) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 22/525 (0) | 28/525 (0) | 6/63 (0) | 41/1008 (0) | 3/504 (0) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 22/525 (0) | 28/525 (0) | 6/63 (0) | 41/1008 (0) | 3/504 (0) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 22/525 (0) | 28/525 (0) | 6/63 (0) | 41/1008 (0) | 3/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 20/525 (0) | 28/525 (2) | 6/63 (0) | 41/1008 (2) | 1/504 (0) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 100.0% (0.0%) n=63 | 92.5% (0.0%) n=67 | 85.7% (0.0%) n=112 | 91.2% (0.0%) n=57 | 97.3% (0.0%) n=451 | - | - | - |
| Pipecat-like: min_words = 1 | 85.7% (0.0%) n=70 | 72.7% (0.0%) n=77 | 0.0% (0.0%) n=151 | 41.9% (0.0%) n=86 | 96.5% (0.0%) n=452 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 100.0% (0.0%) n=63 | 91.2% (0.0%) n=68 | 79.5% (0.0%) n=112 | 80.3% (0.0%) n=61 | 96.5% (0.0%) n=452 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 91.0% (7.5%) n=67 | 83.3% (11.1%) n=72 | 18.4% (66.7%) n=114 | 67.2% (26.9%) n=67 | 100.0% (0.0%) n=448 | 894 | 1822 | 2000 |
| pause(words) after 0 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 47.3% (42.0%) n=112 | 73.8% (26.2%) n=65 | 100.0% (0.0%) n=448 | 1040 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 44.5% (45.4%) n=119 | 71.2% (28.8%) n=66 | 100.0% (0.0%) n=448 | 1120 | 1500 | 1500 |
| pause(words) after 250 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 43.4% (45.1%) n=113 | 71.2% (28.8%) n=66 | 100.0% (0.0%) n=448 | 1012 | 1364 | 1500 |
| pause(words) after 400 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (11.1%) n=72 | 37.2% (50.4%) n=113 | 69.7% (28.8%) n=66 | 100.0% (0.0%) n=448 | 886 | 1500 | 1500 |
| pause(words) after 250 ms, 1.0 s | 96.9% (3.1%) n=65 | 89.9% (10.1%) n=69 | 56.8% (32.4%) n=111 | 75.0% (25.0%) n=64 | 100.0% (0.0%) n=448 | 724 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 59/63 | 63/63 | 48/63 | 2/420 | 33/420 | 0/420 | 6/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| Pipecat-like: min_words = 1 | 60/63 | 54/63 | 0/63 | 36/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 59/63 | 60/63 | 48/63 | 0/420 | 10/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 61/63 | 57/63 | 21/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 0 ms, 1.5 s | 62/63 | 58/63 | 43/63 | 48/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 150 ms, 1.5 s | 62/63 | 58/63 | 43/63 | 47/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 250 ms, 1.5 s | 62/63 | 58/63 | 42/63 | 47/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 400 ms, 1.5 s | 62/63 | 58/63 | 42/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 59/63 | 53/63 | 48/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1958 | far-field recording | -6 dB | 4.74 s | 7.18 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3219 | far-field recording | -10 dB | 4.71 s | 7.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 2479 | far-field recording | -4 dB | 4.74 s | 7.14 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 4701 | far-field recording | -10 dB | 4.74 s | 7.14 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1738 | far-field recording | -4 dB | 4.74 s | 7.14 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 1239 | loudspeaker in a room | -18 dB | 7.75 s | 7.98 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2480 | Turkish broadcast | -18 dB | 4.87 s | 6.28 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 918 | loudspeaker in a room | -6 dB | 8.00 s | 8.52 s | 1402 | resumed |
| pause(words) after 150 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2478 | far-field recording | -10 dB | 5.44 s | 6.98 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4949 | far-field recording | -4 dB | 10.24 s | 11.82 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4393 | street | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.75 s | 9.48 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 426 | far-field recording | -6 dB | 4.99 s | 6.56 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1736 | far-field recording | -18 dB | 5.67 s | 7.34 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2230 | far-field recording | -18 dB | 7.97 s | 9.00 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2724 | far-field recording | -18 dB | 7.97 s | 9.70 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 8 | far-field recording | -10 dB | 8.23 s | 10.06 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4207 | far-field recording | -10 dB | 9.25 s | 11.08 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.75 s | 9.48 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 11.08 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 502 | far-field recording | -10 dB | 8.23 s | 10.06 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 428 | Turkish broadcast | -6 dB | 3.65 s | 4.84 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 958 | music | -22 dB | 9.38 s | 10.44 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 4128 | loudspeaker in a room | -14 dB | 4.61 s | 5.76 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 5156 | dishes | -14 dB | 11.17 s | 12.24 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 198 | clean line | +0 dB | 9.89 s | 10.96 s | 1000 | resumed |


### modelled recognizer (short utterances missed x3), telephone

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 18.9% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 126/525 (24.0%) | 246/525 (46.9%) | **13.0%** | 460 | 460 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 4.9% | 0/525 (0.0%) | 0/525 (0.0%) | 3/525 (0.6%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.9 | 290 / 578 | 0.0% | 0/525 (0.0%) | 113/525 (21.5%) | 87/525 (16.6%) | **7.9%** | 500 | 698 / 906 |
| pause(words) after 0 ms, 1.5 s | 9.5 | 400 / 680 | 0.0% | 0/525 (0.0%) | 112/525 (21.3%) | 86/525 (16.4%) | **7.8%** | 102 | 698 / 906 |
| pause(words) after 150 ms, 1.5 s | 9.5 | 348 / 640 | 0.0% | 0/525 (0.0%) | 112/525 (21.3%) | 87/525 (16.6%) | **7.8%** | 150 | 698 / 906 |
| pause(words) after 250 ms, 1.5 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 112/525 (21.3%) | 86/525 (16.4%) | **7.8%** | 250 | 698 / 906 |
| pause(words) after 400 ms, 1.5 s | 2.5 | 374 / 666 | 0.0% | 0/525 (0.0%) | 112/525 (21.3%) | 86/525 (16.4%) | **7.8%** | 400 | 698 / 906 |
| pause(words) after 250 ms, 1.0 s | 8.2 | 276 / 560 | 0.0% | 0/525 (0.0%) | 109/525 (20.8%) | 85/525 (16.2%) | **7.6%** | 250 | 698 / 906 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 302 | 18.9% | 0.0% | 81.1% | - | - | - | 57/2940 |
| Pipecat-like: min_words = 1 | 307 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 305 | 4.9% | 0.0% | 95.1% | - | - | - | 15/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 280 | 0.0% | 20.4% | 79.6% | 290 | 578 | 666 | 0/2940 |
| pause(words) after 0 ms, 1.5 s | 280 | 0.0% | 100.0% | 0.0% | 400 | 680 | 1060 | 0/2940 |
| pause(words) after 150 ms, 1.5 s | 280 | 0.0% | 100.0% | 0.0% | 348 | 640 | 1016 | 0/2940 |
| pause(words) after 250 ms, 1.5 s | 280 | 0.0% | 86.1% | 13.9% | 276 | 560 | 916 | 0/2940 |
| pause(words) after 400 ms, 1.5 s | 280 | 0.0% | 26.4% | 73.6% | 374 | 666 | 766 | 0/2940 |
| pause(words) after 250 ms, 1.0 s | 280 | 0.0% | 86.1% | 13.9% | 276 | 560 | 916 | 0/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 37.5% (0.0%) n=8 | 27.4% (0.0%) n=179 | 0.0% (0.0%) n=2 | 10.2% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=184 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 8.2% (0.0%) n=182 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (12.5%) n=8 | 0.0% (35.7%) n=157 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=8 | 0.0% (100.0%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=8 | 0.0% (100.0%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (87.5%) n=8 | 0.0% (91.7%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (37.5%) n=8 | 0.0% (42.0%) n=157 | 0.0% (0.0%) n=2 | 0.0% (10.2%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (87.5%) n=8 | 0.0% (91.7%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=8 | 10.7% (0.0%) n=75 | 16.0% (0.0%) n=100 | 23.6% (0.0%) n=72 | 34.0% (0.0%) n=47 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=77 | 0.0% (0.0%) n=100 | 0.0% (0.0%) n=74 | 0.0% (0.0%) n=48 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 1.3% (0.0%) n=77 | 0.0% (0.0%) n=100 | 12.3% (0.0%) n=73 | 10.6% (0.0%) n=47 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (14.3%) n=7 | 0.0% (14.7%) n=68 | 0.0% (17.6%) n=91 | 0.0% (26.9%) n=67 | 0.0% (25.5%) n=47 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=7 | 0.0% (100.0%) n=68 | 0.0% (100.0%) n=91 | 0.0% (100.0%) n=67 | 0.0% (100.0%) n=47 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=7 | 0.0% (100.0%) n=68 | 0.0% (100.0%) n=91 | 0.0% (100.0%) n=67 | 0.0% (100.0%) n=47 |
| pause(words) after 250 ms, 1.5 s | 0.0% (57.1%) n=7 | 0.0% (79.4%) n=68 | 0.0% (91.2%) n=91 | 0.0% (83.6%) n=67 | 0.0% (93.6%) n=47 |
| pause(words) after 400 ms, 1.5 s | 0.0% (14.3%) n=7 | 0.0% (20.6%) n=68 | 0.0% (26.4%) n=91 | 0.0% (28.4%) n=67 | 0.0% (34.0%) n=47 |
| pause(words) after 250 ms, 1.0 s | 0.0% (57.1%) n=7 | 0.0% (79.4%) n=68 | 0.0% (91.2%) n=91 | 0.0% (83.6%) n=67 | 0.0% (93.6%) n=47 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 524 | 1112 | 7 | 400 | 400 | 400 | 400 | 390 | 880 |
| Pipecat-like: min_words = 1 | 1575 | 389 | 17 | 372 | 311 | 1206 | 409 | 460 | 710 | 460 | 710 | 700 | 1060 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 3 | 0 | 3 | 464 | 1119 | 10 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 209 | 9 | 200 | 318 | 1149 | 206 | 500 | 500 | 698 | 906 | 720 | 1030 |
| pause(words) after 0 ms, 1.5 s | 1575 | 201 | 3 | 198 | 366 | 1142 | 202 | 102 | 110 | 698 | 906 | 730 | 1030 |
| pause(words) after 150 ms, 1.5 s | 1575 | 203 | 4 | 199 | 329 | 1150 | 209 | 150 | 150 | 698 | 906 | 720 | 1030 |
| pause(words) after 250 ms, 1.5 s | 1575 | 203 | 5 | 198 | 329 | 1149 | 206 | 250 | 250 | 698 | 906 | 720 | 1030 |
| pause(words) after 400 ms, 1.5 s | 1575 | 202 | 4 | 198 | 318 | 1149 | 205 | 400 | 400 | 698 | 906 | 720 | 1030 |
| pause(words) after 250 ms, 1.0 s | 1575 | 198 | 4 | 194 | 403 | 1132 | 198 | 250 | 250 | 698 | 906 | 730 | 1030 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 126/525 (5) | 246/525 (12) | 31/63 (0) | 308/1008 (0) | 33/504 (17) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 3/525 (0) | 0/63 (0) | 2/1008 (0) | 1/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 113/525 (2) | 87/525 (7) | 21/63 (0) | 162/1008 (4) | 17/504 (5) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 112/525 (0) | 86/525 (3) | 21/63 (0) | 162/1008 (2) | 15/504 (1) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 112/525 (1) | 87/525 (3) | 21/63 (0) | 162/1008 (3) | 16/504 (1) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 112/525 (2) | 86/525 (3) | 21/63 (0) | 162/1008 (4) | 15/504 (1) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 112/525 (2) | 86/525 (2) | 21/63 (0) | 162/1008 (4) | 15/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 109/525 (0) | 85/525 (4) | 21/63 (0) | 162/1008 (3) | 11/504 (1) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 96.9% (0.0%) n=65 | 85.1% (0.0%) n=74 | 76.7% (0.0%) n=116 | 86.2% (0.0%) n=65 | 97.8% (0.0%) n=445 | - | - | - |
| Pipecat-like: min_words = 1 | 83.3% (0.0%) n=72 | 67.1% (0.0%) n=85 | 0.0% (0.0%) n=150 | 37.6% (0.0%) n=93 | 97.1% (0.0%) n=446 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 96.9% (0.0%) n=65 | 79.2% (0.0%) n=77 | 60.0% (0.0%) n=120 | 69.4% (0.0%) n=72 | 97.1% (0.0%) n=446 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 91.0% (7.5%) n=67 | 80.3% (9.2%) n=76 | 6.7% (59.7%) n=134 | 65.4% (19.2%) n=78 | 99.8% (0.0%) n=442 | 762 | 1822 | 2000 |
| pause(words) after 0 ms, 1.5 s | 92.5% (7.5%) n=67 | 80.5% (18.2%) n=77 | 24.1% (57.1%) n=133 | 65.8% (32.9%) n=79 | 99.8% (0.2%) n=442 | 700 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (17.1%) n=76 | 24.1% (57.1%) n=133 | 68.8% (31.2%) n=77 | 99.8% (0.2%) n=442 | 632 | 1332 | 1460 |
| pause(words) after 250 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (15.8%) n=76 | 20.3% (59.4%) n=133 | 66.7% (33.3%) n=78 | 99.8% (0.2%) n=442 | 560 | 1304 | 1500 |
| pause(words) after 400 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (10.5%) n=76 | 15.8% (54.9%) n=133 | 68.8% (26.0%) n=77 | 99.8% (0.2%) n=442 | 710 | 1222 | 1500 |
| pause(words) after 250 ms, 1.0 s | 96.9% (3.1%) n=65 | 82.7% (14.7%) n=75 | 38.0% (51.2%) n=129 | 68.8% (31.2%) n=77 | 99.8% (0.2%) n=442 | 500 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 58/63 | 56/63 | 49/63 | 3/420 | 49/420 | 0/420 | 5/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| Pipecat-like: min_words = 1 | 60/63 | 54/63 | 0/63 | 35/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 57/63 | 51/63 | 46/63 | 0/420 | 15/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 61/63 | 56/63 | 9/63 | 44/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 0 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 150 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 250 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 45/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 400 ms, 1.5 s | 62/63 | 57/63 | 21/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 57/63 | 40/63 | 46/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 969 | far-field recording | -14 dB | 7.68 s | 10.02 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1741 | Turkish broadcast | -4 dB | 6.85 s | 7.52 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 722 | far-field recording | -14 dB | 8.23 s | 10.54 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3192 | far-field recording | -14 dB | 7.71 s | 10.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3686 | far-field recording | -14 dB | 7.75 s | 10.12 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 249 | dry second voice | -10 dB | 5.03 s | 6.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 3169 | Turkish broadcast | -14 dB | 9.44 s | 10.80 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3415 | far-field recording | -6 dB | 4.80 s | 6.22 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3959 | far-field recording | -18 dB | 4.80 s | 6.16 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 1192 | far-field recording | -6 dB | 4.80 s | 6.18 s | 1402 | resumed |
| pause(words) after 150 ms, 1.5 s | 456 | cough | -22 dB | 5.83 s | 6.08 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3891 | keyboard | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4135 | real noise (MUSAN) | -14 dB | 6.95 s | 8.48 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4178 | loudspeaker in a room | -14 dB | 5.06 s | 5.80 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4393 | street | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 4146 | street | -22 dB | 8.45 s | 10.16 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 259 | Turkish broadcast | -4 dB | 5.86 s | 7.52 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 259 | Turkish broadcast | -4 dB | 7.55 s | 9.28 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 456 | cough | -22 dB | 5.83 s | 6.08 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2972 | far-field recording | -10 dB | 7.71 s | 9.44 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 749 | far-field recording | -10 dB | 7.65 s | 9.42 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 2230 | far-field recording | -18 dB | 10.59 s | 12.48 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3390 | far-field recording | -6 dB | 4.23 s | 6.00 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4146 | street | -22 dB | 8.45 s | 10.16 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4409 | cough | -14 dB | 10.27 s | 10.64 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 428 | Turkish broadcast | -6 dB | 3.65 s | 4.84 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 206 | Turkish broadcast | -6 dB | 8.10 s | 9.32 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 1201 | door | -22 dB | 5.38 s | 6.08 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 1439 | far-field recording | -6 dB | 7.84 s | 8.10 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2190 | door | -14 dB | 9.63 s | 10.70 s | 1000 | resumed |


### modelled recognizer (short utterances missed x2), a word in 10% of noise windows, wideband

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 13.1% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 28/525 (5.3%) | 207/525 (39.4%) | **7.2%** | 694 | 694 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 3.2% | 0/525 (0.0%) | 0/525 (0.0%) | 2/525 (0.4%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.2 | 310 / 598 | 1.7% | 0/525 (0.0%) | 14/525 (2.7%) | 17/525 (3.2%) | **1.2%** | 500 | 698 / 882 |
| pause(words) after 0 ms, 1.5 s | 9.9 | 380 / 660 | 1.7% | 0/525 (0.0%) | 14/525 (2.7%) | 17/525 (3.2%) | **1.2%** | 102 | 698 / 882 |
| pause(words) after 150 ms, 1.5 s | 9.9 | 340 / 604 | 1.7% | 0/525 (0.0%) | 14/525 (2.7%) | 17/525 (3.2%) | **1.2%** | 150 | 698 / 882 |
| pause(words) after 250 ms, 1.5 s | 8.7 | 248 / 528 | 1.7% | 0/525 (0.0%) | 14/525 (2.7%) | 17/525 (3.2%) | **1.2%** | 250 | 698 / 882 |
| pause(words) after 400 ms, 1.5 s | 2.0 | 374 / 634 | 1.7% | 0/525 (0.0%) | 14/525 (2.7%) | 17/525 (3.2%) | **1.2%** | 400 | 698 / 882 |
| pause(words) after 250 ms, 1.0 s | 8.7 | 248 / 528 | 1.7% | 0/525 (0.0%) | 13/525 (2.5%) | 17/525 (3.2%) | **1.1%** | 250 | 698 / 882 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 312 | 13.1% | 0.0% | 86.9% | - | - | - | 41/2940 |
| Pipecat-like: min_words = 1 | 314 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 314 | 3.2% | 0.0% | 96.8% | - | - | - | 10/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 291 | 1.7% | 12.0% | 86.3% | 310 | 598 | 666 | 5/2940 |
| pause(words) after 0 ms, 1.5 s | 291 | 1.7% | 98.3% | 0.0% | 380 | 660 | 1060 | 5/2940 |
| pause(words) after 150 ms, 1.5 s | 291 | 1.7% | 98.3% | 0.0% | 340 | 604 | 1016 | 5/2940 |
| pause(words) after 250 ms, 1.5 s | 291 | 1.7% | 86.3% | 12.0% | 248 | 528 | 916 | 5/2940 |
| pause(words) after 400 ms, 1.5 s | 291 | 1.7% | 18.9% | 79.4% | 374 | 634 | 766 | 5/2940 |
| pause(words) after 250 ms, 1.0 s | 291 | 1.7% | 86.3% | 12.0% | 248 | 528 | 916 | 5/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 50.0% (0.0%) n=4 | 19.9% (0.0%) n=166 | 0.0% (0.0%) n=1 | 7.1% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=4 | 0.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=4 | 6.0% (0.0%) n=168 | 0.0% (0.0%) n=1 | 0.0% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=4 | 2.8% (24.1%) n=145 | 0.0% (0.0%) n=1 | 1.2% (0.0%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=4 | 2.8% (97.2%) n=145 | 0.0% (100.0%) n=1 | 1.2% (98.8%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=4 | 2.8% (97.2%) n=145 | 0.0% (100.0%) n=1 | 1.2% (98.8%) n=85 | - (-) n=0 | 0.0% (100.0%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 0.0% (75.0%) n=4 | 2.8% (87.6%) n=145 | 0.0% (100.0%) n=1 | 1.2% (85.9%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 0.0% (50.0%) n=4 | 2.8% (32.4%) n=145 | 0.0% (0.0%) n=1 | 1.2% (7.1%) n=85 | - (-) n=0 | 0.0% (0.0%) n=44 | 0.0% (0.0%) n=12 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 0.0% (75.0%) n=4 | 2.8% (87.6%) n=145 | 0.0% (100.0%) n=1 | 1.2% (85.9%) n=85 | - (-) n=0 | 0.0% (79.5%) n=44 | 0.0% (100.0%) n=12 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=3 | 7.6% (0.0%) n=66 | 10.1% (0.0%) n=109 | 17.0% (0.0%) n=88 | 21.7% (0.0%) n=46 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 0.0% (0.0%) n=89 | 0.0% (0.0%) n=46 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=3 | 0.0% (0.0%) n=67 | 0.0% (0.0%) n=109 | 6.7% (0.0%) n=89 | 8.7% (0.0%) n=46 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (0.0%) n=3 | 1.6% (6.5%) n=62 | 2.9% (5.9%) n=102 | 1.2% (17.1%) n=82 | 0.0% (26.2%) n=42 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=3 | 1.6% (98.4%) n=62 | 2.9% (97.1%) n=102 | 1.2% (98.8%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=3 | 1.6% (98.4%) n=62 | 2.9% (97.1%) n=102 | 1.2% (98.8%) n=82 | 0.0% (100.0%) n=42 |
| pause(words) after 250 ms, 1.5 s | 0.0% (33.3%) n=3 | 1.6% (82.3%) n=62 | 2.9% (87.3%) n=102 | 1.2% (89.0%) n=82 | 0.0% (88.1%) n=42 |
| pause(words) after 400 ms, 1.5 s | 0.0% (0.0%) n=3 | 1.6% (12.9%) n=62 | 2.9% (14.7%) n=102 | 1.2% (22.0%) n=82 | 0.0% (33.3%) n=42 |
| pause(words) after 250 ms, 1.0 s | 0.0% (33.3%) n=3 | 1.6% (82.3%) n=62 | 2.9% (87.3%) n=102 | 1.2% (89.0%) n=82 | 0.0% (88.1%) n=42 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 573 | 1070 | 3 | 400 | 400 | 400 | 400 | 390 | 882 |
| Pipecat-like: min_words = 1 | 1575 | 248 | 13 | 235 | 326 | 1149 | 256 | 694 | 710 | 694 | 710 | 690 | 1010 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 2 | 0 | 2 | 534 | 1078 | 5 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 37 | 6 | 31 | 330 | 1100 | 31 | 500 | 500 | 698 | 882 | 700 | 940 |
| pause(words) after 0 ms, 1.5 s | 1575 | 31 | 0 | 31 | 414 | 1098 | 31 | 102 | 110 | 698 | 882 | 700 | 950 |
| pause(words) after 150 ms, 1.5 s | 1575 | 31 | 0 | 31 | 346 | 1101 | 33 | 150 | 150 | 698 | 882 | 700 | 940 |
| pause(words) after 250 ms, 1.5 s | 1575 | 31 | 0 | 31 | 344 | 1098 | 31 | 250 | 250 | 698 | 882 | 700 | 940 |
| pause(words) after 400 ms, 1.5 s | 1575 | 31 | 0 | 31 | 330 | 1101 | 31 | 400 | 400 | 698 | 882 | 700 | 940 |
| pause(words) after 250 ms, 1.0 s | 1575 | 32 | 2 | 30 | 484 | 1075 | 30 | 250 | 250 | 698 | 882 | 720 | 980 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 28/525 (3) | 207/525 (10) | 21/63 (0) | 201/1008 (0) | 13/504 (13) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 2/525 (0) | 0/63 (0) | 2/1008 (0) | 0/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 14/525 (0) | 17/525 (6) | 6/63 (0) | 24/1008 (0) | 1/504 (6) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 14/525 (0) | 17/525 (0) | 6/63 (0) | 24/1008 (0) | 1/504 (0) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 14/525 (0) | 17/525 (0) | 6/63 (0) | 24/1008 (0) | 1/504 (0) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 14/525 (0) | 17/525 (0) | 6/63 (0) | 24/1008 (0) | 1/504 (0) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 14/525 (0) | 17/525 (0) | 6/63 (0) | 24/1008 (0) | 1/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 13/525 (0) | 17/525 (2) | 6/63 (0) | 24/1008 (2) | 0/504 (0) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 100.0% (0.0%) n=63 | 92.5% (0.0%) n=67 | 85.7% (0.0%) n=112 | 91.2% (0.0%) n=57 | 97.3% (0.0%) n=451 | - | - | - |
| Pipecat-like: min_words = 1 | 85.7% (0.0%) n=70 | 72.7% (0.0%) n=77 | 0.0% (0.0%) n=151 | 41.9% (0.0%) n=86 | 96.5% (0.0%) n=452 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 100.0% (0.0%) n=63 | 91.2% (0.0%) n=68 | 79.5% (0.0%) n=112 | 80.3% (0.0%) n=61 | 96.5% (0.0%) n=452 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 91.0% (7.5%) n=67 | 83.3% (11.1%) n=72 | 18.4% (66.7%) n=114 | 67.2% (26.9%) n=67 | 100.0% (0.0%) n=448 | 894 | 1822 | 2000 |
| pause(words) after 0 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 47.3% (42.0%) n=112 | 73.8% (26.2%) n=65 | 100.0% (0.0%) n=448 | 1040 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 44.5% (45.4%) n=119 | 71.2% (28.8%) n=66 | 100.0% (0.0%) n=448 | 1120 | 1500 | 1500 |
| pause(words) after 250 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (15.3%) n=72 | 43.4% (45.1%) n=113 | 71.2% (28.8%) n=66 | 100.0% (0.0%) n=448 | 1012 | 1364 | 1500 |
| pause(words) after 400 ms, 1.5 s | 92.5% (7.5%) n=67 | 84.7% (11.1%) n=72 | 37.2% (50.4%) n=113 | 69.7% (28.8%) n=66 | 100.0% (0.0%) n=448 | 886 | 1500 | 1500 |
| pause(words) after 250 ms, 1.0 s | 96.9% (3.1%) n=65 | 89.9% (10.1%) n=69 | 56.8% (32.4%) n=111 | 75.0% (25.0%) n=64 | 100.0% (0.0%) n=448 | 724 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 59/63 | 63/63 | 48/63 | 2/420 | 33/420 | 0/420 | 6/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| Pipecat-like: min_words = 1 | 60/63 | 54/63 | 0/63 | 36/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 59/63 | 60/63 | 48/63 | 0/420 | 10/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 61/63 | 57/63 | 21/63 | 45/63 | 0/420 | 4/420 | 0/420 | 1/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 0 ms, 1.5 s | 62/63 | 58/63 | 43/63 | 48/63 | 0/420 | 4/420 | 0/420 | 1/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 150 ms, 1.5 s | 62/63 | 58/63 | 43/63 | 47/63 | 0/420 | 4/420 | 0/420 | 1/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 250 ms, 1.5 s | 62/63 | 58/63 | 42/63 | 47/63 | 0/420 | 4/420 | 0/420 | 1/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 400 ms, 1.5 s | 62/63 | 58/63 | 42/63 | 46/63 | 0/420 | 4/420 | 0/420 | 1/420 | 0/420 | 0/420 | 0/420 | 418/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 59/63 | 53/63 | 48/63 | 0/420 | 4/420 | 0/420 | 1/420 | 0/420 | 0/420 | 0/420 | 418/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 255 | far-field recording | -10 dB | 3.75 s | 6.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 475 | far-field recording | -14 dB | 3.75 s | 6.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 2699 | far-field recording | -6 dB | 3.75 s | 6.16 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 4702 | far-field recording | -4 dB | 4.16 s | 6.60 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3219 | far-field recording | -10 dB | 4.71 s | 7.12 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 1239 | loudspeaker in a room | -18 dB | 7.75 s | 7.98 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 2480 | Turkish broadcast | -18 dB | 4.87 s | 6.28 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 918 | loudspeaker in a room | -6 dB | 8.00 s | 8.52 s | 1402 | resumed |
| pause(words) after 150 ms, 1.5 s | 2232 | far-field recording | -4 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 2452 | far-field recording | -6 dB | 7.91 s | 9.38 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3891 | keyboard | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4393 | street | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3961 | far-field recording | -4 dB | 5.44 s | 7.02 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 426 | far-field recording | -6 dB | 4.99 s | 6.56 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2230 | far-field recording | -18 dB | 7.97 s | 9.00 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 2724 | far-field recording | -18 dB | 7.97 s | 9.70 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.75 s | 9.48 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1736 | far-field recording | -18 dB | 5.67 s | 7.34 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 8 | far-field recording | -10 dB | 8.23 s | 10.06 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4207 | far-field recording | -10 dB | 9.25 s | 11.08 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 502 | far-field recording | -10 dB | 8.23 s | 10.06 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 1167 | far-field recording | -6 dB | 4.71 s | 6.56 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3713 | far-field recording | -10 dB | 9.25 s | 11.08 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 428 | Turkish broadcast | -6 dB | 3.65 s | 4.84 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 198 | clean line | +0 dB | 9.89 s | 10.96 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 255 | far-field recording | -10 dB | 7.55 s | 8.64 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2722 | loudspeaker in a room | -10 dB | 8.03 s | 9.22 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2975 | Turkish broadcast | -10 dB | 7.62 s | 8.80 s | 1000 | resumed |


### modelled recognizer (short utterances missed x2), a word in 10% of noise windows, telephone

**Summary.** *Noise*: candidates of quiet calls with noise without speech in it. *Pauses/100*: pauses of the bot per 100 such calls; *quiet for*: how long the bot was silent before it resumed (ms, median / p90); *cut off*: share of those candidates that cut the bot off for good. *Real*: barge-in calls the VAD started on the caller in but that never got a cut (*held*), as a share of all barge-in calls of that kind (the same denominators in every mode: 525 sentences, 0.9 s commands and 0.5 s command words each); *weighted* counts them 60% sentences, 25% 0.9 s commands, 15% 0.5 s command words (an assumption about what callers say, not a measurement). *Silence*: onset to the caller hearing the bot stop; *final cut*: onset to the cut (ms, median / p90).

| mode | noise pauses/100 | quiet for | noise cut off | held: sentence | 0.9 s command | 0.5 s word | **held, weighted** | silence | final cut |
|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 0.0 | - / - | 18.9% | 0/525 (0.0%) | 0/525 (0.0%) | 0/525 (0.0%) | **0.0%** | 400 | 400 / 400 |
| Pipecat-like: min_words = 1 | 0.0 | - / - | 0.0% | 0/525 (0.0%) | 80/525 (15.2%) | 226/525 (43.0%) | **10.3%** | 460 | 460 / 710 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0 | - / - | 4.9% | 0/525 (0.0%) | 0/525 (0.0%) | 3/525 (0.6%) | **0.1%** | 600 | 600 / 600 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1.9 | 286 / 594 | 2.1% | 0/525 (0.0%) | 66/525 (12.6%) | 57/525 (10.9%) | **4.8%** | 500 | 698 / 902 |
| pause(words) after 0 ms, 1.5 s | 9.5 | 400 / 680 | 2.1% | 0/525 (0.0%) | 65/525 (12.4%) | 57/525 (10.9%) | **4.7%** | 102 | 698 / 906 |
| pause(words) after 150 ms, 1.5 s | 9.5 | 348 / 636 | 2.1% | 0/525 (0.0%) | 65/525 (12.4%) | 58/525 (11.0%) | **4.8%** | 150 | 698 / 902 |
| pause(words) after 250 ms, 1.5 s | 8.2 | 272 / 540 | 2.1% | 0/525 (0.0%) | 65/525 (12.4%) | 57/525 (10.9%) | **4.7%** | 250 | 698 / 902 |
| pause(words) after 400 ms, 1.5 s | 2.5 | 374 / 666 | 2.1% | 0/525 (0.0%) | 65/525 (12.4%) | 57/525 (10.9%) | **4.7%** | 400 | 698 / 906 |
| pause(words) after 250 ms, 1.0 s | 8.2 | 272 / 540 | 2.1% | 0/525 (0.0%) | 63/525 (12.0%) | 56/525 (10.7%) | **4.6%** | 250 | 698 / 906 |

**Noise only.** Every VAD start while the bot speaks in a call where the caller says nothing and some noise is on the line (every noise kind without speech in it: real recordings, cough, keyboard, door, dishes, music, street; all levels; babble is with the voices below). *Cut off*: the bot was cut off for good. *Paused, resumed*: the bot went quiet and carried on. *Not heard*: the bot never stopped (a guard held the candidate). *Quiet for*: how long the bot was silent before it resumed (ms). *Calls cut off*: quiet noise calls in which the bot was cut off at least once.

| mode | candidates | cut off | paused, resumed | not heard | quiet for, median | p90 | max | calls cut off |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 302 | 18.9% | 0.0% | 81.1% | - | - | - | 57/2940 |
| Pipecat-like: min_words = 1 | 307 | 0.0% | 0.0% | 100.0% | - | - | - | 0/2940 |
| LiveKit-like, no resume: interrupt after 500 ms | 305 | 4.9% | 0.0% | 95.1% | - | - | - | 15/2940 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 280 | 2.1% | 19.3% | 78.6% | 286 | 594 | 666 | 6/2940 |
| pause(words) after 0 ms, 1.5 s | 280 | 2.1% | 97.9% | 0.0% | 400 | 680 | 1060 | 6/2940 |
| pause(words) after 150 ms, 1.5 s | 280 | 2.1% | 97.9% | 0.0% | 348 | 636 | 1016 | 6/2940 |
| pause(words) after 250 ms, 1.5 s | 280 | 2.1% | 83.9% | 13.9% | 272 | 540 | 916 | 6/2940 |
| pause(words) after 400 ms, 1.5 s | 280 | 2.1% | 25.0% | 72.9% | 374 | 666 | 766 | 6/2940 |
| pause(words) after 250 ms, 1.0 s | 280 | 2.1% | 83.9% | 13.9% | 272 | 540 | 916 | 6/2940 |

Noise only, by kind: *cut off* (in brackets: paused and resumed) over the candidates of the kind.

| mode | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 37.5% (0.0%) n=8 | 27.4% (0.0%) n=179 | 0.0% (0.0%) n=2 | 10.2% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=184 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 8.2% (0.0%) n=182 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 12.5% (12.5%) n=8 | 3.2% (33.8%) n=157 | 0.0% (0.0%) n=2 | 0.0% (0.0%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 0 ms, 1.5 s | 12.5% (87.5%) n=8 | 3.2% (96.8%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 150 ms, 1.5 s | 12.5% (87.5%) n=8 | 3.2% (96.8%) n=157 | 0.0% (100.0%) n=2 | 0.0% (100.0%) n=49 | - (-) n=0 | 0.0% (100.0%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.5 s | 12.5% (75.0%) n=8 | 3.2% (88.5%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |
| pause(words) after 400 ms, 1.5 s | 12.5% (25.0%) n=8 | 3.2% (40.1%) n=157 | 0.0% (0.0%) n=2 | 0.0% (10.2%) n=49 | - (-) n=0 | 0.0% (0.0%) n=55 | 0.0% (0.0%) n=9 | - (-) n=0 |
| pause(words) after 250 ms, 1.0 s | 12.5% (75.0%) n=8 | 3.2% (88.5%) n=157 | 0.0% (50.0%) n=2 | 0.0% (81.6%) n=49 | - (-) n=0 | 0.0% (72.7%) n=55 | 0.0% (100.0%) n=9 | - (-) n=0 |

Noise only, by level below the caller's speech: *cut off* (in brackets: paused and resumed).

| mode | -30 dB | -22 dB | -14 dB | -6 dB | +0 dB |
|---|---|---|---|---|---|
| baseline (today) | 0.0% (0.0%) n=8 | 10.7% (0.0%) n=75 | 16.0% (0.0%) n=100 | 23.6% (0.0%) n=72 | 34.0% (0.0%) n=47 |
| Pipecat-like: min_words = 1 | 0.0% (0.0%) n=8 | 0.0% (0.0%) n=77 | 0.0% (0.0%) n=100 | 0.0% (0.0%) n=74 | 0.0% (0.0%) n=48 |
| LiveKit-like, no resume: interrupt after 500 ms | 0.0% (0.0%) n=8 | 1.3% (0.0%) n=77 | 0.0% (0.0%) n=100 | 12.3% (0.0%) n=73 | 10.6% (0.0%) n=47 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0.0% (14.3%) n=7 | 1.5% (13.2%) n=68 | 4.4% (15.4%) n=91 | 0.0% (26.9%) n=67 | 2.1% (25.5%) n=47 |
| pause(words) after 0 ms, 1.5 s | 0.0% (100.0%) n=7 | 1.5% (98.5%) n=68 | 4.4% (95.6%) n=91 | 0.0% (100.0%) n=67 | 2.1% (97.9%) n=47 |
| pause(words) after 150 ms, 1.5 s | 0.0% (100.0%) n=7 | 1.5% (98.5%) n=68 | 4.4% (95.6%) n=91 | 0.0% (100.0%) n=67 | 2.1% (97.9%) n=47 |
| pause(words) after 250 ms, 1.5 s | 0.0% (57.1%) n=7 | 1.5% (77.9%) n=68 | 4.4% (86.8%) n=91 | 0.0% (83.6%) n=67 | 2.1% (91.5%) n=47 |
| pause(words) after 400 ms, 1.5 s | 0.0% (14.3%) n=7 | 1.5% (19.1%) n=68 | 4.4% (24.2%) n=91 | 0.0% (28.4%) n=67 | 2.1% (31.9%) n=47 |
| pause(words) after 250 ms, 1.0 s | 0.0% (57.1%) n=7 | 1.5% (77.9%) n=68 | 4.4% (86.8%) n=91 | 0.0% (83.6%) n=67 | 2.1% (91.5%) n=47 |

**Real barge-ins.** The barge-in calls (the caller speaks over the bot; clean line, noise or a second voice on the line). *Missed*: the caller never got through; of those, *VAD*: the VAD never started on the caller's speech (no guard can change that; the same in every mode) and *held*: the VAD started and the guards or the rule never let it through. *Too early*: the bot was cut off before the caller spoke. *Real candidates*: the VAD starts that are the caller's own onset; *lost*: of those, the ones that did not end in a cut (the caller has to say it again). *Silence*: from the speech onset to the caller hearing the bot stop; *final cut*: from the onset to the bot being cut off (both in ms).

| mode | barge-in calls | missed | of which VAD | held | too early | real candidates | lost | silence, median | p90 | final cut, median | p90 | time to interrupt (first word), median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 1575 | 0 | 0 | 0 | 524 | 1112 | 7 | 400 | 400 | 400 | 400 | 390 | 880 |
| Pipecat-like: min_words = 1 | 1575 | 319 | 13 | 306 | 311 | 1205 | 342 | 460 | 710 | 460 | 710 | 695 | 1030 |
| LiveKit-like, no resume: interrupt after 500 ms | 1575 | 3 | 0 | 3 | 464 | 1119 | 10 | 600 | 600 | 600 | 600 | 590 | 1080 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1575 | 129 | 6 | 123 | 318 | 1149 | 129 | 500 | 500 | 698 | 902 | 710 | 1030 |
| pause(words) after 0 ms, 1.5 s | 1575 | 123 | 1 | 122 | 366 | 1142 | 126 | 102 | 110 | 698 | 906 | 720 | 1030 |
| pause(words) after 150 ms, 1.5 s | 1575 | 124 | 1 | 123 | 329 | 1150 | 133 | 150 | 150 | 698 | 902 | 700 | 1010 |
| pause(words) after 250 ms, 1.5 s | 1575 | 124 | 2 | 122 | 329 | 1148 | 129 | 250 | 250 | 698 | 902 | 705 | 1010 |
| pause(words) after 400 ms, 1.5 s | 1575 | 123 | 1 | 122 | 318 | 1148 | 128 | 400 | 400 | 698 | 906 | 700 | 1030 |
| pause(words) after 250 ms, 1.0 s | 1575 | 122 | 3 | 119 | 403 | 1132 | 123 | 250 | 250 | 698 | 906 | 720 | 1030 |

Real barge-ins held (missed although the VAD started on the caller; see the table above) by how they were said, and on what line (held / barge-in calls; in brackets: the VAD never started on the caller):

| mode | sentence | 0.9 s command | 0.5 s command word | clean line | with noise | with a second voice |
|---|---|---|---|---|---|---|
| baseline (today) | 0/525 (0) | 0/525 (0) | 0/525 (0) | 0/63 (0) | 0/1008 (0) | 0/504 (0) |
| Pipecat-like: min_words = 1 | 0/525 (0) | 80/525 (3) | 226/525 (10) | 26/63 (0) | 252/1008 (0) | 28/504 (13) |
| LiveKit-like, no resume: interrupt after 500 ms | 0/525 (0) | 0/525 (0) | 3/525 (0) | 0/63 (0) | 2/1008 (0) | 1/504 (0) |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 0/525 (0) | 66/525 (1) | 57/525 (5) | 14/63 (0) | 97/1008 (1) | 12/504 (5) |
| pause(words) after 0 ms, 1.5 s | 0/525 (0) | 65/525 (0) | 57/525 (1) | 14/63 (0) | 97/1008 (0) | 11/504 (1) |
| pause(words) after 150 ms, 1.5 s | 0/525 (0) | 65/525 (0) | 58/525 (1) | 14/63 (0) | 97/1008 (0) | 12/504 (1) |
| pause(words) after 250 ms, 1.5 s | 0/525 (0) | 65/525 (1) | 57/525 (1) | 14/63 (0) | 97/1008 (1) | 11/504 (1) |
| pause(words) after 400 ms, 1.5 s | 0/525 (0) | 65/525 (1) | 57/525 (0) | 14/63 (0) | 97/1008 (1) | 11/504 (0) |
| pause(words) after 250 ms, 1.0 s | 0/525 (0) | 63/525 (0) | 56/525 (3) | 14/63 (0) | 97/1008 (2) | 8/504 (1) |

**Second voices** (television, a person nearby) and babble, for completeness: the false candidates of quiet calls with a second voice or babble, by kind (*cut off*, in brackets: paused and resumed), and *quiet for* over all of them (ms).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | babble | quiet for, median | p90 | max |
|---|---|---|---|---|---|---|---|---|
| baseline (today) | 96.9% (0.0%) n=65 | 85.1% (0.0%) n=74 | 76.7% (0.0%) n=116 | 86.2% (0.0%) n=65 | 97.8% (0.0%) n=445 | - | - | - |
| Pipecat-like: min_words = 1 | 83.3% (0.0%) n=72 | 67.1% (0.0%) n=85 | 0.0% (0.0%) n=150 | 37.6% (0.0%) n=93 | 97.1% (0.0%) n=446 | - | - | - |
| LiveKit-like, no resume: interrupt after 500 ms | 96.9% (0.0%) n=65 | 79.2% (0.0%) n=77 | 60.0% (0.0%) n=120 | 69.4% (0.0%) n=72 | 97.1% (0.0%) n=446 | - | - | - |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 91.0% (7.5%) n=67 | 80.3% (9.2%) n=76 | 6.7% (59.7%) n=134 | 65.4% (19.2%) n=78 | 99.8% (0.0%) n=442 | 762 | 1822 | 2000 |
| pause(words) after 0 ms, 1.5 s | 92.5% (7.5%) n=67 | 80.5% (18.2%) n=77 | 24.1% (57.1%) n=133 | 65.8% (32.9%) n=79 | 99.8% (0.2%) n=442 | 700 | 1390 | 1406 |
| pause(words) after 150 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (17.1%) n=76 | 24.1% (57.1%) n=133 | 68.8% (31.2%) n=77 | 99.8% (0.2%) n=442 | 632 | 1332 | 1460 |
| pause(words) after 250 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (15.8%) n=76 | 20.3% (59.4%) n=133 | 66.7% (33.3%) n=78 | 99.8% (0.2%) n=442 | 560 | 1304 | 1500 |
| pause(words) after 400 ms, 1.5 s | 92.5% (7.5%) n=67 | 81.6% (10.5%) n=76 | 15.8% (54.9%) n=133 | 68.8% (26.0%) n=77 | 99.8% (0.2%) n=442 | 710 | 1222 | 1500 |
| pause(words) after 250 ms, 1.0 s | 96.9% (3.1%) n=65 | 82.7% (14.7%) n=75 | 38.0% (51.2%) n=129 | 68.8% (31.2%) n=77 | 99.8% (0.2%) n=442 | 500 | 1000 | 1000 |

Per call. *Quiet calls cut off*: the caller says nothing and the bot was cut off for good, by what is on the line (cut off / calls).

| mode | dry second voice | loudspeaker in a room | far-field recording | Turkish broadcast | real noise (MUSAN) | cough | keyboard | door | dishes | music | street | babble |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| baseline (today) | 63/63 | 58/63 | 56/63 | 49/63 | 3/420 | 49/420 | 0/420 | 5/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| Pipecat-like: min_words = 1 | 60/63 | 54/63 | 0/63 | 35/63 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| LiveKit-like, no resume: interrupt after 500 ms | 63/63 | 57/63 | 51/63 | 46/63 | 0/420 | 15/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 413/420 |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 61/63 | 56/63 | 9/63 | 44/63 | 1/420 | 5/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 0 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 45/63 | 1/420 | 5/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 150 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 46/63 | 1/420 | 5/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 250 ms, 1.5 s | 62/63 | 57/63 | 24/63 | 45/63 | 1/420 | 5/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 400 ms, 1.5 s | 62/63 | 57/63 | 21/63 | 46/63 | 1/420 | 5/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |
| pause(words) after 250 ms, 1.0 s | 63/63 | 57/63 | 40/63 | 46/63 | 1/420 | 5/420 | 0/420 | 0/420 | 0/420 | 0/420 | 0/420 | 417/420 |

The five longest pauses of each mode that has pauses, to see what a long one is (scenario, what was on the line, its level, where the candidate began and stopped in the call, what became of it):

         | mode | scenario | on the line | level | onset | stop | quiet for (ms) | outcome |
|---|---|---|---|---|---|---|---|
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 969 | far-field recording | -14 dB | 7.68 s | 10.02 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 3192 | far-field recording | -14 dB | 7.71 s | 10.12 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 1741 | Turkish broadcast | -4 dB | 6.85 s | 7.52 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 722 | far-field recording | -14 dB | 8.23 s | 10.54 s | 2000 | resumed |
| LiveKit-like: pause after 500 ms, resume by 2.0 s | 4702 | far-field recording | -4 dB | 4.23 s | 4.74 s | 2000 | resumed |
| pause(words) after 0 ms, 1.5 s | 249 | dry second voice | -10 dB | 5.03 s | 6.38 s | 1406 | resumed |
| pause(words) after 0 ms, 1.5 s | 3169 | Turkish broadcast | -14 dB | 9.44 s | 10.80 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3415 | far-field recording | -6 dB | 4.80 s | 6.22 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 1192 | far-field recording | -6 dB | 4.80 s | 6.18 s | 1402 | resumed |
| pause(words) after 0 ms, 1.5 s | 3959 | far-field recording | -18 dB | 4.80 s | 6.16 s | 1402 | resumed |
| pause(words) after 150 ms, 1.5 s | 4427 | far-field recording | -14 dB | 9.28 s | 9.68 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3891 | keyboard | -22 dB | 11.49 s | 13.00 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 4178 | loudspeaker in a room | -14 dB | 5.06 s | 5.80 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 200 | dry second voice | -6 dB | 8.29 s | 9.80 s | 1500 | resumed |
| pause(words) after 150 ms, 1.5 s | 3897 | music | -22 dB | 11.46 s | 13.04 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 426 | far-field recording | -6 dB | 4.77 s | 6.50 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 4131 | far-field recording | -6 dB | 4.74 s | 6.48 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 3466 | far-field recording | -10 dB | 7.71 s | 9.44 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 1243 | far-field recording | -10 dB | 7.68 s | 9.42 s | 1500 | resumed |
| pause(words) after 250 ms, 1.5 s | 3897 | music | -22 dB | 11.46 s | 13.04 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 426 | far-field recording | -6 dB | 4.77 s | 6.50 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4131 | far-field recording | -6 dB | 4.74 s | 6.48 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3466 | far-field recording | -10 dB | 7.71 s | 9.44 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 3390 | far-field recording | -6 dB | 4.23 s | 6.00 s | 1500 | resumed |
| pause(words) after 400 ms, 1.5 s | 4409 | cough | -14 dB | 10.27 s | 10.64 s | 1500 | resumed |
| pause(words) after 250 ms, 1.0 s | 428 | Turkish broadcast | -6 dB | 3.65 s | 4.84 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 206 | Turkish broadcast | -6 dB | 8.10 s | 9.32 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 945 | far-field recording | -6 dB | 7.84 s | 8.10 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 1444 | cough | -22 dB | 9.38 s | 10.44 s | 1000 | resumed |
| pause(words) after 250 ms, 1.0 s | 2426 | far-field recording | -14 dB | 9.57 s | 10.70 s | 1000 | resumed |


