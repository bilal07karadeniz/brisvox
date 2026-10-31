# Noise handling beyond a denoiser — options, evidence, ranking

- Researched 2026-10-31. Every external fact below comes from a page fetched that day (URLs in "Sources"); the
  local measurements come from the files named in section 1. Nothing is from memory.
- Not a provider API doc: the `_TEMPLATE.md` fields do not apply (same as `vad-alternatives.md`).
- Marks: **(measured)** = a number measured by us or a third party; **(claimed)** = vendor/author statement without
  method or hardware; **(estimate)** = derived by us, derivation shown; **UNVERIFIED** = not confirmed from a fetched
  source. "Maintainer-published" = published by the project that owns the model (not independent).
- Companion notes: `noise-suppression.md` (denoiser survey, 2026-10-30), `vad-alternatives.md` and `silero-vad.md`
  (VAD facts), `docs/ARCHITECTURE.md` "Interruption policy" (what the engine already does).

## 0. Summary

1. **The measured failure is partly a measurement gap.** Our harness mixes speech with non-speech noise only. The
   problems users report (TV, a second person talking, a far-field voice) are *background speech*, which Silero, the
   STT and every plain denoiser treat as speech. We have not measured that case at all, and it is the case where the
   one number we found for voice isolation is large (LiveKit docs, one gym clip: WER 117.6 % -> 11.8 % with Krisp VIVA,
   section 3). Add competing-speech mixes to the harness before judging any front end.
2. **Silero v6.2.3 is already a strong noise-only rejecter** (maintainer-published: ESC-50 noise-only accuracy 0.87 for
   v6 vs 0.61 for v5, TEN 0.42, FireRedVAD 0.60). A denoiser in front of it has little left to remove and adds
   artifacts, which is what our run shows (fragments up, flips not down). Do not spend more effort on classic
   denoisers in front of the VAD.
3. **What no model is needed for:** the engine already vetoes most wordless noise starts (`min_speech` 300 ms,
   `min_words`, echo window, backchannels; section 4). The harness reports VAD-level false starts, i.e. *before* those
   guards; a post-guard metric is missing and cheap to add.
4. **Open weights for "keep the primary speaker, drop the others" are thin.** Found with permissive licenses:
   Hush (Apache-2.0, trained with competing speakers, no published WER/VAD numbers) and speaker-embedding gating
   (WeSpeaker/3D-Speaker CAM++, Apache-2.0 code; weights follow VoxCeleb CC BY 4.0). Personalized/target-speaker
   enhancement exists only as papers without code (section 3). Krisp VIVA, ai-coustics and LiveKit's adaptive
   interruption model are proprietary or hosted-only.
5. **Rejected on license:** TEN VAD (Apache-2.0 plus a "no competing with Agora" condition), NVIDIA Frame-VAD
   MarbleNet (NVIDIA Open Model License, NeMo runtime). Not MIT/Apache/BSD.

Ranked shortlist is in section 7.

## 1. What our own measurements say (read 2026-10-31)

Files: `bench/results/denoise-quality-musan-background.md`, `denoise-cpu.md`, `denoise-wer-sherpa-en.md` (worktree
`agent-a89004c5f7a5ee322`; Windows, trends only). Production VAD params in `crates/brisvox-inference/src/vad.rs`:
threshold 0.5, neg_threshold 0.35, min_speech 64 ms, min_silence 100 ms, speech_pad 30 ms.

| Cell (102 mixes per SNR) | none | RNNoise | FastEnhancer-T |
|---|---|---|---|
| wideband 10 dB: false starts / fragments / flips of 204 | 0 / 1 / 3 | 6 / 11 / 4 | 3 / 3 / 6 |
| wideband 0 dB | 4 / 9 / 6 | 10 / 56 / 7 | 9 / 40 / 6 |
| telephone 20 dB | 1 / 5 / 0 | 1 / 18 / 1 | 0 / 45 / 16 |
| telephone 0 dB (missed of 204) | 7 (2) / 32 / 3 | 3 (0) / 75 / 8 | 0 (7) / 203 / 10 |
| CPU per call, 20 ms frames | - | 0.38-0.39 % (0.4 cores / 100 calls) | 1.56-1.76 % (1.6-1.8 cores) |

Readings (our interpretation, not tested further):
- Silero alone: 0-1 false starts per quiet minute down to 10 dB, 1.3-2.3 at 0-5 dB, fragments grow with noise
  (telephone path worse: 18-32 fragments per 204 mixes at 10 dB and below). The visible residual problems are
  **fragments** (a second start inside an utterance) and **Smart Turn flips** (8 of 204 at 5 dB wideband), not
  missed speech.
- Both denoisers raise fragments and false starts at 5-10 dB and do not lower flips. FastEnhancer-T on the telephone
  path is the worst (45-203 fragments, up to 22 flips of 204): a 16 kHz model fed 8 kHz mu-law audio upsampled to
  16 kHz is out of its training domain (a hypothesis; `noise-suppression.md` already lists it as UNVERIFIED for every
  candidate).
- WER: one fixture (`en_two_utterances`), one recognizer (`sherpa-onnx-streaming-zipformer-en-2023-06-26`, per
  `docs/providers/local-speech.md`), summed over 9 noises. The recognizer collapses on 8 kHz telephone audio even at
  20 dB (74 % vs 31 % clean), so FastEnhancer's 74 -> 38 % says that recognizer is fragile, not that denoising helps
  production STT. Recognizer dependence is documented (section 2: Whisper and wav2vec 2.0 react in opposite ways).
- The harness never plays bot audio, never has a second speaker, and counts starts *before* the interruption
  policy.

## 2. Denoisers that are ASR-friendly or VAD-friendly

**Evidence that denoising is recognizer-dependent and strength-dependent**

| Source | Finding |
|---|---|
| arXiv 2607.11157 (html fetched), FRCRN / MossFormerGAN enhancers, Whisper-large-v3 and wav2vec 2.0 on VoiceBank+DEMAND plus 4 low SNRs | Magnitude strength is the operative axis. Whisper is **U-shaped**: mild correction (alpha ~0.25) is best (WER 6.44 % noisy -> 5.6 %), full enhancement no better than noisy (p = 0.92). wav2vec 2.0 improves with strength (17.13 % -> 10.2 %). Of six enhancers at full strength only MossFormerGAN-SE helped Whisper (6.0 %); MetricGAN+ 10.6 %, simple mask 11.4 %. Fix is an inference-time strength knob calibrated per recognizer, no retraining |
| DeepFilterNet `atten_lim_db` (source `enhance.py`, fetched) | Not a mask clamp: `enhanced = noisy * 10^(-dB/20) + enhanced * (1 - 10^(-dB/20))` in the complex STFT. 12 dB -> 0.251 weight on the noisy spectrum. Any denoiser can get the same dry/wet blend for free |
| Blog benchmark (creativeainews, 824 VoiceBank-DEMAND clips, faster-whisper small.en, Ryzen 7 9700X; third party, not peer reviewed) | WER clean 2.52 %, noisy 3.75 %, **DFN3 with 12 dB cap 3.70 %**, DFN3 full 4.18 %, RNNoise 0.2 5.68 %. The cap is the only setting that did not raise WER; it ties the noisy baseline, it does not beat it. Clips made worse: 32 (cap) vs 46 (full) |
| arXiv 2512.17562 (search summary) | Enhancement degraded ASR in all 40 configurations tested (medical speech, 4 systems) |
| AssemblyAI blog (vendor, quotes Krisp tests) | BVC -> VAD: false VAD triggers down 3.5x on average; BVC -> VAD -> Whisper v3 raised WER ~2x. Advice: noise-cancelled audio to VAD/turn-taking, original audio to STT; 10-15 ms added |
| Twilio ConversationRelay blog (vendor) | Offline preprocessing libraries (`noisereduce`, ElevenLabs isolation) "not compatible" with their pipeline; their fix for noise was `interruptSensitivity="low"`, `ignoreBackchannel="true"` and the Deepgram `flux` model |

Take-away: a **dry/wet blend of 12 dB (weight 0.25 on the raw signal)** is the only cheap, evidence-backed way to
make a denoiser "ASR-safer", and the expected gain is at best parity with raw audio for Whisper-class recognizers.
It is not an ASR improvement. For the VAD path nobody published a blended-denoiser measurement; ours shows the
full-strength version adds fragments.

**Model candidates (denoiser CPU table is in `noise-suppression.md`)**

| Model | License (code / weights) | Native rate | Evidence for ASR or VAD benefit | CPU per stream |
|---|---|---|---|---|
| Hush (weya-ai) + crate `hush-vani` 0.1.2 | Apache-2.0 / Apache-2.0 (card). Crate Apache-2.0, pure Rust, AVX2 dispatch, weights embedded, 1,149 downloads, created 2026-07-14, updated 2026-10-29 | 16 kHz only; card calls it "telephony-native (G.711, WebRTC, SIP)" | **None published** (no WER, no VAD numbers). Design: ~60 % of training samples have a competing speaker at 12-24 dB SIR, trained on LibriSpeech/VCTK/Common Voice speech, DNS/FreeSound/ESC-50/AudioSet noise, RIRs. Card: very loud competing speakers may not be fully suppressed; a louder-noise model is "coming" | claimed "under 1 ms per 10 ms of audio" (<= 10 %), crate claims RTF ~0.01; hardware unspecified; 20 ms latency, no lookahead |
| DPDFNet 8 kHz (`dpdfnet2_8khz`, `dpdfnet8_8khz`; Ceva) | Apache-2.0 / Apache-2.0 (HF card); the *paper* is CC BY-NC-ND | **8 kHz native** (also 16, 48) | PESQ/STOI/DNSMOS/NISQA only, **no WER** (GitHub and arXiv abstract). Over-attenuation loss and "always-on" fine-tune. Training bandwidth handling not described | 1.29 / 3.99 G MACs, 9.7 / 13.8 MB ONNX; no CPU RTF published. Our estimate ~15-20 % of a core (scaled from DFN3 RTF 0.046 by MACs; see `noise-suppression.md`) |
| UL-UNAS / GTCRN | MIT / repo MIT | 16 kHz | No ASR eval in either paper | 33-35 M MACs; measured 4-7 % (four sources in `noise-suppression.md`) |
| FastEnhancer-T/B | MIT / weights license not stated separately (UNVERIFIED) | 16 kHz | Ours: worse on 16 kHz, mixed on 8 kHz | measured by us: 1.56-1.76 % |
| RNNoise via `nnnoiseless` | BSD-3-Clause both | 48 kHz | Ours: worse | measured by us: 0.38 % |
| DFN2/3 | MIT OR Apache-2.0, weights UNVERIFIED | 48 kHz | Blog above (cap = parity) | 4-5 % measured by others; repo idle since 2024-10 |
| Personalized DeepFilterNet2 (arXiv 2404.08022), Personalized PercepNet, LGTSE/D-LGTSE (arXiv 2508.19583), TargetVoice (Interspeech 2025) | **no code or weights found** (abstract pages list none) | - | papers only | TargetVoice: 12 MB extractor + 10 MB speaker module, 6 G MACs (search snippet, UNVERIFIED) |

Not usable today: any 2026 ultra-small model without released weights (CoFi-Lite, HALO, LiSenNet redesign; see
`noise-suppression.md`).

## 3. Background speakers: primary-speaker detection

**What the closed products do** (for scale of the problem):
- LiveKit (docs fetched): Krisp VIVA "emphasizes the primary speaker and removes competing voices" (extra cost; telephony
  variant tuned for SIP; level 0-100, default 75); Krisp NC / ai-coustics QUAIL_L remove background *noise* only;
  QUAIL_VF_S/L are voice-focus models for near-field mics. Gym-sample WER on that page: original 117.6 %, Krisp VIVA
  11.8 %, QUAIL_VF_S 7.1 %, QUAIL_VF_L 14.3 % (one clip, vendor docs, claimed). Do not enable the same cancellation in
  the client and the agent ("trained on raw audio"). No license is stated; hosted on LiveKit Cloud.
- Pipecat: `KrispVivaFilter` (needs a Krisp SDK license and `api_key`, rates 8-48 kHz, `noise_suppression_level`
  default 100, optional TTS-detection mode that passes audio untouched until the bot stops speaking) and
  `RNNoiseFilter` (pyrnnoise, runs at 48 kHz) as `audio_in_filter`.
- Krisp BVC needs no enrollment (Krisp blog via search result); how it picks the primary voice is not published.

**Open options**

| Approach | Components and licenses | Cost | What it fixes | What it does not |
|---|---|---|---|---|
| Speaker-embedding veto of barge-in candidates (enroll on the caller's first utterance, compare the first ~0.5-1.5 s of a candidate during bot speech, veto only if clearly a different speaker) | WeSpeaker toolkit Apache-2.0; its pretrained models "follow the license of their corresponding dataset" (VoxCeleb = CC BY 4.0 per WeSpeaker docs; VoxCeleb catalogs disagree between CC BY and BY-SA, original YouTube copyright stays with owners: legal read needed). ONNX: `wespeaker_en_voxceleb_CAM++.onnx` 29.3 MB, `..._resnet34.onnx` 26.5 MB (csukuangfj HF mirror of the sherpa-onnx models). 3D-Speaker (Apache-2.0): CAM++ 7.18 M params; the pretrained zh models are trained on a "200k-speaker Mandarin dataset" and have no separate license statement (UNVERIFIED). Runtime: `sherpa-onnx` crate 2.0.1 (Apache-2.0, per `noise-suppression.md`) exposes a speaker-embedding API in its C API (docs found by search) | CAM++ CPU RTF 0.013 single thread (paper Table 3; CPU model not named in what was read), ResNet34 0.032, ECAPA-TDNN 0.033. A 1 s candidate segment ~13 ms (estimate). Run only on VAD candidates while the bot speaks, plus once at enrollment | the "TV or a person nearby starts talking while the bot talks" barge-in, decided before the LLM is cut | transcripts that already contain the background voice (STT sees it); anything that needs separation |
| Personal VAD / target-speaker VAD (Google 2019; HyWA, Huawei, arXiv 2510.12947 v2 2026) | Papers; no code or weights found; HyWA paper license CC BY-NC-ND | small | frame-level target/non-target | not buildable without training |
| Target speaker extraction (WeSep, Apache-2.0) | Recipes for BSRNN, SpEx+, DPCCN, TFGridNet; README: official pretrained models "coming soon", ONNX export not available, streaming not mentioned, last updated 2026-09-21 | n/a | - | no weights, no streaming |
| Diarization (3D-Speaker pipeline Apache-2.0; pyannote and NVIDIA Sortformer not evaluated) | 3D-Speaker overlap detection needs a Hugging Face token | offline-oriented | analysis only | not a real-time gate |
| Hush (section 2) | Apache-2.0 | claimed ~1-10 % | designed to suppress competing speakers inside the denoiser | no published evidence; may attenuate a quiet caller |

**Facts that limit the speaker-veto idea**
- Short segments are the weak point: one paper (search-result summary only, UNVERIFIED in detail) reports a
  Thin-ResNet34 EER of about 13.2 % at 1 s, 7.7 % at 2 s and 5.5 % at 3 s, versus 3.2 % on full utterances. An
  ERes2NetV2 result of 1.48 % EER at 2 s on VoxCeleb1-O also comes from a search summary only. No source fetched covers
  8 kHz mu-law audio or Turkish for these models: both UNVERIFIED. Enrolling from the *same call* (same channel, same
  codec) removes the channel mismatch, which is the argument for self-enrollment.
- A veto must fail open: only drop a candidate on a clearly non-target score, never gate the first turn, and never
  gate when enrollment is shorter than the model needs (a floor of ~3 s of speech is a design choice, not a fetched
  number).
- WeSpeaker pretrained models: "LM" suffix = large-margin fine-tuning, said to work better on audio longer than 3 s.

**Level-relative gating (no model)**: Vapi's "Fourier denoising" (blog and docs fetched) is not spectral
subtraction in the usual sense. It keeps a rolling window (default 3000 ms, range 1000-30000), takes the 85th
percentile (range 1-99) of recent levels as the primary speaker's baseline, drops audio more than
`baselineOffsetDb` (default -15, range -30..-5) below it, updates every 20 ms, and a media-detection mode (low level
variance, sustained moderate level over 1 s) switches to -20 dB and `staticThreshold` -35 -> -30 dB. Claims (vendor,
no method): < 0.5 ms added, 73 % less background-speech interference in call centers. Limitations stated by Vapi:
artifacts for headphone users, unsuited to dynamic environments; docs say never use it alone. Pipecat's analogue is
`min_volume` (docs call `confidence` and `min_volume` "blunt instruments for noisy audio"). Both need only a
loudness estimate, i.e. ~0 CPU, and exploit that background voices are usually quieter than the caller.

## 4. Engine-level policies (no model)

**What brisvox already does** (`docs/ARCHITECTURE.md` "Interruption policy", read today; `crates/brisvox-agent`
`interrupt` module): a VAD start while the bot talks is only a *candidate*; no `Interrupt`, no epoch bump, no
`UserTurnStarted` until the guards pass. Guards: `min_speech` (300 ms with `recommended()`) with `vad_hangover`
100 ms, `min_words` (default 2, `Confirm::All` or `Any`), echo window (400 ms, one word lifts it), backchannel lists
(EN/TR, untuned guesses), transcript wait 350 ms, backchannel cap 1.5 s. At the VAD stop a word waives `min_speech`.
A vetoed candidate gets no reply. Known gaps the doc itself lists: the echo guard is lifted by *any* interim words, so
the bot's own echo (transcribed by STT) can confirm a barge-in; "comparing the interim with the bot's currently played
text would close this gap and is a possible follow-up". There is no noise-floor logic, no level gate, no speaker logic.

**What others do** (docs fetched 2026-10-31)

| System | Mechanisms |
|---|---|
| LiveKit Agents | `interruption` options: `min_duration` (s), `min_words` (needs STT), `mode` `adaptive` or `vad`, `false_interruption_timeout`, `resume_false_interruption` (agent resumes where it stopped when speech produced no words), `discard_audio_if_uninterruptible`. **Adaptive mode** = a trained audio model on the first few hundred ms of overlap speech (waveform shape, onset, duration, prosody; audio encoder + CNN): "86 % precision, 100 % recall at 500 ms overlap", rejects 51 % of VAD barge-ins, <= 30 ms inference, median 216 ms of audio to trigger (claimed, own eval); runs in LiveKit Cloud, open-sourcing not stated; local dev gets 40,000 free requests/month; docs warn it can mistake a real interruption at the start/end of an agent turn for a backchannel. Silero VAD plugin defaults: `activation_threshold` 0.5, `min_silence_duration` 0.55 s, `min_speech_duration` 0.05 s, `prefix_padding_duration` 0.5 s |
| Pipecat | Interruptions on by default; `MinWordsUserTurnStartStrategy(min_words=3)` (example), `VADUserTurnStartStrategy`, transcription-based fallback strategy ("the first strategy to decide starts the turn"); Krisp VIVA "interruption prediction" strategy to separate backchannels (Krisp SDK); `empty_user_turn` config for turns with no transcript (default: run the LLM once). `VADParams`: `start_secs` 0.2, `stop_secs` 0.2, `confidence`/`min_volume` documented as blunt instruments; docs recommend an upstream audio filter (Krisp VIVA, ai-coustics, RNNoise) rather than tuning them |
| Vapi | `stopSpeakingPlan.numWords`, `voiceSeconds` default 0.2 ("higher reduces false triggers from background sounds"), `backoffSeconds` default 1; Smart Denoising (Krisp, `enabled` default false) and Fourier Denoising (section 3); `backgroundSound` defaults `office` for phone, `off` for web |
| Twilio ConversationRelay | `interruptSensitivity` (low raises the bar for background sound), `ignoreBackchannel`, Deepgram `flux` for noisy conditions |

Comparison: the framework docs describe duration and word-count filters like ours; none of the fetched pages describes
a pending-candidate state with an echo window (absence in the docs, not proof). Missing in brisvox relative to the
field: (a) a resume after a false interruption (LiveKit); (b) an audio-level interruption classifier (LiveKit
adaptive; closed); (c) any level or speaker information in the decision. (a) matters only for confirmed-but-false
barge-ins; with `Confirm::All` and `min_words >= 2` such a barge-in needs STT words, i.e. real or background speech,
so (a) would not fix the background-voice case.

**Model-free ideas that have no evidence yet (all UNVERIFIED as improvements; each is measurable with the harness):**
1. Post-guard metric: replay the VAD events of every mix through the real `InterruptionPolicy` (the `min_speech` guard
   needs no STT) and report false barge-ins that survive, per quiet minute.
2. VAD sweep: threshold 0.5 / 0.6 / 0.7 (Pipecat example confidence 0.7), `neg_threshold`, `min_silence` 100 / 200 /
   300 ms (fragments), and a volume gate. Trade-off: longer `min_silence` delays the stop and so the end-of-turn
   work.
3. Noise-floor-adaptive guards: scale `min_speech` / require `min_words` when the running noise floor (e.g. a low
   percentile of 20 ms frame energy over 3-5 s of non-speech frames) is high. No fetched doc describes this in LiveKit,
   Pipecat or Vapi (their guards are static); the idea is ours.
4. Echo-text match: veto a confirmed-by-words candidate whose interim text matches the last ~N words the bot was
   playing (the follow-up the architecture doc names). Zero CPU.
5. Acoustic echo cancellation: `aec3` 0.4.0 (crates.io: license `MIT OR BSD-3-Clause`, pure-Rust port of WebRTC AEC3,
   updated 2026-09-16, 52,823 downloads, README: work in progress, only 48 kHz in the examples, 10 ms frames, render
   and capture frames fed separately) and `webrtc-audio-processing` 2.1.0 (crates.io license field "non-standard",
   C++ wrapper). Brisvox knows the bot's output signal, so AEC is possible, but it needs delay alignment on the
   caller's network and is a large piece of work. Not for the next round.

## 5. Noise-robust VAD instead of a denoiser

| VAD | License | Rate | Noise evidence (fetched) | Verdict for brisvox |
|---|---|---|---|---|
| **Silero v6.2.3** (in use) | MIT (GitHub API SPDX) | 8 and 16 kHz | v6.0 (2025-08-26): "16 % fewer errors on noisy real-life data", "11 % fewer on multi-domain validation"; v6.2 (2025-11-06): reworked training, more stable on OOD data and low-quality phone calls (all maintainer-published). Maintainer wiki: noise-only accuracy ESC-50 v5 0.61 -> v6 0.87, private noise 0.44 -> 0.71; speech sets ROC-AUC v6 = v5 (MSDWild 0.79, VoxConverse 0.94, Libriparty 0.97) | keep; tune parameters (section 4) |
| TEN VAD | Apache-2.0 **with additional conditions** (LICENSE fetched): may not Deploy in a way that competes with Agora's offerings or lets others compete; Deploy only for your own Applications and their direct End Users; derivatives stay under it | 16 kHz only | wiki ESC-50 0.42, MSDWild 0.77 / VoxConverse 0.92 / Libriparty 0.92 ROC-AUC. arXiv 2610.14607 (abstract fetched; PDF unreadable): TEN ROC AUC 0.917 on LibriVAD-small (9 noise families, -5..20 dB), near chance on babble at low SNR; Silero comparable near 32 ms windows, 0.63 at 20 ms (authors suspect their protocol) | **Rejected**: the condition conflicts with unrestricted open-source redistribution. Needs a lawyer, not a benchmark |
| FireRedVAD (FireRedTeam, 2026) | Apache-2.0 (GitHub API and HF card) | 16 kHz | FLEURS-VAD-102 (self-reported, clean read speech, fixed 0.5): F1 97.57, FAR 2.69, MR 3.62 vs Silero 95.95 / 9.41 / 3.95, TEN 95.19 / 15.47 / 2.95, WebRTC 52.30 / 2.83 / 64.15. **No SNR evaluation in the report.** Silero wiki (maintainer): FireRed streaming is worse on noise-only (ESC-50 0.39, private 0.49) and on Libriparty (0.95 AUC vs 0.97) | 0.6 M params DFSMN, zero lookahead streaming, ~2.2 MB fp32, ONNX folder in the repo; feature frontend would need a Rust port. Low priority: no evidence it beats Silero in noise |
| NVIDIA Frame-VAD Multilingual MarbleNet v2.0 | NVIDIA Open Model License ("ready for commercial use"); runtime listed as NeMo 2.3.0 (Linux, NVIDIA GPUs listed), ONNX export shown | 16 kHz, 20 ms output | 91.5 K params; trained with MUSAN/Freesound noise; ROC-AUC 92-97.6 on diarization sets; no comparison with Silero | **Not MIT/Apache/BSD**; skip |
| WebRTC VAD | BSD-3-Clause (libfvad), MIT wrapper (`vad-alternatives.md`) | 8-48 kHz (UNVERIFIED) | maintainer wiki: ROC-AUC 0.62-0.79 on noisy speech sets, noise-only accuracy 0 on ESC-50, "pretty poor at separating speech from noise" | only as a pre-gate; no |
| kiloVAD (arXiv 2607.25870) | abstract does not say | - | 2.1 k params, AUC 0.850 on AVA-Speech, 200 ms context | irrelevant at our accuracy bar |

Wiki caveat: the Silero maintainer's tables use thresholds tuned on their validation set and a harsh noise metric (a
file counts as speech after 100 ms of speech in a row). FLEURS-VAD-102 is not open yet, so FireRedVAD cannot be
reproduced.

## 6. 8 kHz telephony

- Native 8 kHz denoisers: only DPDFNet (Apache-2.0; 1.29 / 3.99 GMACs). Everything else is 16 or 48 kHz, so on a
  phone call it processes upsampled narrowband audio. Our FastEnhancer result on that path is the worst of the run
  (section 1). Hush is described as telephony-native on its card but is a 16 kHz model.
- Silero supports 8 kHz natively; v6.2 mentions "low-quality phone calls" among its edge-case gains (release notes).
  Smart Turn takes 16 kHz mono (README); its README does not mention 8 kHz or noise robustness.
- STT side: Deepgram's model docs list phone-call variants (`nova-phonecall`, `enhanced-phonecall`, `base-phonecall`) for
  low-bandwidth audio; native 8 kHz mu-law is a supported streaming input (OpenClaw's provider doc). No fetched source
  compares native 8 kHz against upsampled audio.
- Bandwidth extension: the only telephony-specific result found is Interspeech 2023 (Lin et al., Embedding-Polished
  Wave-U-Net, jointly denoises and extends 8 kHz): "11.71 % WER reduction" on a bank IVR system, baseline not named on the
  abstract page; code release not stated. A 2014 paper cites ~20 % relative WER reduction from doubling the sample
  rate (search snippet), a 2015 study 8.67 -> 8.26 % on WSJ. No 2024-2026 measurement on real call audio was found.
  Not a candidate (no code, no weights, no real evidence).
- Practical consequence: on the telephone path the safest default is no denoiser; the only model worth a measurement is
  an 8 kHz-native one, and DPDFNet is too heavy for hundreds of calls unless measured otherwise.

## 7. Ranked shortlist: build and measure next

Harness additions needed by all items (cheap, free, local): (a) **competing-speech mixes**: a second utterance from
another fixture (other language and speaker) mixed at -5 / -10 / -15 / -20 dB relative to the primary, in both paths,
with and without "bot speaking" (play a synthetic bot signal into the same stream, or at least score the start
timing); MUSAN `speech/` is available if the 10 GB archive is already local, else our own fixtures; (b) a
**post-guard false-barge-in** metric by replaying VAD events through `InterruptionPolicy`; (c) a **dry/wet blend**
parameter on every denoise backend (`out = w*raw + (1-w)*enhanced`, w = 0.25 for the 12 dB cap); (d) a second local
recognizer for WER (whisper.cpp server is documented in `local-speech.md`; Whisper behaves differently from the
zipformer per arXiv 2607.11157; Turkish coverage of the local options is in that file).

| Rank | What | Why (one line) | CPU per call | License verdict | Effort |
|---|---|---|---|---|---|
| 1 | **Model-free front gating bundle**: VAD sweep (threshold, neg_threshold, min_silence), volume gate, Vapi-style rolling-percentile level gate (P85 of caller speech, 3 s, -15 dB), noise-floor-adaptive `min_speech`/`min_words`, echo-text match | Targets exactly what remains with Silero (fragments, a few false starts, quieter background voices) at zero cost; the post-guard metric may show noise false barge-ins are already close to zero, which would settle the question | ~0 (a few ns-us per 20 ms frame, estimate) | own code, none needed | small |
| 2 | **Speaker-embedding veto of barge-in candidates** (WeSpeaker CAM++ ONNX via the Apache-2.0 `sherpa-onnx` crate, enroll from the caller's first utterance, veto only clear non-target) | The only open, permissive way to separate "caller" from "TV / other person" at the moment of a barge-in; fails open | enrollment once + ~13 ms per 1 s candidate (CAM++ RTF 0.013, estimate); average per call well below 1 % | code Apache-2.0; weights CC BY 4.0 via VoxCeleb (attribution; sources disagree BY vs BY-SA; legal read before shipping; Turkish and 8 kHz behaviour UNVERIFIED) | medium |
| 3 | **Hush (hush-vani) as a harness backend + blend knob**, tested on the new competing-speech mixes, VAD path first | Only open permissive model built for voice-AI with background-speaker suppression; pure Rust, no ORT; quick to wire; also gives the first measurement of the blend knob on our fixtures | claimed ~1 % (RTF ~0.01) to <= 10 %; measure with `denoise cpu` | Apache-2.0 code and weights (card); one-person crate, vendor or pin; training data includes DNS/AudioSet mixes (their terms not checked) | small |
| 4 | **DPDFNet `dpdfnet2_8khz` ceiling check** on the telephone path (VAD path only, a few fixtures) | If even an 8 kHz-native model does not cut fragments and flips on mu-law audio, the denoiser line is closed for telephony; if it does, we know what a distilled/cheaper 8 kHz model must reach | estimate ~15-20 % of a core (not viable at hundreds of calls; measure before dropping) | Apache-2.0 code and HF weights; the arXiv paper is CC BY-NC-ND (paper only) | medium (ONNX streaming graph layout UNVERIFIED; Python `StreamEnhancer` is the reference) |

Do not build now: TEN VAD (license), MarbleNet (license/runtime), FireRedVAD (no noise evidence, port cost), GTCRN /
UL-UNAS (no ASR/VAD evidence, 4-7 % CPU), bandwidth extension (no code), LiveKit adaptive interruption (hosted),
Krisp VIVA / ai-coustics (proprietary SDK licenses), AEC (later, large).

## 8. Open points

- Whether the recognizer used in production (Deepgram, Cartesia/others) behaves like Whisper (U-shaped) or like the
  local zipformer: one short cloud A/B on a handful of clips is the cheapest way to know; not free in the strict sense
  (provider credits).
- Weights license text for FastEnhancer, Hush's training mix (DNS/AudioSet/FreeSound terms), and WeSpeaker's VoxCeleb
  attribution requirement: legal read.
- `aec3` crate: the license of the ported code beyond the crates.io field (`MIT OR BSD-3-Clause`) and its PATENT file.
- Turkish: VAD and speaker models are language-agnostic in design, but no fetched source gives Turkish figures for
  any model in this note (FLEURS contains Turkish; FireRed's per-language results are not published).
- PDFs that WebFetch could not read (arXiv 2610.14607 body, 2607.25870 body, TargetVoice, 2404.08022 body, the
  CAM++ PDF): only abstract pages or search snippets were used for those (the CAM++ efficiency table came from the
  arXiv html v3).

## 9. Measured afterwards (2026-10-31, same day)

The shortlist items 1 and 3 were built and measured with a harness that drives the real turn machine and
interruption policy over calls with the bot speaking, background speech (LibriSpeech, CC BY 4.0), bot echo and
MUSAN noise: `bench/results/noise-handling-calls.md`, reading in `docs/LATENCY.md`, "Noise handling beyond a
denoiser". In short: noise never gets through the guards; what does is a second voice and the bot's own echo;
nothing model-free or denoiser-based separates a voice within 10 dB of the caller (item 2, the speaker check,
stays open); `min_words >= 1` plus the echo-text match removes the echo case completely (cost: the first-interim
wait); Hush helps with voices 15 to 20 dB down and the echo at 1 % of a core and no added delay, and fragments
more on 8 kHz phone audio; the rolling-percentile level gate works at the start of speech only (applied to every
window it adds 70 to 160 fragments to the 48 barge-in calls); noise-floor-adapted guards cost 300 ms on
every noisy call for no gain; VAD threshold and hangover sweeps do not change the false barge-ins. Shipped as
agent-file options: `noise_suppression = "hush"` (experimental, see below), `interruption.echo_text_match`,
`[vad] level_gate_db`. **Correction**, found by the word error rate run: Hush's gain depends on the input level
(it removes the speech of a caller about 20 dB under the fixtures' level, attenuates normal-level speech by 8 dB
and phone-band speech by 12 dB), so it is skipped on calls below 16 kHz, left out of the default features and
checked by `brisvox-bench denoise safety` (`docs/LATENCY.md`).
The Hush crate facts in section 2 were confirmed: `hush-vani` 0.1.2 (2026-10-29), Apache-2.0, depends on
`rustfft` only, `HushStream` streams 160-sample frames, weights load once per
process (8 ms), measured 0.20 ms per 20 ms frame.

## Sources (all fetched 2026-10-31)

- Local: `docs/providers/noise-suppression.md`; `docs/providers/vad-alternatives.md`; `docs/ARCHITECTURE.md` "Interruption
  policy"; `docs/CONFIG.md` (interruption fields); `crates/brisvox-inference/src/vad.rs`; worktree
  `bench/results/denoise-quality-musan-background.md`, `denoise-cpu.md`, `denoise-wer-sherpa-en.md`.
- Silero: https://api.github.com/repos/snakers4/silero-vad/releases ; https://api.github.com/repos/snakers4/silero-vad/license ;
  https://github.com/snakers4/silero-vad/wiki/Quality-Metrics ; https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.3/src/silero_vad/utils_vad.py
- TEN VAD: https://github.com/TEN-framework/ten-vad ; https://raw.githubusercontent.com/TEN-framework/ten-vad/main/LICENSE ;
  https://arxiv.org/abs/2610.14607
- FireRedVAD: https://github.com/FireRedTeam/FireRedVAD ; https://api.github.com/repos/FireRedTeam/FireRedVAD ;
  https://huggingface.co/FireRedTeam/FireRedVAD ; https://arxiv.org/html/2603.10420v1
- MarbleNet: https://huggingface.co/nvidia/frame_vad_multilingual_marblenet_v2.0
- kiloVAD: https://arxiv.org/abs/2607.25870
- Enhancement vs ASR: https://arxiv.org/html/2607.11157v1 ; https://arxiv.org/abs/2512.17562 (via search summary) ;
  https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/DeepFilterNet/df/enhance.py ;
  https://www.creativeainews.com/articles/deepfilternet-vs-rnnoise-vs-ffmpeg-noise-removal-2026/ ;
  https://www.assemblyai.com/blog/noise-cancellation-stt-pros-cons ;
  https://www.twilio.com/en-us/blog/developers/tutorials/product/handle-background-noise-when-using-conversation-relay
- Hush: https://huggingface.co/weya-ai/hush/raw/main/README.md ; https://crates.io/api/v1/crates/hush-vani
- DPDFNet: https://github.com/ceva-ip/DPDFNet ; https://huggingface.co/Ceva-IP/DPDFNet ; https://arxiv.org/abs/2512.16420
- Personalized / target speaker: https://arxiv.org/abs/2404.08022 ; https://arxiv.org/abs/2508.19583 ;
  https://github.com/wenet-e2e/wesep ; Personal VAD https://arxiv.org/pdf/1908.04284 and HyWA https://arxiv.org/html/2510.12947v2
  (search summaries); TargetVoice https://www.isca-archive.org/interspeech_2025/pallala25_interspeech.pdf (search snippet only)
- Speaker embeddings: https://github.com/wenet-e2e/wespeaker ; https://github.com/wenet-e2e/wespeaker/blob/master/docs/pretrained.md ;
  https://github.com/modelscope/3D-Speaker ; https://arxiv.org/html/2303.00332v3 ;
  https://huggingface.co/csukuangfj/speaker-embedding-models/tree/main ; https://k2-fsa.github.io/sherpa/onnx/speaker-identification/index.html ;
  https://k2-fsa.github.io/sherpa/onnx/c-api/html/speaker_embedding.html (search result) ; VoxCeleb license catalog
  https://mm.kaist.ac.kr/datasets/voxceleb/files/license.txt (search result only; Oxford page unreachable)
- Frameworks: https://docs.livekit.io/agents/build/turns/ ; https://docs.livekit.io/agents/build/turns/vad/ ;
  https://livekit.com/blog/adaptive-interruption-handling ; https://docs.livekit.io/transport/media/noise-cancellation/ ;
  https://docs.pipecat.ai/guides/learn/speech-input ; https://docs.pipecat.ai/pipecat/fundamentals/interruptions.md ;
  https://docs.pipecat.ai/api-reference/server/utilities/audio/krisp-viva-filter ;
  https://docs.pipecat.ai/api-reference/server/utilities/audio/rnnoise-filter ;
  https://docs.vapi.ai/customization/speech-configuration ;
  https://docs.vapi.ai/documentation/assistants/conversation-behavior/background-speech-denoising ;
  https://vapi.ai/blog/how-we-built-adaptive-background-speech-filtering-at-vapi
- Echo: https://crates.io/api/v1/crates/aec3 ; https://github.com/rubybit/aec3-rs ; https://crates.io/api/v1/crates/webrtc-audio-processing
- Telephony: https://www.isca-archive.org/interspeech_2023/lin23f_interspeech.html ; Deepgram/phone-model search results
  (docs.openclaw.ai Deepgram provider page, developers.deepgram.com/docs/model, search snippets only)
- Smart Turn: https://github.com/pipecat-ai/smart-turn
