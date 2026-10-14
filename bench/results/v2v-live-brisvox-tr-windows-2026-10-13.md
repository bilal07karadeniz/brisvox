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

- System under test: `brisvox-server` (release, features `provider-deepgram`, `provider-openai-compatible`, `provider-cartesia`, `smart-turn-ort`, `silero-tract`), adaptive turns with Smart Turn v3.2, agent file `bench/pipecat_baseline/agent_live_tr.toml`.
- Blocks (in order): tr-b1 (3 calls)

## Voice-to-voice (V2V)

V2V = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (`docs/LATENCY.md`). Nearest-rank percentiles over answered turns.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| V2V, first block (as specified) | 6 | 1824 | 1994 | 3031 | 3031 | 3031 | 2329 |
| V2V, all blocks pooled | 6 | 1824 | 1994 | 3031 | 3031 | 3031 | 2329 |
| V2V, pooled, turn 0 | 3 | 1941 | 2390 | 2791 | 2791 | 2791 | 2374 |
| V2V, pooled, turn 1 | 3 | 1824 | 1994 | 3031 | 3031 | 3031 | 2283 |

Outcomes (pooled): {"answered": 6}.

## Provider latencies as this system's own timers saw them

From the server's `turn finished` timeline lines (`brisvox::timeline`, `docs/LATENCY.md`, "Turn timeline"), one per turn. `stt_final_delay` is counted from the VAD's backdated end of speech to the last Deepgram final; `llm_ttft` from the request written to the connection to the first text delta (OpenRouter's time to first byte included); `tts_ttfb` from the first text sent to Cartesia to its first audio byte.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| STT final after end of speech (`stt_final_delay`) | 6 | 295 | 305 | 325 | 325 | 325 | 307 |
| LLM first token after the request (`llm_ttft`) | 6 | 975 | 1205 | 2300 | 2300 | 2300 | 1507 |
| LLM first speakable chunk after the request (`llm_first_sentence`) | 6 | 1152 | 1394 | 2332 | 2332 | 2332 | 1648 |
| TTS first audio after the text (`tts_ttfb`) | 6 | 195 | 199 | 224 | 224 | 224 | 202 |

## What is left for the framework

Per turn: V2V minus the provider times above. The first row uses the LLM's first token, the same definition as the Pipecat file (there the wait for the first sentence is not timed, so it stays in the residual); the second row uses the first speakable chunk (the wait for a clause is provider streaming time). What remains is everything the engine and the transport add: turn detection after the final, hand-offs, the output frame grid, the websocket, and the difference between the VAD's end of speech and the fixture's ground truth (~150 ms, in both systems). The server's own response latency (end of speech to the first audio frame handed to the transport) splits the second row into the part inside the server and the client-side part.

| metric (ms) | n | min | p50 | p90 | p95 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| V2V minus (STT final + LLM first token + TTS first byte): the definition used for Pipecat below | 6 | 203 | 275 | 455 | 455 | 455 | 312 |
| V2V minus (STT final + LLM first chunk + TTS first byte) | 6 | 80 | 171 | 217 | 217 | 217 | 172 |
|   of which inside the server (server response latency minus the three providers) | 6 | 0 | 0 | 0 | 0 | 0 | 0 |
|   of which client side (V2V minus the server's response latency: transport, EOS definition, frame grid) | 6 | 80 | 171 | 217 | 217 | 217 | 171 |

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
| tr-b1 | 0 | 0 | answered | 1941 | 309 | 975 | 1226 | 202 | 1737 | 455 | 204 |
| tr-b1 | 0 | 1 | answered | 3031 | 304 | 2300 | 2332 | 224 | 2860 | 203 | 171 |
| tr-b1 | 1 | 0 | answered | 2390 | 295 | 1624 | 1712 | 195 | 2202 | 275 | 188 |
| tr-b1 | 1 | 1 | answered | 1824 | 305 | 1013 | 1152 | 199 | 1656 | 307 | 169 |
| tr-b1 | 2 | 0 | answered | 2791 | 306 | 1922 | 2069 | 199 | 2574 | 364 | 217 |
| tr-b1 | 2 | 1 | answered | 1994 | 325 | 1205 | 1394 | 195 | 1914 | 270 | 80 |

## Raw bench reports (headers)

### tr-b1

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:18080 --fixtures tr_two_utterances --runs 3 --min-gap-ms 9000  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, one call at a time, real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:18080`
- 3 runs of each of tr_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


