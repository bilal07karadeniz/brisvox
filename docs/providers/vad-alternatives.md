# VAD alternatives and turn detection — research notes

- Fetched: 2026-10-08. Scope: alternatives/complements to Silero VAD, Pipecat defaults, Smart Turn.
- Legend: **[V]** = stated on a page fetched today. **[UNVERIFIED]** = not confirmed by a fetched page.
- Not a provider API doc; this file is a research note. Template `_TEMPLATE.md` fields do not apply.

## 1. Pipecat defaults [V]

- Sources: speech-input guide https://docs.pipecat.ai/guides/learn/speech-input ;
  API ref https://docs.pipecat.ai/api-reference/server/utilities/audio/silero-vad-analyzer.md ;
  source https://reference-server.pipecat.ai/en/latest/_modules/pipecat/audio/vad/vad_analyzer.html
- Default VAD: `SileroVADAnalyzer()` with no arguments. Silero is the recommended VAD; Krisp VIVA is offered for higher sample rates.
- `SileroVADAnalyzer(sample_rate=None, params=VADParams())`. `sample_rate` must be 8000 or 16000. Frames: 256 samples at 8 kHz, 512 samples at 16 kHz (32 ms).
- `VADParams` defaults (source constants `VAD_CONFIDENCE`, `VAD_START_SECS`, `VAD_STOP_SECS`, `VAD_MIN_VOLUME`):
  - `confidence` = 0.7 ("higher values make detection more strict", 0..1)
  - `start_secs` = 0.2
  - `stop_secs` = 0.2
  - `min_volume` = 0.6
- Decision rule (source): `speaking = confidence >= params.confidence and volume >= params.min_volume`. Volume is exponentially smoothed with factor 0.2. A high-confidence frame below `min_volume` counts as non-speech.
- State machine (source): QUIET -> STARTING (on first speaking frame) -> SPEAKING (after `start_secs` of speaking frames). SPEAKING -> STOPPING (first non-speaking frame) -> QUIET (after `stop_secs`). STOPPING returns to SPEAKING if speech resumes.
- Model state reset: "Automatically resets model state every 5 seconds" (API ref).
- Discrepancy: an older search snippet listed `stop_secs` default 0.8. The fetched current guide and source both say 0.2. Treat 0.2 as current.
- The speech-input guide says Smart Turn is the default turn-end strategy: `TurnAnalyzerUserTurnStopStrategy(turn_analyzer=LocalSmartTurnAnalyzerV3())`. VAD start/stop signals are inputs to the turn strategy, not the final turn decision. The guide's P99 STT latency numbers assume `stop_secs=0.2`.
- The fetched guide does NOT say `stop_secs` is auto-modified with turn detection (an older snippet claimed this) [UNVERIFIED].

## 2. Silero VAD (current baseline)

- License [V]: MIT, "Copyright (c) 2020-present Silero Team" — https://raw.githubusercontent.com/snakers4/silero-vad/master/LICENSE. A README badge alt text said "CC BY-NC 4.0"; the LICENSE file says MIT, so MIT governs.
- Latency [V, vendor claim, no conditions]: "One audio chunk (30+ ms) takes less than 1 ms to be processed on a single CPU thread" — https://github.com/snakers4/silero-vad. No CPU model, OS, or runtime version given. README also says batching or GPU "can also improve performance considerably."
  - **No Brisvox measurement exists; UNVERIFIED for our container.** Details and the measurement plan: `silero-vad.md` "CPU cost".
- Size: TEN README library-size table lists Silero "2.16MB (JIT) / 2.22MB (ONNX)" [V via TEN README, not Silero's own page]. Pinned v6.2.3 ONNX file is 2327524 bytes (see `silero-vad.md`).
- Version label "v5" and the ONNX graph's dynamic batch axis: [UNVERIFIED] from fetched pages. The Silero README did not state a version.
- Batched inference [V at wrapper level] — https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/utils_vad.py (OnnxWrapper):
  - Hidden state: `torch.zeros((2, batch_size, 128))`. Shape (2, 1, 128) for batch 1. The batch dimension is in the state tensor.
  - Context: `torch.zeros(batch_size, context_size)`, `context_size = 64 if sr == 16000 else 32`, prepended to each chunk and carried over as `x[..., -context_size:]`.
  - Chunk: `num_samples = 512 if sr == 16000 else 256`; other lengths raise ValueError.
  - `batch_size = x.shape[0]`; no assertion limits batch size to 1. Only inputs with more than 2 dims are rejected.
  - A batch-size change calls `reset_states(batch_size)`, which zeroes state for all rows.
  - Implication (inference, [UNVERIFIED] against the raw ONNX graph): per-stream state is row-independent, so N sessions can be stacked as one batch, with per-stream state and context rows gathered and scattered each call. Batch membership must stay fixed between calls, or state is reset. Needs a benchmark and a test that a batched session matches its solo run.

## 3. TEN VAD

- Repo: https://github.com/TEN-framework/ten-vad
- License [V]: "Apache License v2.0 with additional conditions" by Agora (Copyright 2025 Agora), per https://raw.githubusercontent.com/TEN-framework/ten-vad/main/LICENSE. README: "This project is licensed pursuant to the Apache 2.0 with additional conditions." `pitch_est.cc` contains LPCNet-derived code (BSD-2/3-Clause) per NOTICES.
  - Clause 1: may not Deploy "in a way that competes with Agora's offerings."
  - Clause 2: Deploy "solely to create and enable deployment of your Application(s)" for you and your direct End Users.
  - Clause 3: Derivative Works stay under this license.
  - This is not plain Apache-2.0. The competition clause may conflict with unrestricted open-source redistribution. Needs legal review before any use in Brisvox. Not a legal conclusion.
- Size [V]: library size (not model size): Linux 306 KB; Windows 464 KB (x86) / 508 KB (x64); macOS 731 KB; iOS 320 KB; Web 277 KB; Android 373 KB (v7a) / 532 KB (v8a). ONNX model size: not stated [UNVERIFIED].
- Latency [V, README RTF table, fetched 2026-10-08; RTF definition UNVERIFIED]:
  - The README only expands the acronym ("RTF (Real-Time Factor)"). It does NOT state the formula, per-chunk vs per-second basis, hop size, thread count, or measurement procedure. Do not treat these numbers as comparable to our measurements.
  - Only same-machine Silero comparison: Linux Xeon Gold 6348 @ 2.60GHz, TEN 0.0086 vs Silero 0.0127. Row-to-CPU mapping came from a garbled text extraction; re-check on the page.
  - Other TEN RTFs (extraction): Ryzen 9 5900X 0.0150; Xeon Platinum 8253 0.0136; i7-10710U (Windows) 0.0150; M1 0.0160; Web/M1 0.010; iPhone 6 (A8) 0.0210; iPhone 8 (A11) 0.0050; Galaxy J6+ 0.0570; Oppo A3s 0.0490.
  - Derived [inference, assumes RTF = compute time / audio duration]: TEN 0.0086 on a 10 ms hop ~0.09 ms; Silero 0.0127 on a 32 ms window ~0.41 ms.
  - No Brisvox measurement exists. The CPU-per-frame baseline for Silero is still required.
- Input [V]: 16 kHz only (other rates must be resampled). Hop 160 or 256 samples = 10 or 16 ms ("configurable hop sizes"; "optimized frame configurations").
- Platforms [V]: Linux x64; Windows x64/x86; macOS arm64/x86_64; WASM; Android arm64-v8a/armeabi-v7a; iOS arm64 (not simulator).
- Bindings [V]: Python, C, Java, Go, JavaScript. No official Rust crate listed. A third-party `ten-vad-rs` crate exists (https://rust-digger.code-maven.com/crates/ten-vad-rs); its license and maintenance are [UNVERIFIED].
- Accuracy [V, vendor claim]: "superior precision compared to both WebRTC VAD and Silero VAD." Vendor PR curves are an image only; no P/R/F1 in text. Vendor test set: "precisely manually annotated" LibriSpeech/GigaSpeech/DNS audio in repo `testset/` [V]; label format and image values [UNVERIFIED]. Third-party table with numbers: section 8.
- Vendor claim [UNVERIFIED]: Silero "suffers from a delay of several hundred milliseconds" on speech-to-non-speech transitions. No measurement given.

## 4. WebRTC VAD

- Rust crate: `webrtc-vad` 0.4.0. Crate metadata license MIT (https://docs.rs/crate/webrtc-vad/latest). It wraps libfvad and builds it with cc-rs, so a C compiler is required (https://github.com/kaegi/webrtc-vad).
- Maintenance: the search snippet listed four releases between 2019-08-14 and 2019-10-01 [V via search snippet]. Repo shows no Releases section. Treat as stale/unmaintained [UNVERIFIED as a conclusion].
- Underlying C library: libfvad, license BSD-3-Clause [V] (https://github.com/dpirch/libfvad). A fork of the WebRTC VAD engine; API in `include/fvad.h`.
- Sample rates, frame lengths, and aggressiveness modes: NOT in fetched pages [UNVERIFIED; commonly 8/16/32/48 kHz, 10/20/30 ms, four modes, but not confirmed].
- Quality vs neural VAD: third-party F1 52.30 / MR 64.15 at 0.5-threshold-type operating point (section 8) [V, third-party]. Vendor claims only otherwise [UNVERIFIED].
- Pure-Rust alternative: none checked [UNVERIFIED].

## 5. Smart Turn (turn detection)

- Repo: https://github.com/pipecat-ai/smart-turn ; HF card: https://huggingface.co/pipecat-ai/smart-turn-v3 ; HF file tree: https://huggingface.co/pipecat-ai/smart-turn-v3/tree/main
- License [V]: BSD-2-Clause ("a truly open model"), per repo README and HF card.
- Pipecat support: `LocalSmartTurnAnalyzerV3` available since Pipecat v0.0.85 (repo README).
- Architecture [V]: Whisper Tiny encoder layers plus a linear classifier head (HF card: "Whisper Tiny encoder" backbone, "shallow linear classifier", ~8M params). Static int8 QAT, exported to ONNX (Daily blog: https://www.daily.co/blog/announcing-smart-turn-v3-with-cpu-inference-in-just-12ms/).
- Versions and ONNX files [V, HF file tree]: `smart-turn-v3.0.onnx` (8.76 MB), `smart-turn-v3.1-cpu.onnx` (8.68 MB), `smart-turn-v3.1-gpu.onnx` (32.4 MB), `smart-turn-v3.2-cpu.onnx` (8.68 MB), `smart-turn-v3.2-gpu.onnx` (32.4 MB). Repo README is titled "v3.2"; HF card says "v3.x". Which file is current: [UNVERIFIED]. Changelog not found.
- Input contract [V, repo README]: 16 kHz, mono, PCM. Up to 8 s; shorter input is zero-padded at the beginning, longer input keeps the last ~8 s. Pass the full audio of the current user turn. Designed to be used with a lightweight VAD such as Silero.
- ONNX I/O [V, repo `inference.py` at main, fetched 2026-10-08 — https://raw.githubusercontent.com/pipecat-ai/smart-turn/main/inference.py]:
  - Reference code loads `ONNX_MODEL_PATH = "smart-turn-v3.1.onnx"` via `ort.InferenceSession(onnx_path, sess_options=so)` (sequential execution, `inter_op_num_threads = 1`, `ORT_ENABLE_ALL`). Note: the reference file names v3.1, not v3.2.
  - Input tensor name: `"input_features"`; `session.run(None, {"input_features": input_features})`; dtype float32; shape `(1, n_mels, frames)` after `expand_dims(axis=0)`. Exact n_mels/frames not stated in code; n_mels is not set explicitly (Whisper default 80 is [UNVERIFIED] inference).
  - Output: tensor name NOT given; code takes `outputs[0][0].item()` (first output by position). A code comment says the model returns sigmoid probabilities; the code applies no sigmoid [UNVERIFIED that the graph contains the sigmoid].
  - Threshold [V, code]: `prediction = 1 if probability > 0.5 else 0` (strict `>`, 0.5). `predict_endpoint` returns a dict with keys `prediction` and `probability`.
  - Feature extractor [V, code]: `WhisperFeatureExtractor(chunk_length=8)`, `sampling_rate=16000`, `padding="max_length"`, `max_length=8*16000` (128000 samples), `truncation=True`, `do_normalize=True`. Pre-step `truncate_audio_to_last_n_seconds(audio, n_seconds=8)` lives in `audio_utils`, not fetched [UNVERIFIED].
  - Rust implication: `ort` (ONNX Runtime) path is consistent with this code, but the log-mel frontend (Whisper-compatible, 16 kHz, 8 s, normalization) must be reimplemented in Rust and matched to the Python output. UNVERIFIED end-to-end; needs a parity test against Python on fixed audio.
- Size [V]: CPU int8 ONNX 8 MB; unquantized (GPU) 32 MB (README: ~1% more accurate, per README). v2 about 400 MB (Daily blog).
- Inference cost [V, Daily blog table, v3 numbers, includes preprocessing]: CPU AWS c7a.2xlarge 12.6 ms; c8g.2xlarge 15.2 ms; Modal 6-core CPU 17.7 ms; t3.2xlarge 33.8 ms; c8g.medium 59.8 ms; t3.medium 94.8 ms. GPU: L40S 3.3 ms; T4 6.6 ms. Repo README: "as little as 10 ms on some CPUs, under 100 ms on most cloud instances." These are vendor numbers; benchmark methodology and CPU models are not stated, and we have not measured them.
- v2 [search snippets only, UNVERIFIED on primary page]: wav2vec2 backbone, ~94.8M params, ~360 MB, BSD-2-Clause, 14 languages. Not suitable for CPU-only.
- Per-turn cost [inference from README]: one invocation per end-of-turn candidate, on the full turn audio (up to 8 s), not per 32 ms frame. Run on the `brisvox-inference` pool.
- Batching [UNVERIFIED]: not documented. Fixed 8 s padded input suggests a batch dimension might work; needs a test.
- Turkish [UNVERIFIED]: the README lists 23 languages "among others" (named: Arabic, Bengali, Chinese, English, Hindi, Japanese, Spanish, Vietnamese). The HF card has tag "multilingual" and no `tr` tag on the fetched page. Turkish presence not confirmed; treat as unknown.

## 6. Language and general notes

- VAD operates on audio energy/spectral features; no language dependency is assumed for Silero or TEN. Turkish is not separately verified for any VAD here [UNVERIFIED].

## 7. Summary for implementers

- Default: keep Silero (MIT). Vendor claim: <1 ms per 32 ms chunk on one CPU thread (UNVERIFIED for our container; no Brisvox measurement). Batch dimension present in its state tensor [V at wrapper level].
- Latency comparisons: no Brisvox measurement exists, and the TEN RTF values have no stated definition. The per-window CPU must be measured for Silero (Python ORT reference and Rust `ort`) on the target container before any latency claim.
- TEN VAD: better RTF on one benchmark machine [V vendor, definition UNVERIFIED], 16 kHz only, no official Rust binding, and a non-standard Apache license with a competition clause. Needs owner/legal decision before use in an open-source engine; benchmark only.
- WebRTC VAD: MIT wrapper + BSD-3-Clause libfvad; stale crate. Only as a cheap pre-gate if accuracy is validated.
- Smart Turn v3 (BSD-2-Clause, 16 kHz, 8 s window): best-documented candidate for turn detection. Input name `input_features` (float32, Whisper-style features), output = first ONNX output, probability with threshold 0.5 per reference code [V]. Output tensor name and Turkish coverage remain UNVERIFIED. Benchmark on target hardware; run on the inference pool.
- VAD accuracy: use the section 8 protocol. Fixture labels are energy-derived, not independent; hand-label speech edges first.
- Open questions: TEN VAD license acceptability; Smart Turn Turkish coverage and which ONNX version (v3.1 vs v3.2) to ship; hand-labelling of fixture speech edges.

## 8. Accuracy protocol for VAD (metric and labels) — added 2026-10-08

- Repo ground truth [V, files read 2026-10-08]: `fixtures/<name>.json` lists speech segments (`start`, `end` in 16 kHz samples). Committed: `en_two_utterances` (2), `tr_two_utterances` (2), `en_tr_mixed` (2). Local-only `fixtures/large/` has 4 sidecars.
- Caveat [V, docs/TESTING.md "Ground truth"]: boundaries are where the build script's 10 ms energy gate fires, NOT human-labelled onsets. The energy-VAD check is circular, and model VADs may differ by tens of ms at soft word edges. Silence between clips is synthetic, so non-speech is exact; speech edges are the weak part.
- Proposed metric [inference, not a fetched standard]:
  - Frames: 10 ms hop (160 samples at 16 kHz). Reference speech = frame centre inside a segment.
  - Frame F1, miss rate (MR), false-alarm rate (FAR) at a fixed threshold; AUC-ROC (threshold-free).
  - Boundary error: per segment, `|pred_onset - ref_start|` and `|pred_offset - ref_end|` in ms; report median, p90, and share within 50 ms (50 ms is a project choice, UNVERIFIED as a standard).
  - Silero gives one probability per 32 ms chunk (512 samples at 16 kHz). Map to 10 ms frames by holding the value, and say so in the report.
  - Operating points: 0.5 [Silero default threshold UNVERIFIED: README does not state it] and Pipecat `VADParams.confidence` 0.7 [V, section 1].
- Label gap: hand-label the 6 committed speech onsets/offsets (10 ms resolution), e.g. `hand_start`/`hand_end` in each sidecar, and score against them. `docs/TESTING.md` (metric section) and the sidecars are NOT changed by this note.
- Published labelled comparison [V: https://arxiv.org/html/2603.10420, Sec. 7.2, third-party FireRedASR2S paper]: FLEURS-VAD-102 (9,443 files, 102 languages, ~100 files per language from FLEURS test, manual binary labels; voice = speech + singing; non-voice = music, silence, noise). 25 ms window, 10 ms shift, fixed 0.5 posterior. Frame-level %:
  - FireRedVAD: AUC 99.60, F1 97.57, FAR 2.69, MR 3.62.
  - Silero-VAD: AUC 97.99, F1 95.95, FAR 9.41, MR 3.95.
  - WebRTC-VAD: F1 52.30, FAR 2.83, MR 64.15 (no AUC).
  - TEN-VAD: AUC 97.81, F1 95.19, FAR 15.47, MR 2.95.
  - Not reproduced here. FLEURS-VAD-102 download location and license: UNVERIFIED (search found no dataset page). Turkish breakdown: UNVERIFIED.
- Implication for TEN [inference from the table]: at 0.5, TEN's FAR (15.47%) is higher than Silero's (9.41%). That does not support the vendor's precision claim at this operating point. Vendor claim stays UNVERIFIED.
