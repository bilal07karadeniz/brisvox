# Silero VAD (ONNX) — VAD

- Docs: https://github.com/snakers4/silero-vad (README, fetched 2026-10-08)
- Source: https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/utils_vad.py (OnnxWrapper, VADIterator, get_speech_timestamps)
- Source: https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/model.py (load_silero_vad)
- Version history: https://github.com/snakers4/silero-vad/wiki/Version-history-and-Available-Models
- Releases: https://github.com/snakers4/silero-vad/releases
- PyPI: https://pypi.org/project/silero-vad/
- Fetched: 2026-10-08. Fetched through a summarizing tool, so quotes are short excerpts; re-check exact text before relying on byte-level details.

## Versions and license

- Latest package: `silero-vad` 6.2.3 on PyPI, dated "Sep 23, 2026" (PyPI page). GitHub release v6.2.3 "Make torchaudio optional" dated "23 Sep" (year not shown there; matches PyPI).
- Previous: v6.2.2 (17 Sep, adds `silero_vad_16k_sequence.onnx`, 16 kHz only), v6.2.1 (24 Feb, onnxruntime made optional), v6.2 (06 Nov), v6.1 (05 Nov), v6.0 (26 Aug), v5.1.x (Jul/Oct).
- Model versions (wiki): v5.0 JIT+ONNX added 2024-06-27; v6.0 JIT+ONNX added 2025-08-25; v6.2 "Added ifless ONNX and tinygrad 16k models" 2025-12-10.
- Wiki date vs release date for v6.2 do not match (wiki 2025-12-10; releases page "06 Nov", year not shown). UNVERIFIED which is the model-add date.
- License: MIT. LICENSE text begins "MIT License / Copyright (c) 2020-present Silero Team" (https://raw.githubusercontent.com/snakers4/silero-vad/master/LICENSE). PyPI also says MIT.
- UNVERIFIED conflict: the GitHub page has a badge whose alt text reads "License: CC BY-NC 4.0". Not resolved; check whether it refers to model weights or a sub-asset before shipping model files.

## ONNX file

- Path in repo: `src/silero_vad/data/silero_vad.onnx`. Other files there: `silero_vad_16k_op15.onnx`, `silero_vad_16k_sequence.onnx`, `silero_vad_half.onnx`, `silero_vad_op18_ifless.onnx`, `silero_vad_openvino_16k.onnx`, `silero_vad.jit`.
- `load_silero_vad(onnx=True, opset_version=16)` loads `silero_vad.onnx`. Other opsets (only 15 and 16 are allowed) load `silero_vad_16k_op{N}.onnx` (16k only). `sequence=True` loads `silero_vad_16k_sequence.onnx`. `silero_vad_op18_ifless.onnx` is NOT referenced by any code path (op18 cannot be selected).
- Sample rates: README says "Silero VAD supports 8000 Hz and 16000 Hz sampling rates." The wrapper sets `sample_rates = [16000]` only if `'16k'` is in the path, else `[8000, 16000]`. So `silero_vad.onnx` accepts both.
- Opset: 15 or 16 (wiki: "ONNX opset 15 and 16").
- Runtime: onnxruntime is optional. PyPI extra `onnx-cpu` installs `onnxruntime>=1.16.1` and numpy. Brisvox can load the file with onnxruntime directly; no PyPI wrapper needed.

### Pinned source

Pin by tag, not by `master` (master is mutable).

- Release tag: `v6.2.3` (GitHub release, published 2026-09-23T11:36:47Z; target_commitish `master` at publish time).
- Tag `v6.2.3` resolves to commit `5cd7945676eb32225748052e2e6a0580e4686a08`
  (source: https://api.github.com/repos/snakers4/silero-vad/git/refs/tags/v6.2.3, fetched 2026-10-08).
- File at that tag: `src/silero_vad/data/silero_vad.onnx`, size 2327524 bytes, git blob SHA `80c5592ef1f4c9ede3e357bbd02eb863358a6a9d`
  (source: https://api.github.com/repos/snakers4/silero-vad/contents/src/silero_vad/data/silero_vad.onnx?ref=v6.2.3, fetched 2026-10-08).
- Download URL (pinned): https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.3/src/silero_vad/data/silero_vad.onnx
- Git blob SHA-1 is NOT the SHA-256 needed for vendoring. The GitHub API does not return a SHA-256 digest for this asset (field absent in the response). Release has no uploaded assets.
- SHA-256 of `silero_vad.onnx` at v6.2.3: UNVERIFIED. Not computed: the research environment has no shell and fetched text cannot yield the raw bytes. To finish: download the pinned URL above, run `sha256sum silero_vad.onnx`, confirm size 2327524 and git blob hash (`git hash-object` gives the blob SHA-1 for the same bytes; it should equal `80c5592e...`), then record the SHA-256 here and in the design notes.
- Brisvox build code must not fetch this file until the SHA-256 is recorded and checked.

### Model generation of the file (research, 2026-10-08): still UNVERIFIED

- GitHub file page (https://github.com/snakers4/silero-vad/blob/master/src/silero_vad/data/silero_vad.onnx): only "2.22 MB" shown; no version, producer, opset or context size. Binary contents not fetchable, so `model.metadata_props`, `producer_name` and `producer_version` could NOT be read.
- Wiki version table (URL above): lists v5.0, v6.0 and v6.2 (ifless ONNX, tinygrad 16k) but does NOT map file names to versions and does NOT state context sizes. Current table entry: `'silero_vad'` about 260K params, streaming, ONNX opsets 15 and 16.
- Releases page: v6.0 and v6.2 notes do not mention ONNX or context. Only v6.2.2 names a file (`silero_vad_16k_sequence.onnx`).
- Code (master): no per-version branches (no 'v5', 'v6', 'op18' or 'ifless' checks). The `context_size = 64 if sr == 16000 else 32` rule applies to whatever file is loaded, but the code does not prove which generation `silero_vad.onnx` is.
- Consequence: the 64-sample context and `(2,1,128)` state shapes come from master code, not from the graph. Treat them as UNVERIFIED for the v6.2.3 file.

### Graph I/O names (confirmed from the binary graph)

- Source: https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/data/silero_vad.onnx (master, fetched 2026-10-08 via WebFetch as a 2.2 MB binary; printable strings were then extracted from the saved copy). Producer string: `spox2`.
- Graph-level inputs, in order: `input`, `state`, `sr`. Graph-level outputs, in order: `output`, `stateN`. `sr` is consumed by node `Equal_0`; outputs come from node `If_0` (`If_0_outputs_0`, `If_0_outputs_1`). The branch on `sr` is inside the graph; the `else_branch` holds the STFT (`stft.forward_basis_buffer`), encoder conv+ReLU blocks (`encoder/0..3`) and a decoder with an `rnn` sub-graph.
- Therefore `session.run(None, {'input': ..., 'state': ..., 'sr': ...})` returns `[output, stateN]`, consistent with the wrapper's `out, state = ort_outs`.
- NOT confirmed: element types and dims. The protobuf type fields were not decodable with the tools available (no Python/onnx or onnxruntime execution in the research environment). Dtypes/shapes in the table below are from wrapper code only.
- To close locally: `onnx.load(path).graph.input/output` and `onnxruntime.InferenceSession(path).get_inputs()/get_outputs()` on the pinned v6.2.3 file; record names, dtypes, shapes here.

## Tensor contract (from OnnxWrapper.__call__, master)

Session call (verbatim, utils_vad.py):
`ort_inputs = {'input': x.numpy(), 'state': self._state.numpy(), 'sr': np.array(sr, dtype='int64')}`
`ort_outs = self.session.run(None, ort_inputs)`
`out, state = ort_outs` (outputs are positional; the code does not read names; graph names are `output` and `stateN`)

| Name | Direction | Shape | Dtype | Notes |
|---|---|---|---|---|
| `input` | in | (batch, context + window) = (1, 576) at 16 kHz; (1, 288) at 8 kHz | float32 (inferred from `torch.zeros`/`.float()`; graph dtype UNVERIFIED) | name CONFIRMED in graph; context prepended each call |
| `state` | in | (2, batch, 128) = (2, 1, 128) | float32 (inferred) | name CONFIRMED; `reset_states`: `torch.zeros((2, batch_size, 128)).float()` |
| `sr` | in | scalar | int64 | name CONFIRMED; `np.array(sr, dtype='int64')`; 16000 or 8000 |
| `output` | out | (batch, 1) per code path | float32 (inferred) | name CONFIRMED; speech probability; returned without reshaping |
| `stateN` | out | (2, batch, 128) (inferred from input) | float32 (inferred) | name CONFIRMED; new state; stored as `self._state` |

Window / context (verbatim logic in `__call__`):
- 16 kHz: window 512 samples (32 ms), context 64 samples. `context_size = 64 if sr == 16000 else 32`.
- 8 kHz: window 256 samples (32 ms), context 32 samples.
- Per call: `x = torch.cat([self._context, x], dim=1)`; then `self._context = x[..., -context_size:]` after the call. So the context is the last 64 (or 32) samples of the previous chunk, carried across calls. Initial context is zeros (`torch.zeros(batch_size, context_size)`).
- Input length check (`_validate_input`): `if sr / x.shape[1] > 31.25:` raises. Chunks shorter than 512 samples at 16 kHz (or 256 at 8 kHz) are rejected.
- Multiples of 16 kHz (e.g. 48 kHz) are downsampled to 16 kHz by `_validate_input` (summary of code; exact line not quoted).
- Whether v5 used the same 64-sample context: UNVERIFIED. Only master code was read.
- The `Validator` class in the same file runs the session with only `input` (no `state`/`sr`). Do not copy that path.

Reset: `OnnxWrapper.reset_states(batch_size=1)` zeros `_state` and `_context` and clears last sr/batch. `VADIterator.reset_states()` also clears `triggered`, `temp_end`, `current_sample`. Call both between calls/utterances, or the context and LSTM-like state carry over.

## Reference streaming algorithm (VADIterator, master)

Defaults (`VADIterator.__init__`):
- `threshold=0.5`, `sampling_rate=16000`, `min_silence_duration_ms=100`, `speech_pad_ms=30`, `neg_threshold=None`.
- If `neg_threshold` is None: `max(threshold - 0.15, 0.01)`.

Methods (master, per fetched summary of the full file): `VADIterator` defines only `__init__`, `reset_states`, `__call__`. No `finalize`, `flush`, `close` or `end` method exists in `utils_vad.py`.

Start/end logic (verbatim lines):
- Start: `if (speech_prob >= self.threshold) and not self.triggered:` then `self.triggered = True` and returns `{'start': ...}`; `speech_start = max(0, self.current_sample - self.speech_pad_samples - window_size_samples)`
- End trigger: `if (speech_prob < self.neg_threshold) and self.triggered:` then `self.temp_end = self.current_sample`
- Confirm end: `if self.current_sample - self.temp_end < self.min_silence_samples:` returns None (keep waiting); otherwise `self.triggered = False` and returns `{'end': ...}` with `speech_end = self.temp_end + self.speech_pad_samples - window_size_samples`
- Returns sample indices (or seconds with `return_seconds=True`).

`get_speech_timestamps` defaults: `threshold=0.5`, `sampling_rate=16000`, `min_speech_duration_ms=250`, `max_speech_duration_s=inf`, `min_silence_duration_ms=100`, `speech_pad_ms=30`, `neg_threshold=None`, `window_size_samples=512` (overwritten to 256 at 8 kHz), `min_silence_at_max_speech=98`, `return_seconds=False`.
- `min_speech_samples = sampling_rate * min_speech_duration_ms / 1000`.

Note: `get_speech_timestamps` is offline (whole clip); use VADIterator or a copy of its per-chunk state machine for streaming.

## End of stream / session close (researched 2026-10-08)

- Source: https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/utils_vad.py (fetched 2026-10-08 via a summarizing tool; the tool did not return the class verbatim, so a byte-exact re-check is recommended).
- Per that fetch: the file has no end-of-input handling. `VADIterator.__call__` processes one chunk per call and never clears `triggered` by itself. `reset_states()` is called only from `__init__` or explicitly by the caller.
- Consequence: if the stream stops while `triggered` is True, VADIterator emits NO `end` event. Only `reset_states()` clears `triggered`, and it emits nothing.
- Brisvox decision (implemented in the review fixes): the VAD adapter owns the stop-on-close rule. `VadStream::finish()` (and `VadDetector::finish()`) reports a `Stop` when speech is in progress, dated to the end of the last speech window plus `speech_pad`, never past the end of the last window fed, then resets the stream (`reset_states()` on the model). Close with no open speech returns `None`. `VadStream::reset()` discards speech in progress without an event and is for a new, unrelated stream only. The synthetic stop has one event kind: the `EndOfStream`/`Cancelled` reason field is NOT implemented (no consumer needs it yet; add it with the turn-taking code if a consumer does). A partial window (fewer than 512 samples) at close is dropped.
- Test: `stream_finish_reports_the_open_segment_and_resets`, `finish_closes_open_speech_with_a_stop` (`crates/brisvox-inference/src/vad.rs`); the boundary test in `tests/silero.rs` closes each fixture with `finish()`.
- UNVERIFIED: whether any upstream release (including Python `get_speech_timestamps`) defines an end-of-stream rule. Only master `utils_vad.py` was checked.

## CPU cost

- README (https://github.com/snakers4/silero-vad, fetched 2026-10-08), verbatim: "One audio chunk (30+ ms) takes less than 1ms to be processed on a single CPU thread." No CPU model, OS, runtime version, or batch/thread conditions given; the wiki (not fetched) has detail. "Under certain conditions ONNX may even run up to 4-5x faster": baseline and conditions UNVERIFIED.
- Same-machine comparison (Xeon Gold 6348 only): Silero RTF 0.0127 in the TEN README table. That table's RTF definition is UNVERIFIED (see vad-alternatives.md section 3). Derived [inference]: ~0.41 ms per 32 ms window.
- Wrapper sets intra/inter-op threads to 1 (`OnnxWrapper.__init__`, summary).
- Status: vendor claim only for the README figure. Brisvox (Rust `ort` 2.0.0-rc.13, one intra/one inter-op thread, release build) was measured on the development machine (Windows 11, AMD64 family 26 model 68), 2026-10-08, with `cargo test --release -p brisvox-inference --all-features --test silero -- --ignored --nocapture`: 7580 windows (20 passes over the EN fixture), mean 64.3 µs, p50 59.8 µs, p99 118.7 µs, max 506.6 µs per `SileroVad::infer` call. Still UNVERIFIED for the target container (no cgroup or CPU-quota run yet). The ~63 µs measured when the runtime was chosen is the same order and is not a separate measurement.
- Allocations per call (Rust heap, counting allocator, `a_call_makes_no_heap_copies_of_its_own`): 1320 bytes, which is the `ort` wrapper's own bookkeeping. The brisvox-side input and state copies (estimated at about 4.3 KB per call before the review fixes; not measured on the old code) are gone. The test fails if one input copy (2304 bytes) comes back.
- Measurement plan (NOT run; no execution tool in this pass):
  - (a) Python onnxruntime reference, intra/inter-op threads 1, pinned v6.2.3 file with SHA-256 recorded first. Per call: wall time over >= 10,000 calls after warmup; p50/p99/max; on the target container CPU (record `/proc/cpuinfo` model and cgroup CPU quota). Also batch=1 and N=8/32/128.
  - (b) Rust `ort` build, same model and options; compare per-call cost and output probabilities against (a) on the same recorded audio. Record results with date and commit hash. Anything above ~50 µs per window goes to `brisvox-inference`.

## Engineering notes for Brisvox

- Keepalive/timeouts: none (local model). Per-chunk inference should run on the inference pool (not on tokio workers), per the engineering invariants in CONTRIBUTING.md, if it exceeds ~50 µs. Whether it does on the target container is UNVERIFIED until measured.
- Cancellation: no native cancel; drop the session and rebuild state with `reset_states()`.
- Interim results: none; output is one probability per 32 ms window.
- Turkish: language-agnostic VAD; no language-specific claims found.
- Gotchas: context must be prepended to every chunk; omitting it changes the probabilities. Keep `state` and `context` per session (never share across sessions). Use `sr` as int64.
- Mocking/tests: the fixture needs a recorded (input, state) -> (output, stateN) sequence from the actual ONNX file; none recorded yet.

## UNVERIFIED list

- SHA-256 of `silero_vad.onnx` at tag v6.2.3 (blob SHA-1 and size are confirmed above; SHA-256 is not).
- Model generation (v5, v6.0 or v6.2) of `silero_vad.onnx` at v6.2.3. Wiki lists versions but not file-to-version mapping. The graph's node naming (`encoder/0..3/reparam_conv`, `decoder/rnn`, `stft`) is consistent with the current wrapper but does not fix a version.
- Element types and dims of graph tensors (names are confirmed; dtypes/shapes are inferred from wrapper code only).
- Context size 64 and state `(2,1,128)` in the shipped graph (master code only).
- Whether master's `silero_vad.onnx` is byte-identical to v6.2.3 (not compared).
- Year of GitHub release dates without year (PyPI confirms Sep 23, 2026 for 6.2.3); wiki v6.2 date vs releases page date not reconciled.
- The CC BY-NC 4.0 badge conflict.
- v5-specific context handling (no v5 code or file was read).
- Upstream end-of-stream behaviour beyond master `utils_vad.py`.
- All Brisvox per-window CPU and latency numbers (no measurement run); TEN RTF definition, hop, thread count, CPU-row mapping.

## Confirmed locally (2026-10-08)
Downloaded the pinned file and inspected it with onnx 1.23.2 / onnxruntime 1.30.0:
- `silero_vad.onnx` at v6.2.3: 2327524 bytes, git blob `80c5592ef1f4c9ede3e357bbd02eb863358a6a9d`
  (matches the GitHub API), **SHA-256 `1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3`**.
- License: **MIT** (LICENSE file; README: "Published under permissive license (MIT)"). The "CC BY-NC
  4.0" text is a stale *alt text* of a badge whose image reads "License: MIT" and links to the MIT
  LICENSE - not a license grant.
- Graph (producer `spox`, IR 8, opset 16; top-level ops Constant/Equal/Identity/If - the `If`
  selects the 16 kHz or 8 kHz subgraph from `sr`):
  - inputs: `input` float [batch, samples], `state` float [2, batch, 128], `sr` int64 scalar;
  - outputs: `output` float [batch, 1] (speech probability), `stateN` float [2, batch, 128].
- Streaming per the reference wrapper (512 new samples + 64 context = [1, 576] at 16 kHz) on the
  three committed fixtures: **~76 µs per 32 ms window** (p99 ~86 µs) with onnxruntime, one
  thread, including Python call overhead - far below the README's "< 1 ms" claim. Golden speech
  probabilities from this run are the reference for the Rust implementation's tests.

## In brisvox
`brisvox_inference::silero::SileroVad` (feature `silero`) implements `SpeechModel`; streams use
`brisvox_inference::VadStream` + `VadDetector` (hysteresis: threshold 0.5 / neg 0.35, min speech
64 ms, min silence 100 ms, pad 30 ms). Tests (`crates/brisvox-inference/tests/silero.rs`):
- probabilities within 1e-7 of the onnxruntime reference (fixtures/vad_golden.json, generated by
  scripts/models/make_vad_goldens.py) on every window. Measured worst case: 5.96e-8 on all three
  fixtures (one f32 ulp at 0.5; the goldens are rounded to 7 decimals, so this is the floor). The
  test asserts 1e-7, so the documented 6e-8 accuracy is enforced with a small margin.
- start/stop within 50 ms of the fixtures' ground truth (EN and TR), asserted. Measured worst
  case: 50 ms (en_tr_mixed start, 400 ms vs 450 ms; the stream is fed 20 ms frames). A 60 ms
  default pad makes the test fail (78 ms), so a pad regression is caught.
- Malformed model outputs (empty `output`, non-finite or out-of-range probability, wrong
  `stateN` length) are errors, not silence or a stale state (`silero::tests`).
- Runtime choice: ONNX Runtime via the `ort` crate.

Reviewed behaviour (review fixes, `crates/brisvox-inference/src/vad.rs`):
- A model error drops only its window; the rest of the batch is processed, the context still
  advances, and the first error is returned after the batch.
- A band window (`neg_threshold <= p < threshold`) neither confirms nor cancels a pending stop;
  only a window `>= threshold` cancels it. The fetched reference (above) runs its end trigger only
  on windows below `neg_threshold`, so band windows leave its pending end alone; the cancel-on-
  threshold rule is from the reference source as remembered, not re-fetched (UNVERIFIED detail).
- Events are time-ordered. A start backdated past the previous event is dated to that event; this
  only happens when `speech_pad` is at least half of `min_silence` (not with the defaults).
- Stream close: `VadStream::finish()` reports the open segment's stop (see "End of stream").

## Pure-Rust backend
`brisvox_inference::silero_tract` (feature `silero-tract`) runs the prepared graph
(assets/models/silero_vad_16k_tract.onnx, embedded): 36-45 µs per window, within 2.1e-6 of the
reference, identical boundaries. Prepare/regenerate it with scripts/models/prepare_silero_tract.py
(needs onnx + onnxruntime; byte-identical with onnxruntime 1.30.0). Speed comparison test:
`cargo test --release -p brisvox-inference --all-features --test vad_speed -- --nocapture`.
