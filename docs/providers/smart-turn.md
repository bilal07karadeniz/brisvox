# Pipecat Smart Turn — turn-completion model (local ONNX, not a cloud provider)

- Docs: https://github.com/pipecat-ai/smart-turn (README, inference.py, audio_utils.py, LICENSE) ; https://huggingface.co/pipecat-ai/smart-turn-v3 ; https://docs.pipecat.ai/api-reference/server/utilities/turn-detection/smart-turn-overview (fetched 2026-10-08)
- Pipecat source: https://raw.githubusercontent.com/pipecat-ai/pipecat/main/src/pipecat/audio/turn/smart_turn/local_smart_turn_v3.py (fetched 2026-10-08)
- Reference inference script: https://raw.githubusercontent.com/pipecat-ai/smart-turn/main/inference.py (fetched 2026-10-08)
- Reference audio helper: https://raw.githubusercontent.com/pipecat-ai/smart-turn/main/audio_utils.py (fetched 2026-10-08)
- Whisper feature extractor source: https://raw.githubusercontent.com/huggingface/transformers/main/src/transformers/models/whisper/feature_extraction_whisper.py (fetched 2026-10-08)
- Type: local model, ONNX Runtime on CPU. No auth, no endpoint, no network calls.
- Items marked UNVERIFIED were not confirmed by a fetched page.

## Model

- Latest version: **Smart Turn v3.2** (HF commit `f766f81d3cfdf7737ac64aad813d91bbfd56bf93`, title "Smart Turn v3.2", author marcus-daily). Source: https://huggingface.co/pipecat-ai/smart-turn-v3/commits/main
  - Release date: **2026-01-07T18:00:39Z** (CONFIRMED, HF API `date` field of commit `f766f81`, fetched 2026-10-08: https://huggingface.co/api/models/pipecat-ai/smart-turn-v3/commits/main). The HTML commit page shows the same commit at the top of `main` as "Jan 7" (year omitted there).
  - Head of `main`: `f766f81` is the first (newest) entry in both the API list and the HTML history as fetched 2026-10-08. The API has no explicit HEAD flag, so "latest" means newest entry in the commit list, not a separately confirmed ref. Re-check before pinning a newer version.
  - Previous: v3.1 commit `16c8130`, Dec 3, 2025; v3.0 commit `100f018`, Sep 11, 2025.
- HF repo `pipecat-ai/smart-turn-v3` main branch files (https://huggingface.co/pipecat-ai/smart-turn-v3/tree/main):

  | File | Size | Note |
  |---|---|---|
  | `smart-turn-v3.2-cpu.onnx` | 8.68 MB | int8-quantized per README (recommended for CPU) |
  | `smart-turn-v3.2-gpu.onnx` | 32.4 MB | fp32 unquantized; README says ~1% better accuracy than CPU |
  | `smart-turn-v3.1-cpu.onnx` / `-gpu.onnx` | 8.68 MB / 32.4 MB | older |
  | `smart-turn-v3.0.onnx` | 8.76 MB | oldest |

- **SHA-256 of `smart-turn-v3.2-cpu.onnx`**: `2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f` (CONFIRMED on https://huggingface.co/pipecat-ai/smart-turn-v3/blob/main/smart-turn-v3.2-cpu.onnx, fetched 2026-10-08). Exact byte count: UNVERIFIED (page shows only "8.68 MB"). Xet hash `aca6a9dc57f624c40984caae7a92965eee9850a034565a189d3e3969e0eb4080` (not a SHA-256).
- GPU file SHA-256: UNVERIFIED (not fetched).
- ONNX opset of the Pipecat CPU file: **UNVERIFIED** (opset 18 is stated only for a third-party GPU re-export, see Input/output contract).
- Architecture: Whisper Tiny encoder + shallow linear classifier head, ~8M parameters (HF model card). Source: https://huggingface.co/pipecat-ai/smart-turn-v3
- Training data: `pipecat-ai/smart-turn-data-v3.1-train` (~270k) and `-test` (~31.5k) per HF page; README references `smart-turn-data-v3.2-*`.

### Bundled model used by Pipecat (CONFIRMED 2026-10-08)

- Pipecat `LocalSmartTurnAnalyzerV3` (`local_smart_turn_v3.py`): `def __init__(self, *, smart_turn_model_path: str | None = None, cpu_count: int = 1, **kwargs)`.
- When `smart_turn_model_path` is unset, the code sets `model_name = "smart-turn-v3.2-cpu.onnx"` and `package_path = "pipecat.audio.turn.smart_turn.data"`, then loads `impresources.files(package_path).joinpath(model_name)`. The file contains no URL and no download logic, so the model is read from the installed package at runtime. Source: the Pipecat URL above.
- **Pin for Brisvox parity: `smart-turn-v3.2-cpu.onnx` (v3.2, int8 CPU build), SHA-256 `2bb026316b14...e4f`.** Brisvox must select the model by this name and hash, never by an unpinned default path.
- Still UNVERIFIED: that the bytes in the Pipecat wheel's `data/` directory match the HF file (packaged copy's SHA-256 not checked), and that the wheel ships the `.onnx` rather than requiring a separate download (pyproject/manifest not fetched).
- Gotcha (CONFIRMED): `inference.py` (smart-turn repo) sets `ONNX_MODEL_PATH = "smart-turn-v3.1.onnx"`. That filename is not in the current HF file list (only `-v3.1-cpu.onnx` / `-v3.1-gpu.onnx` exist), so the `inference.py` default is stale. Always pass the path explicitly. `build_session(onnx_path)` takes the path as its only argument.
- Default turn stop strategy (CONFIRMED, version check, 2026-10-08): Pipecat's default user turn stop strategy is `TurnAnalyzerUserTurnStopStrategy` with `LocalSmartTurnAnalyzerV3`, "as of v0.0.102" (https://docs.pipecat.ai/api-reference/server/utilities/turn-detection/smart-turn-overview; release https://github.com/pipecat-ai/pipecat/releases/tag/v0.0.102, PR #3689, previous default `TranscriptionUserTurnStopStrategy`; CHANGELOG `[0.0.102] - 2026-02-10`, https://raw.githubusercontent.com/pipecat-ai/pipecat/main/CHANGELOG.md). The GitHub release page shows "11 Feb" without a year; the year comes from the CHANGELOG.
- "Bundles this copy since v0.0.85": UNVERIFIED. The v0.0.85 release page (https://github.com/pipecat-ai/pipecat/releases/tag/v0.0.85) and CHANGELOG `[0.0.85] - 2025-09-12` never mention SmartTurn or bundling. Nearby: 0.0.77 added `LocalSmartTurnAnalyzerV2`; 0.0.86 says torch/torchaudio are "no longer required for running Smart Turn locally". The bundling version is unconfirmed.
- Old docs URL https://docs.pipecat.ai/server/utilities/smart-turn returns HTTP 404 (CONFIRMED 2026-10-08). Do not cite it.

## License

Status (license check, 2026-10-08): code license CONFIRMED. Weights license DECLARED (HF tag and README) but no license text covers the weights. Whisper (MIT) attribution for the derived weights is IDENTIFIED; whether it applies is UNVERIFIED legal interpretation.

- Code: **BSD-2-Clause** (CONFIRMED). https://github.com/pipecat-ai/smart-turn/blob/main/LICENSE: "BSD 2-Clause License", "Copyright (c) 2024–2025, Daily". SPDX header `# SPDX-License-Identifier: BSD 2-Clause License` in Pipecat's `local_smart_turn_v3.py`: https://raw.githubusercontent.com/pipecat-ai/pipecat/main/src/pipecat/audio/turn/smart_turn/local_smart_turn_v3.py
- Weights, declared (declaration only): HF model card renders "License: bsd-2-clause" (https://huggingface.co/pipecat-ai/smart-turn-v3; raw YAML front matter not seen, UNVERIFIED for other license fields). README (https://github.com/pipecat-ai/smart-turn): "This is a truly open model (BSD 2-clause license)." and "The datasets, training script, and model weights are all open source."
- **Gap:** the Daily LICENSE speaks only of "this software" and "source and binary forms" and never mentions model weights. No separate weights LICENSE or NOTICE appears in the HF file list. The BSD-2 grant over the ONNX file therefore rests on the HF tag and README wording alone.
- Base model: **Whisper Tiny (OpenAI)**. No `base_model` field on the rendered HF page (UNVERIFIED for raw front matter). Named in the HF card ("Backbone: Whisper Tiny encoder") and README ("uses Whisper Tiny as a base").
- Whisper license (CONFIRMED): https://github.com/openai/whisper/blob/main/LICENSE is **MIT License**, "Copyright (c) 2022 OpenAI", and "The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software." https://github.com/openai/whisper/blob/main/README.md: "Whisper's code and model weights are released under the MIT License."
- **Attribution obligation:** OpenAI applies MIT to Whisper's weights, and Smart Turn's encoder derives from Whisper Tiny. Conservative reading: redistributing smart-turn ONNX files requires the OpenAI copyright line and MIT permission notice. Whether a fine-tuned derivative is "substantial portions of the Software" is NOT addressed by any fetched source (UNVERIFIED). No fetched page gives a notice instruction for smart-turn.
- **Brisvox recommendation (not yet a recorded decision):** do not commit or ship `smart-turn-*.onnx` in the repo or release artifacts; fetch the pinned file from HF at verify time. If any artifact bundles it, ship a notice with the Daily BSD-2 notice and the OpenAI MIT notice. Record the final choice in the design notes.
- Training datasets license: UNVERIFIED.

## Input/output contract

Sources: upstream `inference.py`, `audio_utils.py`, and Pipecat bundled `local_smart_turn_v3.py` (URLs above), fetched 2026-10-08. The ONNX graph of `smart-turn-v3.2-cpu.onnx` was NOT read (no local runtime; HF `resolve` URL is a binary CDN the fetch tool cannot parse). Tensor shapes, axes, and output name stay UNVERIFIED until the file is inspected.

Input:
- ONNX input name: **`input_features`** (CONFIRMED: `session.run(None, {"input_features": input_features})` in both upstream and Pipecat code).
- Batch dim: `np.expand_dims(x, axis=0)` (CONFIRMED).
- Dtype: **float32** (CONFIRMED in code). Graph declared element type: UNVERIFIED.
- Sample rate: **16 kHz**, mono. Resample upstream (`_MODEL_SAMPLE_RATE = 16000` in Pipecat source).
- Window: **8 s = 128,000 samples** (`truncate_audio_to_last_n_seconds(audio, n_seconds=8, sample_rate=16000)`, audio_utils.py). Too long: keep the **last** 8 s (`audio_array[-max_samples:]`). Too short: `np.pad` with zeros at the start, audio at the end.
- Feature extraction (two code paths; equivalence UNVERIFIED):
  - Upstream `inference.py`: `WhisperFeatureExtractor(chunk_length=8)`, called with `sampling_rate=16000, return_tensors="np", padding="max_length", max_length=8*16000, truncation=True, do_normalize=True`.
  - Pipecat bundled copy: does NOT call `WhisperFeatureExtractor`. It calls a vendored `compute_whisper_log_mel_features(audio_array, do_normalize=True)`, then `np.expand_dims(log_mel, axis=0)`.
- Whisper extractor defaults (CONFIRMED from transformers source, fetched 2026-10-08): `feature_size=80`, `sampling_rate=16000`, `hop_length=160`, `n_fft=400`, `padding_value=0.0`, `dither=0.0`; `chunk_length` default 30 s, overridden to 8 by smart-turn.
- Log-mel steps (CONFIRMED from transformers source, PyTorch path): Hann STFT; power spectrum over all frames except the last (`stft[..., :-1]`); mel filters `mel_filters.T @ magnitudes`; `clamp(min=1e-10).log10()`; floor at `max - 8.0`; then `(log_spec + 4.0) / 4.0`.
- `do_normalize=True` (CONFIRMED): per waveform `(x - mean) / sqrt(var + 1e-7)` over valid (unpadded) samples, applied before the STFT.
- Input shape `(1, 80, 800)`: DERIVED (128000 / 160 = 800 frames after dropping the last frame), NOT read from the graph. Verify with the dump. Fixed vs dynamic axes: **UNVERIFIED**.

Output:
- Read as `outputs[0][0]` (Pipecat, cast to float) or `outputs[0][0].item()` (upstream). Index 0 is the probability. Pipecat comment: "ONNX model returns sigmoid probabilities" (CONFIRMED from code comment; no sigmoid applied in code).
- Output tensor name: **UNVERIFIED** (code passes `None`, so all outputs are returned and the name is never referenced).
- Output dtype and shape: **UNVERIFIED** (code implies a batch-1 array whose first element is the scalar probability).
- Decision: `prediction = 1 if probability > 0.5 else 0`. 1 = complete, 0 = incomplete.
- Pipecat `SmartTurnParams`: `stop_secs` default 3.0 s (silence fallback), `pre_speech_ms` default 500, `max_duration_secs` default 8.0 (as stated in the earlier fetch; not re-checked in the version check).
- VAD `stop_secs`: default **0.2 s since v0.0.102** (was 0.8 s; PR #3659; CONFIRMED on the v0.0.102 release page and CHANGELOG). Docs: "We recommend 0.2 seconds, which is the default value" (https://docs.pipecat.ai/api-reference/server/utilities/turn-detection/smart-turn-overview).
- The 0.5 threshold is the upstream default, not tuned for Turkish (see Languages).

Third-party, NOT the Pipecat CPU file (do not apply without checking the file):
- https://huggingface.co/soniqo/Smart-Turn-v3.2-ONNX describes a re-export of the **GPU** fp32 variant (`smart-turn-v3.2-gpu.onnx`, revision f766f81d) with the log-mel front end INSIDE the graph. Stated I/O: input `audio` float32 `[1, 128000]` (16 kHz mono, zeros at front), output `probability` float32 `[1, 1]`, opset 18. Different contract from `input_features`. Do not conflate.

Session options (CONFIRMED from both sources):
- Pipecat `LocalSmartTurnAnalyzerV3`: `execution_mode = ORT_SEQUENTIAL`, `inter_op_num_threads = 1`, `intra_op_num_threads = cpu_count` (default 1), `graph_optimization_level = ORT_ENABLE_ALL`, `ort.InferenceSession(path, sess_options=so)`. No `providers` argument (ORT default CPU EP).
- Reference `inference.py` `build_session`: same options but **no** `intra_op_num_threads` set. Follow the Pipecat analyzer for thread config.
- Brisvox forbids blocking work on tokio workers, so run inference on the inference pool.

## Languages

- README (https://github.com/pipecat-ai/smart-turn, fetched 2026-10-08) says "Support for 23 languages" and lists **Turkish** among them: Arabic, Bengali, Chinese, Danish, Dutch, German, English, Finnish, French, Hindi, Indonesian, Italian, Japanese, Korean, Marathi, Norwegian, Polish, Portuguese, Russian, Spanish, Turkish, Ukrainian, Vietnamese. (CONFIRMED as a vendor claim; not a tested result.)
- The HF card is tagged `multilingual` but does not list languages. Pipecat docs do not mention Turkish.
- **Turkish accuracy/evaluation numbers: not found (UNVERIFIED).**
- Third-party (not primary): arXiv 2609.05631 (https://arxiv.org/pdf/2609.05631) reports smart-turn-v3.2 zero-shot int8 on Tamil at 70.30%. Expect degradation outside the training distribution; measure Turkish on our own data.

## Accuracy and latency

### Accuracy (vendor or third-party claims)
- No absolute accuracy number on the official pages. README: GPU fp32 is "slightly improved accuracy by around 1%" over CPU int8.
- Third-party soniqo.audio: ~92.9–93.0% on 1,000 clips of the upstream test set. Not verified against a primary source (UNVERIFIED).

### Latency: claimed vs measured

**Status: measured (2026-10-08)** - see "In brisvox" below and bench/results/smart-turn.md. The vendor and third-party claims below are kept for comparison.

- Pipecat README (https://github.com/pipecat-ai/smart-turn): "Runs in as little as 10ms on some CPUs, and under 100ms on most cloud instances." "around 65ms on a standard 1x instance" on Pipecat Cloud (no CPU model; UNVERIFIED for v3.2).
- Daily v3 article (https://www.daily.co/blog/announcing-smart-turn-v3-with-cpu-inference-in-just-12ms/): **12.6 ms on AWS c7a.2xlarge**; c8g.2xlarge 15.2 ms; Modal 6-core 17.7 ms; t3.2xlarge 33.8 ms; c8g.medium 59.8 ms; t3.medium 94.8 ms. Session: `ORT_SEQUENTIAL`, `inter_op_num_threads=1`, `ORT_ENABLE_ALL`, `CPUExecutionProvider`. Intra-op threads and vCPU counts not stated.
- soniqo.audio guide (https://soniqo.audio/guides/turn), Apple M5 Pro, one 8 s window, not comparable to x86: ONNX fp32/int8 about 20 ms at 4 threads, about 36 ms at 2 threads. HF page (https://huggingface.co/soniqo/Smart-Turn-v3.2-ONNX) gives mean/p95 at 2 threads: fp32 36.3 / 51.4 ms, int8 37.3 / 50.6 ms. Which number belongs to which setup: UNVERIFIED.
- Neither source gives ORT version, language binding, run count, warm-up policy, or cold-start timings.

### Benchmark (run 2026-10-08)
Rust: `cargo test --release -p brisvox-inference --all-features --test smart_turn -- --nocapture
--test-threads=1 features_and_model_latency`; Python reference: `scripts/models/bench_smart_turn.py`.
Results, hardware and settings: bench/results/smart-turn.md. Still to do on the target container
(Linux, CPU limits): see `docs/LATENCY.md`.

## Golden fixtures: recorded
`scripts/models/make_smart_turn_goldens.py` (onnxruntime 1.28.0, Pipecat extractor at commit
3b2dccd8) writes fixtures/smart_turn_golden.json: 12 cases (3 fixtures x 2 segments x
complete / cut at 55%) with probability (7 decimals) and the FNV-1a 64 hash of each case's
features, plus the full feature matrix of the first case (fixtures/smart_turn_features.f32).
Tolerances (measured, then enforced): features bit-identical on all 12 cases; probability
< 1e-6 (rounding of the goldens). Only Pipecat's vendored extractor was used; transformers'
`WhisperFeatureExtractor` was not compared.

## Cost

- Model: free, BSD-2-Clause (declared, see License), self-hosted. No per-call API fee. No Pipecat Cloud pricing was fetched (UNVERIFIED).
- Compute: CPU inference only; ~8.7 MB model. Per-turn CPU time on our target container: UNVERIFIED (see benchmark plan).
- GPU path (`onnxruntime-gpu`, 32 MB fp32) is optional per Pipecat docs; not needed for a CPU-first design.

## Open items for Brisvox
1. Confirm the Pipecat wheel's packaged copy matches the HF file byte for byte (UNVERIFIED).
2. Measure Turkish accuracy on our own recorded utterances; the 0.5 threshold is untuned.
3. Latency on the target container (Linux, CPU limits), cold and warm.
4. License: if the ONNX file is ever bundled, ship the Daily BSD-2 and OpenAI MIT notices
   (record the decision). Until then it is fetched at verify time and not redistributed.
5. Version pin: compare against Pipecat >= v0.0.102 behaviour (Smart Turn default, VAD
   stop_secs 0.2). The 0.0.85 bundling claim stays UNVERIFIED.
6. Pure-Rust backend: investigated (2026-10-13) and not adopted; see "Pure-Rust backend" below.

## Confirmed locally (2026-10-08)
- Downloaded `smart-turn-v3.2-cpu.onnx`: **8679182 bytes, SHA-256
  `2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f`** (matches the HF LFS oid);
  fetched by scripts/models/fetch_models.py.
- Graph (onnxruntime 1.30.0): input **`input_features` float [batch, 80, 800]**, output
  **`logits` float [batch, 1]** - despite the name, already a sigmoid probability (values 0..1;
  Pipecat compares it to 0.5).
- Pipecat's `_predict_endpoint` (read at commit 3b2dccd8): resample to 16 kHz, keep the LAST 8 s
  or zero-pad at the START to 128000 samples, `compute_whisper_log_mel_features(do_normalize=True)`
  (vendored numpy: waveform mean/variance normalization (eps 1e-7) in f32, reflect pad 200,
  periodic Hann 400, hop 160, |rFFT|^2, Slaney mel 80 x 201 (0-8 kHz, area-normalized),
  max(1e-10), log10, drop last frame, max(x, max-8), (x+4)/4 -> (80, 800) f32).
- Goldens (scripts/models/make_smart_turn_goldens.py -> fixtures/smart_turn_golden.json and the
  full feature matrix of the first case in fixtures/smart_turn_features.f32; onnxruntime 1.28.0):
  complete utterances 0.91-0.99; utterances cut at 55% from 0.010 to 0.99 (cuts at a natural
  pause score high).

## In brisvox (measured 2026-10-08)
- `brisvox_inference::mel::WhisperLogMel`: Rust port of Pipecat's extractor, **bit-identical**
  features on all 12 golden cases (FNV-1a hashes in the goldens; normalization reproduces
  numpy's float32 pairwise sums and f32 division; Hann and |z|^2 use numpy's formulas; log-mel
  kept in f64 until the final cast). Bit-exactness is measured on Windows x86-64; another libm
  (log10/cos/hypot) could differ in the last ulp, which the hash test would report.
- `brisvox_inference::smart_turn::{SmartTurn, TurnModel}`: per-session PCM16 ring of the last 8 s
  (256 KB) plus a snapshot buffer (256 KB). `analyze()` copies the ring (~2 us, the only work on
  the caller's thread) and runs window, features and model on the inference pool, with scratch
  buffers owned by the pool thread (~3.4 MB per thread that has run one); it returns
  probability, verdict (> 0.5) and pool wait/run. Backend
  `smart_turn::ort_backend::SmartTurnOrt` (feature `smart-turn-ort`): a stack of idle ONNX
  Runtime sessions (1 intra-op thread each); `warm(n)` at startup avoids the ~100 ms cold start,
  `cold_starts()` counts misses. Probabilities within 6e-8 of the reference (golden rounding).
- **Sensitivity (measured):** the int8 model moves by up to 0.13 for +-1e-6 uniform noise on the
  features, and by up to 0.16 between onnxruntime 1.28.0 and 1.30.0 on identical features (e.g.
  tr_two_utterances/0/cut 0.936 vs 0.779; en_tr_mixed/0/cut 0.136 vs 0.177). Its uint8
  activation quantization dominates the output: the same graph with every
  QuantizeLinear/DequantizeLinear removed (int8 weights dequantized to fp32, activations fp32)
  gives 0.932 vs 0.936 on the first case but 0.019 vs 0.940 on en_tr_mixed/0/complete and 0.133
  vs 0.936 on tr_two_utterances/0/cut (onnxruntime on both), so the int8 file is effectively a
  different model from its float version. Pipecat's verdicts
  therefore depend on the installed onnxruntime/numpy; brisvox pins both sides: goldens are made
  with onnxruntime 1.28.0, the version `ort` 2.0.0-rc.13 bundles (make_smart_turn_goldens.py
  refuses other versions).
- **tract** (0.23.8; loads the graph only with its declared value shapes ignored, symbolic
  batch `s6`): its declutter pass moves the int8 output far (0.914 -> 0.201 on the first case;
  default plan up to 0.83 off the reference, ~75 ms). Stepping the pass (Optimizer::stopping_at)
  shows why: an exact-in-math rewrite (GELU's `x / sqrt(2)` -> `x * (1/sqrt(2))`, a last-bit
  change) moves it 0.914 -> 0.940, and each of the 4 attention Softmax nodes on uint8 input
  becomes a u8 lookup table (lossy), moving it 0.940 -> 0.366 -> 0.264 -> 0.356 -> 0.201. Typed
  plan without declutter: all 12 verdicts equal to onnxruntime but up to 0.18 off (e.g. 0.661 vs
  0.829, 0.313 vs 0.136; requantization rounding differs) and ~100 ms.
  On the QDQ-free fp32 copy tract matches onnxruntime on that copy on all 12 cases, and
  node-by-node on all 214 nodes (rel 1e-3), optimized or not. Not used.
- Latency (AMD Ryzen 9 9950X, Windows 11, release, one thread, median of 15): features 1.0-1.2
  ms (Python reference 2.1 ms), model 22.5-28 ms (Python + onnxruntime 1.28.0: 24.2 ms, the same
  runtime), 25-35 ms per analysis on the pool, pool wait ~10-20 us. bench/results/smart-turn.md.
  Tests: `crates/brisvox-inference/tests/smart_turn.rs`.

## Fused GELU (measured 2026-10-23)

brisvox runs the pinned int8 file with one change made in memory when a session is created
(`crates/brisvox-inference/src/smart_turn/fuse.rs`): each of the 8 exact GELUs, exported as five
element-wise passes over a `[400, 1536]` float tensor (`x / √2`, `Erf`, `+ 1`, `· 0.5`, `· x`), becomes
one `com.microsoft.Gelu` node, which ONNX Runtime then fuses with the preceding bias add. The file on
disk and its pin are unchanged; `BRISVOX_SMART_TURN_GRAPH=original` runs the file exactly as published.

Why: an analysis is bound by memory traffic, not arithmetic. ONNX Runtime's profile counts about
330 MB of tensor output per analysis for the published graph and about 200 MB for the fused one; with
4 analyses running at once on a desktop with one memory channel, a memory-copy probe on another core
dropped from 38.7 GB/s (idle) to about 10 GB/s, so concurrent analyses wait on memory, not on the cores
(docs/LATENCY.md "Capacity with the real provider path").

Quality: the official test set `pipecat-ai/smart-turn-data-v3.2-test` (Hugging Face, CC-BY-4.0,
31,527 labelled clips); 4000 clips drawn evenly from two of its ten files, 23 languages; features from
Pipecat's pinned extractor on the last 8 s; ONNX Runtime 1.28.0, verdict `p > 0.5`:

| graph | accuracy against the labels | same verdict as the published graph | mean / max \|Δp\| |
|---|---|---|---|
| published int8 (`smart-turn-v3.2-cpu.onnx`) | 92.67% | 100% | - |
| **fused int8 (brisvox's default)** | **92.70%** | **99.92%** (3 clips differ) | 0.0023 / 0.74 |
| published fp32 (`smart-turn-v3.2-gpu.onnx`) | 93.62% | 96.0% (160 differ) | 0.042 / 0.92 |

The fused graph's three changed verdicts: one Vietnamese clip corrected, and in Bengali one corrected and one
broken against the labels; per language the accuracy was equal (Bengali net) or better (Vietnamese). The model is sensitive near a quantization step: the published graph already
moves by up to 0.096 between CPUs (see "Platform dependence"), and the fused graph moves the same golden case
by the same amount, with the same verdict on all 12 goldens (`tests/smart_turn.rs`, `FUSED_TOLERANCE`).
The Rust rewrite and the Python rewrite used for the 4000-clip run give bit-identical probabilities.

On a CPU without VNNI (ONNX Runtime then takes its AVX2 int8 kernels; emulated with qemu-user as a Haswell,
300 other clips of the same files, 23 languages) the fused graph keeps 299 of 300 verdicts: 92.33% against
92.67%. The changed clip is English and near the line (0.644 published, 0.451 fused). That is within how much
the published graph itself moves between CPUs: on the 192 clips both runs share, the published graph changes
3 verdicts between a VNNI and a non-VNNI CPU, and the two graphs score the same on the non-VNNI one (177 of
192). Operators who want the published graph's numbers exactly set `BRISVOX_SMART_TURN_GRAPH=original`.
The fp32 file is about 1 point more accurate overall but changes 4% of the verdicts and is worse in some
languages (Bengali -4.2 points, Russian and Portuguese slightly), costs 4 times the weight memory per session
and about 9 ms more per analysis on one core, so it is not the default.

Decision delay (`brisvox-bench model-cpu --configs 1:1:original,1:1:fused,2:1:original,2:1:fused`, the
reference desktop on Windows, configurations interleaved, `bench/results/model-cpu-fused-windows-2026-10-23.md`):
one analysis at a time takes 27.1 ms against 27.8 ms with one intra-op thread and 15.1 ms against 15.6 ms with
two (the server's default), at about 3% less CPU; the verdicts of the 12 goldens are unchanged. The gain grows
under concurrent load, where the memory traffic is the limit:

Speed (ONNX Runtime 1.28.0, one intra-op thread per analysis, 16-core desktop, single-channel DDR5; a busy
host, so the trend matters more than the digits): 4 analyses at once 97 -> 107 per second, 8 at once 96 -> 125
per second; one at a time unchanged within noise.

Capacity (`bench/results/capacity-smart-turn-graph-2026-10-23.md`: 4 CPUs / 4 GiB, real models, the real
provider clients against the fake providers, six interleaved pairs on the same desktop): the fused graph
held more calls within the SLO in 5 of 6 pairs; on a quiet host 290 and 320 against 270 and 270 (about
13% more). Below that knee the CPU per call is the same: the gain is where concurrent analyses compete for
memory bandwidth. The spread between runs on that host is about 20 calls.

## Platform dependence (measured 2026-10-15)
The same file, the same features (bit for bit) and the same ONNX Runtime version (1.28.0) give
different probabilities per operating system and CPU instruction set. 12 golden cases, `ort`
2.0.0-rc.13 in the repository's tests, on one AMD Ryzen 9 9950X (AVX-512 with VNNI), difference to
the goldens (which Python onnxruntime 1.28.0 reproduces to 1e-7 on both Windows and Linux on this
CPU):

| runtime | cases exact (< 1e-6) | off | verdict flips |
|---|---|---|---|
| `ort` on Windows x86_64 | 12 | none | 0 |
| `ort` on Linux x86_64, AVX-512 + VNNI (WSL2, and the GitHub `ubuntu-latest` runner for the one case its log shows) | 10 | `en_tr_mixed/0/complete` 0.9333 for 0.9403 (0.007), `tr_two_utterances/0/cut` 0.8395 for 0.9357 (0.096) | 0 |
| `ort` on Linux x86_64 restricted to AVX2, no VNNI (qemu-user TCG, CPU models Haswell and EPYC; emulated, not measured on silicon) | 0 | 4e-4 to 0.66, e.g. `tr_two_utterances/0/cut` 0.272 for 0.936, `en_tr_mixed/0/cut` 0.616 for 0.136 | 2 of 12 |

Why: the model's uint8 activation quantization turns any last-bit float difference into a whole
quantization step (above), and the int8 matrix kernels differ by instruction set (the u8s8 AVX2
kernel saturates its 16-bit intermediate sums, the VNNI one does not; ONNX Runtime documents this
for quantized models) and the Linux `ort` binary's float kernels are not bit-equal to the Windows
one's. Consequences:
- Tests: features and everything that compares two paths on one machine stay bit-exact on every
  machine (the executor equals a direct call, thread counts and spinning change no bit, a bare ONNX
  Runtime session equals `SmartTurnOrt`). The comparison with the goldens runs only where their kernels
  run (`brisvox_sim::platform::smart_turn_goldens_apply_here`: Windows x86_64 with VNNI);
  elsewhere the tests print the deviation. A tolerance would have to be 0.7 to hold for the
  AVX2 row and would then protect nothing.
- Product: a CPU without VNNI (AVX2-only server parts, such as AMD EPYC Zen 2/3 and older Intel
  Xeons) can decide turns differently from the reference on this model. Not verified on real
  hardware of that kind; measuring it on such hardware is an open item. Pipecat
  runs the same file through the same runtime and has the same property.

## Pure-Rust backend (measured 2026-10-13): not adopted
Question: can a pure-Rust runtime reproduce the ORT backend within 1e-3 with identical verdicts, at
least as fast? Answer: no, and the 1e-3 bar is below what different correct implementations of
this graph agree on. ORT stays the backend (the earlier decision to use ORT stands).

**The model is chaotic in the last bit, so there is no implementation-independent answer.**
`scripts/models/smart_turn_runtime_variance.py` (onnxruntime 1.28.0, the 12 goldens, base =
ORT_ENABLE_ALL, 1 thread) compares execution plans of the SAME file. The ORT and numpy rows come
from that script (needs onnxruntime 1.28.0; not re-run in the review-fix session, whose Python had no
onnxruntime); the two tract rows were re-measured on 2026-10-13 by the Rust harness described under
(a):

| plan | max abs difference to the goldens | cases > 1e-3 | verdict flips |
|---|---|---|---|
| ORT_ENABLE_ALL, 4 intra-op threads | 0.0000 | 0/12 | 0 |
| ORT_ENABLE_EXTENDED (QDQ fused to integer QLinear kernels) | 0.0000 | 0/12 | 0 |
| ORT_ENABLE_BASIC / ORT_DISABLE_ALL (Q/DQ kept as float nodes) | 0.1307 | 8/12 | 0 |
| ONNX reference evaluator (numpy, literal semantics; opset raised to 21) | 0.0593 | 8/12 | 0 |
| tract 0.23.8, typed plan, no declutter (`smart_turn_tract` example) | 0.1769 | 9/12 | 0 |
| tract 0.23.8, default `into_optimized` (declutter incl. the Softmax lookup-table fusion, below) | 0.8332 | 12/12 | 10 |

The unfused ORT plan, numpy and tract all execute the graph exactly as the ONNX text says and still
differ from the fused ORT plan, and from each other (tract typed vs ORT BASIC 0.136, vs numpy
0.21; measured in the tract investigation, re-run with `BRISVOX_TRACT_JSON`), by 0.06-0.18: every uint8 activation quantization step turns a last-bit float
difference (summation order of a matmul, erf/exp polynomial, LayerNorm reduction) into a whole
quantization step when a value sits on a rounding boundary, and 12 encoder layers amplify it
(sensitivity measured earlier: +-1e-6 on the features moves the output by up to 0.13). Verdicts
stayed equal in every plan except tract's default, which is a property of these 12 cases, not a
guarantee. To reach 1e-3 a runtime would have to reproduce ORT's graph transformer decisions
(which Q/DQ pairs become QLinear kernels) AND the MLAS float kernels bit for bit, for the CPU
ISA in use (option (b) of the tract investigation); that is not a reasonable engineering target and would break
on every `ort` upgrade (1.28.0 vs 1.30.0 already differ by up to 0.16).

**Options examined**
- (a) tract: 0.23.8 is the newest tract-onnx/tract-core (https://crates.io/crates/tract-onnx,
  published 2026-09-21, checked 2026-10-13; the 0.21.18 and 0.22.4 maintenance releases are from
  2026-09-07, nothing newer). Harness: `crates/brisvox-inference/examples/smart_turn_tract.rs`
  (`cargo run --release -p brisvox-inference --features silero-tract --example smart_turn_tract
  [-- --json tract.json]`, needs the model in `models/`); it runs the 12 goldens through the
  crate's bit-exact feature extractor in two plans, `typed` (`into_typed()` only: no declutter, no
  optimizer) and `optimized` (`into_optimized()`, tract's default), and prints max |d| to the
  goldens, cases > 1e-3, verdict flips and median latency. `BRISVOX_TRACT_JSON=tract.json` makes
  the variance script add the tract-vs-ORT-BASIC and tract-vs-numpy rows. Measured numbers:
  the table above, the bisect and the latency below.
  `smart_turn_tract -- --bisect` runs tract's declutter `Optimizer` one patch at a time
  (`Optimizer::declutter().stopping_at(n)`) on the first golden case and prints every step that
  changes the operator mix or the probability (2026-10-13, 234 steps; case `en_tr_mixed/0/complete`:
  golden 0.9403, typed plan 0.8858, fully optimized 0.2492):

  | step | probability | operator change |
  |---|---|---|
  | 1-50 | 0.8858 (unchanged) | constant folding, AddAxis/Cast clean-up |
  | 51 | 0.9466 (+0.061) | one `Div` becomes a `Mul` (GELU `x / sqrt(2)` -> `x * (1/sqrt(2))`) |
  | 57 | 0.3747 (-0.572) | `DequantizeLinear -> Softmax -> QuantizeLinear` becomes a `LookupTable` |
  | 70 | 0.2798 (-0.095) | the second attention Softmax, same fusion |
  | 83 | 0.4803 (+0.201) | the third |
  | 96 | 0.2492 (-0.231) | the fourth; nothing after step 96 changes the answer (234 = end) |

  So the two suspects of the earlier notes are confirmed pass by pass: the GELU rewrite is a
  last-bit change that already moves the output by 0.06, and the four attention Softmax fusions
  move it by 0.57 / 0.09 / 0.20 / 0.23, i.e. they, not the GELU rewrite, produce the 0.83 error.
  Source reading (tract-core 0.23.8) explains the fusion and points at a probable tract bug:
  `DequantizeLinearF32::declutter` (`src/ops/quant.rs`) walks the nodes between a dequantize and a
  quantize and, when `Op::quantize` has no answer for them, evaluates the whole chain on the 256
  possible inputs and stores the result as a `LookupTable`; it continues through a node when
  `axes_mapping(..).is_element_wise_unary()` is true. `Softmax::axes_mapping`
  (`src/ops/nn/softmax/mod.rs`) returns `AxesMapping::natural`, for which `is_element_wise_unary()`
  (`src/axes/mapping.rs`) is true, although Softmax normalizes across its axis and is not
  pointwise. The table is then softmax over the vector 0..=255, applied per element, which is not
  the quantized softmax. That is consistent with the step-57 jump, but it is a reading of the
  source, not a minimal repro: no upstream issue is filed and disabling just that rule was not
  tried (the optimizer does not expose single rules; `Optimizer.passes` could be filtered in a
  follow-up). It does not change the decision: with the whole declutter off (typed plan) tract is
  still up to 0.18 off the goldens (9/12 cases above 1e-3), so fixing this bug would not give a
  1e-3 match, and the typed plan is slower than ORT. Latency (release, this machine, tract
  single-threaded, median of 12 cases, measured while other builds ran): typed 104 ms, optimized
  72 ms (wrong answers), ORT 25-35 ms, so the "at least as fast" bar fails independently; why tract
  is 3-4x slower was not profiled.
- (b) emulating ORT's QLinear kernels: infeasible, see above (needs the MLAS float kernels too).
- (c) other Rust runtimes (checked 2026-10-13):
  - candle-onnx 0.11.0 (https://crates.io/crates/candle-onnx, published 2026-06-26): the op match
    in https://github.com/huggingface/candle/blob/main/candle-onnx/src/eval.rs (116658 bytes, read
    in full with curl and grepped) has no `QuantizeLinear`, `DequantizeLinear` or
    `LayerNormalization` arm, so it cannot run this file.
  - wonnx 0.5.1 (https://crates.io/crates/wonnx, published 2023-09-30): GPU-only (wgpu) and
    unmaintained since.
  - ort-tract 0.5.0+0.23 (https://crates.io/crates/ort-tract): the `ort` API on top of tract, so the
    same engine and the same result as (a).
  - burn-onnx 0.22.0 (https://crates.io/crates/burn-onnx, published 2026-10-06; `burn-import` is its
    deprecated predecessor): its op table
    (https://github.com/tracel-ai/burn-onnx/blob/main/SUPPORTED-ONNX-OPS.md) lists DequantizeLinear,
    QuantizeLinear, LayerNormalization, Gelu, Erf, MatMulInteger and QLinearMatMul as supported,
    so unlike candle it is not ruled out by op coverage. It was NOT run: it generates Rust from the
    ONNX file at build time and needs the burn crates (not added: no new dependencies in this
    task). It is another implementation of the float and rounding steps, so the variance argument
    above applies to it, but that is an inference, not a measurement.
- The QDQ-free fp32 copy of the model stays excluded (measured earlier: up to 0.92 from int8).

**What would change this:** a Smart Turn release that is robust to float noise (e.g. a float
variant that tract matches node by node, as it does the dequantized copy), or a tolerance defined by verdict
agreement on a labelled set instead of probability equality. Either is a new decision, not a
follow-up of this investigation.
