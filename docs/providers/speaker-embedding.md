# Speaker embedding for the caller voice-print veto — license-clean weights

- Researched 2026-10-31. Every external fact comes from a page fetched that day (URLs in "Sources"). Nothing is from
  memory. Marks: **(measured)** = number from a paper/model card; **(estimate)** = derived by us, derivation shown;
  **UNVERIFIED** = not confirmed from a fetched source (or the primary page was unreachable).
- Not a provider API doc: the `_TEMPLATE.md` fields do not apply. Companion: `noise-handling-options.md` (why a veto,
  section 3 "Facts that limit the speaker-veto idea"), `onnxruntime-rust.md`, `rubato.md`, `g711.md`.
- Not legal advice. Items marked "legal call" need the owner's decision.

## 0. Summary

1. **No existing model has a clean chain.** Every public speaker-embedding checkpoint found with good quality was
   trained on VoxCeleb (or on data with non-commercial / unknown terms). The only plausibly clean pretrained file found
   (a 128-dim Mozilla/Coqui speaker encoder "trained on LibriSpeech", MPL-2.0) has no published EER, no ONNX and a
   third-party provenance; treat it as a throwaway baseline at best (section 2).
2. **The premise "VoxCeleb says research purposes only" is contested in the sources we could reach.** The KAIST mirror
   (search snippet) says "download for research purposes under CC BY 4.0"; Lhotse's recipe docstring quotes "commercial/
   research purposes under CC BY 4.0"; both add "copyright remains with the original owners of the video". The Oxford
   primary page was unreachable (connection refused) in this session, so the current wording is **UNVERIFIED**.
   WeSpeaker states pretrained models follow their dataset's license (verified). Legal call (section 1).
3. **A license-clean model can be trained by us.** Data that is clearly permissive and has speaker labels:
   Mozilla Common Voice (CC0 label, 99.7k English speaker ids, 1.8k Turkish) plus LibriSpeech / LibriTTS / MLS
   (CC BY 4.0). Common Voice is the only source with Turkish.
4. **Evidence on quality is thin and mixed.** A ResNet-34 trained only on Common Voice (TidyVoice, 142k speakers,
   7.8k h) gets 0.39 % EER in-domain but **11.14 % on VoxCeleb1-O** (VoxCeleb-trained: 0.3-0.4 %). VoxCeleb-trained
   models get 4.7-6.7 % on a Common Voice benchmark. Domain gaps are large in both directions, so the decision must be
   made on **our own task metric** (caller vs competing speaker, telephony), not on VoxCeleb1-O.
5. **Recommendation (section 7):** train WeSpeaker CAM++ (7.18M params, ~29 MB ONNX, RTF 0.013 single thread) on
   Common Voice + LibriSpeech/LibriTTS, with telephony augmentation; gate shipping on a pilot (about one day on an L4)
   evaluated with the task metric. Keep a VoxCeleb-trained model only as an internal reference, never shipped.

## 1. License facts

### 1.1 VoxCeleb and models trained on it
| Source | What it says | Mark |
|---|---|---|
| KAIST VoxCeleb mirror (search snippet) | "available to download for research purposes under a Creative Commons Attribution 4.0 International License. The copyright remains with the original owners of the video." | snippet only; page itself timed out |
| Lhotse recipe docstring (fetched) | "The VoxCeleb dataset is available to download for commercial/research purposes under a Creative Commons Attribution 4.0 International License. The copyright remains with the original owners of the video." | fetched |
| Praat-hosted copy of the license (fetched) | CC BY 4.0; "Downloading this dataset implies agreement to follow the same conditions for any modification and/or re-distribution of the dataset in any form"; video copyright stays with original owners | fetched |
| Oxford VGG page (robots.ox.ac.uk/~vgg/data/voxceleb/) | connection refused (3 attempts, also vox1/vox2 pages); web.archive.org blocked | UNVERIFIED |
| VoxKitchen docs (third party) | "Academic use only — commercial use requires a separate agreement"; "License: see source terms" | third party, conflicts with above |
| WeSpeaker `docs/pretrained.md` (fetched) | "The pretrained model in WeNet follows the license of it's corresponding dataset"; VoxCeleb example: CC BY 4.0 | fetched |
| HF `Wespeaker/wespeaker-voxceleb-resnet34-LM` | card license `cc-by-4.0`; trained on VoxCeleb2 Dev (5994 speakers); card states nothing about dataset terms | fetched |
| HF `speechbrain/spkrec-ecapa-voxceleb` | card license `apache-2.0`; trained on VoxCeleb1+2; "no warranty on other datasets"; no statement on data license | fetched |

Reading: the sources disagree on "research only" vs "commercial/research"; all agree on CC BY 4.0 + video copyright
with third parties. A model card's license tag proves nothing about the data chain: `areffarhadi/Resnet34-tidyvoiceX-ASV`
is tagged Apache-2.0 but was pretrained on VoxBlink2 (CC BY-NC-SA 4.0) and VoxCeleb2 (fetched). **Legal call:** if
counsel accepts CC BY 4.0 + attribution for VoxCeleb-trained weights, SpeechBrain ECAPA / WeSpeaker CAM++ would be
shippable; the owner's stated position is "not acceptable", so this document plans for clean data. Re-check the Oxford
page when reachable.

### 1.2 Candidate training data
| Dataset | License (source) | Size | Speakers | Turkish | Verdict |
|---|---|---|---|---|---|
| Common Voice scripted 25.0 (Mozilla Data Collective, released 2026-03; older versions leave HF in Oct 2025) | datasheets list CC0-1.0; some datasheets add "do not re-host/re-share" and "do not determine the identity of speakers"; Hill Mari lists "None provided" (search snippets of several datasheets; the page itself did not render) | English 87.84 GB MP3, 2,574,404 clips, 3,766 h (2,752 h validated) | English 99,724 client ids (impure: several people can share one id, TidyVoice paper) | **yes**: 126,510 clips, 135.29 h (129.22 validated), 1,816 speakers (v25.0) | **use**; check each language's datasheet at download time; account + terms of service required on MDC |
| LibriSpeech (OpenSLR 12) | CC BY 4.0 | ~1,000 h, 16 kHz, train-clean-100 6.3 G / 360 23 G / other-500 30 G | count not on page (2,484 per paper, UNVERIFIED here) | no | use; attribution |
| LibriTTS (OpenSLR 60) | CC BY 4.0 | ~585 h, 24 kHz, 7.7 G + 27 G + 44 G | not on page | no | use only if we want the 24 kHz version; mostly the same readers as LibriSpeech (UNVERIFIED), little new |
| Multilingual LibriSpeech (OpenSLR 94) | CC BY 4.0 | English 44.5 k h (2.4 TB FLAC / 651 GB Opus), 7 other languages 6 k h total | per-language counts not found | **no** (EN, DE, NL, FR, ES, IT, PT, PL) | optional; take a speaker-capped subset of English |
| Libri-Light | code MIT; audio license not stated on the pages fetched (LibriVox is public domain per the MUSAN paper, which names LibriVox as "Public Domain") | 60 k h; metadata JSON has a `speaker` field | not stated | no | optional; only unlabelled-style folders, huge |
| VoxPopuli | data CC0; models/code CC BY-NC 4.0 | 100 k-384 k h unlabelled, 23 languages | speaker ids only for transcribed/S2S manifests | **no** | skip (no Turkish, huge) |
| TidyVoice Tidy-M / Tidy-X (Common Voice, speaker-label cleaned) | paper CC BY 4.0; dataset license not stated; Tidy-X on MDC listed CC0-1.0, 36.72 GB (search snippet); challenge rules forbid other Common Voice data and re-identification | Tidy-M: 212,617 speakers, 81 languages, 8,138 h; Tidy-X: 4,474 speakers, 40 languages, 457 h | see left | Tidy-M ~940 train speakers (Turkish) | Tidy-M download location not found (UNVERIFIED). Labels were cleaned with a VoxBlink2+VoxCeleb2-trained ResNet-293 (NC-SA/CC BY data), gray: prefer our own cleaning |
| CN-Celeb | OpenSLR 82: CC BY-SA 4.0 (ShareAlike); site reportedly wants a license form (UNVERIFIED) | 3,000 Chinese celebrities, ~97 GB | - | no | skip (SA, Mandarin) |
| VoxBlink2 | CC BY-NC-SA 4.0 (GitHub LICENSE, search result) | 111,284 YouTube users | - | multilingual | **excluded** (NC) |
| AISHELL-1 | Apache-2.0 but page also says "free for academic use" | 400 speakers | - | no | skip (ambiguous, Mandarin) |
| 3D-Speaker dataset | license not stated anywhere fetched | - | - | no | skip |
| YODAS, People's Speech | not fetched | - | no speaker labels expected (UNVERIFIED) | - | not evaluated |

Notes: LibriSpeech/LibriTTS/MLS sizes are for read English audiobooks from many readers; they teach speaker
discrimination on clean read speech only. Common Voice adds consumer microphones, accents and Turkish.

### 1.3 Augmentation data
| Set | License (source) | Use |
|---|---|---|
| MUSAN (OpenSLR 17, 11 GB) | page: CC BY 4.0 overall. Paper: music from Jamendo / Free Music Archive / Incompetech / HD Classical under Creative Commons, "care was taken that any of these files can be used for commercial purposes"; noise: Free Sound part is **public domain**, Sound Bible part is **Creative Commons** (not PD); speech: LibriVox (PD) + US government hearings (believed PD); each directory has a per-file LICENSE file | noise + music; skip MUSAN speech (we use real other-speaker babble from our corpora). Keep the per-file LICENSE files for attribution |
| RIRS_NOISES (OpenSLR 28, 1.3 GB) | page: Apache-2.0 overall; real RIRs come from RWCP, REVERB 2014 and AIR, whose own terms the page does not state | use only the **simulated** RIRs, or generate our own with an image-method simulator (no license question). Real-RIR terms UNVERIFIED |
| Codecs | G.711 (own code, `g711.md`), ffmpeg/sox encoders for AMR-NB/Opus (tool licensing not checked) | telephony simulation |

## 2. Existing pretrained models (all checked)

| Model | Weights license | Training data | Clean? | Size / CPU | EER (set) | ONNX | 8 kHz / Turkish |
|---|---|---|---|---|---|---|---|
| WeSpeaker ResNet34 / CAM++ / ECAPA (VoxCeleb) | card: CC BY 4.0; "follows dataset license" | VoxCeleb2 Dev | no (section 1.1) | CAM++ 7.18M params, 1.72 GFLOPs/3 s, RTF 0.013 CPU 1 thread; ResNet34 6.63M, RTF 0.032; ECAPA c1024 14.65M, RTF 0.033 (measured, CAM++ paper, CPU model not stated); ONNX files 26.5-29.6 MB | CAM++ 0.803 / ResNet34 0.867 / ECAPA c512 1.069 (Vox1-O, WeSpeaker README; c512 6.19M params) | yes (csukuangfj HF repo, 21 files) | 16 kHz only stated; no Turkish data |
| SpeechBrain ECAPA (VoxCeleb) | Apache-2.0 tag | VoxCeleb1+2 | no | not measured here | 0.80 % (Vox1-test cleaned) | third-party export only | 16 kHz; no Turkish |
| 3D-Speaker CAM++/ERes2Net (zh-cn "200k speakers") | not stated on GitHub page; code Apache-2.0 | undisclosed 200k-speaker Mandarin set; en models VoxCeleb | no / unknown | CAM++ 28.3 MB ONNX | not listed | yes | all 16 kHz; Mandarin |
| NVIDIA TitaNet-large (NeMo) | CC-BY-4.0 | VoxCeleb1+2, Fisher, Switchboard, LibriSpeech, SRE 2004-2010 (Fisher/Switchboard/SRE are LDC corpora; their terms not fetched, UNVERIFIED) | no | ~23M params | 0.66 % (Vox1 cleaned) | HF sherpa export (101 MB) | 16 kHz |
| Resemblyzer / Real-Time-Voice-Cloning encoder | Apache-2.0 | third-party paper: LibriSpeech-other + VoxCeleb1 + VoxCeleb2 (card says nothing) | no | GE2E LSTM, 256-dim | not reported | no | not stated |
| `czyzi0/MozillaSpeakerEncoderModels` (Mozilla/Coqui speaker encoder) | **MPL-2.0** | `model_mozilla_128`: "trained on LibriSpeech"; `model_mozilla_256`: LibriTTS+VCTK+VoxCeleb+CommonVoice | 128-dim: plausibly clean (single third-party statement, UNVERIFIED); 256-dim: no (VoxCeleb) | not stated | not reported | no (.pth.tar) | not stated |
| Qwen3-TTS speaker encoder (ECAPA, ~12.2M params, 24 kHz, 128 mel) | Apache-2.0 (third-party repo; not the official card) | not disclosed in anything fetched | unknown | - | not reported; built for TTS conditioning | no | no |
| WavLM Base / Base+ (+ `-sv` head) | card gives no license text, points to the UniSpeech repo LICENSE (not fetched, UNVERIFIED) | Base: 960 h LibriSpeech; Base+: Libri-Light 60 k h + GigaSpeech 10 k h + VoxPopuli 24 k h; `-sv` head fine-tuned on VoxCeleb1 | Base chain possibly clean, `-sv` no; GigaSpeech terms not fetched | transformer, order of 10x CAM++ cost (param count not on card, UNVERIFIED) | CommonBench: WavLM-Base 23.05 %, Base+ 20.23 % on Common Voice, "degrade rapidly" with noise+low-bitrate codecs | no | 16 kHz |
| w2v-BERT 2.0 (+ TidyVoice head) | MIT | 4.5 M h, >143 languages, sources unnamed | unknown | 600M params | 3.16-4.35 % on TidyVoice eval | no | far too big for CPU |
| TinyECAPA / ECAPA-TDNNLite (arXiv 2601.16235) | no code/weights license stated | VoxCeleb2 (teacher also VoxCeleb2) | no | 150k / 318k params | 4.26 % / 2.31 % (Vox1-O) | - | - |
| TidyVoice ResNet-34 (Tidy-M scratch) | not found; the Tidy-X baseline on HF is Apache-2.0-tagged but VoxBlink2+VoxCeleb2 pretrained | Common Voice (cleaned by VB2/Vox2 model) | scratch model: data CC0, labels gray; weights not located | 6.6M-class | 0.39 % Tidy-M, 2.83 % Tidy-X, **11.14 % Vox1-O** | WeSpeaker export | CV only; Turkish ~940 train speakers |

Self-supervised backbones: a clean chain would need our own speaker fine-tuning on CC data on top of a >90M-parameter
transformer; CPU cost and the CommonBench degradation under codecs rule them out for a per-candidate CPU job.

No existing model satisfies "permissive weights + clean data + good quality + CPU-cheap + ONNX" in the sources found.

## 3. Training plan (our own model)

### 3.1 Recipe
- **Toolkit:** WeSpeaker (Apache-2.0; README pins torch/torchaudio 2.7.1, CUDA 11.x/12.x builds, conda python 3.9).
  Recipes shipped are VoxCeleb, CNCeleb, SRE16, VoxConverse only; a Common Voice recipe must be written (Kaldi-style
  `wav.scp`/`utt2spk` + shard lists, config derived from the VoxCeleb v2 recipe; exact config file names UNVERIFIED).
- **Model:** CAM++ (7.18M params, 1.72 GFLOPs per 3 s, RTF 0.013) as primary; ECAPA-TDNN C=512 (6.19M params, WeSpeaker
  Vox1-O 1.069 vs CAM++ 0.803) as fallback if CAM++ underperforms. SpeechBrain ECAPA is an alternative toolkit but
  its README gave no config/timing numbers.
- **Recipe from WeSpeaker README (measured settings):** fbank80, 200-frame (2 s) crops, ArcMargin, 150 epochs,
  speed perturbation, aug_prob 0.6, no SpecAugment, cosine scoring with mean normalization, optional large-margin
  fine-tune (LM) and AS-Norm. CAM++ paper: 25 ms / 10 ms fbank, speed {0.9, 1.0, 1.1}, reverb with RIRs, additive MUSAN,
  AAM-softmax margin 0.2 scale 32, SGD momentum 0.9, wd 1e-4, cosine LR 0.1 -> 1e-4 with warm-up, 3 s random crops;
  epochs/GPUs/batch not stated.
- **Speaker labels:** Common Voice client ids mix people (TidyVoice: significant heterogeneity). Clean ourselves:
  train round 1 on raw ids with a conservative cap, embed all clips, drop clips whose cosine to their id's median is
  below a threshold, retrain (TidyVoice used threshold 0.4 per file and 0.2 for cross-language pairs, with a model
  trained on NC data; our thresholds must be chosen on our model). Keep only ids with >= 8 clips; cap clips per id so
  the head is not dominated by a few contributors.
- **Language mix:** Common Voice English + Turkish (upweight tr; 1,816 speakers is small) + several other Common Voice
  languages by speaker count, plus LibriSpeech/LibriTTS read speech. MLS English optional. No cross-lingual verification
  evidence exists for our mix; Tidy-X trials (same speaker in several languages) are the best public test.
- **Telephony / 8 kHz augmentation (design, UNVERIFIED benefit):** with probability ~0.3: low-pass to 3.4 kHz and
  resample 16 -> 8 -> 16 kHz; G.711 mu-law/A-law encode/decode (repo `g711.md`); AMR-NB / Opus through ffmpeg; random
  gain; plus MUSAN noise (SNR 0-20 dB), simulated RIRs, and babble from other training speakers. Inference: resample
  8 kHz input to 16 kHz with rubato so the front end is identical, no separate 8 kHz model in v1. Source evidence:
  CommonBench found GSM/AMR/Opus alone changed EER by <0.1 pt for ECAPA etc. but identical numbers across codecs
  look suspicious (paper does not explain); under noise+RIR+low-bitrate codec EER rises sharply (ECAPA 6.13 % clean ->
  17.89 % with Gaussian noise + RIR). No source covers G.711 or Turkish.
- **Short utterances:** train on 2 s crops and include 0.5-1.5 s crops with probability ~0.3 (our need is 0.5-1.5 s).
  Published degradation for VoxCeleb-trained ResNet34, 5 s enroll vs 1 s test: 1.02 -> 4.33 % (x4.2); ECAPA 1.08 -> 4.84 %.
  Self-enrollment on the same call (same channel and codec) removes channel mismatch.

### 3.2 Compute (estimates, derivation shown; calibrate with a 500-step benchmark before committing)
- Per sample: CAM++ forward 1.72 GFLOPs / 3 s -> ~1.15 GFLOPs for a 2 s crop (if the paper's figure is MACs, double it);
  training ~3x forward = ~3.4 GFLOPs/sample.
- NVIDIA dense FP16 tensor peak (fetched): T4 65 TFLOPS, L4 ~121 (242 sparse), A100 312; RTX 5070 12 GB GDDR7, no FP16
  TFLOPS figure on the page (it lists 988 AI TOPS), so assume L4-class or better.
- Assumed sustained utilization for small conv/TDNN nets plus fbank and data loading: ~3-5 % of peak. This gives
  planning throughput of about 600 samples/s (T4), 1,100 (L4), 3,000 (A100); RTX 5070 ~1,000-1,500 (guess).
- Dataset size for v1: ~1,500 h, ~1 M utterances (3,766 h / 2.57 M clips = 5.3 s per Common Voice clip). One epoch = one
  random 2 s crop per utterance = ~1 M samples: T4 ~28 min, L4 ~15 min, A100 ~6 min.
  - 40 epochs (pilot-to-v1): T4 ~19 h, L4 ~10 h, A100 ~4 h.
  - 100 epochs (full): T4 ~46 h, L4 ~25 h, A100 ~9 h. WeSpeaker uses 150 epochs on VoxCeleb2.
- Colab (docs fetched): free tier GPUs "heavily restricted", runtime up to 12 h "depending on availability"; paid tiers
  up to 12 h (Pro+ up to 24 h with units); idle runtimes time out. Compute-unit burn per hour is **UNVERIFIED** (only
  third-party measurements: T4 1.19-1.76, L4 1.71, A100 5.4-15; Pro $9.99 per 100 units). Plan: checkpoint every epoch to
  Drive, resume across sessions. Free tier is not viable for the full run; one Pro month covers an L4 pilot.
- Local RTX 5070: needs a PyTorch build that supports this GPU (Blackwell; version support UNVERIFIED), and respects
  the repo's disk-hygiene rule (no 100+ GB datasets on C: without checking free space).

### 3.3 Storage
- Common Voice English is 87.84 GB as MP3; decoding everything to 16 kHz int16 WAV would be ~3,766 h x 115 MB/h =
  ~430 GB (estimate), so do not decode all of it. Select ~1,500 h first (cap clips per client id), then store as 16 kHz
  FLAC shards, ~50-60 MB/h (assumption), i.e. ~80-90 GB, or keep Opus/MP3 and decode in the data loader (CPU-bound on
  Colab; vCPU count not checked).
- Practical split: preprocess/select on the local PC or a Colab CPU session, write shards to Drive/GCS, mount per
  session; keep the working set under the VM disk (user note: ~100-200 GB). Downloading needs a Mozilla Data
  Collective account and API key (the TidyVoiceX challenge uses an API key + script).

### 3.4 Export and shipping
- WeSpeaker `wespeaker/bin/export_onnx.py` (fetched): args `--config --checkpoint --output_model [--num_frames] [--mean_vec]`;
  input `feats` shaped (batch, frames, feat_dim) = (1, 200, 80) by default, output `embs`, opset 14, dynamic axes on
  batch and frames unless `--num_frames` > 0; the mean vector is subtracted inside the graph. So the Rust side computes
  80-dim fbank (25 ms / 10 ms) and feeds ONNX Runtime. sherpa-onnx already runs WeSpeaker/3D-Speaker/NeMo ONNX models
  (HF listing: 21 models, 23-221 MB).
- Weights license: our own model on CC0 + CC BY 4.0 data, released Apache-2.0 (or MIT) with a `NOTICE`. WeSpeaker's
  rule "the model follows the dataset license" is satisfied by CC BY attribution. Suggested text:
  "Trained on Mozilla Common Voice (CC0-1.0), LibriSpeech (CC BY 4.0, OpenSLR 12), LibriTTS (CC BY 4.0, OpenSLR 60)
  [and Multilingual LibriSpeech (CC BY 4.0, OpenSLR 94)]; noise from MUSAN (CC BY 4.0, per-file licenses in the corpus).
  Attribution notices are reproduced in NOTICE." Do not mix in CN-Celeb (SA), VoxBlink2 (NC-SA), VoxCeleb.
- Common Voice conditions (datasheet wording varies): no re-hosting of the dataset (we do not publish audio), and no
  determining of speaker identity. A verification model that never maps a voice to a person and is only used to compare
  two voices inside one call does not do the latter, but that reading is a legal call. Do not use a model that
  generated labels from NC data without deciding that explicitly (TidyVoice labels).
- Do not distill from or pseudo-label with a VoxCeleb-trained model (weights inherit its terms per WeSpeaker).

## 4. Expected quality (honest estimate)

Anchors (measured, TidyVoice Table 2, ResNet-34): scratch on Common Voice Tidy-M: Tidy-M 0.39 %, Tidy-X 2.83 %,
Vox1-O 11.14 %. VoxBlink2-trained ResNet-34: Tidy-M 3.34 %, Tidy-X 4.10 %, Vox1-O 0.39 %. VoxCeleb-family models on
CommonBench (Common Voice 16, 11,793 speaker ids, full utterances): ECAPA 6.13 %, MFA-Conformer 6.65 %, TitaNet 4.92 %,
ReDimNet 4.69 %; cross-language trials 3.5-10 %. The two benchmarks use different trial lists; they show domain,
not model, effects.

Estimate for our veto (reasoning, not measured):
- Versus a VoxCeleb-trained CAM++ on VoxCeleb1-O: ours will be roughly an order of magnitude worse (expect high single
  digits to ~11 % EER), because VoxCeleb is interviews / celebrity speech and we train read speech. That number does not
  matter for the product.
- On Common-Voice-like or phone-call speech it should be at least as good as the VoxCeleb models (TidyVoice: 0.39 %
  vs 3.34 % in-domain), though telephony and Turkish are untested for both families.
- At 1 s candidates, expect EER about 4x the full-utterance number (published degradation). With 5-10 s of
  self-enrollment and a conservative threshold (veto only below a very low score, fail-open otherwise), the product
  question is "fraction of bystander candidates caught at ~0 % false vetoes of the caller": plausible in the 40-80 %
  range at -5..-10 dB; **UNVERIFIED, to be measured** (section 5).
- Overlapped speech (bystander and caller at once) is a different failure; a mixture embedding is mostly the louder
  voice. Level-relative gating (Vapi-style, `noise-handling-options.md`) is complementary.

## 5. Evaluation plan
1. **Clean-license EER:** held-out speakers only. Splits by speaker (no overlap with training): LibriSpeech test-clean/
   test-other; held-out Common Voice English speakers; held-out Common Voice Turkish speakers (after our cleaning);
   Tidy-X-style cross-language pairs for languages we trained on and unseen ones. Enrollment 3-10 s, test crops 0.5,
   1.0, 1.5 s. Report EER and minDCF. Repeat on an 8 kHz + G.711 + AMR-NB degraded copy of every trial list.
2. **Task metric (the gate):** synthetic calls in the `noise-handling-calls.md` format. Enroll on the caller's first
   utterance(s) (2-5 s). Candidates: (a) the caller's later utterances (must almost never be vetoed), (b) a second speaker
   (same gender / other gender / other language / Turkish) at -5, -10, -15, -20 dB relative to the caller, (c) caller +
   bystander overlap. Metrics per setting: false-veto rate on (a), veto rate on (b)/(c), added time-to-interrupt from the
   0.5-1.5 s window plus embedding time, per-candidate CPU time on the inference pool. Choose the threshold at a fixed
   false-veto rate. Acceptance: 0 missed real barge-ins and clear drop at -5..-20 dB, otherwise do not ship.
3. **Reference:** score the same trials with a VoxCeleb-trained CAM++ for the gap only (internal, never shipped,
   legal call on evaluating with it).
4. **Pilot gate (about 1 day on L4):** 300 h Common Voice (English + all Turkish) + LibriSpeech 360 h, 30-40 epochs; run
   1-2. Proceed to the full run only if the task metric shows a useful veto rate at a low false-veto rate.

## 6. CPU and integration facts
- Per candidate (1.5 s of audio): CAM++ ~20 ms, ResNet34 / ECAPA-c1024 ~48-50 ms at 1 thread (estimate from
  published RTF 0.013 / 0.032 / 0.033, CPU not stated), plus fbank. Runs once per candidate on the inference pool,
  not per frame. 29 MB fp32 ONNX (CAM++), 26.5 MB (ResNet34) per the sherpa export listing; int8 effect UNVERIFIED.
- Sessions: model shared read-only across sessions, one embedding vector per enrolled caller (192-256 floats).
- The CAM++ speed figure comes from the CAM++ paper; no independent hundreds-of-calls figure was found.

## 7. Recommendation
1. **Do not ship any currently available pretrained speaker model.** Closest candidates fail on license (VoxCeleb
   family, TitaNet, 3D-Speaker, VoxBlink2) or on evidence (the LibriSpeech Mozilla encoder, Qwen3-TTS encoder).
2. **Train WeSpeaker CAM++ on Common Voice (CC0) + LibriSpeech/LibriTTS (CC BY 4.0)** with MUSAN noise, simulated
   RIRs and telephony/codec augmentation, our own speaker-label cleaning, and short-crop sampling. Release as
   Apache-2.0 with a `NOTICE` carrying the CC BY attributions. License verdict: clean as far as sources show; the open
   legal points are Common Voice's datasheet wording (identity clause) and the choice to avoid labels produced by
   NC-trained models.
3. **Run the pilot first** (section 5.4). Expected cost: ~10 h on an L4 for the 40-epoch pilot (estimate), ~25 h for
   100 epochs. Expected quality: far below VoxCeleb models on VoxCeleb1-O, comparable or better on read/consumer-mic
   speech, telephony and Turkish unmeasured. Ship only if the task metric clears that acceptance bar.
4. **Optional parallel path (cheap):** ask counsel whether VoxCeleb CC BY 4.0 + "commercial/research" wording (Lhotse)
   covers weights; if yes, WeSpeaker CAM++ becomes a day-one option and the training becomes a quality improvement
   rather than a blocker. Re-fetch the Oxford VGG license page first.
5. If the pilot fails: widen data (more Common Voice languages, MLS English subset), larger model (ECAPA c512 or
   ResNet34), then reconsider.

## Gotchas / open items
- Common Voice datasheets are JS-rendered; their restrictions text was only seen through search snippets of other
  languages' datasheets. Read the English and Turkish datasheets in a browser before download.
- Turkish speaker count (1,816 in v25.0) is too small to train on alone; cross-lingual transfer is assumed, not proven.
- Parameter counts for WeSpeaker ResNet34 differ across sources (6.34M 3D-Speaker table, 6.63M WeSpeaker README,
  6.70M CAM++ paper); same for ECAPA c1024 (14.65M vs 20.8M in 3D-Speaker's own variant).

## Sources (all fetched 2026-10-31)
- VoxCeleb license: https://mm.kaist.ac.kr/datasets/voxceleb/ (timed out; snippet) ;
  https://www.fon.hum.uva.nl/praat/manual/VoxCeleb_CC-BY-4_0_license.html ;
  https://lhotse.readthedocs.io/en/v1.22/_modules/lhotse/recipes/voxceleb.html ;
  https://voxkitchen.readthedocs.io/en/latest/datasets/voxceleb2/ ; https://www.robots.ox.ac.uk/~vgg/data/voxceleb/ (unreachable)
- WeSpeaker: https://github.com/wenet-e2e/wespeaker ;
  https://github.com/wenet-e2e/wespeaker/blob/master/docs/pretrained.md ;
  https://github.com/wenet-e2e/wespeaker/blob/master/examples/voxceleb/v2/README.md ;
  https://github.com/wenet-e2e/wespeaker/blob/master/wespeaker/bin/export_onnx.py ;
  https://huggingface.co/Wespeaker/wespeaker-voxceleb-resnet34-LM
- Models: https://huggingface.co/speechbrain/spkrec-ecapa-voxceleb ;
  https://github.com/speechbrain/speechbrain/blob/develop/recipes/VoxCeleb/SpeakerRec/README.md ;
  https://huggingface.co/nvidia/speakerverification_en_titanet_large ; https://github.com/resemble-ai/Resemblyzer ;
  https://huggingface.co/czyzi0/MozillaSpeakerEncoderModels ; https://github.com/modelscope/3D-Speaker ;
  https://huggingface.co/csukuangfj/speaker-embedding-models/tree/main ; https://arxiv.org/html/2303.00332v3 (CAM++);
  https://huggingface.co/microsoft/wavlm-base ; https://huggingface.co/microsoft/wavlm-base-plus ;
  https://huggingface.co/microsoft/wavlm-base-plus-sv ; https://huggingface.co/facebook/w2v-bert-2.0 ;
  https://arxiv.org/html/2601.16235 (TinyECAPA); https://docs.rs/crate/qwen_tts/latest/source/src/nn/speaker_encoder.rs
  and https://huggingface.co/marksverdhei/Qwen3-Voice-Embedding-12Hz-1.7B (search results, third party)
- Evaluation papers: https://arxiv.org/html/2601.16358 (TidyVoice) ; https://arxiv.org/html/2601.21960 (TidyVoice 2026 plan) ;
  https://huggingface.co/areffarhadi/w2v-bert-TidyVoice ; https://huggingface.co/areffarhadi/Resnet34-tidyvoiceX-ASV ;
  https://arxiv.org/html/2509.17091v1 (SVeritas, CommonBench numbers) ;
  https://www.isca-archive.org/spsc_2024/hintz24_spsc.pdf (CommonBench; PDF unreadable, details via search/SVeritas) ;
  https://arxiv.org/html/2601.13999 (short-duration degradation)
- Data: https://www.openslr.org/12/ ; https://www.openslr.org/60/ ; https://www.openslr.org/94/ ;
  https://www.openslr.org/17/ ; https://www.openslr.org/28/ ; https://www.openslr.org/33/ ; https://openslr.org/82 ;
  https://github.com/facebookresearch/voxpopuli ; https://github.com/facebookresearch/libri-light (and its
  data_preparation README) ; https://arxiv.org/abs/1510.08484 and https://ar5iv.labs.arxiv.org/html/1510.08484 (MUSAN) ;
  https://huggingface.co/spaces/mozilla-foundation/README/blob/main/README.md (Common Voice moved to MDC, Oct 2025) ;
  Mozilla Data Collective datasheets (via search): Turkish v25.0 https://datacollective.mozillafoundation.org/datasets/cmn2e7kbl01k2mm07gm5n1bc9 ,
  English v25.0 https://prod.datacollective.mozillafoundation.org/datasets/cmndapwry02jnmh07dyo46mot ,
  English v23.0 https://dev.datacollective.mozillafoundation.org/datasets/cmflnn4815p33e4fb0591ef4y ,
  TidyVoiceX https://datacollective.mozillafoundation.org/datasets/cmihtsewu023so207xot1iqqw ;
  VoxBlink2 license: https://raw.githubusercontent.com/VoxBlink2/ScriptsForVoxBlink2/main/LICENSE and https://arxiv.org/pdf/2407.11510 (search results)
- Compute: https://research.google.com/colaboratory/faq.html ; https://www.nvidia.com/en-us/data-center/tesla-t4/ ;
  https://www.nvidia.com/en-us/data-center/l4/ ; https://www.nvidia.com/en-us/data-center/a100/ ;
  https://www.nvidia.com/en-us/geforce/graphics-cards/50-series/rtx-5070-family/ ;
  third-party compute-unit rates: https://mccormickml.com/2024/04/23/colab-gpus-features-and-pricing/ ,
  https://aicoolies.com/pricing/google-colab

## Update 2026-10-31: Common Voice is not usable for this

Common Voice is CC0, but downloading it requires agreeing not to determine the identity of the speakers, and a
Mozilla staff reply on Discourse says that clause covers training speaker-identification models. A voice print is
exactly that, so Common Voice (and with it the only clean Turkish source above) is excluded. Sources:
https://discourse.mozilla.org/t/speaker-ids-for-speaker-identification-model/114358 ;
https://datacollective.mozillafoundation.org/datasets/cmfzu8u8wa555eq8onrk334h4 ;
https://huggingface.co/datasets/legacy-datasets/common_voice (fetched 2026-10-31).

## Decision 2026-10-31: use the WeSpeaker VoxCeleb CAM++ model (CC BY 4.0)

VoxCeleb's license file names Creative Commons Attribution 4.0 International
(https://mm.kaist.ac.kr/datasets/voxceleb/files/license.txt), which allows commercial use with attribution; the
"research purposes" wording on the site describes the dataset, and the redistribution condition is met by keeping the
same terms and credit. WeSpeaker states that its pretrained VoxCeleb models follow the dataset's license, CC BY 4.0
(https://github.com/wenet-e2e/wespeaker/blob/master/docs/pretrained.md; the pyannote copy of the ResNet34 model is
tagged cc-by-4.0: https://huggingface.co/pyannote/wespeaker-voxceleb-resnet34-LM). These models are widely used in
open source and commercial products. brisvox fetches the model file (pinned URL and SHA-256), never relicenses it,
and ships a NOTICE with the CC BY 4.0 attribution (VoxCeleb authors, WeSpeaker). Candidate: CAM++
(https://huggingface.co/Wespeaker/wespeaker-voxceleb-campplus, ONNX `voxceleb_CAM++.onnx`; the `_LM` variant is tuned
for audio longer than 3 s, longer than a barge-in candidate). Training our own model on clean-license data stays a
possible later improvement (Turkish, 8 kHz), not a prerequisite.
