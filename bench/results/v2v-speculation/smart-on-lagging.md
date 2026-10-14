# Voice-to-voice latency (sim)

- command: `brisvox-bench v2v --sim --fixtures en_two_utterances,en_tr_mixed --runs 8 --barge-in-runs 0 --memory-calls 0 --engine-turn smart --speculation --stt-interims lagging --out-md bench/results/v2v-speculation/smart-on-lagging.md --out-json bench/results/v2v-speculation/smart-on-lagging.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the engine (production pipeline, `brisvox_engine::run_call`) over `brisvox_sim` mock providers behind the real websocket transport, in process; one client, one call at a time, real time (SystemClock)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:49462/...`
- 8 runs of each of en_two_utterances, en_tr_mixed (utterances spaced at least 4000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- engine: the production pipeline (`brisvox_engine::run_call`) on a 2-thread inference pool, over `brisvox_sim` mock providers with fixed latencies: STT final 150 ms after `Finalize` (interims every 100 ms), LLM first token 350 ms (then a token every 30 ms), TTS first byte 150 ms (a 40 ms chunk every 20 ms). Energy VAD (RMS >= 500, 40 ms to start, stop after 300 ms of silence); end of turn: the adaptive policy with a Smart Turn model that answers "complete" at every pause (feature extraction real, on the pool).
- floor: end of turn 450 ms after the end of speech (VAD stop + STT final, the finals gate), latency floor 950 ms (end of turn + LLM first token, with which the answer's first clause leaves + TTS first byte). The rest of the measurement is the engine's own time (VAD windows, pool hops, stage queues, the 20 ms frame grid, the output pacer's phase) plus the websocket transport
- speculation: **on**, STT interims lagging. A committed speculation starts the LLM before the end of the turn, so a measurement can land below the floor above by up to the head start (see Speculation), and the engine stage `turn_end_to_llm_request` has no sample for those turns.

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.

## Summary

| scope | turns | answered | talked over | bot speaking | no response | p50 ms | p90 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 32 | 32 | 0 | 0 | 0 | 1002.643 | 1019.602 | 1025.820 | 1036.263 | 1036.263 |
| fixture en_two_utterances | 16 | 16 | 0 | 0 | 0 | 1007.424 | 1024.120 | 1036.263 | 1036.263 | 1036.263 |
| fixture en_tr_mixed | 16 | 16 | 0 | 0 | 0 | 1000.659 | 1019.602 | 1025.820 | 1025.820 | 1025.820 |
| language en | 24 | 24 | 0 | 0 | 0 | 1009.935 | 1024.120 | 1025.820 | 1036.263 | 1036.263 |
| language tr | 8 | 8 | 0 | 0 | 0 | 984.325 | 1003.204 | 1003.204 | 1003.204 | 1003.204 |

## Per turn

| fixture | turn | lang | EOS at ms | answered/turns | p50 ms | p90 ms | p99 ms | max ms |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| en_two_utterances | 0 | en | 6670.000 | 8/8 | 1010.178 | 1036.263 | 1036.263 | 1036.263 |
| en_two_utterances | 1 | en | 13430.000 | 8/8 | 997.033 | 1024.120 | 1024.120 | 1024.120 |
| en_tr_mixed | 0 | en | 4450.000 | 8/8 | 1009.935 | 1025.820 | 1025.820 | 1025.820 |
| en_tr_mixed | 1 | tr | 13940.000 | 8/8 | 984.325 | 1003.204 | 1003.204 | 1003.204 |

## Engine stages

The engine's own turn timeline (`TimelineRecorder`, stamped where the work happens) over 32 turns, 0 of them interrupted by a barge-in. `provider` is mock provider time inside the span; the engine's share is the rest. The budget applies at p95 (docs/LATENCY.md).

| stage | from -> to | n | p50 ms | p95 ms | p99 ms | max ms | provider ms | budget p95 ms | |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| vad_stop_to_finalize | vad_stop -> stt_finalize | 32 | 0.010 | 0.027 | 0.047 | 0.047 | 0.000 | - |  |
| turn_end_to_llm_request | turn_end -> llm_request | 32 | 0.080 | 0.150 | 0.162 | 0.162 | 0.000 | 3.000 | met |
| llm_request_to_first_token | llm_request -> llm_first_token | 32 | 357.623 | 365.781 | 365.919 | 365.919 | 350.000 | - |  |
| first_token_to_tts_text | llm_first_token -> tts_request | 32 | 0.034 | 0.085 | 0.195 | 0.195 | 0.000 | - |  |
| tts_request_to_first_byte | tts_request -> tts_first_byte | 32 | 157.570 | 165.020 | 166.124 | 166.124 | 150.000 | - |  |
| tts_audio_to_transport | tts_first_byte -> first_audio_out | 32 | 0.044 | 0.124 | 0.157 | 0.157 | 0.000 | 2.000 | met |
| interrupt_to_clear | interrupted -> audio_out_stopped | 0 | - | - | - | - | 0.000 | 1.000 |  |
| turn_end_to_first_audio | turn_end -> first_audio_out | 32 | 515.846 | 524.898 | 530.106 | 530.106 | 500.000 | - |  |

## Speculation

Speculative generation was on. Over 16 latency calls: 32 speculative LLM runs started, 0 committed (hit rate 0.0%, mean head start -), 32 missed, 0 superseded, 0 withdrawn, 0 abandoned, 0 other; 0 failed. Wasted: 0 output tokens (estimated from the streamed bytes), 0 input tokens as reported by the provider (a discarded run reports none: a provider bills its prompt anyway).

Memory: not measured (no memory calls, or not reported by this platform).

## Calls

16 calls (0 barge-in calls, 0 memory calls), 0 failed. Largest client send lateness 23.832 ms (a frame sent later than its deadline delays the end of speech the server sees by as much).
