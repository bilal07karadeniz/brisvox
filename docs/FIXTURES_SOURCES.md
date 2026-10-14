# Speech fixture sources

Research date: 2026-10-07. Every fact below was read from the cited URL on that date unless marked
UNVERIFIED. The research itself ran no download.

**Update 2026-10-07 (after `scripts/fixtures/build_fixtures.py` ran):** the pinned `resolve` URLs
work anonymously; read-and-close prefix streaming works (en 16 MB, tr 64 MB prefixes); tar
members are keyed by basename; the WAVs are 16 kHz mono **32-bit float** (format 3), not PCM16,
and are converted; clip levels vary by more than 100x and some clips carry long leading/trailing
silence (hence normalization + trimming); Turkish dev clips often contain mid-sentence pauses up
to 1.6 s and are filtered. Still UNVERIFIED: that FLEURS transcripts are FLORES-101 text under
CC BY-SA (handled conservatively). The per-clip notes further down are the original
research and may not match the clips finally selected (see `fixtures/sources.lock`).

Goal: short (a few seconds) English + Turkish speech clips with transcripts, permissive license,
anonymous HTTPS download (python3 stdlib or curl), < 10 MB committed, larger sets fetched with SHA-256.

## Recommendation (primary)

**Google FLEURS** (`google/fleurs` on Hugging Face), configs `en_us` and `tr_tr`.
- One source for both languages, `gated: false`, license `cc-by-4.0`, 16 kHz audio, transcripts in TSV.
- Files are public over plain HTTPS `resolve` URLs (no token, no account).
- Pin the dataset commit `70bb2e84b976b7e960aa89f1c648e09c59f894dd` ("Convert dataset to Parquet", 2026-05-15)
  in every URL so bytes never change under us.
- Caveat (legal, see "License risk"): the sentences come from FLORES, which is CC BY-SA 4.0.

Fallbacks: LibriSpeech (English only, CC BY 4.0) and OpenSLR 108 MediaSpeech (Turkish only, CC BY 4.0 but
YouTube-derived). Rejected: Common Voice (account + redistribution ban), ISSAI TSC (single huge tarball),
Tatoeba (per-clip licenses, some unusable).

## Candidate comparison

| Source | Languages | License | Anonymous HTTPS | Smallest unit to fetch | Transcripts | Verdict |
|---|---|---|---|---|---|---|
| FLEURS (HF google/fleurs) | en_us, tr_tr (+100) | CC-BY-4.0 (card); text from FLORES CC BY-SA 4.0 | yes, not gated | dev.tar.gz 171 MB (en) / 199 MB (tr); read only a prefix | yes, TSV | PRIMARY |
| Common Voice (Mozilla Data Collective) | en, tr | CC0 historically (NOT re-verified now) | NO, account + terms | n/a | yes | rejected |
| LibriSpeech (OpenSLR 12) | en only | CC BY 4.0 | yes | dev-clean.tar.gz 337 MB | yes (in tar) | English fallback |
| MediaSpeech (OpenSLR 108) | fr, ar, es, **tr** (no en) | CC BY 4.0 | yes | TR.tgz 618 MB | yes | Turkish fallback, YouTube copyright caveat |
| ISSAI Turkish Speech Corpus (HF) | tr only | HF card says MIT; arXiv/ISSAI says CC BY 4.0 (inconsistent, UNVERIFIED which is right) | yes | one 218 h tarball (size not fetched) | yes | rejected (huge, license inconsistency) |
| Tatoeba audio | many | per-contributor, mixed | yes, per file | single mp3 | sentence text separate | rejected (license per clip) |

## FLEURS details

Sources (all fetched 2026-10-07):
- Card: https://huggingface.co/datasets/google/fleurs and raw https://huggingface.co/datasets/google/fleurs/raw/main/README.md
- API: https://huggingface.co/api/datasets/google/fleurs (fields: `"gated": false`, `"private": false`, `"license": "cc-by-4.0"`, `sha` 70bb2e84..., `lastModified` 2026-05-15)
- Paper: https://arxiv.org/abs/2205.12446 (Conneau et al. 2022)
- Commit history: https://huggingface.co/datasets/google/fleurs/commits/main (latest: 70bb2e8 "Convert dataset to Parquet (#31)")

License quotes:
- Card, "Licensing Information": "All datasets are licensed under the Creative Commons license (CC-BY)." YAML: `license: cc-by-4.0`.
- Card, "Dataset Summary": "Fleurs is the speech version of the FLoRes machine translation benchmark."
- FLORES repo (https://github.com/facebookresearch/flores) states in its Licenses section: "FLORES-200: CC-BY-SA 4.0".
  UNVERIFIED: that FLEURS prompts are exactly the FLORES-101 sentences under the same SA license (the card
  says speech version of FLoRes; we did not fetch a FLORES-101 license statement; the FLORES-101 README and
  the OLDI GitHub page did not state it explicitly, the OLDI page only loosely mentioned CC BY-SA 4.0 and Wikinews/Wikijunior/Wikivoyage).
- The Google TFDS XTREME-S catalog page (https://www.tensorflow.org/datasets/catalog/xtreme_s) carries only the generic page license (CC BY 4.0 / Apache 2.0 for code) and the citation; it gives no dataset-specific license.

Audio / metadata:
- 16 kHz (card "Sample Rate: 16,000 Hz"; dataset viewer feature `"audio": {"sampling_rate": 16000}`).
- Files are `.wav` (viewer serves `audio/wav`). UNVERIFIED: 16-bit PCM mono (assumed from FLEURS WAV convention; check with `wave` module).
- `num_samples` is samples at 16 kHz: 94080 = 5.88 s. At 16-bit mono, 32 000 bytes/s, so a 5 s clip is about 160 KB and 10 clips per language is about 1.6 MB.
- Many sentences have several speakers (same id, different filename, gender MALE/FEMALE).
- TSV (`data/<lang>/{train,dev,test}.tsv`) has 7 tab-separated columns, no header (observed on test.tsv, both languages):
  `id, filename.wav, raw_transcription, transcription (normalized), tokenized chars with "|", num_samples, gender`.
  (The Turkish row fetched had 7 columns once the tokenized field is counted; one summary mis-counted it as 6.)
- Gotcha (observed in tr_tr dev.tsv through the fetch summarizer, re-verify): the normalized transcription lowercases "I-dot" as `i` + combining dot (U+0307) e.g. "i̇spanyollar". Compare Turkish text with Unicode normalization or use `raw_transcription`.
- Data fields on HF viewer: id, num_samples, path, audio, transcription, raw_transcription, gender, lang_id, language, lang_group_id.
- Paper: about 12 h supervision per language (abstract); card says about 10 h in train.

Files (repo tree API, commit 70bb2e84...), with LFS sha256 of whole files:

| Path | Bytes | sha256 (LFS oid) |
|---|---|---|
| data/en_us/audio/dev.tar.gz | 171,250,900 | 2658fda72f199e12676ecac9415094667a4e14e149b146e568ea00b2a2f0954c |
| data/en_us/audio/test.tar.gz | 289,851,356 | d9c2e37b41aacd41bc283554a0a82b5476b36887049774ecb2819dcaaa55a356 |
| data/en_us/audio/train.tar.gz | 1,380,572,241 | 5f4491948c2bd29ac00f4b8afae2378f0a1dcdde4041b5cd284a80dff01fa9f5 |
| data/tr_tr/audio/dev.tar.gz | 198,683,867 | 7476fc01bc50750f2854dbc1b4fa285f00092808e7e43244e0b978e031cbe712 |
| data/tr_tr/audio/test.tar.gz | 464,259,241 | a7db7a681e4460d2922626bf2037ddc1a9146efba27d78bfc9543250eabc3a09 |
| data/tr_tr/audio/train.tar.gz | 1,468,477,481 | bb8f81d19282cab8068b915265f60147f84127d37c71ad920fd45409ba5072aa |
| data/en_us/{dev,test,train}.tsv | 213,065 / 367,864 / 1,414,907 | not fetched |
| data/tr_tr/{dev,test,train}.tsv | 191,300 / 447,909 / 1,460,325 | not fetched |

Parquet alternative (`parquet-data/<lang>/{train,validation,test}-00000-of-00001.parquet`): 236 MB to 1.9 GB,
single shard each, footer-at-end format; not usable with stdlib. Not recommended. The `refs/convert/parquet` branch only has empty language dirs plus .gitattributes in the tree listing we saw.

Per-clip access options:
1. **Tar prefix streaming (recommended).** `.tar.gz` is a sequential stream, so the first N MB decompress to the
   first complete members. Fetch with stdlib `urllib.request` (follows the 302 to the CDN), read only the first
   ~12-16 MB, close the connection, decompress with `zlib.decompressobj(wbits=31)`, parse with `tarfile.open(mode="r|")`
   and ignore the truncation error on the last partial member. Optional header `Range: bytes=0-16777215`; the script
   must NOT depend on Range being honoured (read-and-close works either way). UNVERIFIED: tar member naming
   (the old loader script `fleurs.py` matches on `audio_path.split("/")[-1]`, so members may sit under a `dev/` style
   prefix; key everything by basename) and which clips fall inside the prefix. Therefore the script selects
   clips from the prefix by looking each basename up in `dev.tsv` and keeping those with `num_samples <= 96000`.
2. Dataset viewer API (anonymous, per-row): `https://datasets-server.huggingface.co/first-rows?dataset=google%2Ffleurs&config=en_us&split=validation`
   returned JSON with `audio[0].src` = signed, expiring URL to `.../assets/google/fleurs/--/<sha>/--/en_us/validation/<row>/audio/audio.wav?Expires=...`.
   Docs: https://huggingface.co/docs/dataset-viewer/rows says the audio `src` is a signed URL that expires and
   cached assets may be emptied ("you may have to call /rows again"). The `/rows` endpoint (offset/length, max 100) returned HTTP 500 once in our test
   and `first-rows` worked. The WAV is re-rendered by the viewer, so its bytes are NOT guaranteed stable: only use for exploration, never for checksummed fixtures.
3. Whole-tar download: sizes above; verify against the LFS sha256. Not needed for the committed set.

Concrete URL patterns (replace `<REV>` with `70bb2e84b976b7e960aa89f1c648e09c59f894dd`):
```
https://huggingface.co/datasets/google/fleurs/resolve/<REV>/data/en_us/dev.tsv
https://huggingface.co/datasets/google/fleurs/resolve/<REV>/data/tr_tr/dev.tsv
https://huggingface.co/datasets/google/fleurs/resolve/<REV>/data/en_us/audio/dev.tar.gz   (171,250,900 B)
https://huggingface.co/datasets/google/fleurs/resolve/<REV>/data/tr_tr/audio/dev.tar.gz   (198,683,867 B)
```
(The `resolve/main/...tsv` URLs for `test.tsv` were fetched successfully today; the `<REV>` forms for dev.tsv also worked.
`resolve` returns a redirect to the storage CDN; the HF docs note redirects, so the client must follow them.)

Candidate short clips seen today (via a summarizing fetcher, so treat names as hints; the script must re-read the TSV):
- en_us dev.tsv: `4175831345558885921.wav` (96,000 samples = 6.0 s, FEMALE, "there are many beaches due to auckland's straddling of two harbours..."),
  `9742912202579443140.wav` (99,840), `2842775607363710885.wav` (90,240, "with only eighteen medals available a day...").
- tr_tr dev.tsv: `14650425844216857989.wav` (94,080 = 5.88 s, MALE, "i̇spanyollar üç asır süren kolonileşme dönemini başlattı"),
  `4110479544867812528.wav` (120,000), `602812882332095246.wav` (120,000), `11297332020673814086.wav` (116,160).
  Turkish dev rows seen were all MALE speakers; gender balance for TR is UNVERIFIED.
- en_us test.tsv: `621925716775998155.wav` (94,080, FEMALE, id 1914, "Twentieth century research has shown that there are two pools of genetic variation: hidden and expressed."), in test.tar.gz (290 MB).
These do not give us exact end-of-speech times: FLEURS has no alignments. UNVERIFIED whether clips carry leading/trailing silence. The fixture builder must compute end-of-speech itself (energy threshold on the 16 kHz PCM) and store it in the committed manifest; the manifest then is the ground truth.

Size budget: 12 clips per language, each <= 6 s, is about 2.3 MB total at 16 kHz mono 16-bit (estimate). Commit WAV (or trimmed WAV) plus a `manifest.json` (filename, language, speaker gender, transcript, num_samples, sha256 of committed bytes, end_of_speech_sample).

## License risk and attribution

CC BY 4.0 legal code, section 3(a)(1) (https://creativecommons.org/licenses/by/4.0/legalcode.en), requires on redistribution:
(i) identification of creator(s) / those designated for attribution, (ii) a copyright notice, (iii) a notice referring to the Public License,
(iv) a notice referring to the disclaimer of warranties, (v) a URI/hyperlink to the Licensed Material where reasonably practicable;
and (B) "indicate if You modified the Licensed Material and retain an indication of any previous modifications". Section 3(a)(2): conditions may be satisfied "in any reasonable manner based on the medium, means, and context". Concatenating clips or trimming silence counts as a modification: say so.

Redistribution inside an Apache-2.0/MIT repo: allowed as long as the fixtures keep their own license notice; CC BY data is not relicensed. Keep it in a dedicated directory (e.g. `tests/fixtures/speech/`) with its own `LICENSE`/`NOTICE`, excluded from the code license claim. Generated mixes are "Adapted Material" and must carry the same notice.

ShareAlike risk (a legal question for the maintainers): FLORES text is CC BY-SA 4.0 (per the FLORES repo). Under CC BY-SA 4.0 section 3(b), adapted material must be offered under BY-SA (or compatible) and "You may not offer or impose any additional or different terms or conditions" restricting the adapter license. The FLEURS card labels the whole dataset CC-BY and Google presumably cleared this, but the question applies to the transcripts (and possibly the readings). Mitigation options: (a) ship the fixtures directory under CC BY-SA 4.0 notice (safest, harmless for tests); (b) ship audio without sentence text and put transcripts only in the fetched (uncommitted) layer; (c) use LibriSpeech (public-domain books, CC BY 4.0 audio) for English-only committed clips. Our recommendation is (a). This is not legal advice.

Attribution text to ship (`tests/fixtures/speech/NOTICE`):
```
Speech clips in this directory are taken from the FLEURS dataset (Google),
https://huggingface.co/datasets/google/fleurs (revision 70bb2e84b976b7e960aa89f1c648e09c59f894dd),
licensed under Creative Commons Attribution 4.0 International (CC BY 4.0),
https://creativecommons.org/licenses/by/4.0/ . Sentence texts originate from the FLORES benchmark
(Wikimedia-derived, CC BY-SA 4.0, https://creativecommons.org/licenses/by-sa/4.0/); this directory is
therefore also made available under CC BY-SA 4.0. The clips were modified by Brisvox: selected,
trimmed and/or concatenated with synthetic silence, and resampled/re-encoded only if stated in manifest.json.
No endorsement by the dataset creators or speakers is implied. The material is provided "as is"
without warranties (see the license's disclaimer of warranties).
Citation: Conneau, Ma, Khanuja, Zhang, Axelrod, Dalmia, Riesa, Rivera, Bapna. "FLEURS: Few-shot Learning
Evaluation of Universal Representations of Speech." arXiv:2205.12446, 2022.
```

## Rejected / fallback candidates

### Mozilla Common Voice
- Since October 2025 Common Voice datasets are "exclusively available through Mozilla Data Collective" (notice on https://huggingface.co/datasets/mozilla-foundation/common_voice_17_0, which is effectively deprecated; search summary of the Mozilla announcements, https://datacollective.mozillafoundation.org).
- MDC terms (https://mozilladatacollective.com/terms): users must register a Data Consumer Account; rights are "determined by individual Data Providers"; the terms forbid to "copy, scrape, download or otherwise acquire any Dataset ... for the purpose of (A) mirroring, duplicating or reproducing the Dataset in whole or in substantial part, or (B) hosting, storing or making the Dataset available on any platform, server or repository other than the Platform", and forbid re-identification of contributors.
- Result: account + terms acceptance (violates "no accounts"), and committing clips to our repo conflicts with the platform terms. The current CC0 status on MDC was NOT re-verified (FAQ URL returned 404; discourse posts did not state it). Rejected.

### LibriSpeech (OpenSLR 12) - English only
- https://www.openslr.org/12/ : license CC BY 4.0, 16 kHz read English. Files: dev-clean.tar.gz 337M, dev-other 314M, test-clean 346M, test-other 328M, train-clean-100 6.3G; md5sum.txt. Mirrors: `https://openslr.trmal.net/resources/12/<file>`, `https://openslr.elda.org/resources/12/<file>`, `https://openslr.magicdatatech.com/resources/12/<file>`. (OpenSLR publishes md5, not SHA-256: pin our own SHA-256 after first download.)
- Citation: Panayotov, Chen, Povey, Khudanpur, "LibriSpeech: an ASR corpus based on public domain audio books", ICASSP 2015.
- Archives are FLAC inside tar.gz (UNVERIFIED from this fetch); same prefix-streaming trick works. Forced alignments for word end times were not researched.

### MediaSpeech (OpenSLR 108) - Turkish, no English
- https://www.openslr.org/108/ : AR/ES/FR/TR, 10 h each, CC BY 4.0, file `https://openslr.trmal.net/resources/108/TR.tgz` (618M). Citation: Kolobov et al. 2021, arXiv:2103.16193.
- https://github.com/NTRLab/MediaSpeech : "converted to single-channel 16 kHz 16-bit PCM encoded WAV files using FFmpeg"; "distributed under the Creative Commons Attribution 4.0 International License"; but audio is from YouTube channels and "copyright remains with the original owners of the video", with a takedown policy. Higher legal risk than FLEURS for committing clips; usable only as a fetched (not committed) Turkish extension (news-style, noisy, ASR-cut segments, so durations are less controlled).

### ISSAI Turkish Speech Corpus
- https://huggingface.co/datasets/issai/Turkish_Speech_Corpus : not gated, only `ISSAI_TSC_218.tar.gz` (218.2 h, 186,171 utterances), HF license tag `mit`. Another search summary listed it as CC BY 4.0 (UNVERIFIED which is authoritative). Single huge tarball, sample rate not stated on the card. Rejected.

### Tatoeba
- https://tatoeba.org/en/downloads : per-file URL `https://tatoeba.org/audio/download/<audio_id>`; export `https://downloads.tatoeba.org/exports/sentences_with_audio.tar.bz2` (IDs only). License is "chosen by the contributor"; "If the license field is empty, you may not reuse the audio outside the Tatoeba project." Per-clip license vetting would be needed. Rejected.

### TTS-free alternatives
All above are human recordings. Self-recorded clips by the maintainers would avoid licenses entirely (and give clean silence) but need a human; consider them if FLEURS ends up unsuitable. Synthetic silence/noise gaps are produced by the fixture builder, not downloaded.

## Open items for the implementer
1. Run the fetch script once; record: HTTP status chain for the `resolve` URL, whether Range is honoured, tar member names, WAV header (channels, bit depth), first-member order, leading/trailing silence of chosen clips.
2. Pick clips by `num_samples <= 96000` from the dev TSV, balance genders where possible, compute end-of-speech by energy gate, write `manifest.json`.
3. Pin SHA-256 of each committed WAV in the manifest; the fetch script for larger sets pins the LFS sha256 above (whole-tar) or per-clip hashes recorded after first extraction.
4. Add the NOTICE text above; record the CC BY-SA assumption (option a).

## Source URLs (all accessed 2026-10-07)
- https://huggingface.co/datasets/google/fleurs ; /raw/main/README.md ; /commits/main ; /raw/d7c758a/fleurs.py (old loader)
- https://huggingface.co/api/datasets/google/fleurs ; .../tree/main/{data,parquet-data}/{en_us,tr_tr}[/audio] ; .../tree/70bb2e84.../data/{en_us,tr_tr}/audio
- https://datasets-server.huggingface.co/first-rows?dataset=google%2Ffleurs&config=en_us&split=validation ; https://huggingface.co/docs/dataset-viewer/rows
- https://arxiv.org/abs/2205.12446 ; https://www.tensorflow.org/datasets/catalog/xtreme_s ; https://github.com/facebookresearch/flores
- https://creativecommons.org/licenses/by/4.0/legalcode.en ; https://creativecommons.org/licenses/by-sa/4.0/legalcode.en
- https://mozilladatacollective.com/terms ; https://huggingface.co/datasets/mozilla-foundation/common_voice_17_0 ; https://discourse.mozilla.org/t/datasets-a-public-api-and-more-coming-in-2025/145859
- https://www.openslr.org/12/ ; https://www.openslr.org/108/ ; https://arxiv.org/abs/2103.16193 ; https://github.com/NTRLab/MediaSpeech
- https://huggingface.co/datasets/issai/Turkish_Speech_Corpus ; https://tatoeba.org/en/downloads
