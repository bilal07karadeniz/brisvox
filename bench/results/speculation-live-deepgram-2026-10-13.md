# Speculation hit rate with real Deepgram

Measured 2026-10-13 with Deepgram nova-3 (streaming, adapter defaults: smart_format on, punctuate off, endpointing 300 ms, interim results on), fixtures streamed in real time in 20 ms chunks plus 1.2 s of trailing silence; 131 s of audio in total. `finalize-at-stop` sends `Finalize` at the VAD stop (segment end + 100 ms), as the engine does; `natural-endpointing` leaves the final to Deepgram's 300 ms endpointing.

Candidate rule = the engine's `TurnDriver::partial_text` at the VAD stop (`on_partial`): the `is_final` segments heard so far, else the latest interim. A **hit** means the normalized candidate equals the normalized utterance final (case, punctuation, Turkish-aware i/I). `optimistic` would also append the trailing interim to the finals. The last-interim columns compare the last interim before the final with the final.

| group | turns | final before stop | empty candidate | hit | miss (wasted request) | optimistic hit | last interim == final | median interim->final ms | median stop->final ms (all) |
|---|---|---|---|---|---|---|---|---|---|
| EN (finalize-at-stop) | 6 | 0 | 0 | 17% (1/6) | 83% (5/6) | 17% (1/6) | 17% (1/6) | 623 (max 769) | 288 (max 291) |
| EN (natural-endpointing) | 4 | 0 | 0 | 25% (1/4) | 75% (3/4) | 25% (1/4) | 0% (0/4) | 1242 (max 2481) | 596 (max 651) |
| TR (finalize-at-stop) | 5 | 0 | 0 | 0% (0/5) | 100% (5/5) | 0% (0/5) | 0% (0/5) | 240 (max 470) | 332 (max 373) |
| TR (natural-endpointing) | 2 | 0 | 0 | 0% (0/2) | 100% (2/2) | 0% (0/2) | 0% (0/2) | 825 (max 825) | 608 (max 608) |

STT-only WER on the fixtures (finalize-at-stop runs):

- en_two_utterances/en/finalize-at-stop: WER 0.103
- tr_two_utterances/tr/finalize-at-stop: WER 0.000
- en_two_utterances/en/finalize-at-stop: WER 0.103
- tr_two_utterances/tr/finalize-at-stop: WER 0.000

Misses:

- en_two_utterances/en/finalize-at-stop stop0: candidate `The UN also hopes to finalize a fund to help` vs final `The UN also hopes to finalize a fund to help countries affected by global warming to cope with the impact.`
- en_two_utterances/en/finalize-at-stop stop1: candidate `The major religion in Moldova is` vs final `The major religion in Moldova is Orthodox Christian.`
- en_two_utterances/en/finalize-at-stop stop0: candidate `The UN also hopes to finalize a fund to help` vs final `The UN also hopes to finalize a fund to help countries affected by global warming to cope with the impact.`
- en_two_utterances/en/finalize-at-stop stop1: candidate `The major religion in Moldova is Orthodox` vs final `The major religion in Moldova is Orthodox Christian.`
- en_tr_mixed/en/finalize-at-stop stop0: candidate `The East African Islands are in the Indian Ocean off the Eastern Coast of` vs final `The East African Islands are in the Indian Ocean off the Eastern Coast Of Africa.`
- en_two_utterances/en/natural-endpointing stop0: candidate `The UN also hopes to finalize a fund to help` vs final `The UN also hopes to finalize a fund to help countries affected by global warming to cope with the impact.`
- en_two_utterances/en/natural-endpointing stop1: candidate `The major religion in Moldova is` vs final `The major religion in Moldova is Orthodox Christian.`
- en_tr_mixed/en/natural-endpointing stop0: candidate `The East African Islands are in the Indian Ocean off the Eastern Coast of` vs final `The East African Islands are in the Indian Ocean off the Eastern Coast Of Africa.`
- tr_two_utterances/tr/finalize-at-stop stop0: candidate `Ürünlerimizin yüzde seksenlik bir bölümü Orta Amerika ülkelerindeki` vs final `Ürünlerimizin yüzde seksenlik bir bölümü Orta Amerika ülkelerindeki oranlar üzerinden vergilendirildi.`
- tr_two_utterances/tr/finalize-at-stop stop1: candidate `Zamanla yeni nüfus yeni çevresine uyum sağlamaya başladıkça` vs final `Zamanla yeni nüfus yeni çevresine uyum sağlamaya başladıkça diğer popülasyondan gittikçe daha farklı görünmeye başlarlar.`
- en_tr_mixed/tr/finalize-at-stop stop1: candidate `Noel, Hristiyanlığın en mühim bayramlarından biridir.` vs final `Noel, Hristiyanlığın en mühim bayramlarından biridir. İsa'nın doğum günü olarak kutlanır.`
- tr_two_utterances/tr/finalize-at-stop stop0: candidate `Ürünlerimizin yüzde seksenlik bir bölümü Orta Amerika ülkelerindeki` vs final `Ürünlerimizin yüzde seksenlik bir bölümü Orta Amerika ülkelerindeki oranlar üzerinden vergilendirildi.`
- tr_two_utterances/tr/finalize-at-stop stop1: candidate `Zamanla yeni nüfus yeni çevresine uyum sağlamaya başladıkça` vs final `Zamanla yeni nüfus yeni çevresine uyum sağlamaya başladıkça diğer popülasyondan gittikçe daha farklı görünmeye başlarlar.`
- tr_two_utterances/tr/natural-endpointing stop0: candidate `Ürünlerimizin yüzde seksenlik bir bölümü Orta Amerika ülkelerindeki` vs final `Ürünlerimizin yüzde seksenlik bir bölümü Orta Amerika ülkelerindeki oranlar üzerinden vergilendirildi.`
- tr_two_utterances/tr/natural-endpointing stop1: candidate `Zamanla yeni nüfus yeni çevresine uyum sağlamaya` vs final `Zamanla yeni nüfus yeni çevresine uyum sağlamaya diğer popülasyondan gittikçe daha farklı görünmeye başlarlar.`

## What the events look like (why the hit rate is low)

Deepgram nova-3 emits an interim about once per second (1.0 s +- 0.1 s between interims in every
run; the docs say "typically every second"), and an `is_final` segment every few seconds that
restarts the interim. A VAD stop (speech end + 100 ms) therefore finds a partial that is one
interim old: on the slow, read FLEURS speech (about 2-3 words per second) that is 1-5 words short
of the final. The final that `Finalize` produces arrives about 290 ms (EN) / 330 ms (TR) after the
stop. With natural endpointing (300 ms) it arrives 600 ms after the stop. The engine's rule takes
the `is_final` group in preference to a newer interim, which makes the candidate even older when
the utterance already had a segment final (the `optimistic` column shows appending the trailing
interim would not have changed a single outcome here, because the interim itself lags).

## Result and recommendation

- Hit rate of the candidate at the VAD stop: 2 of 17 candidates (12%) in the run above (EN
  finalize-at-stop 1/6, EN natural endpointing 1/4, TR 0/5 and 0/2); an earlier run of the same
  plan gave 1 of 17 (the hits are single lucky utterances). The last interim equals the final in 1
  of 17 turns. Every miss is a wasted LLM request: 15 of 17 candidates, about 0.9 wasted requests
  per turn. A hit saves at most the finalize round trip (median 288 ms EN, 332 ms TR after the
  stop; under natural endpointing the final comes about 600 ms after the stop).
- Expected saving per turn = hit rate x 290 ms = about 35 ms, against about 0.9 extra LLM
  requests per turn. **Recommendation: `on_partial = false` for Deepgram, EN and TR, under the
  adaptive policy as well (speculation then reduces to the final-only mode, which equals off under
  the adaptive policy per docs/LATENCY.md), i.e. leave `speculation.enabled = false` with Deepgram
  unless a fixed silence of 600 ms or more is used (there `enabled = true, on_partial = false`
  stays the recommendation).** Turkish is no better than English; the sample is small (about 17
  utterances, 2 fixtures, 1 voice per language), so the numbers are indicative, but the cause (a
  1 s interim cadence against a 250-300 ms finalize round trip) is structural.
- Not measured: short utterances ("evet", "tamam", a digit string), where one interim after the
  first second can already equal the final; they are where `on_partial` could pay off. A short
  utterance set (TTS clips) is the follow-up if wanted a per-length answer.
- AssemblyAI and Gladia: no keys; not measured (a live run for them is still open).
