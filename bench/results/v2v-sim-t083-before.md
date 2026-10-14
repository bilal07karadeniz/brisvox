# Voice-to-voice latency (sim)

- command: `brisvox-bench v2v --sim --runs 10 --out-md bench/results/v2v-sim-t083-before.md --out-json bench/results/v2v-sim-t083-before.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the engine (production pipeline, `brisvox_engine::run_call`) over `brisvox_sim` mock providers behind the real websocket transport, in process; one client, one call at a time, real time (SystemClock)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:62169/...`
- 10 runs of each of en_two_utterances, tr_two_utterances, en_tr_mixed (utterances spaced at least 4000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- engine: the production pipeline (`brisvox_engine::run_call`) on a 2-thread inference pool, over `brisvox_sim` mock providers with fixed latencies: STT final 150 ms after `Finalize` (interims every 100 ms), LLM first token 350 ms (then a token every 30 ms), TTS first byte 150 ms (a 40 ms chunk every 20 ms). Energy VAD (RMS >= 500, 40 ms to start, stop after 300 ms of silence); end of turn: the adaptive policy with a Smart Turn model that answers "complete" at every pause (feature extraction real, on the pool).
- floor: end of turn 450 ms after the end of speech (VAD stop + STT final, the finals gate), latency floor 950 ms (end of turn + LLM first token, with which the answer's first clause leaves + TTS first byte). The rest of the measurement is the engine's own time (VAD windows, pool hops, stage queues, the 20 ms frame grid, the output pacer's phase) plus the websocket transport

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.

## Summary

| scope | turns | answered | talked over | bot speaking | no response | p50 ms | p90 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 60 | 60 | 0 | 0 | 0 | 1009.412 | 1026.075 | 1028.024 | 1042.808 | 1042.808 |
| fixture en_two_utterances | 20 | 20 | 0 | 0 | 0 | 1011.549 | 1023.930 | 1026.753 | 1028.024 | 1028.024 |
| fixture tr_two_utterances | 20 | 20 | 0 | 0 | 0 | 1009.316 | 1024.010 | 1026.090 | 1030.539 | 1030.539 |
| fixture en_tr_mixed | 20 | 20 | 0 | 0 | 0 | 1003.044 | 1026.075 | 1030.704 | 1042.808 | 1042.808 |
| language en | 30 | 30 | 0 | 0 | 0 | 1013.205 | 1026.753 | 1030.704 | 1042.808 | 1042.808 |
| language tr | 30 | 30 | 0 | 0 | 0 | 1002.989 | 1018.834 | 1026.090 | 1030.539 | 1030.539 |

## Per turn

| fixture | turn | lang | EOS at ms | answered/turns | p50 ms | p90 ms | p99 ms | max ms |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| en_two_utterances | 0 | en | 6670.000 | 10/10 | 1017.331 | 1026.753 | 1028.024 | 1028.024 |
| en_two_utterances | 1 | en | 13430.000 | 10/10 | 1002.893 | 1013.205 | 1015.621 | 1015.621 |
| tr_two_utterances | 0 | tr | 6300.000 | 10/10 | 1011.947 | 1026.090 | 1030.539 | 1030.539 |
| tr_two_utterances | 1 | tr | 17090.000 | 10/10 | 1002.177 | 1011.165 | 1014.662 | 1014.662 |
| en_tr_mixed | 0 | en | 4450.000 | 10/10 | 1015.590 | 1030.704 | 1042.808 | 1042.808 |
| en_tr_mixed | 1 | tr | 13940.000 | 10/10 | 991.417 | 1003.044 | 1004.433 | 1004.433 |

## Engine stages

The engine's own turn timeline (`TimelineRecorder`, stamped where the work happens) over 120 turns, 30 of them interrupted by a barge-in. `provider` is mock provider time inside the span; the engine's share is the rest. The budget applies at p95 (docs/LATENCY.md).

| stage | from -> to | n | p50 ms | p95 ms | p99 ms | max ms | provider ms | budget p95 ms | |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| vad_stop_to_finalize | vad_stop -> stt_finalize | 60 | 0.007 | 0.016 | 0.021 | 0.021 | 0.000 | - |  |
| turn_end_to_llm_request | turn_end -> llm_request | 60 | 0.051 | 0.074 | 0.079 | 0.079 | 0.000 | 3.000 | met |
| llm_request_to_first_token | llm_request -> llm_first_token | 60 | 357.163 | 359.442 | 364.597 | 364.597 | 350.000 | - |  |
| first_token_to_tts_text | llm_first_token -> tts_request | 60 | 0.025 | 0.031 | 0.113 | 0.113 | 0.000 | - |  |
| tts_request_to_first_byte | tts_request -> tts_first_byte | 60 | 155.400 | 156.539 | 156.792 | 156.792 | 150.000 | - |  |
| tts_audio_to_transport | tts_first_byte -> first_audio_out | 60 | 0.037 | 0.055 | 0.059 | 0.059 | 0.000 | 2.000 | met |
| interrupt_to_clear | interrupted -> audio_out_stopped | 30 | 0.015 | 0.027 | 0.045 | 0.045 | 0.000 | 1.000 | met |
| turn_end_to_first_audio | turn_end -> first_audio_out | 60 | 512.371 | 515.395 | 521.511 | 521.511 | 500.000 | - |  |

**Memory** (resident set of the whole process: engine, transport and harness): 10.8 MiB with the server idle and cold, 30.8 MiB idle and warm after the calls above (+19.9 MiB of one-time warm-up), peak 38.3 MiB with 8 memory calls alive at once. **RSS per call 0.9 MiB** (peak over warm idle, per call).

## Calls

68 calls (30 barge-in calls, 8 memory calls), 0 failed. Largest client send lateness 20.897 ms (a frame sent later than its deadline delays the end of speech the server sees by as much).
