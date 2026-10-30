# Language switching mid-call — STT language detection, TTS voice per language

- Fetched: 2026-10-30. Every fact below carries the URL it came from. "UNVERIFIED" = not found in a
  fetched official page, or only seen in a search snippet.
- Scope: the providers brisvox implements today (`crates/brisvox-providers/src/`): STT `deepgram`,
  `assemblyai`, `gladia`, `openai_realtime_stt`, `sherpa_onnx`; TTS `cartesia`, `elevenlabs`,
  `deepgram_aura`, `openai_speech` (local servers: Kokoro, Chatterbox, Speaches, vLLM-Omni); speech-to-speech
  `openai_realtime`, `gemini_live`. Speechmatics is NOT a brisvox provider and was not researched (its
  language-identification page returned 404 on 2026-10-30).
- Current repo shape (read from source, not a provider fact): `SttConfig.language: Option<Arc<str>>` is the only
  language input; `SttEvent::Transcript { text, is_final, speech_final, from_finalize }` carries no language;
  `TtsConfig { sample_rate, voice }` carries no language (voice is one provider id fixed per session). The Deepgram
  adapter defaults to model `nova-3`.

---------------------------------------------------------------------------------------------------

## 1. STT (streaming)

### 1.1 Deepgram (`wss://api.deepgram.com/v1/listen`)

- Auto language detection (`detect_language`) is NOT available when streaming. Quote: "Language Detection is
  not currently supported for streaming." Its batch fields are `detected_language` / `language_confidence`
  (35 languages). Source: https://developers.deepgram.com/docs/language-detection
- Streaming multilingual / code-switching = `model=nova-3&language=multi` (also Nova-2 and `nova-3-medical`;
  Flux Multilingual `flux-general-multi` on `/v2/listen`). Source:
  https://developers.deepgram.com/docs/multilingual-code-switching
  - Nova-3 `multi`: "transcribes 55 languages with one model, including speech that switches languages
    mid-conversation." Turkish is in the supported table (`tr`), so `language_hint=tr` is valid. Source:
    https://developers.deepgram.com/docs/nova-3-language-prompting
  - Optional repeatable `language_hint=<code>` (e.g. `language_hint=en&language_hint=tr`) biases detection; hints
    "bias detection; they do not restrict it". Only effective with `language=multi`, ignored otherwise; Nova-2 has
    no hints; an unsupported code is HTTP 400 `INVALID_QUERY_PARAMETER` (streaming: WebSocket rejected with 400).
    Locale subtags (`en-GB`) accepted and returned as base codes. Source: same two pages.
  - Docs recommend `endpointing=100` (ms) for code-switching. Source: multilingual-code-switching page.
  - Language is fixed in the URL at connect: there is no documented mid-stream message to change `language`
    (UNVERIFIED either way; the Deepgram docs pages fetched do not describe one). Note `language=multi` makes
    this unnecessary because one connection hears all languages.
- Reporting (live `Results` message uses the same structure): per word `channel.alternatives[].words[].language`
  (BCP-47); per segment `alternatives[].languages` (array, sorted by descending word count) and
  `alternatives[].languages_hinted` (normalised hints, empty array if none sent). There is NO language
  confidence for `multi` (only the usual `alternatives[].confidence` and `words[].confidence`).
  Flux Multilingual reports `languages` in each `TurnInfo` event. Source: multilingual-code-switching page.
- Languages: Nova-3 monolingual and general multilingual sets include Turkish (`tr`, `tr-TR`); Flux
  (`flux-general-en`, `flux-general-multi` with 10 languages: en, es, fr, de, hi, ru, pt, ja, it, nl) does NOT
  support Turkish. `multi` on `nova-3-medical` is those 10 languages. Source:
  https://developers.deepgram.com/docs/models-languages-overview
- Accuracy/latency cost of `multi` vs fixed language: NOT stated in the docs (no benchmark, no latency figure).
  Price (streaming, pay as you go): Nova-3 monolingual $0.0077/min, Nova-3 multilingual $0.0092/min; Flux
  $0.0077/min English, $0.0078/min multilingual. Source: https://deepgram.com/pricing
- Gotcha for design: the same page lists multilingual code-switching as "Specific languages only".
  Source: https://developers.deepgram.com/docs/stt-streaming-feature-overview

### 1.2 AssemblyAI (`wss://streaming.assemblyai.com/v3/ws`)

- Models (`speech_model` query param): `universal-3-6-pro` (32 languages, native mid-sentence code-switching,
  includes Turkish), `universal-streaming-multilingual` (en, es, de, fr, pt, it only; code-switching per turn),
  `universal-streaming-english` (en only). Sources:
  https://www.assemblyai.com/docs/streaming/universal-streaming/multilingual-transcription and
  https://assemblyai.com/docs/streaming/api-spec/streaming-websocket.md (`universal-3-5-pro` also listed, with a
  smaller language set than 3.6).
- Code-switching on `universal-3-6-pro` is on by default, no configuration needed ("keeps each language as spoken
  rather than translating everything"). Source: multilingual-transcription page.
- Steering: `language_codes` (query param, array e.g. `["en","tr"]`) biases toward expected languages while still
  allowing code-switching; single item = monolingual session; omit = full multilingual switching. It can be
  changed MID-SESSION with an `UpdateConfiguration` message (`{"type":"UpdateConfiguration","language_codes":[...]}`),
  effective from the next turn, empty list clears steering, no acknowledgement. `language_detection` is NOT
  updatable. Universal-3.6/3.5 Pro only. Sources: the two pages above plus
  https://www.assemblyai.com/docs/streaming/universal-streaming/message-sequence (UpdateConfiguration: "The server
  sends no acknowledgement. The new settings apply to audio processed after the update").
- Reporting: connect with `language_detection=true` (default false; supported on Universal-3.6 Pro, 3.5 Pro and
  Universal Streaming Multilingual). Then each `Turn` message gets `language_code` (string) and
  `language_confidence` (0..1), "Only populated when language detection is enabled and an utterance is complete or
  turn is final". So it is per TURN, only on the final / utterance-complete message, not on interims, not per word.
  "`language_detection` only controls reporting and doesn't change how the model transcribes." Source:
  streaming-websocket.md + multilingual-transcription page.
- Cost: Universal-3.6 Pro Realtime $0.45/hr; Universal-Streaming Multilingual and English $0.15/hr; no multilingual
  surcharge listed; pricing for the language-detection flag is not mentioned (UNVERIFIED whether free). Source:
  https://www.assemblyai.com/pricing . Accuracy/latency cost of multilingual vs fixed: not stated.

### 1.3 Gladia live (`POST /v2/live`, model `solaria-1`)

- `language_config: { languages: [codes], code_switching: bool }` in the init request. `languages` default `[]`;
  one language = used for the whole session; none or several = auto-detect. `code_switching` default `false`:
  false = language detected on the FIRST utterance and reused for the whole session; true = detected on EACH
  utterance; ignored if exactly one language is set. Source: https://docs.gladia.io/api-reference/v2/live/init
- Config is fixed at session creation (the init call); no mid-session reconfiguration message is documented
  (UNVERIFIED beyond this; see `docs/providers/gladia.md`, which records the client messages as only audio chunks and
  `stop_recording`).
- Reporting: each `transcript` message `data.utterance.language` (BCP-47 string) per utterance. The code-switching
  page says the language code is returned "per utterance and per word" (the documented example has no per-word
  language field: per-word UNVERIFIED). There is no language-confidence field in the documented transcript schema.
  Sources: https://docs.gladia.io/chapters/language/code-switching.md ; field names `data.utterance.language`
  as already recorded from the AsyncAPI spec in `docs/providers/gladia.md` (the Mintlify transcript page itself,
  https://docs.gladia.io/api-reference/v2/live/message/transcript , is a stub without the schema).
- Limits/warnings: "Do not enable `code_switching` with an empty `languages` list" (detector evaluates every
  utterance against 100+ languages, "frequent misdetections", worst for similar-sounding languages); "Multiple
  languages or code switching can reduce partial transcript accuracy"; "For best accuracy and latency, provide a
  small set of expected languages"; setting the language explicitly avoids detection overhead. Sources: the
  code-switching page and https://docs.gladia.io/chapters/live-stt/recommended-parameters.md
- Turkish (`tr`) is supported for Solaria-1, with auto-discovery and code-switching; 100 languages listed (the page
  does not say whether the list applies to live). Source: https://docs.gladia.io/chapters/language/supported-languages.md
- Price: real-time 0.68 EUR/hour (self-serve); the page says it "may carry a surcharge for code switching or
  language detection" without giving a rate (UNVERIFIED). Source: https://www.gladia.io/pricing

### 1.4 OpenAI realtime transcription (`wss://api.openai.com/v1/realtime?intent=transcription`)

- `session.update` with `"type":"transcription"`, `audio.input.transcription.model` (recommended
  `gpt-live-transcribe`), `audio.input.turn_detection: null`. `gpt-live-transcribe` takes `languages` (PLURAL, array
  of expected input languages; do not send singular `language` with it) — "Add `languages` when more than one input
  language is expected." Codes: ISO 639-1, selected 639-3, `zh-cn|zh-tw|zh-hk`; malformed codes rejected.
  Source: https://developers.openai.com/api/docs/guides/realtime-transcription
- `gpt-live-transcribe` "doesn't return detected-language predictions". `gpt-transcribe` does: its
  `conversation.item.input_audio_transcription.completed` event carries a `languages` array such as
  `[{ "code": "fr" }]` (empty if no reliable prediction); it needs a committed audio turn and a WebSocket. Same source.
- Code-switching: the guide says to test "code-switching" with representative audio but does not describe
  mid-utterance behaviour (UNVERIFIED). Turkish is not mentioned on the page (UNVERIFIED there; check the code list).
  `transcription.delay` (`minimal`..`xhigh`) trades accuracy for latency. Pricing not fetched.

### 1.5 sherpa-onnx (local, `sherpa_onnx` adapter)

- Streaming (online) models are per language: Zipformer English, French, Korean, Chinese, Bengali; bilingual
  zh-en Zipformer and Paraformer models. No streaming model is labelled multilingual beyond zh-en, and none is
  listed for Turkish in the official lists (the Turkish model brisvox uses, `atasoglu/seda-v0.1`, is a
  community streaming Zipformer for Turkish only, Apache-2.0 acoustic model). Sources:
  https://k2-fsa.github.io/sherpa/onnx/pretrained_models/online-transducer/index.html ,
  https://k2-fsa.github.io/sherpa/onnx/pretrained_models/index.html , https://huggingface.co/atasoglu/seda-v0.1
- Spoken language identification exists but only with Whisper multilingual models (`tiny|base|small|medium`) and
  its docs example takes a whole WAV file; no streaming LID is documented. Source:
  https://k2-fsa.github.io/sherpa/onnx/spoken-language-identification/pretrained_models.html
- Consequence: no provider-reported language and no code-switching. The streaming server protocol has no language
  parameter (`docs/providers/sherpa-onnx.md`). Language switching with sherpa = run one server/model per language and
  pick by an external signal (UNVERIFIED design; not a provider feature).

### 1.6 Speech-to-speech adapters (for completeness; they replace STT+LLM+TTS)

- Gemini Live: native audio models "don't support explicitly setting the language code" and choose the language
  automatically; to restrict, say so in the system instructions; native audio models can switch languages during a
  conversation; 99 languages (BCP-47). `input_audio_transcription` returns user transcript (no language field
  documented). Voice via `speech_config.voice_config.prebuilt_voice_config.voice_name`. Source:
  https://ai.google.dev/gemini-api/docs/live-guide
- OpenAI Realtime: `session.update` can change most properties anytime EXCEPT the output `voice` after the model
  has responded with audio once in the session. Source:
  https://developers.openai.com/api/docs/guides/realtime-conversations

---------------------------------------------------------------------------------------------------

## 2. TTS

### 2.1 Cartesia (`wss://api.cartesia.ai/tts/websocket`)

- Models: `sonic-3.6` (44 languages incl. Turkish `tr`; latest), `sonic-3.5` / `sonic-3` (42 languages incl.
  `tr`). `sonic-2` and `sonic-turbo` do not list Turkish and "will stop working after October 20, 2026". Sources:
  https://docs.cartesia.ai/build-with-cartesia/tts-models/latest ,
  https://docs.cartesia.ai/build-with-cartesia/tts-models/older-models
- Request fields (every generation request carries them): `model_id`, `transcript`, `voice` (id string or object
  with `id`), `output_format`, `context_id`, `language` OR `locale` (mutually exclusive, "setting both returns an
  error"; `locale` e.g. `en-GB` needs sonic-3.6+), `continue`, `max_buffer_delay_ms`, `add_timestamps`, ... Source:
  https://docs.cartesia.ai/api-reference/tts/websocket
- One voice, many languages: "Every voice can speak [every language the model supports]." It sounds native only in
  the accents the voice has; otherwise "it falls back to its original accent". Multilingual voices have several
  `accents` (Get Voice API). "Most requests only need language or locale: the voice speaks that language".
  Source: https://docs.cartesia.ai/build-with-cartesia/capability-guides/multilingual-voices.md
- Code-switching within one request: "Send one language per request"; mixing "works where it's common" (Hinglish,
  Taglish); elsewhere it "may sound accented". Same source.
- Per-context vs per-connection: within one context "Inputs on the same context must keep all fields except
  `transcript`, `continue`, and `duration` the same" (so `language`, `model_id`, voice cannot change inside a
  context). Multiple concurrent contexts per connection are supported; the page does NOT say that two contexts may
  differ in voice/language/model on one connection (UNVERIFIED, though each request carries its own fields).
  Contexts "automatically expire 1 second after the last audio output". Source:
  https://docs.cartesia.ai/use-the-api/tts-websocket/contexts.md
- Switching language therefore = start a new `context_id` with the new `language` (+ voice if wanted). No new
  connection is documented as required (the per-context claim is the UNVERIFIED part; live-test it).
- Pricing was recorded earlier in `docs/providers/cartesia.md`; not re-fetched here.

### 2.2 ElevenLabs (`/v1/text-to-speech/{voice_id}/multi-stream-input`, `/stream-input`)

- Voice is in the URL path; `model_id` (default `eleven_multilingual_v2`) and `language_code` are QUERY params. In the
  multi-context message schema there is no per-context `voice_id`, `model_id` or `language_code`; per-context
  settings are only `voice_settings`, `generation_config`, pronunciation dictionaries. So voice, model and language
  are fixed per connection; changing voice or language = new WebSocket. Sources:
  https://elevenlabs.io/docs/api-reference/text-to-speech/v-1-text-to-speech-voice-id-multi-stream-input ,
  https://elevenlabs.io/docs/api-reference/text-to-speech/v-1-text-to-speech-voice-id-stream-input (page does not
  address changing model/language after connect: inferred from the schema).
- `language_code`: "Language code (ISO 639-1) used to enforce a language for the model and text normalization.
  If the model does not support the provided language code, it will be ignored. This parameter is not supported
  for multilingual_v2 models." (HTTP convert endpoint doc; the WebSocket pages give no description.) The models
  page documents `language_code` only for the v3 and v4 families. Sources:
  https://elevenlabs.io/docs/api-reference/text-to-speech/convert , https://elevenlabs.io/docs/overview/models
- Models/languages (Turkish yes in all): `eleven_flash_v2_5` 32 languages ~100 ms median; `eleven_multilingual_v2`
  29; `eleven_v3` 70+ (85 listed); `eleven_v4` / `eleven_v4_turbo` 90+ (`eleven_v4_turbo` ~100 ms). Without
  `language_code` the model picks the language from the text (UNVERIFIED as a documented statement; consistent with
  "ignored if unsupported"). Source: models page.
- One voice, many languages: "All ElevenLabs voices support multiple languages."; accent follows the voice, so for
  natural results "Match your voice selection to your target region". Source:
  https://elevenlabs.io/docs/overview/capabilities/voices . (Search snippet only, not fetched: pre-made voices are
  English-accented in other languages; Eleven v4 generates the target-language accent. UNVERIFIED.)
- Price: Flash/Turbo $0.04 per 1K chars; Multilingual v2 and v3 $0.08 per 1K chars; no language surcharge.
  Source: https://elevenlabs.io/pricing/api

### 2.3 Deepgram Aura-2 (`wss://api.deepgram.com/v1/speak`)

- Aura-2 languages: en, es, de, fr, nl, it, ja. NO Turkish. Voice id = model: `aura-2-<name>-<lang>`
  (`aura-2-thalia-en`, `aura-2-celeste-es`, `aura-2-julius-de`, `aura-2-agathe-fr`, `aura-2-rhea-nl`,
  `aura-2-livia-it`, `aura-2-fujin-ja`). Each voice is bound to one language/accent. Only the Spanish voices
  Aquila, Carina, Diana, Javier and Selena are listed as able to switch between English and Spanish. Source:
  https://developers.deepgram.com/docs/tts-models
- Voice cannot change on an open connection: "After you establish a connection, you will not be able to change the
  voice or media output settings"; use one WebSocket per conversation. Language switch = new connection with the
  other voice's `model=`. Source: https://developers.deepgram.com/docs/streaming-text-to-speech
- Price $0.030 per 1,000 characters. Source: https://deepgram.com/pricing

### 2.4 OpenAI-compatible local servers (`openai_speech` adapter)

- Kokoro-82M: voice id prefix selects language: `af_/am_` American English, `bf_/bm_` British English, `ef_/em_`
  Spanish, `ff_` French, `hf_/hm_` Hindi, `if_/im_` Italian, `jf_/jm_` Japanese, `pf_/pm_` Brazilian Portuguese,
  `zf_/zm_` Mandarin; the `KPipeline` `lang_code` letter (`a b j z e f h i p`) matches. No Turkish voices. Whether a
  voice may speak another language is not stated (UNVERIFIED). License Apache-2.0. Sources:
  https://huggingface.co/hexgrad/Kokoro-82M/blob/main/VOICES.md , https://huggingface.co/hexgrad/Kokoro-82M
- Chatterbox Multilingual: 23+ languages incl. Turkish; `language_id` is a per-`generate()` parameter; cross-language
  voice cloning works but the reference clip "matches the specified language tag" or the output may take the clip's
  accent; MIT. So language switch is per request there (the HTTP server wrapper's field mapping is in
  `docs/providers/openai-speech.md`). Source: https://huggingface.co/ResembleAI/chatterbox
- These servers take one HTTP request per text piece, so a different voice/language per piece needs no connection
  change (the adapter holds a voice per session today).

---------------------------------------------------------------------------------------------------

## 3. LLM side (brief)

- OpenAI voice-prompting guide (language rules for gpt-realtime-2; useful as a pattern for any LLM): "English is the
  default response language."; "Only switch languages if the user explicitly asks or provides a substantive
  utterance in another language" (complete request, question or correction); do NOT switch on accent, filler words,
  short backchannels, names/addresses, isolated foreign words; keep preambles, tool messages and answers in the same
  language; if unsure ask "Would you like me to continue in English or [LANGUAGE]?". It discourages broad
  "Mirror the user" instructions and says "Pin output to a target language if you see unwanted language switching."
  Source: https://developers.openai.com/api/docs/guides/voice-prompting
- Live-prompting guide: "Use the language configured by your application until the caller chooses another";
  "Write the prompt and examples in the language you want the assistant to speak". Source:
  https://developers.openai.com/api/docs/guides/live-prompting
- Pattern for brisvox (recommendation): keep the system prompt stable and add one injected line per turn, e.g.
  `Reply in <language name>.` (set by the engine's current language), instead of rewriting the prompt; tool
  results/messages in the same language.

---------------------------------------------------------------------------------------------------

## 4. Summary table (design-relevant)

| Provider | Detects language while streaming? field | Code-switching ok? | Notes |
|---|---|---|---|
| Deepgram Nova-3 `language=multi` | Yes, no separate detect: `words[].language` (per word), `alternatives[].languages` (per segment, by word count); no confidence | Yes (55 languages incl. tr; `language_hint` biases) | `detect_language` NOT supported streaming; set at connect; `endpointing=100` advised; $0.0092/min vs $0.0077; accuracy cost not documented |
| Deepgram Flux | `languages` in `TurnInfo` (multi model) | multi: 10 languages | No Turkish |
| AssemblyAI `universal-3-6-pro` | Yes with `language_detection=true`: `Turn.language_code` + `language_confidence`, final/utterance-complete messages only | Yes, native mid-sentence, default on (32 languages incl. tr) | `language_codes` steering updatable mid-session via `UpdateConfiguration`; `universal-streaming-multilingual` has no tr |
| Gladia live | Yes: `data.utterance.language` per utterance; no confidence | Yes if `code_switching:true` + a small non-empty `languages` list | Else first utterance fixes language for the session; partial accuracy drops; config fixed at init; possible surcharge UNVERIFIED |
| OpenAI `gpt-live-transcribe` | No | UNVERIFIED (give `languages` array) | `gpt-transcribe` returns `languages:[{code}]` per completed item but is turn-committed only |
| sherpa-onnx streaming | No | No (one model per language) | LID only offline with Whisper; no tr in official lists (community seda for tr) |
| Gemini Live (S2S) | No language field documented | Yes, model switches by itself | language cannot be set on native audio; steer via system instruction |
| OpenAI Realtime (S2S) | Not applicable | Model follows prompt | output `voice` fixed after first audio |
| Cartesia TTS | n/a | One language per request (Hinglish/Taglish excepted) | New `context_id` with `language`; same voice all languages (native accent only where voice has accents); fields frozen within a context; per-context difference on one socket UNVERIFIED |
| ElevenLabs TTS | n/a | n/a | Voice/model/`language_code` fixed per WebSocket: new connection; all voices multilingual; `language_code` not for multilingual_v2 |
| Deepgram Aura-2 | n/a | 5 Spanish voices en/es only | Voice per language (`aura-2-<name>-<lang>`), new connection to change; no Turkish |
| openai_speech local | n/a | n/a | Kokoro: voice prefix = language, no tr; Chatterbox multilingual: `language_id` per request, tr yes |

---------------------------------------------------------------------------------------------------

## 5. Recommendation (brisvox judgement built on the facts above; not provider facts)

Minimum mechanism that works across ALL providers:

1. STT is configured to hear several languages, never to decide alone: Deepgram `language=multi` (+ `language_hint`
   of the agent's configured languages), AssemblyAI `universal-3-6-pro` with `language_codes`, Gladia
   `languages:[...]` + `code_switching:true`, OpenAI `languages:[...]`. sherpa-onnx cannot (single language).
2. Language of a user turn = first non-empty of: (a) provider-reported (Deepgram: majority of `words[].language` or
   `alternatives[].languages[0]` of the final segment; AssemblyAI `Turn.language_code`; Gladia `utterance.language`),
   then (b) a provider-independent text-based detector run on the FINAL transcript by brisvox (candidate for
   crates.io check; no crate verified yet), then (c) keep the current language. Add `language: Option<Arc<str>>`
   (and optionally a confidence) to `SttEvent::Transcript`; providers without a field send `None`.
3. Restrict to the agent's configured language set (ignore a reported language that is not in it) and add hysteresis:
   switch only on a final transcript with enough words (a "substantive utterance", per OpenAI's guidance), never on
   interims, names or single foreign words; ask in doubt via the LLM.
4. The switch takes effect at a turn boundary. The engine injects the new language into the LLM prompt line (section
   3) and selects the voice from a config map `language -> voice id` (+ per-language TTS model where the provider
   needs it).
5. TTS session handling by provider: Cartesia = new `context_id` with `language` (voice may stay the same);
   ElevenLabs, Deepgram Aura = new connection (voice is in the URL / model), so the TTS stage needs a per-language
   session or reconnect, `warm()` for the second language; openai_speech = per-request voice/`language_id`.
   A language without a voice in the chosen provider (Aura/Kokoro and Turkish) must fail at config validation.
6. Fallback with no STT detection and no text detector: let the LLM name its reply language (e.g. a leading tag in
   its output stripped before TTS) — works with every TTS, costs one token of latency; UNVERIFIED in practice.

Open items to verify live (BRISVOX_LIVE_TESTS=1): Cartesia different language/voice per context on one socket;
Deepgram `language=multi` + `tr` accuracy and latency vs `language=tr`; Gladia per-word `language`; whether
AssemblyAI `language_detection` adds cost; ElevenLabs `language_code` on the WebSocket with `eleven_flash_v2_5`.
