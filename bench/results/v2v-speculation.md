# V2V over the websocket with speculative LLM start

- commands: `brisvox-bench v2v --sim --fixtures en_two_utterances,en_tr_mixed --runs 8 --barge-in-runs 0 --memory-calls 0 --engine-turn smart|fixed:700|fixed:450 [--speculation | --speculation-final-only] [--stt-interims lagging|converged]` (one file pair per row in `v2v-speculation/`)
- release build, windows x86_64, one process, the engine over mock providers with fixed latencies (STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms), real time: **Windows numbers are trends** (timer overshoot of about 8 ms per mock sleep, a shared machine)
- V2V in ms, client side (first non-silent bot audio after the ground-truth end of speech); 16 calls x 2 turns = 32 turns per row, so p90/p99 are coarse. delta is the p50 against the same policy with speculation off. hit rate = committed over decided speculations; head start = mean time a committed speculation was ahead of the turn end; extra LLM requests per turn = speculative runs that were not committed (each one a full prompt sent for nothing) per answered turn: 0.00 is no waste, 1.00 means every turn paid a second request; wasted tokens = output tokens of discarded runs as counted by the engine (the mock cancels before its first token, so 0); floor = the configured latency floor without speculation.

| policy | speculation | turns | V2V p50 | p90 | p99 | delta p50 | hit rate | head start ms | extra LLM requests per turn | wasted output tokens | floor ms | failed calls |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| adaptive (Smart Turn: complete) | off | 32/32 | 1005 | 1026 | 1047 | - | - | - | - | - | 950 | 0 |
| adaptive (Smart Turn: complete) | on, lagging interims | 32/32 | 1003 | 1020 | 1036 | -3 | 0% | - | 1.00 | 0 | 950 | 0 |
| adaptive (Smart Turn: complete) | on, converged interims | 32/32 | 849 | 875 | 887 | -156 | 100% | 155 | 0.00 | 0 | 950 | 0 |
| fixed 700 ms | off | 32/32 | 1263 | 1282 | 1297 | - | - | - | - | - | 1200 | 0 |
| fixed 700 ms | on, lagging interims | 32/32 | 1011 | 1038 | 1055 | -252 | 50% | 253 | 1.00 | 0 | 1200 | 0 |
| fixed 700 ms | on, converged interims | 32/32 | 903 | 921 | 938 | -361 | 100% | 410 | 0.00 | 0 | 1200 | 0 |
| fixed 700 ms | on, final only | 32/32 | 1009 | 1032 | 1061 | -255 | 100% | 261 | 0.00 | 0 | 1200 | 0 |
| fixed 450 ms | off | 32/32 | 1017 | 1042 | 1046 | - | - | - | - | - | 950 | 0 |
| fixed 450 ms | on, lagging interims | 32/32 | 1006 | 1020 | 1030 | -11 | 44% | 16 | 1.00 | 0 | 950 | 0 |
| fixed 450 ms | on, converged interims | 32/32 | 845 | 869 | 882 | -172 | 100% | 168 | 0.00 | 0 | 950 | 0 |
| fixed 450 ms | on, final only | 32/32 | 1002 | 1028 | 1043 | -14 | 100% | 15 | 0.00 | 0 | 950 | 0 |
