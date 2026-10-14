# Speculative LLM start on the fixtures

- command: `brisvox-bench speculation --seeds 20 --out bench/results/speculation-fixtures.md`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 1 tokio workers (threads, not a CPU limit: other threads may use any core)
- engine over mock providers on a paused clock (deterministic per seed), FLEURS real-speech fixtures (fixtures/NOTICE)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

The production engine (`EngineFactory`) over mock providers on a paused clock; 20 seeds x 3 fixtures. Latencies are log-normal (sigma 0.3) around STT final 150 ms, TTS first byte 150 ms and the LLM first token of the row (350 ms by default, and a fast LLM of 100 ms that answers before the STT final), one seed per call and the same seeds in every row. V2V is server side (`UserSpeechEnd` to `FirstAudioOut`), in ms. `lagging` interims: the mock's own word prefixes, the last one a word short of the final (every partial misses). `converged`: the full text is also an interim before the VAD stop (every partial hits). Real STTs are between the two. `final only`: speculation at the STT final, never from a partial. delta = p50 against the same policy and LLM latency with speculation off.

Cost columns. `extra requests per turn` = (started - committed) / turns, from the engine's counters: the speculative runs that were not used, each a full prompt sent for nothing. It can reach 1.00 even when every turn commits (fixed 700 ms with lagging interims starts a run at the partial, supersedes it at the final and commits the second). `turns committed` = committed / turns. `run hit rate` = committed / (committed + discarded), counted per run, not per turn. `wasted output tokens` are the tokens a discarded run had buffered (provider-reported, else 4 bytes per token): nonzero only when the run streamed before the miss was known. `wasted input tokens` are what the provider reported for discarded runs: the mock reports usage when its stream reaches the end, so a run cancelled before that reports none (0 for the slow LLM) and a run that had finished by then does (the fast LLM). `prompt tokens per turn` is the estimate (4 characters per token) over every request the provider received, discarded ones included.

Stable-partial gate. `gate N ms` rows offer the partial at the VAD stop only after the caller stayed silent, and the partial text did not change, for N ms (`SpeculationConfig::partial_stable_for`); the STT final of the stop supersedes the wait. Scenarios. `continuous`: the fixtures as recorded. `resumed after P ms`: every utterance is cut in two at the middle of its audio, the halves P ms apart, so the caller pauses mid-sentence and goes on; the turn is the whole utterance (a fixed silence of 900 ms: longer than the pause plus the quiet the cut lands next to), and the answers are the same. The VAD reports the stop 300 ms into the pause and the resumption about 40 ms after the speech restarts, so the engine sees the resumption (P - 300 + 40) ms after the stop. A candidate started by the first half is wasted: `extra requests per turn` counts it, `withdrawn` in the second table the candidates taken back by the driver. The first half's STT final (150 ms after the stop, median) is also a candidate and the gate does not cover it.

| scenario | LLM first token ms | policy | speculation | turns | V2V p50 | p90 | p99 | mean | delta p50 | LLM requests per turn | extra requests per turn | turns committed | run hit rate | mean head start ms | wasted output tokens per turn | wasted input tokens per turn | prompt tokens per turn |
|---|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| continuous | 350 | adaptive | off | 120/120 | 923 | 1106 | 1168 | 941 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| continuous | 350 | adaptive | on, lagging interims | 120/120 | 925 | 1113 | 1244 | 954 | +2 | 2.00 | 1.00 | 0% | 0% | - | 0.00 | 0.0 | 71 |
| continuous | 350 | adaptive | on, converged interims | 120/120 | 782 | 911 | 1013 | 789 | -141 | 1.00 | 0.00 | 100% | 100% | 154 | 0.00 | 0.0 | 37 |
| continuous | 350 | adaptive | on, final only | 120/120 | 923 | 1106 | 1168 | 941 | +0 | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| continuous | 350 | adaptive | on, lagging interims, gate 100 ms | 120/120 | 925 | 1113 | 1244 | 954 | +2 | 2.00 | 1.00 | 0% | 0% | - | 0.00 | 0.0 | 71 |
| continuous | 350 | adaptive | on, converged interims, gate 100 ms | 120/120 | 882 | 1011 | 1113 | 887 | -41 | 1.00 | 0.00 | 100% | 100% | 54 | 0.00 | 0.0 | 37 |
| continuous | 350 | adaptive | on, lagging interims, gate 200 ms | 120/120 | 918 | 1143 | 1300 | 955 | -5 | 1.12 | 0.12 | 0% | 0% | - | 0.00 | 0.0 | 41 |
| continuous | 350 | adaptive | on, converged interims, gate 200 ms | 120/120 | 918 | 1074 | 1168 | 935 | -5 | 1.00 | 0.00 | 12% | 100% | 51 | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 450 ms | off | 120/120 | 963 | 1106 | 1183 | 968 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 450 ms | on, lagging interims | 120/120 | 925 | 1113 | 1244 | 954 | -38 | 2.00 | 1.00 | 72% | 42% | 38 | 0.00 | 0.0 | 71 |
| continuous | 350 | fixed 450 ms | on, converged interims | 120/120 | 782 | 911 | 1013 | 789 | -181 | 1.00 | 0.00 | 100% | 100% | 181 | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 450 ms | on, final only | 120/120 | 923 | 1106 | 1168 | 941 | -40 | 1.00 | 0.00 | 72% | 100% | 38 | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 450 ms | on, lagging interims, gate 100 ms | 120/120 | 925 | 1113 | 1244 | 954 | -38 | 2.00 | 1.00 | 72% | 42% | 38 | 0.00 | 0.0 | 71 |
| continuous | 350 | fixed 450 ms | on, converged interims, gate 100 ms | 120/120 | 882 | 1011 | 1113 | 887 | -81 | 1.00 | 0.00 | 100% | 100% | 81 | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 450 ms | on, lagging interims, gate 200 ms | 120/120 | 918 | 1143 | 1300 | 955 | -45 | 1.12 | 0.12 | 72% | 85% | 38 | 0.00 | 0.0 | 41 |
| continuous | 350 | fixed 450 ms | on, converged interims, gate 200 ms | 120/120 | 918 | 1074 | 1168 | 935 | -45 | 1.00 | 0.00 | 85% | 100% | 40 | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 700 ms | off | 120/120 | 1202 | 1331 | 1433 | 1207 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 700 ms | on, lagging interims | 120/120 | 925 | 1113 | 1244 | 965 | -277 | 2.00 | 1.00 | 100% | 50% | 266 | 0.00 | 0.0 | 71 |
| continuous | 350 | fixed 700 ms | on, converged interims | 120/120 | 857 | 954 | 1013 | 874 | -345 | 1.00 | 0.00 | 100% | 100% | 420 | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 700 ms | on, final only | 120/120 | 925 | 1106 | 1168 | 951 | -277 | 1.00 | 0.00 | 100% | 100% | 266 | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 700 ms | on, lagging interims, gate 100 ms | 120/120 | 925 | 1113 | 1244 | 965 | -277 | 2.00 | 1.00 | 100% | 50% | 266 | 0.00 | 0.0 | 71 |
| continuous | 350 | fixed 700 ms | on, converged interims, gate 100 ms | 120/120 | 893 | 1011 | 1113 | 911 | -309 | 1.00 | 0.00 | 100% | 100% | 320 | 0.00 | 0.0 | 37 |
| continuous | 350 | fixed 700 ms | on, lagging interims, gate 200 ms | 120/120 | 923 | 1143 | 1300 | 965 | -279 | 1.12 | 0.12 | 100% | 89% | 266 | 0.00 | 0.0 | 41 |
| continuous | 350 | fixed 700 ms | on, converged interims, gate 200 ms | 120/120 | 923 | 1074 | 1168 | 945 | -279 | 1.00 | 0.00 | 100% | 100% | 273 | 0.00 | 0.0 | 37 |
| continuous | 100 | adaptive | off | 120/120 | 677 | 799 | 838 | 693 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| continuous | 100 | adaptive | on, lagging interims | 120/120 | 678 | 803 | 843 | 697 | +1 | 2.00 | 1.00 | 0% | 0% | - | 1.95 | 17.4 | 71 |
| continuous | 100 | adaptive | on, converged interims | 120/120 | 572 | 701 | 729 | 596 | -105 | 1.00 | 0.00 | 100% | 100% | 154 | 0.00 | 0.0 | 37 |
| continuous | 100 | adaptive | on, final only | 120/120 | 677 | 799 | 838 | 693 | +0 | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| continuous | 100 | adaptive | on, lagging interims, gate 100 ms | 120/120 | 678 | 803 | 843 | 697 | +1 | 2.00 | 1.00 | 0% | 0% | - | 0.28 | 2.2 | 71 |
| continuous | 100 | adaptive | on, converged interims, gate 100 ms | 120/120 | 637 | 721 | 747 | 647 | -40 | 1.00 | 0.00 | 100% | 100% | 54 | 0.00 | 0.0 | 37 |
| continuous | 100 | adaptive | on, lagging interims, gate 200 ms | 120/120 | 673 | 803 | 873 | 697 | -4 | 1.12 | 0.12 | 0% | 0% | - | 0.03 | 1.2 | 41 |
| continuous | 100 | adaptive | on, converged interims, gate 200 ms | 120/120 | 677 | 773 | 823 | 688 | +0 | 1.00 | 0.00 | 12% | 100% | 51 | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 450 ms | off | 120/120 | 707 | 799 | 838 | 720 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 450 ms | on, lagging interims | 120/120 | 678 | 803 | 843 | 697 | -29 | 2.00 | 1.00 | 72% | 42% | 38 | 1.95 | 17.4 | 71 |
| continuous | 100 | fixed 450 ms | on, converged interims | 120/120 | 607 | 704 | 729 | 621 | -100 | 1.00 | 0.00 | 100% | 100% | 181 | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 450 ms | on, final only | 120/120 | 677 | 799 | 838 | 693 | -30 | 1.00 | 0.00 | 72% | 100% | 38 | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 450 ms | on, lagging interims, gate 100 ms | 120/120 | 678 | 803 | 843 | 697 | -29 | 2.00 | 1.00 | 72% | 42% | 38 | 0.28 | 2.2 | 71 |
| continuous | 100 | fixed 450 ms | on, converged interims, gate 100 ms | 120/120 | 637 | 721 | 747 | 648 | -70 | 1.00 | 0.00 | 100% | 100% | 81 | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 450 ms | on, lagging interims, gate 200 ms | 120/120 | 673 | 803 | 873 | 697 | -34 | 1.12 | 0.12 | 72% | 85% | 38 | 0.03 | 1.2 | 41 |
| continuous | 100 | fixed 450 ms | on, converged interims, gate 200 ms | 120/120 | 677 | 773 | 823 | 688 | -30 | 1.00 | 0.00 | 85% | 100% | 40 | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 700 ms | off | 120/120 | 950 | 1033 | 1067 | 959 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 700 ms | on, lagging interims | 120/120 | 844 | 930 | 954 | 860 | -106 | 2.00 | 1.00 | 100% | 50% | 266 | 1.95 | 17.4 | 71 |
| continuous | 100 | fixed 700 ms | on, converged interims | 120/120 | 844 | 930 | 954 | 860 | -106 | 1.00 | 0.00 | 100% | 100% | 420 | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 700 ms | on, final only | 120/120 | 844 | 930 | 954 | 860 | -106 | 1.00 | 0.00 | 100% | 100% | 266 | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 700 ms | on, lagging interims, gate 100 ms | 120/120 | 844 | 930 | 954 | 860 | -106 | 2.00 | 1.00 | 100% | 50% | 266 | 0.28 | 2.2 | 71 |
| continuous | 100 | fixed 700 ms | on, converged interims, gate 100 ms | 120/120 | 844 | 930 | 954 | 860 | -106 | 1.00 | 0.00 | 100% | 100% | 320 | 0.00 | 0.0 | 37 |
| continuous | 100 | fixed 700 ms | on, lagging interims, gate 200 ms | 120/120 | 844 | 930 | 954 | 860 | -106 | 1.12 | 0.12 | 100% | 89% | 266 | 0.03 | 1.2 | 41 |
| continuous | 100 | fixed 700 ms | on, converged interims, gate 200 ms | 120/120 | 844 | 930 | 954 | 860 | -106 | 1.00 | 0.00 | 100% | 100% | 273 | 0.00 | 0.0 | 37 |
| resumed after 350 ms | 350 | fixed 900 ms | off | 120/120 | 1402 | 1531 | 1633 | 1407 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| resumed after 350 ms | 350 | fixed 900 ms | on, lagging interims | 120/120 | 1066 | 1194 | 1488 | 1099 | -336 | 3.04 | 2.04 | 96% | 32% | 454 | 0.17 | 2.0 | 94 |
| resumed after 350 ms | 350 | fixed 900 ms | on, converged interims | 120/120 | 1044 | 1130 | 1154 | 1060 | -358 | 2.00 | 1.00 | 100% | 50% | 620 | 0.00 | 0.0 | 61 |
| resumed after 350 ms | 350 | fixed 900 ms | on, final only | 120/120 | 1066 | 1153 | 1160 | 1071 | -336 | 1.04 | 0.04 | 100% | 96% | 455 | 0.00 | 0.0 | 37 |
| resumed after 350 ms | 350 | fixed 900 ms | on, lagging interims, gate 100 ms | 120/120 | 1071 | 1194 | 1488 | 1092 | -331 | 2.48 | 1.48 | 97% | 39% | 454 | 0.03 | 0.7 | 78 |
| resumed after 350 ms | 350 | fixed 900 ms | on, converged interims, gate 100 ms | 120/120 | 1056 | 1137 | 1154 | 1065 | -346 | 1.50 | 0.50 | 100% | 67% | 521 | 0.00 | 0.0 | 47 |
| resumed after 350 ms | 350 | fixed 900 ms | on, lagging interims, gate 200 ms | 120/120 | 1070 | 1154 | 1190 | 1078 | -332 | 1.32 | 0.32 | 100% | 76% | 455 | 0.00 | 0.0 | 49 |
| resumed after 350 ms | 350 | fixed 900 ms | on, converged interims, gate 200 ms | 120/120 | 1057 | 1137 | 1160 | 1068 | -345 | 1.04 | 0.04 | 100% | 96% | 466 | 0.00 | 0.0 | 37 |
| resumed after 350 ms | 100 | fixed 900 ms | off | 120/120 | 1150 | 1233 | 1267 | 1159 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| resumed after 350 ms | 100 | fixed 900 ms | on, lagging interims | 120/120 | 1053 | 1137 | 1169 | 1065 | -97 | 3.04 | 2.04 | 96% | 32% | 454 | 2.62 | 24.7 | 94 |
| resumed after 350 ms | 100 | fixed 900 ms | on, converged interims | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 2.00 | 1.00 | 100% | 50% | 620 | 1.23 | 1.7 | 61 |
| resumed after 350 ms | 100 | fixed 900 ms | on, final only | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 1.04 | 0.04 | 100% | 96% | 455 | 0.00 | 0.0 | 37 |
| resumed after 350 ms | 100 | fixed 900 ms | on, lagging interims, gate 100 ms | 120/120 | 1053 | 1137 | 1169 | 1063 | -97 | 2.48 | 1.48 | 97% | 39% | 454 | 0.62 | 7.0 | 78 |
| resumed after 350 ms | 100 | fixed 900 ms | on, converged interims, gate 100 ms | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 1.50 | 0.50 | 100% | 67% | 521 | 0.00 | 0.0 | 47 |
| resumed after 350 ms | 100 | fixed 900 ms | on, lagging interims, gate 200 ms | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 1.32 | 0.32 | 100% | 76% | 455 | 0.10 | 0.0 | 49 |
| resumed after 350 ms | 100 | fixed 900 ms | on, converged interims, gate 200 ms | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 1.04 | 0.04 | 100% | 96% | 466 | 0.00 | 0.0 | 37 |
| resumed after 400 ms | 350 | fixed 900 ms | off | 120/120 | 1402 | 1531 | 1633 | 1407 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| resumed after 400 ms | 350 | fixed 900 ms | on, lagging interims | 120/120 | 1101 | 1488 | 1882 | 1185 | -301 | 3.28 | 2.28 | 72% | 24% | 451 | 0.42 | 8.8 | 99 |
| resumed after 400 ms | 350 | fixed 900 ms | on, converged interims | 120/120 | 1044 | 1130 | 1154 | 1060 | -358 | 2.00 | 1.00 | 100% | 50% | 620 | 0.00 | 0.0 | 61 |
| resumed after 400 ms | 350 | fixed 900 ms | on, final only | 120/120 | 1066 | 1153 | 1181 | 1073 | -336 | 1.28 | 0.28 | 100% | 78% | 455 | 0.00 | 0.0 | 43 |
| resumed after 400 ms | 350 | fixed 900 ms | on, lagging interims, gate 100 ms | 120/120 | 1079 | 1468 | 1691 | 1166 | -323 | 3.21 | 2.21 | 76% | 26% | 453 | 0.24 | 6.0 | 96 |
| resumed after 400 ms | 350 | fixed 900 ms | on, converged interims, gate 100 ms | 120/120 | 1056 | 1130 | 1154 | 1065 | -346 | 2.00 | 1.00 | 100% | 50% | 521 | 0.00 | 0.0 | 61 |
| resumed after 400 ms | 350 | fixed 900 ms | on, lagging interims, gate 200 ms | 120/120 | 1070 | 1154 | 1234 | 1079 | -332 | 1.58 | 0.58 | 100% | 63% | 455 | 0.00 | 0.0 | 54 |
| resumed after 400 ms | 350 | fixed 900 ms | on, converged interims, gate 200 ms | 120/120 | 1057 | 1153 | 1171 | 1069 | -345 | 1.30 | 0.30 | 100% | 77% | 466 | 0.00 | 0.0 | 43 |
| resumed after 400 ms | 100 | fixed 900 ms | off | 120/120 | 1150 | 1233 | 1267 | 1159 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| resumed after 400 ms | 100 | fixed 900 ms | on, lagging interims | 120/120 | 1074 | 1170 | 1284 | 1092 | -76 | 3.28 | 2.28 | 72% | 24% | 451 | 3.29 | 35.4 | 99 |
| resumed after 400 ms | 100 | fixed 900 ms | on, converged interims | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 2.00 | 1.00 | 100% | 50% | 620 | 1.84 | 11.3 | 61 |
| resumed after 400 ms | 100 | fixed 900 ms | on, final only | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 1.28 | 0.28 | 100% | 78% | 455 | 0.14 | 0.1 | 43 |
| resumed after 400 ms | 100 | fixed 900 ms | on, lagging interims, gate 100 ms | 120/120 | 1071 | 1169 | 1278 | 1085 | -79 | 3.21 | 2.21 | 76% | 26% | 453 | 1.17 | 11.2 | 96 |
| resumed after 400 ms | 100 | fixed 900 ms | on, converged interims, gate 100 ms | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 2.00 | 1.00 | 100% | 50% | 521 | 0.39 | 0.6 | 61 |
| resumed after 400 ms | 100 | fixed 900 ms | on, lagging interims, gate 200 ms | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 1.58 | 0.58 | 100% | 63% | 455 | 0.22 | 0.3 | 54 |
| resumed after 400 ms | 100 | fixed 900 ms | on, converged interims, gate 200 ms | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 1.30 | 0.30 | 100% | 77% | 466 | 0.14 | 0.1 | 43 |
| resumed after 600 ms | 350 | fixed 900 ms | off | 120/120 | 1402 | 1531 | 1633 | 1407 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| resumed after 600 ms | 350 | fixed 900 ms | on, lagging interims | 120/120 | 1434 | 1551 | 1719 | 1430 | +32 | 3.98 | 2.98 | 2% | 1% | 410 | 1.32 | 32.3 | 117 |
| resumed after 600 ms | 350 | fixed 900 ms | on, converged interims | 120/120 | 1044 | 1130 | 1154 | 1060 | -358 | 2.00 | 1.00 | 100% | 50% | 620 | 0.99 | 5.4 | 61 |
| resumed after 600 ms | 350 | fixed 900 ms | on, final only | 120/120 | 1072 | 1154 | 1234 | 1083 | -330 | 1.98 | 0.97 | 100% | 51% | 455 | 0.14 | 0.1 | 60 |
| resumed after 600 ms | 350 | fixed 900 ms | on, lagging interims, gate 100 ms | 120/120 | 1434 | 1551 | 1719 | 1416 | +32 | 3.90 | 2.90 | 10% | 3% | 482 | 1.00 | 24.0 | 113 |
| resumed after 600 ms | 350 | fixed 900 ms | on, converged interims, gate 100 ms | 120/120 | 1056 | 1130 | 1154 | 1065 | -346 | 2.00 | 1.00 | 100% | 50% | 521 | 0.16 | 2.1 | 61 |
| resumed after 600 ms | 350 | fixed 900 ms | on, lagging interims, gate 200 ms | 120/120 | 1066 | 1154 | 1484 | 1092 | -336 | 2.48 | 1.48 | 92% | 39% | 463 | 0.17 | 1.2 | 78 |
| resumed after 600 ms | 350 | fixed 900 ms | on, converged interims, gate 200 ms | 120/120 | 1071 | 1152 | 1205 | 1074 | -331 | 2.00 | 1.00 | 100% | 50% | 466 | 0.14 | 0.1 | 61 |
| resumed after 600 ms | 100 | fixed 900 ms | off | 120/120 | 1150 | 1233 | 1267 | 1159 | - | 1.00 | 0.00 | - | - | - | 0.00 | 0.0 | 37 |
| resumed after 600 ms | 100 | fixed 900 ms | on, lagging interims | 120/120 | 1161 | 1258 | 1308 | 1166 | +11 | 3.98 | 2.98 | 2% | 1% | 410 | 3.39 | 66.1 | 117 |
| resumed after 600 ms | 100 | fixed 900 ms | on, converged interims | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 2.00 | 1.00 | 100% | 50% | 620 | 1.00 | 23.5 | 61 |
| resumed after 600 ms | 100 | fixed 900 ms | on, final only | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 1.98 | 0.97 | 100% | 51% | 455 | 1.47 | 14.7 | 60 |
| resumed after 600 ms | 100 | fixed 900 ms | on, lagging interims, gate 100 ms | 120/120 | 1161 | 1258 | 1308 | 1159 | +11 | 3.90 | 2.90 | 10% | 3% | 482 | 2.59 | 51.4 | 113 |
| resumed after 600 ms | 100 | fixed 900 ms | on, converged interims, gate 100 ms | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 2.00 | 1.00 | 100% | 50% | 521 | 1.00 | 23.5 | 61 |
| resumed after 600 ms | 100 | fixed 900 ms | on, lagging interims, gate 200 ms | 120/120 | 1056 | 1153 | 1220 | 1066 | -94 | 2.48 | 1.48 | 92% | 39% | 463 | 1.38 | 19.2 | 78 |
| resumed after 600 ms | 100 | fixed 900 ms | on, converged interims, gate 200 ms | 120/120 | 1044 | 1130 | 1154 | 1060 | -106 | 2.00 | 1.00 | 100% | 50% | 466 | 1.44 | 17.0 | 61 |

What became of the speculative runs (counts over all turns of the row):

| scenario | LLM first token ms | policy | speculation | started | kept | committed | missed | superseded | withdrawn | abandoned | other discarded | skipped (busy) | failed |
|---|---:|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| continuous | 350 | adaptive | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | adaptive | on, lagging interims | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | adaptive | on, converged interims | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | adaptive | on, final only | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | adaptive | on, lagging interims, gate 100 ms | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | adaptive | on, converged interims, gate 100 ms | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | adaptive | on, lagging interims, gate 200 ms | 15 | 0 | 0 | 15 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | adaptive | on, converged interims, gate 200 ms | 15 | 0 | 15 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 450 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 450 ms | on, lagging interims | 207 | 0 | 87 | 33 | 87 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 450 ms | on, converged interims | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 450 ms | on, final only | 87 | 0 | 87 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 450 ms | on, lagging interims, gate 100 ms | 207 | 0 | 87 | 33 | 87 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 450 ms | on, converged interims, gate 100 ms | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 450 ms | on, lagging interims, gate 200 ms | 102 | 0 | 87 | 15 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 450 ms | on, converged interims, gate 200 ms | 102 | 0 | 102 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 700 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 700 ms | on, lagging interims | 240 | 0 | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 700 ms | on, converged interims | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 700 ms | on, final only | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 700 ms | on, lagging interims, gate 100 ms | 240 | 0 | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 700 ms | on, converged interims, gate 100 ms | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 700 ms | on, lagging interims, gate 200 ms | 135 | 0 | 120 | 0 | 15 | 0 | 0 | 0 | 0 | 0 |
| continuous | 350 | fixed 700 ms | on, converged interims, gate 200 ms | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | adaptive | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | adaptive | on, lagging interims | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | adaptive | on, converged interims | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | adaptive | on, final only | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | adaptive | on, lagging interims, gate 100 ms | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | adaptive | on, converged interims, gate 100 ms | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | adaptive | on, lagging interims, gate 200 ms | 15 | 0 | 0 | 15 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | adaptive | on, converged interims, gate 200 ms | 15 | 0 | 15 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 450 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 450 ms | on, lagging interims | 207 | 0 | 87 | 33 | 87 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 450 ms | on, converged interims | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 450 ms | on, final only | 87 | 0 | 87 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 450 ms | on, lagging interims, gate 100 ms | 207 | 0 | 87 | 33 | 87 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 450 ms | on, converged interims, gate 100 ms | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 450 ms | on, lagging interims, gate 200 ms | 102 | 0 | 87 | 15 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 450 ms | on, converged interims, gate 200 ms | 102 | 0 | 102 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 700 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 700 ms | on, lagging interims | 240 | 0 | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 700 ms | on, converged interims | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 700 ms | on, final only | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 700 ms | on, lagging interims, gate 100 ms | 240 | 0 | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 700 ms | on, converged interims, gate 100 ms | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 700 ms | on, lagging interims, gate 200 ms | 135 | 0 | 120 | 0 | 15 | 0 | 0 | 0 | 0 | 0 |
| continuous | 100 | fixed 700 ms | on, converged interims, gate 200 ms | 120 | 0 | 120 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 350 | fixed 900 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 350 | fixed 900 ms | on, lagging interims | 360 | 0 | 115 | 5 | 120 | 120 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 350 | fixed 900 ms | on, converged interims | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 350 | fixed 900 ms | on, final only | 125 | 0 | 120 | 0 | 0 | 5 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 350 | fixed 900 ms | on, lagging interims, gate 100 ms | 294 | 0 | 116 | 4 | 114 | 60 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 350 | fixed 900 ms | on, converged interims, gate 100 ms | 180 | 0 | 120 | 0 | 0 | 60 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 350 | fixed 900 ms | on, lagging interims, gate 200 ms | 158 | 0 | 120 | 0 | 33 | 5 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 350 | fixed 900 ms | on, converged interims, gate 200 ms | 125 | 0 | 120 | 0 | 0 | 5 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 100 | fixed 900 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 100 | fixed 900 ms | on, lagging interims | 360 | 0 | 115 | 5 | 120 | 120 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 100 | fixed 900 ms | on, converged interims | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 100 | fixed 900 ms | on, final only | 125 | 0 | 120 | 0 | 0 | 5 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 100 | fixed 900 ms | on, lagging interims, gate 100 ms | 294 | 0 | 116 | 4 | 114 | 60 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 100 | fixed 900 ms | on, converged interims, gate 100 ms | 180 | 0 | 120 | 0 | 0 | 60 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 100 | fixed 900 ms | on, lagging interims, gate 200 ms | 158 | 0 | 120 | 0 | 33 | 5 | 0 | 0 | 0 | 0 |
| resumed after 350 ms | 100 | fixed 900 ms | on, converged interims, gate 200 ms | 125 | 0 | 120 | 0 | 0 | 5 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 350 | fixed 900 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 350 | fixed 900 ms | on, lagging interims | 360 | 0 | 86 | 34 | 120 | 120 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 350 | fixed 900 ms | on, converged interims | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 350 | fixed 900 ms | on, final only | 154 | 0 | 120 | 0 | 0 | 34 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 350 | fixed 900 ms | on, lagging interims, gate 100 ms | 356 | 0 | 91 | 29 | 116 | 120 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 350 | fixed 900 ms | on, converged interims, gate 100 ms | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 350 | fixed 900 ms | on, lagging interims, gate 200 ms | 190 | 0 | 120 | 0 | 34 | 36 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 350 | fixed 900 ms | on, converged interims, gate 200 ms | 156 | 0 | 120 | 0 | 0 | 36 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 100 | fixed 900 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 100 | fixed 900 ms | on, lagging interims | 360 | 0 | 86 | 34 | 120 | 120 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 100 | fixed 900 ms | on, converged interims | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 100 | fixed 900 ms | on, final only | 154 | 0 | 120 | 0 | 0 | 34 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 100 | fixed 900 ms | on, lagging interims, gate 100 ms | 356 | 0 | 91 | 29 | 116 | 120 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 100 | fixed 900 ms | on, converged interims, gate 100 ms | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 100 | fixed 900 ms | on, lagging interims, gate 200 ms | 190 | 0 | 120 | 0 | 34 | 36 | 0 | 0 | 0 | 0 |
| resumed after 400 ms | 100 | fixed 900 ms | on, converged interims, gate 200 ms | 156 | 0 | 120 | 0 | 0 | 36 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 350 | fixed 900 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 350 | fixed 900 ms | on, lagging interims | 360 | 0 | 3 | 117 | 120 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 350 | fixed 900 ms | on, converged interims | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 350 | fixed 900 ms | on, final only | 237 | 0 | 120 | 0 | 0 | 117 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 350 | fixed 900 ms | on, lagging interims, gate 100 ms | 360 | 0 | 12 | 108 | 120 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 350 | fixed 900 ms | on, converged interims, gate 100 ms | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 350 | fixed 900 ms | on, lagging interims, gate 200 ms | 288 | 0 | 111 | 9 | 48 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 350 | fixed 900 ms | on, converged interims, gate 200 ms | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 100 | fixed 900 ms | off | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 100 | fixed 900 ms | on, lagging interims | 360 | 0 | 3 | 117 | 120 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 100 | fixed 900 ms | on, converged interims | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 100 | fixed 900 ms | on, final only | 237 | 0 | 120 | 0 | 0 | 117 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 100 | fixed 900 ms | on, lagging interims, gate 100 ms | 360 | 0 | 12 | 108 | 120 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 100 | fixed 900 ms | on, converged interims, gate 100 ms | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 100 | fixed 900 ms | on, lagging interims, gate 200 ms | 288 | 0 | 111 | 9 | 48 | 120 | 0 | 0 | 0 | 0 |
| resumed after 600 ms | 100 | fixed 900 ms | on, converged interims, gate 200 ms | 240 | 0 | 120 | 0 | 0 | 120 | 0 | 0 | 0 | 0 |
