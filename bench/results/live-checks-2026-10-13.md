# Live provider checks, 2026-10-13

Command: `BRISVOX_LIVE_TESTS=1` + keys, `cargo test -p brisvox-providers --features
provider-deepgram,provider-cartesia,provider-elevenlabs,provider-deepgram-aura --test live_checks
-- --nocapture` (`crates/brisvox-providers/tests/live_checks.rs`). Windows, home network, real
providers. The synthesized audio is written to `bench/results/live-audio/` when the live checks run; it is provider output and not published with the repository.

## Turkish round trip (Deepgram, Cartesia, ElevenLabs)

TTS (24 kHz) -> `StreamResampler` to 16 kHz -> Deepgram nova-3 `language=tr` (`smart_format=false`,
`punctuate=true`), scored against the spoken form; normalized for case, punctuation and Turkish
i/I. Word timestamps: monotonic, inside the audio, ç ğ ı İ ö ş ü intact in the word texts (all
pass, both providers; Aura-2 has no Turkish voice and no word events).

| sentence | Cartesia sonic-3.6 | ElevenLabs flash v2.5 |
|---|---|---|
| s1 `Yarın saat üçte randevum var mı?` | WER 0.167 | 0.167 |
| s2 `Çağrınızı şimdi İstanbul şubesine ...` | 0.000 | 0.000 |
| s3 raw digits `250 lira ... 14.30'da` | 0.308 / 0.308 (two runs: read `14.30` as `bir dört üç sıfır`) | 0.154 / 0.000 |
| s4 = s3 through `brisvox_agent::normalize` | 0.154 | 0.154 |
| first audio (s1..s4) | 205-240 ms | 135-200 ms |

s1: Deepgram heard `randevun` for both voices (an STT error, not a TTS one). s4: the whole 0.154
is `otuzda` written as `otuz da` (spacing, not a misreading). Deepgram on `tr_two_utterances.wav`
(FLEURS): WER 0.038 and 0.000 in two runs. Conclusion: Turkish synthesis is intelligible on both;
the Turkish number normaliser (docs/TURKISH.md section 1) is needed for Cartesia and harmless for
ElevenLabs.

## One-word first chunk

`Okay,` / `Tamam,` as a first chunk and the rest 150 ms later, one flush, vs one chunk; two runs
each; silences of at least 80 ms between the first and the last speech (frames of 10 ms below
-44 dBFS), and a real-time playout model of the received audio (underrun = a chunk arrives after
the queued audio ran out; 20 ms or more).

| provider/lang | first audio split / single | gaps >= 80 ms, split (2 runs) | gaps >= 80 ms, single (2 runs) | underruns |
|---|---|---|---|---|
| Cartesia en | 210, 207 / 218, 214 ms | 90 ms @450, none | 160 ms @410, none | 0 |
| Cartesia tr | 199, 201 / 191, 194 ms | none, 80 ms @640 | 150 ms @380, none | 0 |
| ElevenLabs en | 183, 121 / 145, 144 ms | none, 200 ms @370 | none, 170 ms @340 | 0 |
| ElevenLabs tr | 199, 140 / 155, 146 ms | 230 ms @350, 210 ms @450 | none, none | 0 |
| Aura-2 en | 418, 413 / 257, 271 ms | 290 ms @650, three of 80-100 ms | 90 ms @160, 260 ms @10 | 0 |

- Cartesia: no gap beyond the natural comma pause of the single file. Safe.
- ElevenLabs en: the split pause (200 ms) equals the single's own comma pause (170 ms). ElevenLabs
  tr: the split file has a 210-230 ms pause after `Tamam,` that the single file does not; listen.
- Aura-2: the first chunk gives NO first-audio gain (413 ms vs 257 ms: the server does not start
  generating `Okay,` before the next text/flush, so the audio starts 150 ms later than with one
  chunk) and one run had a 290 ms pause. One-word first chunks do not help on Aura; the unverified
  point "Aura starts without a Flush" holds only for text that ends a sentence.
- Pacer underruns: 0 in all runs (the model counts the audio as the provider delivered it, a
  one-word burst then 150 ms of waiting for the rest never ran the buffer dry: `Okay,` is ~0.5 s
  of audio, longer than the 150 ms delay). FIRST_MIN_WORDS is unchanged (no evidence against 1
  for Cartesia/ElevenLabs; the maintainers' listening decides the intonation question).

## Speculation hit rate with Deepgram

See `speculation-live-deepgram-2026-10-13.md`: 2 of 17 candidates at the VAD stop equal the final
(Deepgram interims arrive once per second, the finalize round trip is 290-330 ms); recommendation:
`on_partial = false` for Deepgram in EN and TR. 131 s of audio (the large fixtures are not in the
repository).
