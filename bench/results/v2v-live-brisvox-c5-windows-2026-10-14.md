# Live V2V, brisvox, 5 concurrent calls, Windows, real providers (2026-10-14)

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

- System under test: `brisvox-server` (release, features `provider-deepgram`, `provider-openai-compatible`, `provider-cartesia`, `provider-deepgram-aura`, `smart-turn-ort`, `silero-tract`), adaptive turns with Smart Turn v3.2, agent file `bench/pipecat_baseline/agent_live_en_aura.toml`.
- Blocks (in order): b1 (5 calls), b2 (5 calls), b3 (5 calls), b4 (5 calls), b5 (5 calls), b6 (5 calls), b7 (5 calls), b8 (5 calls), b9 (5 calls), b10 (5 calls)

## LLM route and upstream provider

Model `openai/gpt-6-luna`, `reasoning_effort` `none`, pinned upstream `openai` (`allow_fallbacks` false). Upstream reported by the first stream chunk of each response: {"OpenAI": 100} (models: {"openai/gpt-6-luna": 100}); LLM requests logged: 100, responses that reported an upstream: 100. **All reported upstreams are the pinned one: True.**

## Voice-to-voice (V2V)

V2V = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (`docs/LATENCY.md`). Nearest-rank percentiles over answered turns.

| metric (ms) | n | min | p50 | p90 | p95 | p99 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| V2V, first block | 10 | 1426 | 1930 | 2935 | 3114 | 3114 | 3114 | 2298 |
| V2V, all blocks pooled | 100 | 1358 | 1846 | 2209 | 2876 | 5199 | 5235 | 1937 |
| V2V, pooled, turn 0 | 50 | 1490 | 1867 | 2876 | 3114 | 5235 | 5235 | 2103 |
| V2V, pooled, turn 1 | 50 | 1358 | 1795 | 1960 | 2098 | 2419 | 2419 | 1772 |

V2V p50 per block (ms): b1 1930, b2 1935, b3 1881, b4 1566, b5 1729, b6 1772, b7 1653, b8 1821, b9 1803, b10 1722.

Outcomes (pooled): {"answered": 100}.

## Provider latencies as this system's own timers saw them

From the server's `turn finished` timeline lines (`brisvox::timeline`, `docs/LATENCY.md`, "Turn timeline"), one per turn. `stt_final_delay` is counted from the VAD's backdated end of speech to the last Deepgram final; `llm_ttft` from the request written to the connection to the first text delta (OpenRouter's time to first byte included); `tts_ttfb` from the first text sent to the TTS provider to its first audio byte.

| metric (ms) | n | min | p50 | p90 | p95 | p99 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| STT final after end of speech (`stt_final_delay`) | 100 | 253 | 300 | 337 | 363 | 376 | 390 | 302 |
| LLM first token after the request (`llm_ttft`) | 100 | 676 | 931 | 1311 | 1984 | 4282 | 4305 | 1086 |
| LLM first speakable chunk after the request (`llm_first_sentence`) | 100 | 690 | 1129 | 1641 | 2259 | 4531 | 4559 | 1263 |
| TTS first audio after the text (`tts_ttfb`) | 100 | 175 | 220 | 395 | 416 | 432 | 720 | 256 |

## What is left for the framework

Per turn: V2V minus the provider times above. The first row uses the LLM's first token, the same definition as the Pipecat file (there the wait for the first sentence is not timed, so it stays in the residual); the second row uses the first speakable chunk (the wait for a clause is provider streaming time). What remains is everything the engine and the transport add: turn detection after the final, hand-offs, the output frame grid, the websocket, and the difference between the VAD's end of speech and the fixture's ground truth (~150 ms, in both systems). The server's own response latency (end of speech to the first audio frame handed to the transport) splits the second row into the part inside the server and the client-side part.

| metric (ms) | n | min | p50 | p90 | p95 | p99 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| V2V minus (STT final + LLM first token + TTS first byte): the definition used for Pipecat below | 100 | 55 | 300 | 556 | 681 | 853 | 890 | 293 |
| V2V minus (STT final + LLM first chunk + TTS first byte) | 100 | 54 | 88 | 160 | 401 | 487 | 496 | 116 |
|   of which inside the server (server response latency minus the three providers) | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
|   of which client side (V2V minus the server's response latency: transport, EOS definition, frame grid) | 100 | 54 | 88 | 160 | 400 | 487 | 496 | 116 |

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
| b1 | 0 | 0 | answered | 3114 | 286 | 1967 | 2234 | 178 | 2699 | 682 | 416 |
| b1 | 0 | 1 | answered | 1632 | 325 | 879 | 950 | 301 | 1576 | 126 | 56 |
| b1 | 1 | 0 | answered | 2935 | 272 | 2152 | 2410 | 179 | 2861 | 333 | 75 |
| b1 | 1 | 1 | answered | 1473 | 287 | 712 | 714 | 416 | 1417 | 57 | 56 |
| b1 | 2 | 0 | answered | 2876 | 319 | 2018 | 2282 | 181 | 2782 | 359 | 94 |
| b1 | 2 | 1 | answered | 1930 | 265 | 1221 | 1233 | 377 | 1874 | 67 | 56 |
| b1 | 3 | 0 | answered | 2905 | 286 | 2113 | 2344 | 180 | 2811 | 325 | 94 |
| b1 | 3 | 1 | answered | 1426 | 325 | 716 | 832 | 213 | 1370 | 172 | 55 |
| b1 | 4 | 0 | answered | 2815 | 286 | 1984 | 2259 | 180 | 2725 | 366 | 90 |
| b1 | 4 | 1 | answered | 1876 | 325 | 711 | 950 | 179 | 1454 | 661 | 423 |
| b2 | 0 | 0 | answered | 1638 | 312 | 803 | 1049 | 183 | 1543 | 342 | 95 |
| b2 | 0 | 1 | answered | 2022 | 300 | 861 | 1254 | 317 | 1871 | 544 | 151 |
| b2 | 1 | 0 | answered | 1924 | 314 | 1114 | 1321 | 182 | 1818 | 313 | 107 |
| b2 | 1 | 1 | answered | 1932 | 316 | 1103 | 1177 | 378 | 1871 | 135 | 61 |
| b2 | 2 | 0 | answered | 1960 | 312 | 838 | 1369 | 191 | 1871 | 620 | 89 |
| b2 | 2 | 1 | answered | 1957 | 300 | 1270 | 1272 | 324 | 1896 | 63 | 61 |
| b2 | 3 | 0 | answered | 1577 | 280 | 768 | 1008 | 184 | 1471 | 345 | 106 |
| b2 | 3 | 1 | answered | 1966 | 303 | 1245 | 1341 | 177 | 1821 | 241 | 144 |
| b2 | 4 | 0 | answered | 2004 | 280 | 866 | 1417 | 178 | 1875 | 681 | 130 |
| b2 | 4 | 1 | answered | 1935 | 303 | 1274 | 1282 | 289 | 1874 | 69 | 61 |
| b3 | 0 | 0 | answered | 5235 | 326 | 4305 | 4559 | 245 | 5131 | 359 | 104 |
| b3 | 0 | 1 | answered | 1875 | 376 | 874 | 875 | 369 | 1620 | 255 | 255 |
| b3 | 1 | 0 | answered | 1867 | 344 | 883 | 1173 | 263 | 1780 | 377 | 87 |
| b3 | 1 | 1 | answered | 1881 | 309 | 1121 | 1263 | 254 | 1827 | 196 | 54 |
| b3 | 2 | 0 | answered | 1919 | 390 | 950 | 1208 | 244 | 1842 | 336 | 78 |
| b3 | 2 | 1 | answered | 1893 | 334 | 1094 | 1104 | 402 | 1839 | 64 | 54 |
| b3 | 3 | 0 | answered | 1693 | 330 | 812 | 1031 | 252 | 1612 | 300 | 81 |
| b3 | 3 | 1 | answered | 1857 | 332 | 1103 | 1107 | 312 | 1751 | 110 | 106 |
| b3 | 4 | 0 | answered | 5199 | 322 | 4282 | 4531 | 260 | 5112 | 336 | 87 |
| b3 | 4 | 1 | answered | 1905 | 334 | 1087 | 1115 | 358 | 1807 | 127 | 98 |
| b4 | 0 | 0 | answered | 2209 | 296 | 1441 | 1641 | 185 | 2121 | 288 | 88 |
| b4 | 0 | 1 | answered | 1677 | 269 | 902 | 931 | 416 | 1616 | 91 | 61 |
| b4 | 1 | 0 | answered | 1566 | 296 | 699 | 955 | 179 | 1430 | 392 | 137 |
| b4 | 1 | 1 | answered | 1478 | 265 | 807 | 836 | 317 | 1417 | 90 | 61 |
| b4 | 2 | 0 | answered | 2109 | 284 | 1311 | 1562 | 175 | 2021 | 339 | 88 |
| b4 | 2 | 1 | answered | 1547 | 269 | 800 | 1018 | 177 | 1464 | 301 | 83 |
| b4 | 3 | 0 | answered | 1533 | 284 | 684 | 934 | 188 | 1406 | 378 | 127 |
| b4 | 3 | 1 | answered | 1533 | 269 | 824 | 1018 | 185 | 1472 | 254 | 61 |
| b4 | 4 | 0 | answered | 2134 | 294 | 806 | 1172 | 181 | 1647 | 853 | 487 |
| b4 | 4 | 1 | answered | 1578 | 314 | 905 | 911 | 293 | 1518 | 66 | 61 |
| b5 | 0 | 0 | answered | 1729 | 343 | 892 | 1109 | 177 | 1629 | 317 | 100 |
| b5 | 0 | 1 | answered | 1953 | 328 | 1112 | 1112 | 413 | 1853 | 100 | 100 |
| b5 | 1 | 0 | answered | 1688 | 265 | 952 | 1158 | 180 | 1604 | 290 | 84 |
| b5 | 1 | 1 | answered | 1788 | 297 | 1003 | 1247 | 187 | 1731 | 300 | 56 |
| b5 | 2 | 0 | answered | 1838 | 320 | 1033 | 1245 | 180 | 1746 | 305 | 93 |
| b5 | 2 | 1 | answered | 1442 | 314 | 752 | 892 | 179 | 1385 | 197 | 56 |
| b5 | 3 | 0 | answered | 1734 | 257 | 923 | 1163 | 177 | 1596 | 377 | 138 |
| b5 | 3 | 1 | answered | 1479 | 274 | 745 | 745 | 404 | 1423 | 56 | 56 |
| b5 | 4 | 0 | answered | 1662 | 257 | 931 | 1135 | 177 | 1570 | 298 | 93 |
| b5 | 4 | 1 | answered | 1904 | 274 | 1226 | 1390 | 184 | 1848 | 220 | 56 |
| b6 | 0 | 0 | answered | 1702 | 263 | 756 | 1052 | 183 | 1498 | 500 | 204 |
| b6 | 0 | 1 | answered | 1749 | 266 | 782 | 1203 | 181 | 1650 | 521 | 100 |
| b6 | 1 | 0 | answered | 2098 | 325 | 1259 | 1447 | 186 | 1958 | 328 | 140 |
| b6 | 1 | 1 | answered | 1739 | 270 | 1133 | 1134 | 279 | 1684 | 57 | 55 |
| b6 | 2 | 0 | answered | 1754 | 307 | 881 | 1170 | 187 | 1664 | 379 | 91 |
| b6 | 2 | 1 | answered | 1850 | 306 | 1132 | 1265 | 181 | 1752 | 230 | 98 |
| b6 | 3 | 0 | answered | 2266 | 290 | 1670 | 1711 | 178 | 2179 | 128 | 87 |
| b6 | 3 | 1 | answered | 2101 | 285 | 747 | 1141 | 178 | 1605 | 890 | 496 |
| b6 | 4 | 0 | answered | 1836 | 307 | 934 | 1212 | 179 | 1698 | 416 | 138 |
| b6 | 4 | 1 | answered | 1772 | 306 | 690 | 690 | 720 | 1717 | 55 | 55 |
| b7 | 0 | 0 | answered | 1833 | 307 | 1041 | 1244 | 186 | 1737 | 299 | 96 |
| b7 | 0 | 1 | answered | 1960 | 274 | 884 | 1104 | 182 | 1560 | 621 | 401 |
| b7 | 1 | 0 | answered | 1904 | 372 | 1038 | 1268 | 175 | 1814 | 320 | 90 |
| b7 | 1 | 1 | answered | 1795 | 305 | 1010 | 1073 | 358 | 1736 | 122 | 60 |
| b7 | 2 | 0 | answered | 2102 | 337 | 1271 | 1497 | 180 | 2015 | 313 | 86 |
| b7 | 2 | 1 | answered | 1514 | 305 | 772 | 795 | 354 | 1455 | 83 | 60 |
| b7 | 3 | 0 | answered | 1653 | 302 | 849 | 1093 | 182 | 1577 | 320 | 76 |
| b7 | 3 | 1 | answered | 1469 | 265 | 783 | 961 | 183 | 1409 | 238 | 60 |
| b7 | 4 | 0 | answered | 1608 | 294 | 773 | 1049 | 180 | 1523 | 360 | 84 |
| b7 | 4 | 1 | answered | 1358 | 256 | 834 | 835 | 207 | 1298 | 61 | 60 |
| b8 | 0 | 0 | answered | 1793 | 374 | 829 | 1068 | 257 | 1699 | 333 | 94 |
| b8 | 0 | 1 | answered | 1821 | 299 | 1077 | 1081 | 383 | 1763 | 63 | 59 |
| b8 | 1 | 0 | answered | 1858 | 366 | 1029 | 1129 | 265 | 1759 | 199 | 99 |
| b8 | 1 | 1 | answered | 1858 | 320 | 1044 | 1049 | 429 | 1799 | 64 | 59 |
| b8 | 2 | 0 | answered | 1961 | 332 | 676 | 995 | 251 | 1577 | 703 | 384 |
| b8 | 2 | 1 | answered | 1750 | 275 | 1077 | 1159 | 257 | 1691 | 140 | 58 |
| b8 | 3 | 0 | answered | 1739 | 363 | 817 | 1029 | 252 | 1643 | 308 | 96 |
| b8 | 3 | 1 | answered | 1945 | 286 | 1117 | 1120 | 409 | 1815 | 133 | 130 |
| b8 | 4 | 0 | answered | 1629 | 360 | 704 | 912 | 260 | 1532 | 305 | 97 |
| b8 | 4 | 1 | answered | 1893 | 282 | 1107 | 1111 | 350 | 1743 | 153 | 150 |
| b9 | 0 | 0 | answered | 2063 | 327 | 923 | 1094 | 256 | 1677 | 556 | 386 |
| b9 | 0 | 1 | answered | 1751 | 302 | 946 | 1130 | 259 | 1691 | 245 | 61 |
| b9 | 1 | 0 | answered | 2137 | 305 | 1272 | 1482 | 244 | 2031 | 316 | 107 |
| b9 | 1 | 1 | answered | 1846 | 349 | 999 | 1043 | 393 | 1785 | 105 | 61 |
| b9 | 2 | 0 | answered | 1925 | 292 | 1030 | 1294 | 246 | 1832 | 358 | 94 |
| b9 | 2 | 1 | answered | 1551 | 293 | 718 | 948 | 249 | 1490 | 291 | 61 |
| b9 | 3 | 0 | answered | 1713 | 296 | 849 | 1065 | 263 | 1624 | 305 | 89 |
| b9 | 3 | 1 | answered | 1929 | 294 | 1138 | 1154 | 421 | 1868 | 76 | 61 |
| b9 | 4 | 0 | answered | 1803 | 301 | 884 | 1157 | 257 | 1715 | 362 | 88 |
| b9 | 4 | 1 | answered | 1526 | 320 | 735 | 895 | 250 | 1466 | 220 | 61 |
| b10 | 0 | 0 | answered | 1490 | 276 | 740 | 910 | 220 | 1407 | 252 | 83 |
| b10 | 0 | 1 | answered | 1722 | 264 | 864 | 866 | 432 | 1562 | 162 | 160 |
| b10 | 1 | 0 | answered | 2667 | 276 | 1834 | 2081 | 215 | 2572 | 342 | 95 |
| b10 | 1 | 1 | answered | 2098 | 268 | 997 | 1157 | 209 | 1634 | 624 | 463 |
| b10 | 2 | 0 | answered | 1980 | 270 | 1182 | 1414 | 208 | 1892 | 320 | 88 |
| b10 | 2 | 1 | answered | 2419 | 253 | 1206 | 1885 | 221 | 2359 | 739 | 60 |
| b10 | 3 | 0 | answered | 2074 | 279 | 1047 | 1483 | 218 | 1980 | 529 | 93 |
| b10 | 3 | 1 | answered | 1563 | 268 | 840 | 840 | 395 | 1503 | 60 | 60 |
| b10 | 4 | 0 | answered | 1718 | 306 | 903 | 1103 | 215 | 1624 | 294 | 94 |
| b10 | 4 | 1 | answered | 1658 | 334 | 883 | 883 | 382 | 1599 | 60 | 60 |

## Raw bench reports (headers)

### b1

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b2

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b3

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b4

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b5

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b6

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b7

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b8

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b9

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


### b10

# Voice-to-voice latency (live)

- command: `brisvox-bench v2v --url ws://127.0.0.1:28080 --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000 --keep-invalid  
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- live: one client, groups of concurrent calls (one group at a time), real time (SystemClock), 20 ms frames
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run
- target: `ws://127.0.0.1:28080`
- 5 runs of each of en_two_utterances (utterances spaced at least 9000 ms apart, 20 ms frames, silence below peak 200, response window 10000 ms after the fixture)
- 5 calls played at once

**V2V** = arrival at the client of the first non-silent bot audio after the ground-truth end of the user's speech, minus that end of speech (docs/LATENCY.md). Percentiles are nearest rank over answered turns; below 100 samples p99 is the maximum.


