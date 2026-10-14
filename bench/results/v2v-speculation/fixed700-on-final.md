# Voice-to-voice latency (sim)

- command: `brisvox-bench v2v --sim --fixtures en_two_utterances,en_tr_mixed --runs 8 --barge-in-runs 0 --memory-calls 0 --engine-turn fixed:700 --speculation-final-only --out-md bench/results/v2v-speculation/fixed700-on-final.md --out-json bench/results/v2v-speculation/fixed700-on-final.json`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- sim: the engine (production pipeline, `brisvox_engine::run_call`) over `brisvox_sim` mock providers behind the real websocket transport, in process; one client, one call at a time, real time (SystemClock)
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:52688/...`
- 8 runs of each of en_two_utterances, en_tr_mixed (utterances spaced at least 4000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- engine: the production pipeline (`brisvox_engine::run_call`) on a 2-thread inference pool, over `brisvox_sim` mock providers with fixed latencies: STT final 150 ms after `Finalize` (interims every 100 ms), LLM first token 350 ms (then a token every 30 ms), TTS first byte 150 ms (a 40 ms chunk every 20 ms). Energy VAD (RMS >= 500, 40 ms to start, stop after 300 ms of silence); end of turn: a fixed 700 ms of silence after the backdated VAD stop.
- floor: end of turn 700 ms after the end of speech (VAD stop + STT final, the finals gate, or the fixed silence if longer), latency floor 1200 ms (end of turn + LLM first token, with which the answer's first clause leaves + TTS first byte). The rest of the measurement is the engine's own time (VAD windows, pool hops, stage queues, the 20 ms frame grid, the output pacer's phase) plus the websocket transport
- speculation: **on**, STT interims lagging. A committed speculation starts the LLM before the end of the turn, so a measurement can land below the floor above by up to the head start (see Speculation), and the engine stage `turn_end_to_llm_request` has no sample for those turns.

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.

## Summary

| scope | turns | answered | talked over | bot speaking | no response | p50 ms | p90 ms | p95 ms | p99 ms | max ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 32 | 32 | 0 | 0 | 0 | 1008.814 | 1032.398 | 1045.332 | 1060.703 | 1060.703 |
| fixture en_two_utterances | 16 | 16 | 0 | 0 | 0 | 1008.837 | 1045.332 | 1060.703 | 1060.703 | 1060.703 |
| fixture en_tr_mixed | 16 | 16 | 0 | 0 | 0 | 1007.296 | 1025.947 | 1032.398 | 1032.398 | 1032.398 |
| language en | 24 | 24 | 0 | 0 | 0 | 1015.440 | 1037.053 | 1045.332 | 1060.703 | 1060.703 |
| language tr | 8 | 8 | 0 | 0 | 0 | 995.061 | 1010.826 | 1010.826 | 1010.826 | 1010.826 |

## Per turn

| fixture | turn | lang | EOS at ms | answered/turns | p50 ms | p90 ms | p99 ms | max ms |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| en_two_utterances | 0 | en | 6670.000 | 8/8 | 1018.491 | 1060.703 | 1060.703 | 1060.703 |
| en_two_utterances | 1 | en | 13430.000 | 8/8 | 1002.157 | 1018.838 | 1018.838 | 1018.838 |
| en_tr_mixed | 0 | en | 4450.000 | 8/8 | 1015.780 | 1032.398 | 1032.398 | 1032.398 |
| en_tr_mixed | 1 | tr | 13940.000 | 8/8 | 995.061 | 1010.826 | 1010.826 | 1010.826 |

## Engine stages

The engine's own turn timeline (`TimelineRecorder`, stamped where the work happens) over 32 turns, 0 of them interrupted by a barge-in. `provider` is mock provider time inside the span; the engine's share is the rest. The budget applies at p95 (docs/LATENCY.md).

| stage | from -> to | n | p50 ms | p95 ms | p99 ms | max ms | provider ms | budget p95 ms | |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| vad_stop_to_finalize | vad_stop -> stt_finalize | 32 | 0.008 | 0.022 | 0.026 | 0.026 | 0.000 | - |  |
| turn_end_to_llm_request | turn_end -> llm_request | 0 | - | - | - | - | 0.000 | 3.000 |  |
| llm_request_to_first_token | llm_request -> llm_first_token | 32 | 362.827 | 366.319 | 366.522 | 366.522 | 350.000 | - |  |
| first_token_to_tts_text | llm_first_token -> tts_request | 32 | 0.042 | 0.063 | 0.168 | 0.168 | 0.000 | - |  |
| tts_request_to_first_byte | tts_request -> tts_first_byte | 32 | 158.407 | 166.259 | 166.974 | 166.974 | 150.000 | - |  |
| tts_audio_to_transport | tts_first_byte -> first_audio_out | 32 | 0.047 | 0.085 | 0.087 | 0.087 | 0.000 | 2.000 | met |
| interrupt_to_clear | interrupted -> audio_out_stopped | 0 | - | - | - | - | 0.000 | 1.000 |  |
| turn_end_to_first_audio | turn_end -> first_audio_out | 32 | 253.740 | 275.180 | 282.285 | 282.285 | 500.000 | - |  |

## Speculation

Speculative generation was on. Over 16 latency calls: 32 speculative LLM runs started, 32 committed (hit rate 100.0%, mean head start 261 ms), 0 missed, 0 superseded, 0 withdrawn, 0 abandoned, 0 other; 0 failed. Wasted: 0 output tokens (estimated from the streamed bytes), 0 input tokens as reported by the provider (a discarded run reports none: a provider bills its prompt anyway).

Memory: not measured (no memory calls, or not reported by this platform).

## Calls

16 calls (0 barge-in calls, 0 memory calls), 0 failed. Largest client send lateness 20.903 ms (a frame sent later than its deadline delays the end of speech the server sees by as much).
