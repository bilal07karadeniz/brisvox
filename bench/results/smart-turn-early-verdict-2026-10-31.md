# Smart Turn: early verdict vs stop verdict, published vs fused graph (2026-10-31)

The engine takes an early Smart Turn verdict 64 ms into the caller's silence (the snapshot holds about 96 ms of it)
and completes the turn on it when it says "complete"; the normal verdict comes at the stop (about 160 ms). Each clip
of the official test set `pipecat-ai/smart-turn-data-v3.2-test` (Hugging Face, CC-BY-4.0; clips drawn evenly from
two of its ten files, 23 languages) was scored twice per graph, with 96 ms and with 160 ms of zeros appended, the
same comparison `crates/brisvox-engine/tests/early_analysis_realtime.rs` makes on its fixture cuts. Features:
Pipecat's pinned whisper extractor on the last 8 s; ONNX Runtime 1.28.0, verdict p > 0.5. "too soon" = complete
early, incomplete at the stop. The margin rows count early commits only at p >= 0.5 + margin.

## With VNNI (reference desktop, AMD Ryzen 9 9950X)

```
1000 clips
4000 clips
4000 clips (native)

base-int8.onnx: early/stop agree 3870/4000 (96.75%)
  margin | early commits | too soon (stop says incomplete) | early commit on an incomplete label | stop complete on incomplete label
    0.00 |          2171 |                              63 |                                 269 | 276
    0.05 |          2153 |                              57 |                                 256 | 276
    0.10 |          2136 |                              53 |                                 247 | 276
    0.15 |          2111 |                              48 |                                 236 | 276
    0.20 |          2075 |                              35 |                                 215 | 276
    0.25 |          2041 |                              26 |                                 198 | 276
    0.30 |          1991 |                              18 |                                 173 | 276

rust-fused.onnx: early/stop agree 3870/4000 (96.75%)
  margin | early commits | too soon (stop says incomplete) | early commit on an incomplete label | stop complete on incomplete label
    0.00 |          2169 |                              63 |                                 267 | 275
    0.05 |          2149 |                              58 |                                 253 | 275
    0.10 |          2132 |                              53 |                                 245 | 275
    0.15 |          2106 |                              48 |                                 232 | 275
    0.20 |          2079 |                              38 |                                 217 | 275
    0.25 |          2040 |                              27 |                                 195 | 275
    0.30 |          1991 |                              18 |                                 171 | 275
```

## Without VNNI (qemu-user, CPU model Haswell, AVX2 kernels)

```
1000 clips
1000 clips (haswell)

base-int8.onnx: early/stop agree 973/1000 (97.30%)
  margin | early commits | too soon (stop says incomplete) | early commit on an incomplete label | stop complete on incomplete label
    0.00 |           540 |                               8 |                                  82 | 91
    0.05 |           537 |                               8 |                                  80 | 91
    0.10 |           533 |                               6 |                                  77 | 91
    0.15 |           523 |                               6 |                                  72 | 91
    0.20 |           516 |                               6 |                                  69 | 91
    0.25 |           502 |                               4 |                                  63 | 91
    0.30 |           494 |                               3 |                                  60 | 91

rust-fused.onnx: early/stop agree 974/1000 (97.40%)
  margin | early commits | too soon (stop says incomplete) | early commit on an incomplete label | stop complete on incomplete label
    0.00 |           541 |                               8 |                                  83 | 92
    0.05 |           538 |                               8 |                                  81 | 92
    0.10 |           534 |                               6 |                                  79 | 92
    0.15 |           523 |                               6 |                                  72 | 92
    0.20 |           517 |                               5 |                                  70 | 92
    0.25 |           504 |                               3 |                                  64 | 92
    0.30 |           491 |                               2 |                                  58 | 92
```

## Turns ended too soon, early and stop verdict together

| CPU | graph | incomplete clips ended (margin 0 / 0.1 / 0.2 / 0.3) | complete clips on the fast path (margin 0 / 0.2) | complete clips missed (margin 0 / 0.2) |
|---|---|---|---|---|
| VNNI, 4000 clips | published | 315 / 307 / 296 / 285 of 2022 | 1902 / 1860 of 1978 | 55 / 64 |
| VNNI, 4000 clips | fused | 313 / 306 / 296 / 284 of 2022 | 1902 / 1862 of 1978 | 55 / 63 |
| no VNNI, 1000 clips | published | 97 of 524 (margin 0) | 458 of 476 | 14 |
| no VNNI, 1000 clips | fused | 98 of 524 (margin 0) | 458 of 476 | 15 |

Reading: the two graphs behave the same on the early verdict, on both CPU kinds. The one fixture cut where the fused
graph completed early and the published graph did not (0.530 vs 0.288) is within this shared rate.
