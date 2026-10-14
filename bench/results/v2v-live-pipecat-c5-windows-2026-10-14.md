# Live V2V, pipecat, 5 concurrent calls, Windows, real providers (2026-10-14)

## Environment

- **Windows, real providers over the public internet.** Machine: Windows-10-10.0.26300-SP0, 32 logical cores,
  Python 3.10.11. Both systems and the benchmark client ran on this one machine, one system at a
  time, sequentially (never concurrently), against loopback. The machine was shared with other
  work (builds, a Docker/WSL benchmark in another session), so absolute numbers carry noise; the
  comparison interleaves the two systems block by block to spread that noise.
- **Not a Linux run.** The Windows timer ticks every ~15.6 ms; the largest client send lateness of
  each block is in its raw report below (a late frame delays the end of speech the server sees by as
  much). `docs/LATENCY.md` asks for Linux numbers for target claims.
- **Providers (identical for both systems, one agent file: `bench/pipecat_baseline/agent_live_en_aura.toml`):**
  STT Deepgram `nova-3` (live websocket, endpointing 300 ms, `smart_format`); LLM
  `openai/gpt-6-luna` through **OpenRouter** (`https://openrouter.ai/api/v1`, Chat Completions,
  streaming, `max_tokens` 120, `reasoning_effort` `none`: the model thinks by default, which adds
  about a second to the first token), upstream pinned with `provider.order` `openai` and `allow_fallbacks` false; TTS Deepgram Aura `aura-2-thalia-en` (websocket `/v1/speak`, linear16, 24 kHz). Turns: adaptive, Smart Turn v3.2
  (the same ONNX file on both sides), Silero VAD v6.2.3, one-sentence prompt.
- **IPv6 note.** This machine's IPv6 route silently drops packets. brisvox races address
  families for its provider websockets; the Pipecat baseline enables asyncio's happy-eyeballs
  racing for its websocket services (`bot.py`), so connection setup is not part of the difference.
  (The Cartesia handshake failure that motivated this does not apply here: the TTS is Deepgram.)
- **Sample size and concurrency.** 5 calls at once, 2 turns per call; 10 blocks (100 turns) interleaved with the other system's blocks (b1 p1 b2 p2 ...), so both saw the same provider conditions. Every call plays the same fixture, so the 5 calls speak at nearly the same moment and the providers see 5 simultaneous requests. p99 over 100 turns is the 99th value, one above the 2nd largest.
- Keys never appear in any of these files; they were passed through the environment only.

- System under test: Pipecat 1.12.0 bot (`bench/pipecat_baseline/bot.py`, Python 3.13), the same agent file, Smart Turn v3.2 and Silero v6.2.3, LLM through Pipecat's `OpenAILLMService` (Chat Completions) with `base_url` OpenRouter, happy-eyeballs websocket dialing on.
- Blocks (in order): p1 (5 calls), p2 (5 calls), p3 (5 calls), p4 (5 calls), p5 (5 calls), p6 (5 calls), p7 (5 calls), p8 (5 calls), p9 (5 calls), p10 (5 calls)

## LLM route and upstream provider

Model `openai/gpt-6-luna`, `reasoning_effort` `none`, pinned upstream `openai` (`allow_fallbacks` false). Upstream reported by the first stream chunk of each response: {"OpenAI": 102} (models: {"openai/gpt-6-luna": 102}); LLM requests logged: 167, responses that reported an upstream: 102. **All reported upstreams are the pinned one: True.**

## Voice-to-voice (V2V)

V2V = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (`docs/LATENCY.md`). Nearest-rank percentiles over answered turns.

| metric (ms) | n | min | p50 | p90 | p95 | p99 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| V2V, first block | 10 | 1664 | 1979 | 2847 | 2860 | 2860 | 2860 | 2209 |
| V2V, all blocks pooled | 100 | 1522 | 1972 | 2980 | 4348 | 6141 | 7529 | 2343 |
| V2V, pooled, turn 0 | 50 | 1565 | 2167 | 3026 | 5750 | 7529 | 7529 | 2618 |
| V2V, pooled, turn 1 | 50 | 1522 | 1892 | 2544 | 3689 | 4680 | 4680 | 2068 |

V2V p50 per block (ms): p1 1979, p2 2167, p3 2084, p4 1845, p5 2097, p6 1932, p7 1967, p8 2039, p9 1914, p10 1866.

Outcomes (pooled): {"answered": 100}.

## Provider latencies as this system's own timers saw them

From Pipecat's own service timers (`enable_metrics`, logged by the bot as `PCMETRIC`, `--provider-metrics`). STT TTFB is counted from the VAD stop (100 ms of silence after the end of speech, 32 ms windows) to the final transcript; LLM TTFB from the request to the first token; TTS TTFB from the first text to the first audio. A turn the user resumed mid-sentence can show more than one LLM request; the last one is the answered one (`llm_requests` in the JSON).

| metric (ms) | n | min | p50 | p90 | p95 | p99 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| STT final after the VAD stop (Pipecat STT TTFB) | 100 | 293 | 355 | 399 | 405 | 421 | 431 | 359 |
| STT final after end of speech (TTFB + 100 ms VAD stop) | 100 | 393 | 455 | 499 | 505 | 521 | 531 | 459 |
| LLM first token after the request (Pipecat LLM TTFB) | 100 | 686 | 889 | 1381 | 3483 | 3892 | 4520 | 1109 |
| TTS first audio after the text (Pipecat TTS TTFB) | 100 | 174 | 251 | 259 | 265 | 270 | 270 | 242 |

## What is left for the framework

Per turn: V2V minus the three provider times above (STT from the end of speech, the LLM to its first token, TTS to its first byte). It is not the same residual as brisvox's: Pipecat does not time the wait for the first sentence (its aggregator holds text until a sentence ends) nor the Smart Turn run, so those, the soxr resampler's 100 ms bursts, the serializer and the pacer are in this number, together with the transport and the end-of-speech definition.

| metric (ms) | n | min | p50 | p90 | p95 | p99 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| V2V minus (100 ms VAD stop + STT TTFB + LLM TTFB + TTS TTFB) | 100 | -41 | 398 | 1143 | 1328 | 1494 | 5982 | 533 |

## Failures

- failed calls: 0
- no_response turns: 0
- talked_over turns: 0
- bot_speaking turns: 0
- turns without a provider timeline: 0
- ERROR log lines: 0
- WARN log lines: 0
- answered turns with more than one LLM request (cause not established): 56
- LLM requests cancelled before any chunk (no upstream to report): 65

## Every turn

| block | run | turn | outcome | v2v_ms | stt_ttfb_ms | llm_ttfb_ms | tts_ttfb_ms | llm_requests | residual_ms |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|
| p1 | 0 | 0 | answered | 2860 | 366 | 802 | 265 | 3 | 1328 |
| p1 | 0 | 1 | answered | 2351 | 389 | 1010 | 257 | 2 | 595 |
| p1 | 1 | 0 | answered | 1664 | 342 | 707 | 257 | 1 | 259 |
| p1 | 1 | 1 | answered | 1743 | 360 | 838 | 265 | 1 | 179 |
| p1 | 2 | 0 | answered | 2847 | 344 | 963 | 249 | 2 | 1191 |
| p1 | 2 | 1 | answered | 2484 | 383 | 1033 | 265 | 2 | 703 |
| p1 | 3 | 0 | answered | 2366 | 383 | 768 | 257 | 3 | 859 |
| p1 | 3 | 1 | answered | 1892 | 388 | 806 | 256 | 2 | 343 |
| p1 | 4 | 0 | answered | 1979 | 421 | 723 | 265 | 2 | 470 |
| p1 | 4 | 1 | answered | 1906 | 369 | 792 | 257 | 2 | 387 |
| p2 | 0 | 0 | answered | 2412 | 394 | 759 | 253 | 3 | 905 |
| p2 | 0 | 1 | answered | 2722 | 403 | 1381 | 250 | 2 | 588 |
| p2 | 1 | 0 | answered | 2167 | 352 | 1008 | 255 | 2 | 452 |
| p2 | 1 | 1 | answered | 2040 | 368 | 1145 | 242 | 1 | 185 |
| p2 | 2 | 0 | answered | 2919 | 366 | 762 | 252 | 3 | 1439 |
| p2 | 2 | 1 | answered | 1837 | 326 | 1017 | 256 | 1 | 137 |
| p2 | 3 | 0 | answered | 7529 | 342 | 848 | 257 | 2 | 5982 |
| p2 | 3 | 1 | answered | 1876 | 345 | 991 | 256 | 1 | 183 |
| p2 | 4 | 0 | answered | 2795 | 374 | 1582 | 252 | 2 | 487 |
| p2 | 4 | 1 | answered | 1982 | 355 | 975 | 259 | 1 | 293 |
| p3 | 0 | 0 | answered | 2140 | 370 | 949 | 183 | 2 | 539 |
| p3 | 0 | 1 | answered | 1934 | 318 | 1360 | 174 | 1 | -19 |
| p3 | 1 | 0 | answered | 4348 | 296 | 3512 | 185 | 1 | 255 |
| p3 | 1 | 1 | answered | 1522 | 320 | 808 | 179 | 1 | 115 |
| p3 | 2 | 0 | answered | 2274 | 399 | 1121 | 183 | 2 | 472 |
| p3 | 2 | 1 | answered | 1619 | 295 | 882 | 181 | 1 | 161 |
| p3 | 3 | 0 | answered | 6141 | 298 | 4520 | 176 | 2 | 1046 |
| p3 | 3 | 1 | answered | 4260 | 307 | 3483 | 180 | 1 | 190 |
| p3 | 4 | 0 | answered | 1712 | 338 | 807 | 180 | 1 | 286 |
| p3 | 4 | 1 | answered | 2084 | 326 | 1060 | 187 | 1 | 411 |
| p4 | 0 | 0 | answered | 5750 | 346 | 3892 | 270 | 2 | 1143 |
| p4 | 0 | 1 | answered | 1845 | 377 | 761 | 254 | 2 | 352 |
| p4 | 1 | 0 | answered | 2057 | 351 | 894 | 257 | 2 | 455 |
| p4 | 1 | 1 | answered | 1613 | 351 | 779 | 244 | 1 | 138 |
| p4 | 2 | 0 | answered | 2389 | 338 | 819 | 245 | 2 | 888 |
| p4 | 2 | 1 | answered | 1791 | 340 | 794 | 245 | 1 | 312 |
| p4 | 3 | 0 | answered | 2115 | 350 | 868 | 243 | 1 | 554 |
| p4 | 3 | 1 | answered | 2581 | 349 | 1388 | 258 | 1 | 485 |
| p4 | 4 | 0 | answered | 1683 | 341 | 744 | 250 | 1 | 249 |
| p4 | 4 | 1 | answered | 1639 | 382 | 812 | 255 | 1 | 90 |
| p5 | 0 | 0 | answered | 3026 | 365 | 818 | 250 | 3 | 1494 |
| p5 | 0 | 1 | answered | 3689 | 431 | 2468 | 256 | 2 | 435 |
| p5 | 1 | 0 | answered | 1787 | 334 | 686 | 243 | 1 | 425 |
| p5 | 1 | 1 | answered | 2097 | 388 | 937 | 257 | 1 | 415 |
| p5 | 2 | 0 | answered | 1947 | 355 | 781 | 250 | 2 | 461 |
| p5 | 2 | 1 | answered | 2385 | 405 | 1235 | 249 | 2 | 396 |
| p5 | 3 | 0 | answered | 1736 | 334 | 834 | 259 | 1 | 210 |
| p5 | 3 | 1 | answered | 1954 | 385 | 841 | 252 | 2 | 377 |
| p5 | 4 | 0 | answered | 2138 | 393 | 901 | 252 | 2 | 491 |
| p5 | 4 | 1 | answered | 4680 | 366 | 3713 | 246 | 1 | 256 |
| p6 | 0 | 0 | answered | 2789 | 351 | 824 | 249 | 3 | 1265 |
| p6 | 0 | 1 | answered | 1937 | 379 | 900 | 247 | 2 | 312 |
| p6 | 1 | 0 | answered | 1932 | 355 | 814 | 243 | 2 | 419 |
| p6 | 1 | 1 | answered | 1972 | 381 | 1146 | 243 | 1 | 102 |
| p6 | 2 | 0 | answered | 3001 | 374 | 1344 | 257 | 3 | 926 |
| p6 | 2 | 1 | answered | 1632 | 337 | 785 | 243 | 1 | 166 |
| p6 | 3 | 0 | answered | 1969 | 363 | 966 | 254 | 2 | 287 |
| p6 | 3 | 1 | answered | 1817 | 367 | 755 | 252 | 2 | 343 |
| p6 | 4 | 0 | answered | 1804 | 347 | 943 | 257 | 1 | 157 |
| p6 | 4 | 1 | answered | 1761 | 387 | 798 | 261 | 1 | 214 |
| p7 | 0 | 0 | answered | 2453 | 362 | 1042 | 253 | 2 | 695 |
| p7 | 0 | 1 | answered | 2353 | 401 | 1178 | 248 | 2 | 426 |
| p7 | 1 | 0 | answered | 2851 | 358 | 963 | 270 | 3 | 1160 |
| p7 | 1 | 1 | answered | 1793 | 335 | 965 | 250 | 1 | 143 |
| p7 | 2 | 0 | answered | 2008 | 383 | 889 | 247 | 2 | 389 |
| p7 | 2 | 1 | answered | 1745 | 334 | 930 | 251 | 1 | 129 |
| p7 | 3 | 0 | answered | 1967 | 408 | 754 | 259 | 2 | 445 |
| p7 | 3 | 1 | answered | 1862 | 406 | 742 | 259 | 1 | 355 |
| p7 | 4 | 0 | answered | 1913 | 372 | 726 | 262 | 2 | 453 |
| p7 | 4 | 1 | answered | 2121 | 396 | 936 | 256 | 2 | 433 |
| p8 | 0 | 0 | answered | 2980 | 350 | 916 | 243 | 2 | 1372 |
| p8 | 0 | 1 | answered | 2039 | 377 | 900 | 255 | 2 | 408 |
| p8 | 1 | 0 | answered | 2158 | 358 | 740 | 255 | 2 | 705 |
| p8 | 1 | 1 | answered | 1694 | 342 | 893 | 246 | 1 | 114 |
| p8 | 2 | 0 | answered | 2067 | 403 | 849 | 256 | 2 | 459 |
| p8 | 2 | 1 | answered | 1968 | 383 | 816 | 244 | 2 | 424 |
| p8 | 3 | 0 | answered | 2487 | 396 | 865 | 258 | 3 | 869 |
| p8 | 3 | 1 | answered | 1695 | 345 | 856 | 264 | 1 | 130 |
| p8 | 4 | 0 | answered | 2492 | 378 | 904 | 258 | 2 | 852 |
| p8 | 4 | 1 | answered | 1820 | 326 | 978 | 248 | 1 | 169 |
| p9 | 0 | 0 | answered | 5362 | 353 | 3717 | 245 | 3 | 947 |
| p9 | 0 | 1 | answered | 1727 | 343 | 888 | 243 | 1 | 153 |
| p9 | 1 | 0 | answered | 2819 | 364 | 897 | 259 | 3 | 1197 |
| p9 | 1 | 1 | answered | 1914 | 369 | 845 | 259 | 2 | 342 |
| p9 | 2 | 0 | answered | 1892 | 344 | 946 | 257 | 1 | 246 |
| p9 | 2 | 1 | answered | 1909 | 407 | 815 | 246 | 1 | 341 |
| p9 | 3 | 0 | answered | 2427 | 391 | 1013 | 254 | 2 | 669 |
| p9 | 3 | 1 | answered | 1745 | 399 | 885 | 251 | 1 | 110 |
| p9 | 4 | 0 | answered | 1924 | 352 | 807 | 260 | 2 | 406 |
| p9 | 4 | 1 | answered | 1938 | 363 | 823 | 255 | 2 | 398 |
| p10 | 0 | 0 | answered | 2487 | 345 | 761 | 204 | 2 | 1077 |
| p10 | 0 | 1 | answered | 1592 | 331 | 838 | 204 | 1 | 119 |
| p10 | 1 | 0 | answered | 2948 | 294 | 920 | 206 | 2 | 1428 |
| p10 | 1 | 1 | answered | 1619 | 328 | 879 | 214 | 1 | 98 |
| p10 | 2 | 0 | answered | 1961 | 353 | 891 | 219 | 2 | 398 |
| p10 | 2 | 1 | answered | 1892 | 339 | 868 | 215 | 1 | 370 |
| p10 | 3 | 0 | answered | 1565 | 314 | 748 | 220 | 1 | 182 |
| p10 | 3 | 1 | answered | 1770 | 305 | 1190 | 216 | 1 | -41 |
| p10 | 4 | 0 | answered | 1866 | 293 | 1048 | 210 | 1 | 215 |
| p10 | 4 | 1 | answered | 2544 | 304 | 1620 | 224 | 1 | 295 |

## Raw bench reports (headers)

### p1

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p2

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p3

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p4

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p5

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p6

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p7

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p8

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p9

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### p10

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28765 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28765`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


