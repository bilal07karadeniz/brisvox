# Live V2V, brisvox, Windows, real providers (2026-10-13)

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

- System under test: `brisvox-server` (release, features `provider-deepgram`, `provider-openai-compatible`, `provider-cartesia`, `smart-turn-ort`, `silero-tract`), adaptive turns with Smart Turn v3.2, agent file `bench/pipecat_baseline/agent_live_en.toml`.
- Blocks (in order): en-b1 (3 calls), en-b2 (6 calls), en-b3 (6 calls)

## Voice-to-voice (V2V)

V2V = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (`docs/LATENCY.md`). Nearest-rank percentiles over answered turns.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| V2V, first block (as specified) | 6 | 1446 | 1580 | 2102 | 2102 | 2102 | 1681 |
| V2V, all blocks pooled | 30 | 1352 | 1686 | 2203 | 2927 | 4287 | 1831 |
| V2V, pooled, turn 0 | 15 | 1579 | 1763 | 2089 | 4287 | 4287 | 1926 |
| V2V, pooled, turn 1 | 15 | 1352 | 1596 | 2211 | 2927 | 2927 | 1736 |

Outcomes (pooled): {"answered": 30}.

## Provider latencies as this system's own timers saw them

From the server's `turn finished` timeline lines (`brisvox::timeline`, `docs/LATENCY.md`, "Turn timeline"), one per turn. `stt_final_delay` is counted from the VAD's backdated end of speech to the last Deepgram final; `llm_ttft` from the request written to the connection to the first text delta (OpenRouter's time to first byte included); `tts_ttfb` from the first text sent to Cartesia to its first audio byte.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| STT final after end of speech (`stt_final_delay`) | 30 | 242 | 285 | 319 | 320 | 322 | 282 |
| LLM first token after the request (`llm_ttft`) | 30 | 633 | 741 | 1423 | 2229 | 3178 | 954 |
| LLM first speakable chunk after the request (`llm_first_sentence`) | 30 | 694 | 979 | 1431 | 2229 | 3546 | 1120 |
| TTS first audio after the text (`tts_ttfb`) | 30 | 196 | 212 | 231 | 250 | 261 | 215 |

## What is left for the framework

Per turn: V2V minus the provider times above. The first row uses the LLM's first token, the same definition as the Pipecat file (there the wait for the first sentence is not timed, so it stays in the residual); the second row uses the first speakable chunk (the wait for a clause is provider streaming time). What remains is everything the engine and the transport add: turn detection after the final, hand-offs, the output frame grid, the websocket, and the difference between the VAD's end of speech and the fixture's ground truth (~150 ms, in both systems). The server's own response latency (end of speech to the first audio frame handed to the transport) splits the second row into the part inside the server and the client-side part.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| V2V minus (STT final + LLM first token + TTS first byte): the definition used for Pipecat below | 30 | 187 | 400 | 494 | 529 | 608 | 380 |
| V2V minus (STT final + LLM first chunk + TTS first byte) | 30 | 56 | 219 | 259 | 265 | 282 | 214 |
|   of which inside the server (server response latency minus the three providers) | 30 | 0 | 0 | 0 | 0 | 0 | 0 |
|   of which client side (V2V minus the server's response latency: transport, EOS definition, frame grid) | 30 | 56 | 219 | 259 | 265 | 282 | 214 |

## Failures

- failed calls: 0
- no_response turns: 0
- talked_over turns: 0
- bot_speaking turns: 0
- turns without a provider timeline: 0
- ERROR log lines: 0
- WARN log lines: 0

## Every turn

| block | run | turn | outcome | v2v_ms | stt_final_ms | llm_ttft_ms | llm_first_sentence_ms | tts_ttfb_ms | response_latency_ms | residual_strict_ms | residual_ms |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| en-b1 | 0 | 0 | answered | 1706 | 294 | 724 | 1003 | 208 | 1505 | 480 | 202 |
| en-b1 | 0 | 1 | answered | 1448 | 289 | 720 | 753 | 208 | 1250 | 230 | 198 |
| en-b1 | 1 | 0 | answered | 1580 | 266 | 636 | 888 | 219 | 1373 | 459 | 208 |
| en-b1 | 1 | 1 | answered | 1446 | 270 | 783 | 783 | 207 | 1259 | 187 | 187 |
| en-b1 | 2 | 0 | answered | 1804 | 254 | 850 | 1099 | 206 | 1560 | 494 | 244 |
| en-b1 | 2 | 1 | answered | 2102 | 253 | 1185 | 1382 | 204 | 1839 | 460 | 263 |
| en-b2 | 0 | 0 | answered | 2089 | 259 | 1260 | 1431 | 216 | 1906 | 354 | 183 |
| en-b2 | 0 | 1 | answered | 1596 | 260 | 741 | 881 | 261 | 1402 | 333 | 193 |
| en-b2 | 1 | 0 | answered | 1821 | 320 | 813 | 1056 | 208 | 1584 | 479 | 237 |
| en-b2 | 1 | 1 | answered | 1462 | 298 | 724 | 762 | 205 | 1264 | 235 | 197 |
| en-b2 | 2 | 0 | answered | 1763 | 319 | 755 | 994 | 217 | 1531 | 470 | 232 |
| en-b2 | 2 | 1 | answered | 1352 | 254 | 660 | 694 | 217 | 1165 | 221 | 186 |
| en-b2 | 3 | 0 | answered | 1673 | 322 | 652 | 897 | 240 | 1459 | 458 | 214 |
| en-b2 | 3 | 1 | answered | 2211 | 286 | 1527 | 1528 | 210 | 2024 | 189 | 188 |
| en-b2 | 4 | 0 | answered | 1771 | 288 | 793 | 1054 | 208 | 1549 | 482 | 222 |
| en-b2 | 4 | 1 | answered | 1628 | 285 | 778 | 841 | 219 | 1346 | 345 | 282 |
| en-b2 | 5 | 0 | answered | 4287 | 289 | 3178 | 3546 | 212 | 4047 | 608 | 240 |
| en-b2 | 5 | 1 | answered | 2203 | 305 | 1423 | 1423 | 209 | 1938 | 265 | 265 |
| en-b3 | 0 | 0 | answered | 1721 | 311 | 733 | 979 | 211 | 1501 | 466 | 220 |
| en-b3 | 0 | 1 | answered | 1686 | 256 | 833 | 1013 | 196 | 1466 | 400 | 220 |
| en-b3 | 1 | 0 | answered | 1670 | 316 | 711 | 913 | 221 | 1451 | 421 | 219 |
| en-b3 | 1 | 1 | answered | 1470 | 288 | 735 | 772 | 250 | 1309 | 197 | 161 |
| en-b3 | 2 | 0 | answered | 1579 | 258 | 640 | 874 | 215 | 1348 | 466 | 231 |
| en-b3 | 2 | 1 | answered | 2927 | 264 | 2229 | 2229 | 212 | 2705 | 223 | 223 |
| en-b3 | 3 | 0 | answered | 1871 | 261 | 931 | 1125 | 231 | 1618 | 448 | 253 |
| en-b3 | 3 | 1 | answered | 1407 | 242 | 633 | 791 | 197 | 1229 | 335 | 177 |
| en-b3 | 4 | 0 | answered | 1746 | 274 | 739 | 998 | 215 | 1487 | 517 | 259 |
| en-b3 | 4 | 1 | answered | 1462 | 253 | 721 | 940 | 213 | 1406 | 275 | 56 |
| en-b3 | 5 | 0 | answered | 1804 | 320 | 729 | 1022 | 226 | 1568 | 529 | 236 |
| en-b3 | 5 | 1 | answered | 1644 | 296 | 777 | 933 | 197 | 1425 | 375 | 219 |

## Raw bench reports (headers)

### en-b1

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:18080 --fixtures en_two_utterances --runs 3 --min-gap-ms 9000  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, one call at a time, real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:18080`
- 3 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### en-b2

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:18080 --fixtures en_two_utterances --runs 6 --min-gap-ms 9000  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, one call at a time, real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:18080`
- 6 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### en-b3

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:18080 --fixtures en_two_utterances --runs 6 --min-gap-ms 9000  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, one call at a time, real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:18080`
- 6 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


