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
- **Providers (identical for both systems, one agent file: `bench/pipecat_baseline/agent_live_en.toml`):**
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
  measured. Percentiles of 30 turns are still a small sample: p90 and p95 are the 2nd and 3rd
  largest values, not tail estimates.
- Keys never appear in any of these files; they were passed through the environment only.

- System under test: Pipecat 1.12.0 bot (`bench/pipecat_baseline/bot.py`, Python 3.13), the same agent file, Smart Turn v3.2 and Silero v6.2.3, LLM through Pipecat's `OpenAILLMService` (Chat Completions) with `base_url` OpenRouter, happy-eyeballs websocket dialing on.
- Blocks (in order): en-p1 (3 calls), en-p2 (6 calls), en-p3 (6 calls)

## Voice-to-voice (V2V)

V2V = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (`docs/LATENCY.md`). Nearest-rank percentiles over answered turns.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| V2V, first block (as specified) | 6 | 2125 | 2173 | 2588 | 2588 | 2588 | 2264 |
| V2V, all blocks pooled | 30 | 1743 | 2029 | 2370 | 2544 | 2588 | 2079 |
| V2V, pooled, turn 0 | 15 | 1911 | 2073 | 2383 | 2544 | 2544 | 2139 |
| V2V, pooled, turn 1 | 15 | 1743 | 1955 | 2370 | 2588 | 2588 | 2019 |

Outcomes (pooled): {"answered": 30}.

## Provider latencies as this system's own timers saw them

From Pipecat's own service timers (`enable_metrics`, logged by the bot as `PCMETRIC`, `--provider-metrics`). STT TTFB is counted from the VAD stop (100 ms of silence after the end of speech, 32 ms windows) to the final transcript; LLM TTFB from the request to the first token; TTS TTFB from the first text to the first audio. A turn the user resumed mid-sentence can show more than one LLM request; the last one is the answered one (`llm_requests` in the JSON).

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| STT final after the VAD stop (Pipecat STT TTFB) | 30 | 275 | 320 | 360 | 371 | 385 | 327 |
| STT final after end of speech (TTFB + 100 ms VAD stop) | 30 | 375 | 420 | 460 | 471 | 485 | 427 |
| LLM first token after the request (Pipecat LLM TTFB) | 30 | 914 | 1064 | 1328 | 1454 | 1884 | 1127 |
| TTS first audio after the text (Pipecat TTS TTFB) | 30 | 193 | 211 | 229 | 244 | 250 | 214 |

## What is left for the framework

Per turn: V2V minus the three provider times above (STT from the end of speech, the LLM to its first token, TTS to its first byte). It is not the same residual as brisvox's: Pipecat does not time the wait for the first sentence (its aggregator holds text until a sentence ends) nor the Smart Turn run, so those, the soxr resampler's 100 ms bursts, the serializer and the pacer are in this number, together with the transport and the end-of-speech definition.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| V2V minus (100 ms VAD stop + STT TTFB + LLM TTFB + TTS TTFB) | 30 | 120 | 308 | 477 | 543 | 594 | 311 |

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
| en-p1 | 0 | 0 | answered | 2173 | 304 | 1218 | 221 | 1 | 329 |
| en-p1 | 0 | 1 | answered | 2588 | 275 | 1884 | 210 | 1 | 120 |
| en-p1 | 1 | 0 | answered | 2246 | 371 | 974 | 206 | 2 | 594 |
| en-p1 | 1 | 1 | answered | 2125 | 360 | 1011 | 199 | 2 | 454 |
| en-p1 | 2 | 0 | answered | 2327 | 338 | 1237 | 210 | 1 | 442 |
| en-p1 | 2 | 1 | answered | 2127 | 302 | 1278 | 208 | 1 | 239 |
| en-p2 | 0 | 0 | answered | 2064 | 328 | 1097 | 229 | 1 | 311 |
| en-p2 | 0 | 1 | answered | 2370 | 385 | 1207 | 202 | 2 | 477 |
| en-p2 | 1 | 0 | answered | 2544 | 368 | 1328 | 204 | 2 | 543 |
| en-p2 | 1 | 1 | answered | 2029 | 346 | 1104 | 210 | 1 | 269 |
| en-p2 | 2 | 0 | answered | 1980 | 318 | 1035 | 216 | 1 | 310 |
| en-p2 | 2 | 1 | answered | 1764 | 284 | 999 | 218 | 1 | 163 |
| en-p2 | 3 | 0 | answered | 1937 | 318 | 1037 | 215 | 1 | 267 |
| en-p2 | 3 | 1 | answered | 1869 | 335 | 983 | 213 | 1 | 238 |
| en-p2 | 4 | 0 | answered | 2005 | 321 | 1019 | 225 | 1 | 341 |
| en-p2 | 4 | 1 | answered | 1743 | 320 | 914 | 242 | 1 | 167 |
| en-p2 | 5 | 0 | answered | 2058 | 305 | 1145 | 211 | 1 | 297 |
| en-p2 | 5 | 1 | answered | 1955 | 339 | 1138 | 250 | 1 | 127 |
| en-p3 | 0 | 0 | answered | 2313 | 315 | 1357 | 202 | 1 | 339 |
| en-p3 | 0 | 1 | answered | 2176 | 360 | 1039 | 193 | 2 | 484 |
| en-p3 | 1 | 0 | answered | 2073 | 300 | 1064 | 244 | 1 | 364 |
| en-p3 | 1 | 1 | answered | 1843 | 342 | 994 | 214 | 1 | 193 |
| en-p3 | 2 | 0 | answered | 2383 | 309 | 1454 | 212 | 1 | 308 |
| en-p3 | 2 | 1 | answered | 1826 | 308 | 1007 | 212 | 1 | 199 |
| en-p3 | 3 | 0 | answered | 1911 | 320 | 967 | 199 | 1 | 325 |
| en-p3 | 3 | 1 | answered | 1900 | 348 | 987 | 203 | 1 | 261 |
| en-p3 | 4 | 0 | answered | 2131 | 324 | 1157 | 202 | 1 | 348 |
| en-p3 | 4 | 1 | answered | 2013 | 354 | 1089 | 204 | 1 | 266 |
| en-p3 | 5 | 0 | answered | 1935 | 313 | 963 | 211 | 1 | 348 |
| en-p3 | 5 | 1 | answered | 1952 | 284 | 1118 | 229 | 1 | 221 |

## Raw bench reports (headers)

### en-p1

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:18765 --fixtures en_two_utterances --runs 3 --min-gap-ms 9000  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, one call at a time, real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:18765`
- 3 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### en-p2

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:18765 --fixtures en_two_utterances --runs 6 --min-gap-ms 9000  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, one call at a time, real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:18765`
- 6 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### en-p3

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:18765 --fixtures en_two_utterances --runs 6 --min-gap-ms 9000  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, one call at a time, real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:18765`
- 6 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


