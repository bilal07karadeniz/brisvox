# Live V2V, pipecat, Windows, real providers (2026-10-13)

## Environment

- **Windows, real providers over the public internet.** Machine: Windows-10-10.0.26300-SP0, 32 logical cores,
  Python 3.10.11. Both systems and the benchmark client ran on this one machine, one system at a
  time, sequentially (never concurrently), against loopback. The machine was shared with other
  work (builds, a Docker/WSL benchmark in another session), so absolute numbers carry noise; the
  comparison interleaves the two systems block by block to spread that noise.
- **Not a Linux run.** The Windows timer ticks every ~15.6 ms; the largest client send lateness of
  each block is in its raw report below (a late frame delays the end of speech the server sees by as
  much). `docs/LATENCY.md` asks for Linux numbers for target claims.
- **Providers (identical for both systems, one agent file: `bench/pipecat_baseline/agent_live_tr.toml`):**
  STT Deepgram `nova-3` (live websocket, endpointing 300 ms, `smart_format`); LLM
  `openai/gpt-6-luna` through **OpenRouter** (`https://openrouter.ai/api/v1`, Chat Completions,
  streaming, `max_tokens` 120, `reasoning_effort` `none`: the model thinks by default, which adds
  about a second to the first token); TTS Cartesia `sonic-3.6` (websocket, 24 kHz, the voice of
  `CARTESIA_VOICE_ID`). Turns: adaptive, Smart Turn v3.2 (the same ONNX file on both sides),
  Silero VAD v6.2.3, one-sentence prompt.
- **IPv6 note.** This machine's IPv6 route silently drops packets while `api.cartesia.ai`
  and `openrouter.ai` publish AAAA records. brisvox races address families (RFC 8305) for its
  provider websockets and relies on reqwest for HTTP. Pipecat 1.12.0 as shipped connects its
  provider websockets one address at a time: the first Cartesia handshake of a call timed out
  after 10 s ("timed out during opening handshake") and that call's first turn got no audio. The
  baseline bot therefore enables asyncio's happy-eyeballs racing for Pipecat's websocket services
  (`bot.py` `enable_happy_eyeballs`, `--no-happy-eyeballs` restores Pipecat as shipped). Without
  that fix Pipecat fails on this host; with it the connection setup is no longer part of the
  difference.
- **Sample size.** 2 turns per call. The first block (3 calls, 6 turns) is the run as
  specified; the later blocks were added because the LLM provider's variance (OpenRouter first
  token 0.6 s to 3 s for the same request) is larger than the framework differences being
  measured. Percentiles of 6 turns are still a small sample: p90 and p95 are the 2nd and 3rd
  largest values, not tail estimates.
- Keys never appear in any of these files; they were passed through the environment only.

- System under test: Pipecat 1.12.0 bot (`bench/pipecat_baseline/bot.py`, Python 3.13), the same agent file, Smart Turn v3.2 and Silero v6.2.3, LLM through Pipecat's `OpenAILLMService` (Chat Completions) with `base_url` OpenRouter, happy-eyeballs websocket dialing on.
- Blocks (in order): tr-p1 (3 calls)

## Voice-to-voice (V2V)

V2V = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (`docs/LATENCY.md`). Nearest-rank percentiles over answered turns.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| V2V, first block (as specified) | 6 | 2252 | 2519 | 3477 | 3477 | 3477 | 2732 |
| V2V, all blocks pooled | 6 | 2252 | 2519 | 3477 | 3477 | 3477 | 2732 |
| V2V, pooled, turn 0 | 3 | 2501 | 2836 | 3477 | 3477 | 3477 | 2938 |
| V2V, pooled, turn 1 | 3 | 2252 | 2519 | 2805 | 2805 | 2805 | 2525 |

Outcomes (pooled): {"answered": 6}.

## Provider latencies as this system's own timers saw them

From Pipecat's own service timers (`enable_metrics`, logged by the bot as `PCMETRIC`, `--provider-metrics`). STT TTFB is counted from the VAD stop (100 ms of silence after the end of speech, 32 ms windows) to the final transcript; LLM TTFB from the request to the first token; TTS TTFB from the first text to the first audio. A turn the user resumed mid-sentence can show more than one LLM request; the last one is the answered one (`llm_requests` in the JSON).

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| STT final after the VAD stop (Pipecat STT TTFB) | 6 | 347 | 391 | 407 | 407 | 407 | 389 |
| STT final after end of speech (TTFB + 100 ms VAD stop) | 6 | 447 | 491 | 507 | 507 | 507 | 489 |
| LLM first token after the request (Pipecat LLM TTFB) | 5 | 1250 | 1460 | 2013 | 2013 | 2013 | 1553 |
| TTS first audio after the text (Pipecat TTS TTFB) | 5 | 194 | 200 | 209 | 209 | 209 | 200 |

## What is left for the framework

Per turn: V2V minus the three provider times above (STT from the end of speech, the LLM to its first token, TTS to its first byte). It is not the same residual as brisvox's: Pipecat does not time the wait for the first sentence (its aggregator holds text until a sentence ends) nor the Smart Turn run, so those, the soxr resampler's 100 ms bursts, the serializer and the pacer are in this number, together with the transport and the end-of-speech definition.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| V2V minus (100 ms VAD stop + STT TTFB + LLM TTFB + TTS TTFB) | 5 | 200 | 561 | 758 | 758 | 758 | 535 |

## Failures

- failed calls: 0
- no_response turns: 0
- talked_over turns: 0
- bot_speaking turns: 0
- turns without a provider timeline: 0
- ERROR log lines: 0
- WARN log lines: 0

## Every turn

| block | run | turn | outcome | v2v_ms | stt_ttfb_ms | llm_ttfb_ms | tts_ttfb_ms | llm_requests | residual_ms |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|
| tr-p1 | 0 | 0 | answered | 3477 | 407 | 2013 | 200 | 2 | 758 |
| tr-p1 | 0 | 1 | answered | 2805 | 384 | 1646 | 197 | 2 | 479 |
| tr-p1 | 1 | 0 | answered | 2836 | 405 | 1460 | 194 | 2 | 677 |
| tr-p1 | 1 | 1 | answered | 2519 | 402 | - | - | 0 | - |
| tr-p1 | 2 | 0 | answered | 2501 | 391 | 1250 | 200 | 2 | 561 |
| tr-p1 | 2 | 1 | answered | 2252 | 347 | 1396 | 209 | 1 | 200 |

## Raw bench reports (headers)

### tr-p1

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:18765 --fixtures tr_two_utterances --runs 3 --min-gap-ms 9000  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, one call at a time, real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:18765`
- 3 runs of each of tr_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


