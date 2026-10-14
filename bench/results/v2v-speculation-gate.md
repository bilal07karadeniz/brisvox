# V2V over the websocket with the stable-partial gate

- commands: `brisvox-bench v2v --sim --fixtures en_two_utterances,en_tr_mixed --runs 8 --barge-in-runs 0 --memory-calls 0 --engine-turn smart|fixed:700 [--speculation --stt-interims lagging|converged [--speculation-gate-ms N]]` (one file pair per row in `v2v-speculation-gate/`, all rows run back to back in one session)
- release build, windows x86_64, one process, the engine over mock providers with fixed latencies (STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms), real time: **Windows numbers are trends** (timer overshoot of about 8 ms per mock sleep, a shared machine)
- V2V in ms, client side; 16 calls x 2 turns = 32 turns per row, so p90/p99 are coarse. delta is the p50 against the same policy with speculation off (this session). gate = `SpeculationConfig::partial_stable_for`: the partial of a VAD stop is offered only after that long with the caller silent and the text unchanged. head start = mean time a committed speculation was ahead of the turn end; extra LLM requests per turn = speculative runs that were not committed (`started - hits`) per answered turn.
- This bench plays continuous speech only (what the gate costs). The resumed-speech scenario (what it saves) is in `speculation-fixtures.md`: it needs a caller who pauses and goes on, which the websocket client does not play.

| policy | speculation | gate | turns | V2V p50 | p90 | p99 | delta p50 | hit rate | head start ms | extra LLM requests per turn | failed calls |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| adaptive (Smart Turn: complete) | off | - | 32/32 | 1017 | 1039 | 1047 | - | - | - | - | 0 |
| adaptive (Smart Turn: complete) | on, converged interims | none | 32/32 | 852 | 875 | 900 | -165 | 100% | 157 | 0.00 | 0 |
| adaptive (Smart Turn: complete) | on, lagging interims | none | 32/32 | 1011 | 1037 | 1051 | -6 | 0% | - | 1.00 | 0 |
| adaptive (Smart Turn: complete) | on, converged interims | 100 ms | 32/32 | 964 | 992 | 1006 | -53 | 100% | 47 | 0.00 | 0 |
| adaptive (Smart Turn: complete) | on, converged interims | 200 ms | 32/32 | 1014 | 1035 | 1050 | -3 | - | - | 0.00 | 0 |
| fixed 700 ms | off | - | 32/32 | 1274 | 1292 | 1303 | - | - | - | - | 0 |
| fixed 700 ms | on, lagging interims | none | 32/32 | 1012 | 1034 | 1043 | -262 | 50% | 256 | 1.00 | 0 |
| fixed 700 ms | on, lagging interims | 200 ms | 32/32 | 1014 | 1038 | 1048 | -260 | 100% | 257 | 0.00 | 0 |
| fixed 700 ms | on, converged interims | 100 ms | 32/32 | 968 | 989 | 1010 | -306 | 100% | 303 | 0.00 | 0 |
