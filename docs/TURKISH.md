# Turkish quality pass

Turkish is a first-class language of brisvox (risk R10 in `docs/RISKS.md`). This page covers the
three Turkish-specific pieces: the TTS text normaliser, the Turkish backchannel defaults
of the interruption policy, and the Turkish real-speech fixtures in the benchmarks. Everything
here is a documented default chosen without live data; none of it is tuned (see "Unverified").

## 1. TTS text normaliser

Code: `brisvox_agent::normalize` (`Normalizer`, `normalize`, `spell_cardinal`, `spell_ordinal`,
`spell_digits`, `INLINE_CHUNK_CHARS`). Tests: `crates/brisvox-agent/tests/normalize.rs` (table rows in both languages,
Turkish letters, per-chunk use with the segmenter including a streaming test that feeds the
segmenter 1 to 1000 chars at a time and compares the spoken chunks with the spoken whole text,
a scaling test that 16 times the input costs under 64 times the time, absolute cost guards for a
typical and a digit-dense chunk (release bounds, see `docs/LATENCY.md`), proptest: never panics,
bounded growth). Every abbreviation of the two tables has at least one row, as has every rule
named below that the docs call "pinned" or give an example for.

**Status: wired.** The TTS stage (`crates/brisvox-engine/src/tts.rs`, `spoken`)
normalizes every chunk before the provider: short chunks inline, long ones on the inference pool;
the context records the raw text the LLM wrote (tests: `crates/brisvox-engine/tests/tts_stage.rs`,
`chunks_are_spoken_normalized_but_recorded_verbatim`, `a_long_chunk_is_normalized_on_the_offload_and_gives_the_same_text`).
Live 2026-10-13 (`bench/results/live-checks-2026-10-13.md`): Cartesia reads a raw `14.30'da`
digit by digit, the normalized `on dört otuzda` correctly, which is what a call sends.

LLM text is written for eyes (`%15`, `1.250.000`, `150 TL`, `12.03.2026`, `14:30`, `Dr.`). TTS
engines read such tokens inconsistently, and differently per provider. The normaliser rewrites
them into the words a person says, so the spoken form is the same whichever TTS provider is used.
It is a pure function of `(text, Language)`: no state, no clock, no allocation when there is
nothing to rewrite (the result is a `Cow::Borrowed`; a period alone costs one scan), no panic on any input, linear time.

Cost (release, dev machine, 2026-10-08): about 4 us for a 250-char chunk without numbers, 9-10 us
for 328 chars of mixed Turkish or English text (numbers, date, time, phone), 0.7 us for a 14-char
chunk, about 40 us for 1,230 mixed chars, and about 0.1 ms per 1,250 chars for pathological text
with a number every six chars. The caller should pass chunks of at most
`INLINE_CHUNK_CHARS` (256, the segmenter's default `max_chunk_chars` is 250): 256 digit-dense
chars cost 11-26 us in Turkish and 15-45 us in English (worst shape `1 2 3 ` in English, 45 us),
which is within the ~50 us rule for work on a tokio worker, but with little margin on that shape.
These are measurements; the tests only guard loose bounds (`docs/LATENCY.md`). A chunk the segmenter hard-cut (up to five times
`max_chunk_chars`) should be split again, or normalised off the worker. There is no input cap in
the function itself; memory is a `Vec<char>` plus the output per call.

### Using it

```rust
use brisvox_agent::{Language, Normalizer, Segmenter, SegmenterConfig};

let norm = Normalizer::new(Language::Tr);
let mut seg = Segmenter::new(SegmenterConfig::new(Language::Tr));
let mut chunks = Vec::new();
seg.push("Fatura 1.250,50 TL, ödeme 12.03.2026 tarihinde.", &mut chunks);
seg.finish(&mut chunks);
for chunk in &mut chunks {
    norm.normalize_in_place(chunk); // "Fatura bin iki yüz elli lira elli kuruş, ..."
}
```

The normaliser is meant to be applied **per chunk, after the segmenter**; the segmenter itself is
unchanged (its output is the raw text, tested in
`works_per_segmenter_chunk_without_changing_the_segmenter`; the test calls the normaliser itself,
as the pipeline does not yet). The segmenter already keeps `1.250`, `3,14`, `10:30`, `3. sınıf`
and abbreviations inside one chunk, so the normaliser sees whole tokens. The TTS stage
is the natural caller and the owner of the integration (see "Status" above).

### What is rewritten

Output uses the real Turkish letters.

| Kind | Turkish | English |
|---|---|---|
| Integers | `1.250.000` -> `bir milyon iki yüz elli bin` (`bir` is dropped before `yüz` and `bin` only: `1000` -> `bin`, `100.000` -> `yüz bin`, `1.000.000` -> `bir milyon`) | `1,250,000` -> `one million two hundred fifty thousand` |
| Decimals | `3,5` -> `üç virgül beş`; `0,05` -> `sıfır virgül sıfır beş`; `3,25` -> `üç virgül yirmi beş`; `3,14159` -> `üç virgül bir dört bir beş dokuz` | `3.5` -> `three point five` (fraction digit by digit) |
| Negative | `-5` -> `eksi beş` | `minus five` |
| Ordinals | `3. sınıf` -> `üçüncü sınıf`; `21. yüzyıl` -> `yirmi birinci yüzyıl`; `12. Sok.` -> `on ikinci Sokak`; `5. Cadde` -> `beşinci Cadde`; `2026. yılında` -> `iki bin yirmi altıncı yılında` | `3rd` -> `third`, `22nd` -> `twenty-second`, `1000th` -> `one thousandth` |
| Percent | `%15` / `15%` -> `yüzde on beş` | `15%` -> `fifteen percent` |
| Currency | `150 TL`, `150TL`, `TL 150`, `150 TRY`, `₺150` -> `yüz elli lira`; `US$20`, `USD 20` -> `yirmi dolar`; `5 milyar $` -> `beş milyar dolar`; `150,50 TL` -> `yüz elli lira elli kuruş`; `20 $`, `$20`, `20 USD` -> `yirmi dolar`; `€20`, `20 EUR` -> `yirmi avro`; `£5` -> `beş sterlin`; `₺1,5 milyon` -> `bir virgül beş milyon lira` | `$1.50` -> `one dollar fifty cents`; `$1.5 million` -> `one point five million dollars`; `£5.01` -> `five pounds one penny` |
| Dates | `12.03.2026`, `12/03/2026`, `12-03-2026`, `2026-03-12` -> `on iki Mart iki bin yirmi altı` (ranges: `12.03.2026-15.03.2026` -> `on iki Mart iki bin yirmi altı-on beş Mart iki bin yirmi altı`); `12 Mart 2026` already reads correctly (cardinal, month, cardinal) | `March 12, 2026` -> `March twelfth, twenty twenty-six`; `12 March 2026` -> `the twelfth of March, twenty twenty-six`; `2026-03-12`; `03/12/2026` |
| Times | `09:00-18:00` -> `dokuz-on sekiz` (a range: each side is a time, the dash stays); `14:30` -> `on dört otuz`; `09:05` -> `dokuz sıfır beş`; `14:00` -> `on dört`; `14:30:15`; `Saat 14.30 gibi` -> `Saat on dört otuz gibi`; `yarın 10.30'da` -> `yarın on otuzda` | `9:05` -> `nine oh five`; `9:00` -> `nine o'clock`; `9:30 PM` -> `nine thirty p m`; `14:00` -> `fourteen hundred`; years: `in 1999` -> `in nineteen ninety-nine`, `the 1990s` -> `the nineteen nineties` |
| Phone numbers | `+90 532 123 45 67` -> `artı dokuz sıfır, beş üç iki, bir iki üç, dört beş, altı yedi`; `05321234567` regrouped 4-3-2-2; `5321234567` and `532 123 45 67` (no leading zero) regrouped 3-3-2-2; `444 12 34` -> `dört dört dört, bir iki, üç dört` | `555-123-4567` -> `five five five, one two three, four five six seven` |
| Versions / addresses | `sürüm 2.0.1` -> `sürüm iki nokta sıfır nokta bir`; `v2.0.1` unchanged | `version 2.0.1` -> `version two dot zero dot one` |
| Abbreviations (context rules below) | `Dr.` Doktor, `Prof.` Profesör, `Doç.` Doçent, `Öğr. Gör.` Öğretim Görevlisi, `Av.` Avukat, `Sn.` Sayın, `Mah.` Mahallesi, `Cad.` Caddesi, `Sok.` Sokak, `A.Ş.` Anonim Şirketi, `T.C.` Türkiye Cumhuriyeti, `vb.` ve benzeri, `vs.` vesaire, `vd.` ve diğerleri, `dk.` dakika, `sn.` saniye, `bkz.`, `örn.`, ... | `Mr.`, `Mrs.`, `Ms.`, `Dr.`, `Prof.`, `Jr.`, `Sr.`, `Inc.`, `vs.` versus, `etc.` et cetera, `e.g.`, `i.e.` |
| Apostrophe suffixes (Turkish) | `15'inde` -> `on beşinde`; `3'üncü` -> `üçüncü`; `1990'larda` -> `bin dokuz yüz doksanlarda`; `14:30'da` -> `on dört otuzda` | not applied |

### Rules that are guesses (change them in one place, `normalize.rs`)

- **Separators follow the language.** Turkish: `.` thousands, `,` decimal. A lone `.` not followed
  by exactly three digits is read as a decimal point, because LLMs often write `3.5` (`3.5` ->
  `üç virgül beş`). `1.250` is bin iki yüz elli, never one point two five zero. English is the
  reverse. A Turkish comma list without spaces (`1,2,3`) is read `bir virgül iki,üç`.
- **Fractions.** Turkish reads one or two fraction digits as a number (leading zeros spoken), a
  longer fraction digit by digit (`3,14159`). English always reads digit by digit.
- **Ordinal heuristic (Turkish).** Digits (at most four), a period, a space and then a lowercase
  word, an abbreviated noun (`Sok.`) or a capitalised noun from a short list (`Cadde`, `Sokak`,
  `Kat`, `Sınıf`, `Bölge`, `Bölüm`, `Lig`, `Sezon`, `Daire`, `Blok`, `Sayfa`, `Madde`, `Dönem`,
  `Yüzyıl`, `Asır`, `Etap`, `Kısım`) is an ordinal. A period before any other capital is a
  sentence end (`Ben 3. Sonra gittik.` -> `Ben üç. Sonra gittik.`), the same rule the segmenter
  uses. English ordinals (`1000th`) take any number of digits.
- **Capitalised ordinal nouns across a chunk boundary.** The segmenter cuts `digit. Capital` as
  a sentence end (the pipeline's rule, deliberately unchanged), so `kızım 5. Cadde üzerinde` is one chunk
  only while the segmenter does not cut there. When it cuts (pinned by
  `a_capitalised_ordinal_noun_split_by_the_segmenter_is_a_documented_limit`), the first chunk
  ends in `5.` and is read `beş.`; lowercase nouns (`5. cadde`) are never split.
- **Suffix after a currency is dropped** (`150 TL'ye` -> `yüz elli lira`). The suffix of `TL'ye`
  is inflected for the letters "te-le"; re-inflecting `lira` (`liraya`) needs vowel and
  consonant harmony on top of the symbol, which is not implemented. The meaning survives
  (the case ending is lost, the TTS prosody is flat there).
- **Abbreviation context.** Many abbreviations are also words or end a sentence (`Elm Dr.`,
  `Bul.` "find!", `Av.` "hunt", `Tel.` "wire"), and expanding them would change the sentence.
  Titles (`Dr.`, `Prof.`, `Doç.`, `Av.`, `Sn.`, `Mr.`, `Mrs.` ...) are expanded only when a
  capitalised name follows (`Dr. Ayşe`, `Prof. Dr. Ahmet`; `Dr.` at the end of a chunk is left).
  `Sok.`, `Bul.`, `Tel.` are expanded only after a capitalised word or a number (`Gazi Bul.`,
  `12. Sok.`) or before a digit (`Tel. 0212 ...`); English `approx.`, `vs.`, `e.g.`, `i.e.` need a
  word after them. Other entries (`Mah.`, `Cad.`, `Apt.`, `Ltd.`, `vb.`, `dk.` ...) are not words
  and always expand. `yak.` was removed (it is the imperative "yak" before a period). Pinned in
  the table (`Ateşi yak.`, `Hemen bul.`, `Elm Dr.`, `Dr.`).
- **Abbreviation periods.** An expanded abbreviation loses its period (`Dr. Ayşe` -> `Doktor
  Ayşe`). Sentence-final ones (`vb.`, `vs.`, `vd.`, `dk.`, `sn.`, `etc.`) keep it when they are
  the last thing in the chunk or a capital follows; `A.Ş.`, `T.C.`, `Inc.` keep it only at the
  very end of the chunk (a company name is usually followed by more words). Matching is case
  sensitive (`Sn.` Sayın, `sn.` saniye).
- **Numbers glued to letters** are left alone (`A4`, `mp3`, `COVID19`); a number followed by a
  unit gets a space (`5km` -> `beş km`).
- **Dates.** Turkish numeric dates are day-month-year. English `.` is day-month-year; English `/`
  and `-` are US month-day-year unless the first number is above 12. A two-digit year is not a
  date. A date that does not exist on the calendar (`32.13.2026`, `31.02.2026`, `31.04.2026`,
  `29.02.2026`; `29.02.2024` is fine) is read as dotted numbers. Month names with a day
  (`February 30`) are not checked.
- **Ranges.** A `-` followed by a digit continues a number only when the number itself is
  written with `-` (`12-03-2026-15` is not a date). After a `:`, `.` or `/` token it is the dash of
  a range: `Pazartesi-Cuma 09:00-18:00` -> `Pazartesi-Cuma dokuz-on sekiz`,
  `12.03.2026-15.03.2026` and `12/03/2026-15/03/2026` read as two dates. Before this fix the first
  half of such a range was read digit by digit (`sıfır dokuz:sıfır sıfır-on sekiz`). The dash is
  left in the text for the TTS to pause on. The second half of a dotted time range is a time when
  the first half follows `saat` (`saat 09.00-17.00` -> `saat dokuz-on yedi`); without `saat` both
  halves stay decimals (`17.00-18.00` -> `on yedi virgül sıfır sıfır-on sekiz virgül sıfır sıfır`).
  Phone numbers are unchanged: a `-` between digit groups joins them into one number.
- **Times** need a two-digit minute and hour 0-23 (1-12 with AM/PM). `16:9` is not a time. The
  Turkish dotted form `14.30` is also a decimal point (`10.30 TL`), so it is a time only after
  the word `saat` (`Saat 14.30 gibi`) or with a time case suffix (`10.30'da`, `'dan`, `'a`, `'ye`
  ...). Ambiguity kept: `Fiyat 12.50'ye` reads as a time; `7.30` alone is `yedi virgül otuz`.
- **Phone numbers** are recognised by shape: `+` and 8-15 digits, a leading `0` with 10-11
  digits, in Turkish a 10-digit mobile number starting with `5` (bare or `532 123 45 67`) and a
  7-digit `444` call-centre number (`444 12 34`, `444 0 123`, `444 1234`), or 10-11 digits
  grouped 3-3-4 / 1-3-3-4. Any other bare digit run is a number (`1234567890` is read as a
  billion ...), as are 7-digit local numbers (`123 45 67`). A Turkish amount of five billion
  written without separators is therefore read as a phone number. Groups are separated by `, `
  so the TTS pauses between them.
- **Space-grouped thousands.** `100 000 TL`, `1 000 000` and the no-break-space forms are one
  number (a first group of 1-3 digits without a leading zero, then groups of exactly three).
  Two quantities written next to each other (`2 150 TL` for two items at 150) cannot be told
  apart and are read as 2150.
- **Currency forms.** `150 TL`, `150TL`, `TL 150`, `₺150`, `USD 20`, `US$20`, `US$ 5`,
  `5 milyar $`. `$` after a letter other than `US` (`R$ 5`, `A$5`) is another currency: symbol
  and amount are left as written. `$ 5` (symbol, space) alone is not rewritten.
- **English years and decades.** Four digits from 1100 to 2099 after `in`, `since`, `by`,
  `until`, `till`, `from`, `during`, `before` or `after`, not before a unit, `%` or currency, are
  read as a year (`in 1999` -> `in nineteen ninety-nine`); `1990s`, `1990's`, `1900s`, `2000s`
  are decades. Other four-digit numbers stay cardinals (`1995 people`).
- **Large numbers** go up to `kentilyon` (10^18) in Turkish and `quintillion` in English
  (`u64::MAX` is spelled correctly in both); anything longer than 18 digits, or with a leading
  zero (`007`), is read digit by digit.
- Not covered: units (`km`, `kg`, `°C`), Roman numerals, fractions (`1/2`), two-digit years,
  e-mail and URLs (the segmenter keeps them whole; the TTS spells them), emoji.

### Interaction with played-text tracking and provider normalisation

This is a design proposal; none of it is built (see "Status").

- If the TTS provider is sent the **normalised** text, its word timestamps are words of the
  spoken form (`yüz`, `elli`, `lira`). Feed `PlayedText` the same normalised chunk it sent, and
  the heard text in the LLM context will be the spoken form ("yüz elli lira") instead of the
  raw "150 TL". That is consistent with "the context contains only what was played" and needs no
  mapping. Normalise exactly once: feeding `PlayedText` the raw chunk while the TTS got the
  normalised one, or normalising the already normalised text again, breaks the word alignment.
  If the context must keep the raw text, `PlayedText` needs the raw chunk plus a raw-to-spoken
  alignment; that is not built.
- Some TTS providers have their own normalisation switch (for example a text-normalisation
  option in the request). Turn it off or leave it on, but not both layers expecting different
  results: with brisvox normalising first, the provider sees words and has nothing left to do.
  Provider behaviour was not checked (see "Unverified").

## 2. Turkish backchannels

Code: `BackchannelSet::turkish()` in `crates/brisvox-agent/src/interrupt.rs`; tests:
`crates/brisvox-agent/tests/turkish_backchannels.rs` (plus the existing table in
`tests/interruption.rs`, unchanged). The test file lists all 60 phrases below and asserts that the
set contains every one of them and has exactly 60 entries, so a typo, a swap, a duplicate or a
silent removal fails. A backchannel is a listener sound or short acknowledgement
that must not interrupt the bot (`InterruptionPolicy::recommended()` unions the English and
Turkish sets). Matching is on whole normalised tokens of the latest interim: lower case, every
non-alphanumeric is a separator, capital `İ` folds to `i`, the dotless `ı` stays distinct
(`hı` is not `hi`, and an all-capitals `HIHI` is not `hıhı`). Repetitions need no entries:
`evet evet`, `hı hı hı`, `tamam tamam` are sequences of listed phrases.

The original list had 19 phrases (evet, hı-hı, hıhı, hı, he, hm, hmm, mhm, mm, tamam, tamamdır,
aynen, anladım, anlıyorum, peki, tabii, tabi, öyle mi, ya). It missed what Turkish STT writes for
the same sounds and several common acknowledgements. The Turkish pass extends it to 60 phrases (the original 19 plus 41) in four groups:

- **Listener sounds as STT spells them**: ha, haa, hıı, hım, hımm, ımm, mmm, aha, aa, ah, oh,
  ohh, ay, of.
- **Fillers while thinking** (the caller is not speaking to the bot): ee, eee, ıı, ııı, şey, yani.
- **Acknowledgements**: olur, doğru, tabii ki, tabi ki, elbette, anlaşıldı, anlaşılıyor,
  eyvallah, peki tamam.
- **Reactions**: gerçekten mi, sahi mi, ciddi mi, yok artık, hadi ya, demek öyle, vay, vay be,
  harika, güzel, süper, iyi.

Deliberately **not** in the list, because they stop, correct or answer the bot: hayır, olmaz,
yok, dur, bekle, bir dakika, dinle, and the thanks `teşekkürler` / `sağ ol` (thanks often end the
call). The test `substantive_turkish_still_interrupts` pins these, including near misses:
`evet ama`, `iyi değil`, `güzel ama pahalı`, `olur mu`, `gerçekten mi dedin`, `şey var`.

Consistency note: `evet` is in the list (it was in the original list) although an English "yes" is not,
because a Turkish "evet" mid-answer is overwhelmingly a listener signal ("yes, go on"), while a
direct answer to a question normally arrives when the bot has finished speaking. `olur` and
`doğru` are the same kind of call. All of this is a guess until live Turkish calls say otherwise;
the lists are plain data on `BackchannelSet` and can be replaced per deployment.

## 3. Turkish fixtures in the benchmarks

Before the Turkish pass, `brisvox-bench` (`sim-load`, `infer-load`, `resample`) used no recorded speech:
`sim-load` plays a synthetic scripted caller, so none of the Turkish FLEURS fixtures
(`fixtures/tr_two_utterances`, `fixtures/en_tr_mixed`, see `fixtures/NOTICE` and
`docs/TESTING.md`) was in the benchmark set; they were only in the correctness suites
(`brisvox-sim` `tests/fixtures.rs`).

The Turkish pass adds `brisvox-bench fixtures` (`crates/brisvox-bench/src/fixture_load.rs`): each committed
fixture (`en_two_utterances`, `tr_two_utterances`, `en_tr_mixed`) is played as N concurrent
simulated calls in real time through the full reference pipeline against mock providers with
the same latency model as `sim-load`. The mock STT returns the fixture transcript; replies are
Turkish for Turkish utterances and English for English ones (same length, so the mock TTS audio
is identical). Both are checked, not assumed: every call's conversation as the LLM saw it must be
exactly `[user: transcript 1, bot: reply for language 1, user: transcript 2, ...]`
(`FixtureReport::conversation_mismatches`; a run with a mismatch is invalid and exits non-zero;
the integration test asserts 0 and was mutation-checked by answering every turn in English). The
integration test's sanity floor for the median voice-to-voice is derived from the configured
latency medians (`response_floor()`: 60 percent of LLM first token plus TTS first byte), not a
magic number. It reports server-side voice-to-voice per fixture and per utterance language, plus
the internal overhead.

```
cargo run --release -p brisvox-bench -- fixtures --sessions 10 --out bench/results/fixtures-sim.md
```

The recordings leave only 1.0-1.5 s between utterances, which is less than any bot's answer
(about 1.2 s voice-to-voice plus the audio), so as recorded the caller would talk over every
answer. `pad_gaps` therefore inserts digital silence so that every gap between utterances is at
least 4 s (`FixtureLoadConfig::gap`); the speech samples and their
ground truth are untouched (tested byte for byte). The run is invalid (non-zero exit, nothing
written) when a call fails, a turn goes unanswered or a barge-in happens.

Result (2026-10-08, release, 4 tokio workers, 10 calls per fixture, shared desktop;
`bench/results/fixtures-sim.md`): every turn answered, no barge-ins, voice-to-voice p50 995 ms
(`en_two_utterances`), 1009 ms (`tr_two_utterances`), 994 ms (`en_tr_mixed`; English turns 1039,
Turkish turns 932 ms). The language makes no systematic difference, as expected with mock
providers: the numbers show the pipeline treats Turkish speech like English, not how fast a
Turkish provider is.

What this does **not** measure: Turkish recognition quality or real provider latency. The mock
STT is perfect and the mock providers are synthetic; word error rate and TTS first byte for
Turkish need live provider runs (`BRISVOX_LIVE_TESTS=1`).

## Live Turkish checks (2026-10-13)

Deepgram nova-3 `language=tr` (Turkish is listed for nova-3, nova-2 and base; not for Flux; docs
fetched 2026-10-13): WER 0.038 and 0.000 on the FLEURS fixture `tr_two_utterances.wav`, WER 0.00-0.17
on Cartesia and ElevenLabs Turkish speech of ç ğ ı İ ö ş ü sentences (0.31 for Cartesia on raw
digits). Both TTS providers return word timestamps with the Turkish letters intact, first audio 135-240
ms. One-word first chunk `Tamam,` (FIRST_MIN_WORDS_TR = 1): Cartesia shows no extra pause; ElevenLabs
adds a 210-230 ms pause after `Tamam,` that a single chunk does not have; Aura-2 has no Turkish voice.
The intonation question is for a listening test of the audio the live checks write to `bench/results/live-audio/` (provider output, not published with the repository).
Deepgram's interims arrive once per second in Turkish as in English, so `on_partial` speculation does
not pay off for Turkish either (`bench/results/speculation-live-deepgram-2026-10-13.md`).

## Unverified

- Live check 2026-10-13 (`bench/results/live-checks-2026-10-13.md`): Cartesia and ElevenLabs read
  the normaliser's output (`iki yüz elli lira`, `on dört otuzda`) correctly and do not re-normalise
  it (Deepgram transcribes it back with WER 0.154 from the spacing `otuz da` alone). Aura-2 was not
  checked (no Turkish voice). Without the normaliser, Cartesia reads `14.30'da` digit by digit
  (`bir dört üç sıfır da`) while ElevenLabs reads it correctly: the normaliser is needed for
  Cartesia. Whether a provider's own normaliser should be switched off remains unverified.
- The backchannel list and the ordinal / separator heuristics are reasoned from how Turkish is
  written and spoken, not measured on Turkish calls.
- English year reading (`twenty twenty-six`) and the US reading of ambiguous English dates are
  conventions, not requirements.
