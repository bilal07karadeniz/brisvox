# Noise suppression on the caller's audio — model survey

Researched 2026-10-30 from the sources listed at the end (fetched that day). Nothing here comes from memory.
Marks: **(measured)** = a number a third party or the authors measured and published; **(claimed)** =
published by the vendor without method or hardware; **(estimate)** = derived by us from published numbers
(the derivation is shown); **UNVERIFIED** = not confirmed from a fetched source.

Scope: opt-in denoiser on the caller's PCM16 (8 kHz mu-law-decoded or 16 kHz, 20 ms frames) before VAD and
STT. CPU per stream decides; commercial OSS redistribution of code AND weights is required.

## Summary and recommendation

- **Recommended: FastEnhancer (Tiny, DNS-trained weights; Base as the quality tier)** — MIT repo, ICASSP 2026,
  pushed 2026-10-12, ONNX releases with explicit streaming caches. Lowest published single-thread RTF of any
  candidate with PESQ ~3 on VoiceBank-DEMAND: Tiny RTF 0.012 on a Xeon thread (22 K params), i.e. ~1.2 % of one
  core per call. 16 kHz native. A Rust crate (`decibri`) already runs FastEnhancer-T through `ort`.
  Caveat: the weights' license is not stated separately from the MIT repo (UNVERIFIED), tract compatibility is
  UNVERIFIED, and the quality evidence is on VoiceBank-DEMAND / DNS1, not on telephone audio.
- **Runner-up: RNNoise via `nnnoiseless` 0.5.2** (BSD-3-Clause, pure Rust, no ONNX, weights embedded). Cheapest
  (RTF 0.004-0.007 for the C version) but 48 kHz native (always resample up and back), and the weakest quality
  (PESQ 2.33 vs noisy 1.97, STOI no gain) and the worst measured effect on Whisper WER.
  Heavier quality tier if wanted: **DPDFNet** (Apache-2.0 code and HF weights, native 8 kHz models) but
  1.3-4.4 GMACs/s, ~15x the CPU of FastEnhancer-T by our estimate.
- **Do not make "denoised audio to STT" the default.** Every independent study found (fetched, see below)
  says neural denoisers often raise STT WER; denoising helps VAD/turn-taking. Design the option as
  "denoised -> VAD/turn detection only, raw -> STT" with a separate switch for denoised -> STT, and
  measure WER on our own Turkish and English fixtures per model before promoting any model.
- Rejected on license: NVIDIA RE-USE (NSCLv1, non-commercial), DPDFNet *paper* (CC BY-NC-ND; the code and
  weights are Apache-2.0, so only the PDF is NC). Rejected on CPU: FRCRN/MossFormer class (ClearerVoice-Studio,
  Apache-2.0 but 10 M+ params, 12 GMACs and up), NSNet2 (6.2 M params, 0.43 GMACs, worse than DFN2).

## Candidates (newest first)

| Model | Year | Code license | Weights license | Native rate | Alg. latency | Params | Compute |
|---|---|---|---|---|---|---|---|
| FastEnhancer T/B/S/M/L | 2025-26 (ICASSP 2026) | MIT | repo MIT; no separate weights statement (UNVERIFIED) | 16 kHz (also 48 kHz set) | FFT 512, hop 256 (T,B,S) = 16 ms hop; lookahead not stated (UNVERIFIED) | 22 K / 91 K / 194 K / 492 K / 1.1 M | 60 M / 262 M / 664 M / 2.9 G / 11-12 G MACs (per second assumed, the paper does not define the unit) |
| DPDFNet (Ceva) | 2025-12 | Apache-2.0 (repo) | Apache-2.0 (HF model card) | 8 / 16 / 48 kHz variants | ~20 ms first output, then ~10 ms per block | 2.3-3.6 M | 0.36 G (baseline) ... 4.37 G (16k), 1.29 / 3.99 G (8k), 2.42 / 7.17 G (48k) |
| Hush (weya-ai, DFN3 derivative) | 2026 | Apache-2.0 | Apache-2.0 | 16 kHz only | ~20 ms, zero lookahead (320 window / 160 hop) | ~1.8 M (8 MB) | not published (UNVERIFIED) |
| UL-UNAS | 2025-26 (TASLP) | MIT | repo MIT (checkpoint released 2026-02-01) | 16 kHz (512 FFT, 32 ms window) | 0 ms lookahead | 171 K | 35 M MACs/s |
| CoFi-Lite, HALO, LiSenNet-redesign, TSDCA-BA | 2026 | no code linked | - | - | - | 83 K / n.a. / 37 K / 112 K | 12.9 M / n.a. / n.a. / 0.22 M (claimed) |
| GTCRN | 2024 (ICASSP) | MIT | repo MIT | 16 kHz | 0 ms lookahead, 32 ms window | 48.2 K (23.7 K in paper, ERB counted differently) | 33.0 M MACs/s (39.6 M in paper) |
| RNNoise 0.2 | 2024-04-15 | BSD-3-Clause | BSD-3-Clause, trained on public datasets only | 48 kHz | 10 ms frame (demo page) | 0.06 M, 85 kB int8 | 0.04 G MACs/s |
| nnnoiseless 0.5.2 | crate 2025-12-18 | BSD-3-Clause | embedded RNNoise weights (which RNNoise version: UNVERIFIED) | 48 kHz | 10 ms frame | same as RNNoise | same |
| DeepFilterNet2 / 3 | 2022 / 2023 | MIT OR Apache-2.0 | no separate statement; models ship in the dual-licensed repo (UNVERIFIED) | 48 kHz | 40 ms (20 ms window, 2-frame lookahead); `_ll` models exist | 2.31 M / 2.14 M | 0.356 G / 0.35 G MACs/s |
| NSNet2 (Microsoft) | 2020-21 | MIT (DNS repo code) | UNVERIFIED | 16 kHz | n.a. | 6.17 M | 0.43 G |
| NVIDIA RE-USE | 2026-03 | NSCLv1 non-commercial | same | 8-48 kHz | n.a. | 9.6 M | GPU class |

Details and evidence per candidate follow. Where a number conflicts between sources both are given.

### FastEnhancer (ICASSP 2026, SNU) - https://github.com/aask1357/fastenhancer, arXiv 2509.21867
- License MIT (GitHub API, SPDX `MIT`), not archived, last push 2026-10-12, 162 stars.
- Design: RNNFormer (time GRU + frequency attention), time-axis kernel size 1 (no frame buffering), batch norm
  (fusable). Config table: Tiny H=256 L=2 K=2; Base H=256 K=3; Small H=256 L=3; Medium H=160; Large H=100.
  STFT FFT size 512 (1024 for the 48 kHz models).
- Params/MACs (README, 16 kHz): T 22 K/60 M, B 91 K/262 M, S 194 K/664 M, M 492 K/2.9 G, L 1105 K/12 G
  (paper Table 2: T 55 M, B 262 M, S 664 M, M 2.9 G, L 11 G).
- RTF single thread, streaming (measured, paper Table 2): T 0.012 Xeon / 0.013 M1; B 0.022 / 0.026;
  S 0.034 / 0.048; M 0.101 / 0.173; L 0.313 / 0.632. README, Apple M5 single thread: T 0.0058, B 0.0110,
  S 0.0189, M 0.0386, L 0.1052. Which runtime produced them (PyTorch vs ONNX Runtime) is not stated in what was
  read (UNVERIFIED). The Xeon model is not named.
- Quality, VoiceBank-DEMAND (paper Table 2): T PESQ 2.99 / STOI 0.940 / DNSMOS P.808 3.42 / WER 3.6 %;
  B 3.13 / 0.945 / 3.47 / 3.2 %; GTCRN 2.87 / 0.940 / 3.43 / 3.6 %. WER by Whisper-large-v3-turbo. The
  noisy-input WER row is NOT in the table text we got, so "does it help ASR" is not answered by this paper.
  The README adds DNS-Challenge 1 (16 kHz): Large PESQ 2.94, STOI 0.971 (single seed).
- Training data: the paper trains on VoiceBank-DEMAND (small: DEMAND noise only). Separate DNS-trained
  checkpoints and ONNX files exist (`ckpt-dns-v1.0.0`, `onnx-dns-v1.0.0`: t 125 KB, s 833 KB, m 2.03 MB,
  b/l also listed). Use the DNS ones for real calls. A 48 kHz set (`onnx-48khz-v1`, 2026-02-25, conv-STFT
  variants 2026-03-11) uses dynamic low-pass during training so band-limited input is handled (README).
- ONNX: releases ship `fastenhancer_<size>.onnx` (waveform in/out) and `.spec.onnx` (spectrogram in/out);
  streaming with ONNX Runtime documented in the repo. `decibri` 5.4.0 (Rust) wraps FastEnhancer-T through
  `ort`: 512-sample window, 256-sample hop, three per-channel streaming caches, 256 samples added latency,
  model loaded from a path (the crate embeds no model bytes). Its license was not visible (UNVERIFIED).
- tract compatibility: UNVERIFIED (not tested). ONNX Runtime is the documented path.

### DPDFNet (Ceva) - https://github.com/ceva-ip/DPDFNet, https://huggingface.co/Ceva-IP/DPDFNet, arXiv 2512.16420
- Code: Apache-2.0 (GitHub API `Apache-2.0`, last push 2026-10-20). HF model card license: Apache-2.0. The arXiv
  abstract page shows CC BY-NC-ND 4.0, which is the paper's license only (conflict noted; the code and weights
  statements are what matter, but get a legal read before shipping weights).
- DFN2 + dual-path RNN blocks in the encoder; over-attenuation loss; fine-tuning for always-on use. Trained on DNS4
  (16 kHz), 4000 h of English MLS read speech, noise from MUSAN and FSD50K.
- Variants (sherpa-onnx and GitHub tables), params M / MACs G per second: 8 kHz `dpdfnet2_8khz` 2.51/1.29,
  `dpdfnet8_8khz` 3.56/3.99; 16 kHz `baseline` 2.31/0.36, `dpdfnet2` 2.49/1.35, `dpdfnet4` 2.84/2.36,
  `dpdfnet8` 3.54/4.37; 48 kHz `dpdfnet2_48khz_hr` 2.58/2.42, `dpdfnet8_48khz_hr` 3.63/7.17.
  ONNX files: https://github.com/k2-fsa/sherpa-onnx/releases/download/speech-enhancement-models/ (sizes not listed).
- Only candidate with **native 8 kHz** models, but 1.29 G MACs/s at the small size. No CPU RTF is published
  (the only RTF in the paper is the Ceva NPU: DPDFNet-4 0.97 on NPN32).
- Quality on the paper's own 12-language low-SNR set (324 clips, 0/5/10 dB): noisy PESQ 1.27 / STOI 83.2;
  GTCRN 2.00/87.2; DFN2 2.35/91.3; DFN3 2.45/90.6; DPDFNet-2 2.79/92.6; -4 2.82/93.0; -8 2.85/93.4. No WER.
- Streaming: `StreamEnhancer` keeps RNN state, any chunk size, resamples internally, one instance per stream.
- sherpa-onnx lists DPDFNet; its streaming denoiser exists in the core C API (Python binding was missing at the time).

### Hush (weya-ai) and `hush-vani` - https://huggingface.co/weya-ai/hush, https://crates.io/crates/hush-vani
- Model card: "first open-source speech enhancement model built for Voice AI", DFN3 derivative, Apache-2.0,
  16 kHz only, ~1.8 M params / 8 MB, 320-sample frame / 160-sample hop, "under 1 ms of processing per 10 ms of
  audio" on CPU (claimed, hardware unspecified), ~20 ms algorithmic latency, zero lookahead. Trained on >10 000 h:
  speech LibriSpeech/VCTK/Common Voice, noise DNS/FreeSound/ESC-50/AudioSet, RIRs MIT IR Survey/OpenAIR/BUT
  ReverbDB; ~60 % of samples carry a competing speaker at 12-24 dB SIR. It also suppresses background speakers.
  The card publishes **no** DNSMOS, PESQ or WER. A retrained model for louder noise is announced "coming soon".
  Risk for us: background-speaker suppression can also eat a quiet or distant caller; unmeasured.
- `hush-vani` 0.1.2 (crates.io, 2026-10-29; 0.1.0/0.1.1 on 2026-07-14): Apache-2.0, "pure Rust", runtime-dispatched
  AVX2, no ORT/BLAS/Python, 2.44 MB embedded int8 weights dequantised to f32 at load (the NOTICE file says f16,
  4.56 MB - the two statements differ), mono 16 kHz f32. Claims ~100x real time on one core (RTF ~0.01, hardware
  unspecified, "drifts ~20 %"), 1.07-1.12x faster than ORT single thread. A one-person project (13 commits, 0
  stars, 26 downloads of the latest version). Treat as a reference or a vendoring candidate, not a dependency.

### UL-UNAS / GTCRN / newer ultra-light papers
- GTCRN (https://github.com/Xiaobin-Rong/gtcrn, MIT, last push 2026-08-03, 763 stars): 48.2 K params, 33.0 MMACs/s
  (paper: 23.7 K, 39.6 M). Streaming version in `stream/`. RTF 0.07 on an i5-12400 (README, measured, runtime not
  stated). Quality: VCTK-DEMAND PESQ 2.87; DNS3 DNSMOS P.808/BAK/SIG/OVRL 3.44/3.90/3.00/2.70 (README) or
  OVRL 2.63 / P.808 3.50 / PESQ 2.10 in the UL-UNAS test setup. ONNX: `gtcrn_simple.onnx` (523 KB) via sherpa-onnx,
  16 kHz, RTF 0.071 offline, 1 thread, hardware not stated. Rust: `sherpa-onnx` crate 2.0.1 (Apache-2.0, updated
  2026-10-27) has GTCRN offline and streaming examples. tract compatibility UNVERIFIED. The README also tells
  that explicit feature rearrangement was removed to make it streamable.
- UL-UNAS (TASLP; https://github.com/Xiaobin-Rong/UL-UNAS, MIT; arXiv 2503.00340): 171 K params, 35 M MACs/s,
  0 ms lookahead; DNS3 PESQ 2.30 vs GTCRN 2.10, DNSMOS OVRL 2.71 vs 2.63; VCTK PESQ 2.96 (3.09 with PESQ loss). No RTF,
  no ASR eval in the paper. Streaming ONNX code added 2026-02-13 in `ulunas_onnx`. Best "GTCRN successor" at equal MACs.
- CoFi-Lite (arXiv 2607.10142): 83.12 K params, 12.87 M MACs/s (~40 % of GTCRN), beats GTCRN per abstract;
  no code link. HALO (2606.12328): frame-rate halving plug-in, no code, no numbers in the abstract.
  LiSenNet redesign for an NPU (2609.29866): 37 K params, no code. LiSenNet (ICASSP 2025, 2409.13285): 37 K, 56 M MACs,
  PESQ 3.07 on VCTK. None of these can be used today (no code or weights found).

### RNNoise and nnnoiseless
- RNNoise (https://github.com/xiph/rnnoise, BSD-3-Clause): release 0.2 on 2024-04-15 (training improvements, SSE4.1
  and AVX2 kernels, run-time CPU detection, models trained only on public datasets listed in `datasets.txt`);
  last commits 2025-02-01 and 2025-02-22 (build/doc housekeeping). No 2025-26 model update. A "little" sparser
  model exists. Model format changed since v0.1.1 (binary, loadable at run time). 48 kHz in and out.
  No DRED/new-model mention in the README. The Valin demo page: 3 GRU layers, 22 Bark bands, 42 features,
  85 kB int8 (340 kB f32), "~60x real time on x86 / 7x on Raspberry Pi 3" for the early unoptimised C.
- Quality (DFN2 paper Table 1, VoiceBank+Demand): RNNoise 0.06 M params, 0.04 G MACs, RTF 0.027 (their laptop),
  PESQ 2.33, STOI 0.922 (noisy: 1.97 / 0.921, i.e. no intelligibility gain). DNS4 blind DNSMOS SIG/BAK/OVL:
  RNNoise 3.884/3.694/3.378 vs noisy 4.144/2.94/3.291 (SIG drops: speech distortion).
- `nnnoiseless` 0.5.2 (https://crates.io/crates/nnnoiseless, BSD-3-Clause, repo jneem/nnnoiseless): pure Rust (two
  small `unsafe` f32->Complex casts), 12 versions, ~1.05 M downloads (343 K recent), no `rust_version` set. crates.io
  says 0.5.2 was published 2025-12-18; GitHub shows "Release 0.5.2 (#38)" on 2026-09-27 followed by dependency and
  cleanup commits to 2026-09-28 (the dates disagree; maintained either way). Custom weights through
  `RnnModel::from_bytes`. No published speed numbers (UNVERIFIED vs the C library). 48 kHz demo; for 16 k or 8 k
  input resample up and down (the project's `StreamResampler` costs 1-2 us per 20 ms frame, per
  `docs/providers/sherpa-onnx.md`; hold: that is our own earlier measurement).

### DeepFilterNet 2/3 - https://github.com/Rikorose/DeepFilterNet
- Code MIT OR Apache-2.0 (root `LICENSE`, `LICENSE-MIT`, `LICENSE-APACHE`; GitHub reports SPDX NOASSERTION because it
  is dual). The README gives no separate weights license; the ONNX/zip models live in `models/`
  (`DeepFilterNet3_onnx.tar.gz`, `DeepFilterNet3_ll_onnx.tar.gz`, `DeepFilterNet2_onnx*.tar.gz`). UNVERIFIED whether the
  weights carry extra terms (they were trained on DNS data).
- Last push **2024-10-17**; 67 open issues; not archived. Crate `deep_filter` on crates.io is stale: 0.2.5 from
  2022-07-28. `libDF` in the repo is 0.5.7-pre with a `tract` feature pinned to **tract ^0.21.4** (our workspace
  uses tract-onnx 0.23.8: two tract versions or a port). `deep-filter` binary: 48 kHz WAV only.
- DFN2 (paper): 2.306 M params, 0.356 G MACs/s, RTF 0.04 on an i5-8250U, 0.42 on a Raspberry Pi 4, 40 ms latency, PESQ
  3.03-3.08 on VoiceBank+Demand, DNSMOS DNS4 SIG/BAK/OVL 4.196/4.427/3.882. DFN3 (Interspeech 2023 show-and-tell):
  RTF 0.19 "single threaded notebook CPU" (abstract); 2.14 M params / 0.35 G MACs (DPDFNet Table I). DFN1 paper:
  5 ms minimum latency with N_FFT 240 and no lookahead.
- Third-party CPU measurement (blog, one pinned core of a Ryzen 7 9700X, 10 min of audio, deep-filter DFN3): 27.3 s =
  RTF ~0.046, not faster with more cores.

### Others examined
- NSNet2 (Microsoft): in the DFN2 table: 6.168 M params, 0.43 G MACs, RTF 0.022, PESQ 2.47, STOI 0.903 (below noisy).
  The `NSNet2-baseline` page returned 404, so its license is UNVERIFIED. Not competitive.
- NVIDIA RE-USE (https://huggingface.co/nvidia/RE-USE): 9.6 M params, 8-48 kHz, license "NVIDIA One-Way
  Noncommercial License (NSCLv1)", "research and development only". Excluded. NVIDIA Studio Voice (Open Model License)
  is a studio-quality enhancer, not a small real-time model (not verified in detail).
- ClearerVoice-Studio (Apache-2.0): FRCRN/MossFormer models, 16 and 48 kHz; sizes not on the page; FRCRN is 10.27 M params / 12.3 G MACs in the DFN2 table. Too heavy.
- Proprietary and out of scope: Krisp, ai-coustics, Picovoice Koala.

## Does denoising help or hurt STT? (evidence)

| Source | Finding |
|---|---|
| AssemblyAI blog (vendor, cites Krisp) | Krisp BVC -> VAD -> STT: false VAD triggers down 3.5x; but cleaned audio to Whisper v3 raised WER ~2x. Advises: send cleaned audio to VAD/turn-taking only, keep original audio for STT; NC adds 10-15 ms. |
| Deepgram guidance (vendor) | "Test without preprocessing first"; many A/B tests improved when noise suppression was removed; no data published. |
| arXiv 2512.17562 (medical ASR, MetricGAN+, 4 ASR systems, 500 recordings) | Noisy audio beat enhanced audio in all 40 configurations; absolute semantic WER rose 1.1-46.6 %. |
| arXiv 2603.04710 (SAM-Audio + Whisper) | Preprocessing raised WER and CER; worse for larger Whisper. (separation, not a small denoiser) |
| arXiv 2607.11157 | Whisper-large-v3 responds U-shaped to enhancement strength; wav2vec 2.0 benefits from strong suppression. Effect depends on the recognizer. |
| Blog benchmark, 824 VoiceBank-DEMAND clips, faster-whisper small.en, Ryzen 9700X | WER: clean 2.52 %, noisy 3.75 %, DFN3 (12 dB cap) 3.70 %, DFN3 4.18 %, RNNoise-OBS 5.18 %, RNNoise 0.2 5.68 %. No denoiser beat untouched audio meaningfully. Single setup, not peer reviewed. |
| FastEnhancer paper | WER of enhanced audio with Whisper-large-v3-turbo on VoiceBank-DEMAND: 2.8-3.6 %; noisy-input WER not available to us (UNVERIFIED). |

Reading: there is no evidence in what we fetched that a small denoiser lowers STT WER on cloud STT (Deepgram,
Whisper-class); the evidence that it helps VAD false triggers comes from a vendor. Telephone audio and Turkish
are untested by every source. The engine should (a) default off, (b) let the denoised stream feed the VAD and
turn detector only, (c) make "denoised audio to STT" a separate explicit option, (d) ship a WER regression
test on our Turkish and English fixtures mixed with noise at several SNRs and through the mu-law path.

## CPU per call (fraction of one modern x86 core, one thread, streaming)

Per-call CPU = RTF. For 100 simultaneous calls multiply by 100 (= cores). All of these exceed the ~50 us rule per
frame (Tiny: 0.012 x 16 ms = ~190 us per hop; RNNoise AVX2: ~42 us per 10 ms frame = ~84 us per 20 ms frame), so
they belong on the `brisvox-inference` pool.

| Model | Per-call CPU | 100 calls | Basis |
|---|---|---|---|
| RNNoise 0.2, C, AVX2 | 0.4 % (RTF 0.0042) | ~0.4 core | (measured) 9700X, one pinned core; plain C 0.0066 |
| nnnoiseless | 0.7-2 % | ~1-2 cores | (estimate) no published figure; assumed within 1-3x of the plain-C build; benchmark before trusting. Add ~0.02 % for two resampler passes at 8/16 kHz |
| FastEnhancer-T | 1.2 % (Xeon), 1.3 % (M1), 0.6 % (M5) | ~1.2 cores | (measured, authors) single thread streaming; runtime used not stated |
| FastEnhancer-B | 2.2-2.6 % | ~2.5 cores | (measured, authors) |
| UL-UNAS | ~4-7 % | ~5 cores | (estimate) 35 M MACs ~ GTCRN's 33 M, GTCRN measured at 4-7 %; no RTF published |
| GTCRN | 4-7 % (0.042 M1, 0.060 Xeon, 0.07 i5-12400, 0.071 sherpa-onnx) | ~6 cores | (measured) four sources |
| DeepFilterNet3 / DFN2 | 4.6 % (9700X, 48 kHz) / 4 % (i5-8250U) | ~4-5 cores | (measured) blog; DFN2 paper |
| Hush | <=10 % (card), 1 % (hush-vani, claimed) | 1-10 cores | (claimed) hardware unspecified; treat as unknown |
| DPDFNet-2 (16 kHz) / 8 kHz-2 | ~15-20 % | ~15-20 cores | (estimate) scaling DFN3's 0.046 RTF by MACs (1.35 or 1.29 G / 0.35 G); the DPRNN may be more or less efficient per MAC |

Streaming adds per-call overhead not in offline RTFs (one runtime invocation per hop, state copies): for ORT
about tens of microseconds per call (UNVERIFIED for these graphs); per 16 ms hop that is under 1 % of a core.
Hop sizes do not divide the 20 ms frame (FastEnhancer/GTCRN 16 ms; RNNoise/DPDFNet/Hush 10 ms): a small ring
buffer is needed; algorithmic delay is added to the VAD path (10-40 ms, see table).

## 8 kHz telephony

- Native 8 kHz: only DPDFNet (`dpdfnet2_8khz`, `dpdfnet8_8khz`). All others are 16 kHz or 48 kHz: upsample
  8 -> 16 kHz (or 48 kHz for RNNoise), enhance, and downsample for any consumer that needs 8 kHz.
- Whether 16 kHz models trained on wideband speech behave on upsampled mu-law audio (empty 4-8 kHz band,
  mu-law quantisation noise) is UNVERIFIED for every candidate; no source tested it. FastEnhancer's 48 kHz
  set trains with dynamic low-pass filtering (README), the others do not state band-limited training.
  Must be measured on mu-law round-tripped fixtures before enabling for telephony.
- Frame-to-model rate rules: RNNoise always runs at 48 kHz (100 frames/s regardless of input rate).

## Rust and runtime fit

| Model | Pure Rust | ONNX | tract | In-repo path |
|---|---|---|---|---|
| nnnoiseless | yes (crate) | not needed | n.a. | add crate, BSD-3-Clause, embedded model |
| FastEnhancer | no (decibri wraps via `ort`) | yes (release assets, streaming caches) | UNVERIFIED | `ort` (workspace pins `=2.0.0-rc.13`; crates.io newest 2.0.0-rc.15, no stable; MIT OR Apache-2.0) |
| GTCRN / UL-UNAS | no | yes (sherpa-onnx `gtcrn_simple.onnx`; `ulunas_onnx`) | UNVERIFIED | `ort` or `sherpa-onnx` crate 2.0.1 (Apache-2.0) |
| DFN2/3 | `libDF` tract path (v0.21.4) | yes (`*_onnx.tar.gz`, three-graph split) | yes upstream, but old version | vendoring needed; repo idle since 2024-10 |
| Hush | `hush-vani` (AVX2, no ORT) | yes (tarball in HF repo) | UNVERIFIED | vendor or ORT; small-maintainer risk |
| DPDFNet | no | yes | UNVERIFIED | `ort`; Python `StreamEnhancer` is the reference |

tract-onnx on crates.io: 0.23.9 (updated 2026-10-15), MIT OR Apache-2.0 (workspace uses 0.23.8).

## Licensed noise recordings for test fixtures

Needed: real noise to mix into our speech fixtures at several SNRs (and through the 8 kHz mu-law path).

| Corpus | Size | Rate | License (as fetched) | Short clips in the repo? |
|---|---|---|---|---|
| **MUSAN** noise part (https://www.openslr.org/17/) | 929 files, ~6 h of noise (paper); the OpenSLR archive `musan.tar.gz` is ~10-11 GB for all of music+speech+noise | 16 kHz WAV | "All files fall under a Creative Commons license or the US Public Domain"; noise is limited to content allowing commercial use (paper). Sub-folders: `noise/free-sound/LICENSE` = every recording marked Public Domain on Freesound; `noise/sound-bible/LICENSE` = per-file "Attribution 3.0" or "Public Domain". OpenSLR page: CC BY 4.0 | **Yes**: take clips from `free-sound` (public domain, no attribution needed) and list file names in a NOTICE; `sound-bible` clips need attribution (CC BY 3.0) |
| DEMAND (https://zenodo.org/records/1227121) | 7.4 GB; 16 kHz zips 78-130 MB each, 48 kHz 253-376 MB | 16 / 48 kHz, 16-channel | **Conflict**: Zenodo field `cc-by-4.0`, description text CC BY-SA 3.0; DNS-Challenge repo says CC BY-SA 3.0 | Not committed. Download at test time if wanted (share-alike adaptation risk) |
| ESC-50 (https://github.com/karolpiczak/ESC-50) | ~600 MB, 2000 x 5 s | 44.1 kHz | Whole set CC BY-NC 3.0; ESC-10 subset CC BY 3.0; each clip has its own Freesound license (CC0, CC-BY, CC-BY-NC, CC-Sampling+) in `LICENSE` | **No** (NC and mixed per-clip licenses; the ESC-10 claim and the per-clip table are not reconciled) |
| DNS Challenge noise (https://github.com/microsoft/DNS-Challenge) | `noise_fullband` 58 GB (whole dataset ~1 TB) | 48 kHz | Code MIT; docs CC-BY-4.0; datasets "under their original terms": AudioSet CC BY 4.0, Freesound CC0 only, DEMAND CC BY-SA 3.0. No per-clip table | Too big and no per-clip attribution; not for the repo |
| WHAM!, FSD50K, QUT-NOISE, MS-SNSD | not checked | - | UNVERIFIED (WHAM!/FSD50K are believed to include non-commercial terms - do not use without reading) | - |

**Fixture plan:** use MUSAN `noise/free-sound` (public domain, 16 kHz, same rate as the fixtures). Download the
archive once outside the repo (10-11 GB), pick ~6-10 clips (street/traffic, babble-like, hum, white noise,
keyboard, wind), cut 5-10 s each, store as 16-bit 16 kHz WAV (about 160-320 KB each) with a NOTICE that lists the
original file names and "Public Domain per MUSAN noise/free-sound/LICENSE". Mix at 20, 10, 5 and 0 dB SNR at test
time (synthesise mixes in the test; do not commit them) and also run each mix through resample-to-8 kHz + G.711 mu-law
for telephony cases. Babble: sum several MUSAN speech files (LibriVox / US Government sources; per-file license
not checked, UNVERIFIED) or use two of our own fixtures.

## Verified on 2026-10-30 (implementation)

What the points marked UNVERIFIED above turned out to be, from files fetched and run that day.

**FastEnhancer weights license: not clear, so not shipped.** The repository's `LICENSE` is MIT
(Copyright (c) 2025 AHN Sung Hwan), but nothing licenses the weights separately or says that
the release files are covered: the README has no license statement for models, the bodies of the
releases (`onnx-dns-v1.0.0`, `ckpt-dns-v1.0.0`, ...) only name the training data ("Trained on
DNS-Challenge 3 wideband (16kHz) (without emotional_speech and singing_voice) (with VCTK-0.92
clean speech except p232 and p257)"), and that data carries several licenses of its own (the
DNS noise includes CC BY-SA material, see "Licensed noise recordings"). So the agent file offers
RNNoise only. FastEnhancer stays in the code as the `denoise-fastenhancer` cargo feature of
`brisvox-inference`, for the benchmark (`brisvox-bench denoise ...`, model file pinned by URL and
SHA-256 in `scripts/models/fetch_models.py --evaluation`). Asking the author for a license line on
the weights is the way to promote it.

**FastEnhancer on `ort` and `tract`.** `onnx-dns-v1.0.0/fastenhancer_t.onnx` (125 233 bytes, SHA-256
`a6466367...5ee7d`):
- loads and runs in `ort` 2.0.0-rc.13, the version the workspace pins (`examples/denoise_probe.rs`
  prints the graph; CPU provider, one intra-op thread);
- tract-onnx 0.23.8 parses the graph but cannot optimise it: `SplitToSequence` is not implemented
  ("Operator can not be made a TypedOp"). So ONNX Runtime is the only runtime for it.

**Streaming tensors** (read from the file, T model): input `wav_in` f32 `[1, 256]` (the next 256
samples, floats in -1..1, 16 kHz), `cache_in_0` and `cache_in_1` f32 `[1, 256]` (STFT input overlap
and overlap-add output, `n_fft - hop` samples), `cache_in_2` and `cache_in_3` f32 `[1, 16, 20]`
(recurrent states); output `wav_out` `[1, 256]` and `cache_out_0..3` of the same shapes, fed back
as the next step's caches (all zero at the start). Hop 256 samples = 16 ms, FFT 512, the output lags
the input by `n_fft - hop` = 256 samples (16 ms); the M and L graphs use hops of 160 and 100
(`docs` of the repository, `onnx.md`). The backend reads hop and cache shapes from the file.
Measured delay of the backend, by cross-correlation with the input on `en_two_utterances`: 16 ms
at 16 kHz, 32 ms on an 8 kHz call (two resampler passes of 8 ms), within 2 ms of what `delay()`
reports (`crates/brisvox-inference/tests/denoise.rs`).

**RNNoise through `nnnoiseless` 0.5.2** (crates.io, 2025-12-18, BSD-3-Clause, pure Rust, no
default features needed): `DenoiseState::new()` (embedded weights; which RNNoise generation they
come from is still not stated by the crate), `process_frame(&mut out, &input)` on 480 `f32`
samples in the 16-bit scale at 48 kHz. The first output frame is fade-in. The backend resamples
16 kHz or 8 kHz to 48 kHz and back with the engine's `StreamResampler`; measured delay 20 ms at
16 kHz (10 ms model plus two 5 ms resampler passes), 25 ms at 8 kHz.

**CPU, measured** (release, Ryzen 9950X, one thread, 20 ms frames of the fixtures, three runs of
120 s; `bench/results/denoise-cpu.md`): RNNoise 59-71 microseconds per frame (p50), 0.33-0.38 %
of a core at 16 kHz and 0.39-0.47 % at 8 kHz; FastEnhancer-T 0.17-0.28 ms per frame (p50), 1.1-1.6 %
at 16 kHz and 1.15-1.8 % at 8 kHz. Both are above the ~50 microsecond bar, so they run on the
inference pool. They match the published numbers (RNNoise estimate 0.4-2 %, FastEnhancer-T 1.2 %).

**False barge-ins and turn verdicts**, measured with real noise: `bench/results/denoise-quality.md`
(the committed noise set) and `docs/LATENCY.md`, "Noise suppression".

## Open points for the implementer

- Run our own comparison: PESQ/STOI/DNSMOS-free metrics are not enough; measure STT WER on EN and TR fixtures
  (Deepgram path and sherpa-onnx path) with raw vs denoised, per SNR, 16 kHz and 8 kHz mu-law, for FastEnhancer-T
  (DNS weights), FastEnhancer-B, GTCRN (or UL-UNAS), nnnoiseless.
- Verify tract can load FastEnhancer / GTCRN ONNX (workspace has tract-onnx 0.23.8 and ort rc.13); if not, use ort.
- Confirm in writing the weights' license for FastEnhancer, GTCRN and DFN (repo license vs model files).
- Check that `ort` pinned `=2.0.0-rc.13` loads the FastEnhancer graph (opset, GRU) - not tested.

## Sources (all fetched 2026-10-30)

- RNNoise: https://github.com/xiph/rnnoise ; https://api.github.com/repos/xiph/rnnoise/releases/latest ;
  https://api.github.com/repos/xiph/rnnoise/commits ; https://jmvalin.ca/demo/rnnoise/
- nnnoiseless: https://crates.io/api/v1/crates/nnnoiseless ; https://crates.io/api/v1/crates/nnnoiseless/versions ;
  https://docs.rs/crate/nnnoiseless/latest/source/README.md ; https://api.github.com/repos/jneem/nnnoiseless/commits
- DeepFilterNet: https://github.com/Rikorose/DeepFilterNet ; https://api.github.com/repos/Rikorose/DeepFilterNet ;
  https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/Cargo.toml ;
  https://github.com/Rikorose/DeepFilterNet/tree/main/models ; https://crates.io/api/v1/crates/deep_filter ;
  https://arxiv.org/abs/2305.08227 ; https://ar5iv.labs.arxiv.org/html/2205.05474 ; https://ar5iv.labs.arxiv.org/html/2110.05588
- GTCRN / UL-UNAS: https://github.com/Xiaobin-Rong/gtcrn ; https://api.github.com/repos/Xiaobin-Rong/gtcrn ;
  https://github.com/Xiaobin-Rong/UL-UNAS ; https://arxiv.org/html/2503.00340 ;
  https://k2-fsa.github.io/sherpa/onnx/speech-enhancement/models.html ; https://crates.io/api/v1/crates/sherpa-onnx
- FastEnhancer: https://github.com/aask1357/fastenhancer ; https://api.github.com/repos/aask1357/fastenhancer ;
  https://api.github.com/repos/aask1357/fastenhancer/releases ; https://arxiv.org/html/2509.21867v1 ;
  https://docs.rs/crate/decibri/latest/source/src/denoise.rs
- DPDFNet: https://huggingface.co/Ceva-IP/DPDFNet ; https://github.com/ceva-ip/DPDFNet ;
  https://api.github.com/repos/ceva-ip/DPDFNet ; https://arxiv.org/abs/2512.16420 ; https://arxiv.org/html/2512.16420v1 ;
  https://k2-fsa.github.io/sherpa/onnx/speech-enhancement/dpdfnet.html
- Hush: https://huggingface.co/weya-ai/hush ; https://huggingface.co/weya-ai/hush/raw/main/README.md ;
  https://github.com/Allenmylath/Hush-vani ; https://raw.githubusercontent.com/Allenmylath/Hush-vani/main/hush-vani/NOTICE ;
  https://crates.io/api/v1/crates/hush-vani
- Newer papers: https://arxiv.org/abs/2607.10142 (CoFi-Lite) ; https://arxiv.org/abs/2606.12328 (HALO) ;
  https://arxiv.org/abs/2609.29866 (LiSenNet redesign) ; LiSenNet https://arxiv.org/pdf/2409.13285 (via search summary only)
- NVIDIA: https://huggingface.co/nvidia/RE-USE/blob/main/README.md ; ClearerVoice: https://github.com/modelscope/ClearerVoice-Studio
- Runtime crates: https://crates.io/api/v1/crates/ort ; https://crates.io/api/v1/crates/tract-onnx ; workspace `Cargo.toml` lines 60-61
- ASR effect: https://www.assemblyai.com/blog/noise-cancellation-stt-pros-cons ;
  https://deepgram.com/learn/the-noise-reduction-paradox-why-it-may-hurt-speech-to-text-accuracy ;
  https://arxiv.org/abs/2512.17562 ; https://arxiv.org/pdf/2603.04710 and https://arxiv.org/html/2607.11157v1 (search summaries only) ;
  https://www.creativeainews.com/articles/deepfilternet-vs-rnnoise-vs-ffmpeg-noise-removal-2026/
- Noise corpora: https://www.openslr.org/17/ ; https://arxiv.org/abs/1510.08484 ; https://ar5iv.labs.arxiv.org/html/1510.08484 ;
  https://huggingface.co/datasets/FluidInference/musan (mirror: README and `noise/*/LICENSE` files) ;
  https://zenodo.org/api/records/1227121 ; https://github.com/karolpiczak/ESC-50 (README, LICENSE) ;
  https://github.com/microsoft/DNS-Challenge
