# Voice-to-voice latency (sim)

- command: `brisvox-bench v2v --sim --target mock-bot --runs 10 --out-md bench/results/v2v-mock-bot.md --out-json bench/results/v2v-mock-bot.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: in-process mock bot behind the real websocket transport (a self-check of the harness and the transport, not the engine), one client, one call at a time, real time (SystemClock)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:54623/...`
- 10 runs of each of en_two_utterances, tr_two_utterances, en_tr_mixed (utterances spaced at least 4000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- mock bot: end-of-turn after 500 ms of silence, then 300 ms (+ up to 0 ms jitter) of simulated provider time, then a 600 ms reply. Latency floor (end-of-turn + provider time) 800 ms; the rest of the measurement is the bot's 20 ms frame granularity plus the websocket transport

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.

## Summary

| scope | turns | answered | talked over | bot speaking | no response | p50 ms | p90 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 60 | 60 | 0 | 0 | 0 | 827.907 | 845.991 | 858.035 | 858.035 |
| fixture en_two_utterances | 20 | 20 | 0 | 0 | 0 | 827.943 | 845.046 | 852.525 | 852.525 |
| fixture tr_two_utterances | 20 | 20 | 0 | 0 | 0 | 826.156 | 841.240 | 847.998 | 847.998 |
| fixture en_tr_mixed | 20 | 20 | 0 | 0 | 0 | 831.467 | 849.719 | 858.035 | 858.035 |
| language en | 30 | 30 | 0 | 0 | 0 | 832.592 | 849.719 | 858.035 | 858.035 |
| language tr | 30 | 30 | 0 | 0 | 0 | 825.851 | 839.941 | 847.998 | 847.998 |

## Per turn

| fixture | turn | lang | EOS at ms | answered/turns | p50 ms | p90 ms | p99 ms | max ms |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| en_two_utterances | 0 | en | 6670.000 | 10/10 | 837.946 | 845.991 | 852.525 | 852.525 |
| en_two_utterances | 1 | en | 13430.000 | 10/10 | 823.260 | 827.690 | 827.943 | 827.943 |
| tr_two_utterances | 0 | tr | 6300.000 | 10/10 | 825.851 | 841.539 | 847.998 | 847.998 |
| tr_two_utterances | 1 | tr | 17090.000 | 10/10 | 826.156 | 834.256 | 841.240 | 841.240 |
| en_tr_mixed | 0 | en | 4450.000 | 10/10 | 839.846 | 857.308 | 858.035 | 858.035 |
| en_tr_mixed | 1 | tr | 13940.000 | 10/10 | 817.116 | 817.786 | 832.964 | 832.964 |

## Calls

30 calls, 0 failed. Largest client send lateness 26.039 ms (a frame sent later than its deadline delays the end of speech the server sees by as much).
