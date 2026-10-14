# ONNX Runtime from Rust (ort) — local inference (VAD, optional STT/TTS models)

- Docs (fetched 2026-10-08):
  - crates.io API: https://crates.io/api/v1/crates/ort
  - ort guide: https://ort.pyke.io/ (setup/linking, setup/cargo-features, setup/platforms, misc/version-mapping, misc/prebuilt-binaries)
  - Prebuilt target table (authoritative for download-binaries, tag v2.0.0-rc.13): https://raw.githubusercontent.com/pykeio/ort/v2.0.0-rc.13/ort-sys/build/download/dist.tsv
  - Release notes: https://github.com/pykeio/ort/releases (v2.0.0-rc.13, 28 Jul)
  - docs.rs: https://docs.rs/ort/2.0.0-rc.13/ort/ , .../ort/session/struct.Session.html , .../ort/session/builder/struct.SessionBuilder.html , .../ort/session/struct.IoBinding.html , .../ort/value/index.html , .../ort/value/type.TensorRef.html , .../ort/fn.api.html
  - IoBinding guide: https://ort.pyke.io/perf/io-binding
  - Repo: https://github.com/pykeio/ort
  - ORT release (upstream): https://github.com/microsoft/onnxruntime/releases/tag/v1.30.0 (exists; "ONNX Runtime v1.30.0", released 10 Sep per the page)
  - Official ORT threading: https://onnxruntime.ai/docs/performance/tune-performance/threading.html
  - Official ORT FAQ: https://raw.githubusercontent.com/microsoft/onnxruntime/main/docs/FAQ.md
  - Official ORT threading notes: https://raw.githubusercontent.com/microsoft/onnxruntime/main/docs/NotesOnThreading.md
- 404 or no usable content: ort.pyke.io/perf/threading, ort.pyke.io/fundamentals/session, ort.pyke.io/setup/linking (no version text), ort.pyke.io/setup/platforms (no data), raw GitHub docs/setup/platforms.mdx and docs/perf/threading.mdx (404), raw/blob ort-sys/build.rs (404), docs.rs .../value/struct.TensorRef.html (use type.TensorRef.html). Unverified items are marked below.
- WebFetch returns model-summarised text, not verbatim bytes. "Quoted" below means paraphrased by the fetch tool; re-check the raw page before quoting in code comments.

## Versions, license, MSRV
- crates.io: NO stable 2.x release is published. Latest is `2.0.0-rc.13` (created 2026-07-28), not yanked. Previous: rc.12 (2026-03-05), rc.11 (2026-01-07). Source: crates.io API above.
- Pin exactly: `ort = "=2.0.0-rc.13"` (form shown on ort.pyke.io). Pre-release means API may still change; pin and record any bump in the design notes.
- License: MIT OR Apache-2.0 (crates.io; repo has LICENSE-MIT and LICENSE-APACHE). Compatible with the project rule.
- MSRV: 1.88 (crates.io `rust_version` of rc.13). Toolchain must be >= 1.88.

## ORT version (research gap)
- Documented target of ort 2.0.x: ONNX Runtime 1.28.
  - crates.io description of ort-sys/ort (rc.13): "Unsafe Rust bindings for ONNX Runtime 1.28" / "A safe Rust wrapper for ONNX Runtime 1.28" (docs.rs ort, ort-sys).
  - ort.pyke.io/misc/version-mapping: table row "v2.0.0+ | v1.28.0"; sentence "i.e. 2.0 ships with ONNX Runtime 1.28, but 2.1 may ship with 1.29."
- CONFIRMED for the rc.13 tag: its dist.tsv (URL above) lists only rows with ORT version 1.28.0. Release notes say rc.13 "skips ahead 4 ONNX Runtime versions to v1.28" (https://github.com/pykeio/ort/releases). So rc.13 downloads 1.28.0 prebuilts.
- Conflicting signal: the GitHub README badge reads "onnxruntime-v1.30.0", and the `main` branch dist.tsv lists 1.30.0 rows. Both are AHEAD of the rc.13 tag; do not use them for rc.13.
- Runtime print check: NOT EXECUTED (no cargo available to the author). Close with a minimal crate that prints the runtime version via the API (confirm names on docs.rs) and inspects `target/*/build/ort-sys-*/output`.
- Opset 16 for Silero: UNVERIFIED. The silero-vad README gives only `onnxruntime>=1.16.1` as a pip extra, and no opset number. Load the actual model with the ORT the build reports and record the result here.
- Decision rule: treat the loaded runtime version as authoritative; design for 1.28.

## Cargo features (ort.pyke.io/setup/cargo-features)
- Default: `std`, `ndarray`, `tracing`, `download-binaries`, `copy-dylibs`, `tls-native`.
- Not default: `load-dynamic`, `alternative-backend`, `pkg-config`, `lax-feature-matching`, `half`, `num-complex`, `preload-dylibs`, `tls-rustls`, `tls-native-vendored`, and one feature per execution provider (e.g. `cuda`; full list not read).
- `download-binaries`: fetches prebuilt ORT from pyke's CDN over HTTPS; requires a TLS feature.
- `copy-dylibs`: copies dylibs next to the target so the loader finds them (relevant for CUDA-style EPs; on Windows only for binary crates unless Developer Mode).
- `load-dynamic`: no link-time dependency; the library is loaded at runtime. Call `ort::init_from(path)` or set `ORT_DYLIB_PATH` BEFORE any other ort use. Relative `ORT_DYLIB_PATH` is resolved relative to the executable (examples/tests build to different dirs).
- Static/compile-time link: `ORT_LIB_PATH` (dir with .a/.lib), `ORT_LIB_PROFILE`; `ORT_PREFER_DYNAMIC_LINK=1` for dynamic link. Static is preferred if both exist.
- rpath note from docs: Linux needs `-Clink-args=-Wl,-rpath,$ORIGIN` for dynamic link.

## Prebuilt binaries per target (download-binaries)
Source: dist.tsv at tag v2.0.0-rc.13 (URL above). All rows are ORT 1.28.0:
- `x86_64-unknown-linux-gnu`: feature sets `none`, `webgpu`, `nvrtx`, `cuda13,tensorrt,nvrtx`
- `aarch64-unknown-linux-gnu`: `none` only
- `x86_64-pc-windows-msvc`: `directml`, `webgpu`, `nvrtx,directml`, `cuda13,tensorrt,nvrtx,directml` (no `none` row)
- `aarch64-pc-windows-msvc`: `directml` only
- No `*-musl` rows (CONFIRMED absent).

Constraints from https://ort.pyke.io/misc/prebuilt-binaries (fetched 2026-10-08):
- "All x86-64 binaries are compiled with a baseline requirement of x86-64-v3" (Haswell/AVX2-class CPU or later).
- "Linux binaries are compiled with Clang, not GCC, and depend on `libc++`." Container images must provide libc++ or ship it.
- "All Windows builds come with the DirectML EP enabled." CPU-only Windows selection: UNVERIFIED.
- Release notes rc.13: CUDA 13 only (CUDA 12 dropped). rc.11 dropped Intel macOS.

Status for Brisvox (Linux first): Linux glibc x86_64 and aarch64 have a CPU (`none`) row, so download-binaries is a candidate; actual download, link, and libc++ presence in the target image: UNVERIFIED until a CI build runs. Windows rows are DirectML-only. musl has no rows: use `load-dynamic` with a musl-built `libonnxruntime.so` or a source build (neither verified).

## Session API (docs.rs 2.0.0-rc.13)
```rust
use ort::session::{builder::GraphOptimizationLevel, Session};

let mut model = Session::builder()?
    .with_optimization_level(GraphOptimizationLevel::Level3)?
    .with_intra_threads(4)?
    .commit_from_file("yolov8m.onnx")?;
```
(example from ort.pyke.io home page)

Signatures (docs.rs SessionBuilder):
- `pub fn commit_from_file<P>(&mut self, model_path: P) -> Result<Session> where P: AsRef<Path>` (requires `std`)
- `pub fn commit_from_memory(&mut self, model_bytes: &[u8]) -> Result<Session>` (not on wasm)
- `pub fn with_intra_threads(self, num_threads: usize) -> BuilderResult`
- `pub fn with_inter_threads(self, num_threads: usize) -> BuilderResult`
- `pub fn with_parallel_execution(self, parallel_execution: bool) -> BuilderResult`
- `pub fn with_memory_pattern(self, enable: bool) -> BuilderResult`
- `pub fn with_allocator(self, info: MemoryInfo<'_>) -> BuilderResult`
- `pub fn with_config_entry(self, key: impl AsRef<str>, value: impl AsRef<str>) -> BuilderResult`
- `pub fn with_independent_thread_pool(self) -> BuilderResult`
- `BuilderResult` is a type alias (expansion not read; the `?` usage shows it is fallible).

Running (docs.rs Session):
- `pub fn run<'s, 'i, 'v: 'i, const N: usize>(&'s mut self, input_values: impl Into<SessionInputs<'i, 'v, N>>) -> Result<SessionOutputs<'s>>`
- Inputs are built with `ort::inputs!["name" => tensor]`; `run` is `&mut self` (reason not stated).
- Outputs: `let predictions = outputs["output0"].try_extract_array::<f32>()?;` (ort.pyke.io home page).
- `run_async` exists (std feature) and requires `with_intra_threads` > 1 per the docs; not needed for a dedicated CPU pool.

Tensors (docs.rs value module, type.TensorRef.html):
- `Tensor::from_array(([1usize, 2, 3], vec![...].into_boxed_slice()))?` — shape + boxed slice (owned data).
- `Tensor::from_array(ndarray::Array4::<f32>::zeros((1, 16, 16, 3)))?` — requires `ndarray` feature (default on).
- `TensorRef<'v, T>` = `ValueRef<'v, TensorValueType<T>>`; `TensorRefMut` analogous.

## Zero-copy input (CONFIRMED signature, copy behaviour UNVERIFIED)
- `pub fn from_array_view(input: impl TensorArrayData<T> + 'a) -> Result<TensorRef<'a, T>>` in `impl<'a, T: PrimitiveTensorElementType + Debug> TensorRef<'a, T>` (https://docs.rs/ort/2.0.0-rc.13/ort/value/type.TensorRef.html). It is the only constructor listed there.
- Doc text: "Construct a tensor from borrowed data." Accepted with `ndarray`: `&CowArray`, `&Array`, `ArcArray`, `ArrayView`; also `(shape, data)` with data `&[T]`, `Arc<[T]>` or `Arc<Box<[T]>>`.
- Ndarray inputs must be standard layout; otherwise an error. Lifetime `'a` ties the tensor to the borrowed buffer.
- NOT stated: whether it copies internally. Zero-copy is UNVERIFIED until the allocation test below passes.

## Threading and concurrency (researched 2026-10-08)
Confirmed by fetched pages (quoted by fetch tool):
- Default `intra_op_num_threads` = 0/unset: "INTRA Threads Total = Number of physical CPU Cores"; main thread on core 1 not affinitized. Source: https://onnxruntime.ai/docs/performance/tune-performance/threading.html
- Default `inter_op_num_threads` = one per physical core; the inter-op pool is only created in parallel execution mode. Default execution mode is sequential. Same URL.
- "By default, session.run() uses all the computer's cores." Source: https://raw.githubusercontent.com/microsoft/onnxruntime/main/docs/FAQ.md
- "If built with OpenMP, set the environment variable OMP_NUM_THREADS to 1. If not built with OpenMP, set the session options intra_op_num_threads to 1." Same FAQ.
- OpenMP is a build-time choice (`--use_openmp`); inter-op always uses the ORT thread pool. Source: https://raw.githubusercontent.com/microsoft/onnxruntime/main/docs/NotesOnThreading.md
- docs.rs `with_intra_threads`: "If ONNX Runtime was built with OpenMP (as is the case with Microsoft's prebuilt binaries), this will have no effect"; use `OMP_NUM_THREADS`. Source: https://docs.rs/ort/2.0.0-rc.13/ort/session/builder/struct.SessionBuilder.html
- docs.rs `with_parallel_execution`: "By default, this is disabled." `with_independent_thread_pool`: opt-in instead of "defaulting to the Environment's global thread pool if one was defined." Same URL.

NOT confirmed:
- Whether pyke CDN binaries are OpenMP builds. The ort doc comment names only Microsoft's prebuilt binaries. If OpenMP, `with_intra_threads` is ignored and `OMP_NUM_THREADS` decides. UNVERIFIED by docs; MEASURED on the binary brisvox uses (Windows, 2026-10-08 and 2026-10-12): Smart Turn gets faster with 2 and 4 intra-op threads (~23 / 12.5 / 8 ms), so `with_intra_threads` takes effect there (docs/LATENCY.md; ignored test `more_threads_cut_the_latency_of_warm_and_cold_sessions`). Re-run that test on the Linux target image.
- Whether ort overrides ORT default thread counts (ort.pyke.io/perf/threading 404). UNVERIFIED.
- Concurrent `run` on one `Session` from several threads: not stated on any fetched official page; `run*` takes `&mut self`. A third-party package claims concurrent calls are permitted and a forum report shows a crash under an ACL EP when tensors were shared. Treat as UNSUPPORTED: one Session per worker thread, or `Mutex<Session>`.
- Latency and thread count of a 1-thread session on the target container: NOT BENCHMARKED (no shell or target container for the author). The implementer must measure.

Settings for a 1-thread-per-session CPU pool (design choice from the FAQ, not a doc-stated recommendation for this setup):
- `OMP_NUM_THREADS=1` in the process environment (OpenMP builds) AND `.with_intra_threads(1)?` (non-OpenMP builds). Both are harmless otherwise.
- `.with_inter_threads(1)?` and `.with_parallel_execution(false)?` (sequential is the default).
- `.with_independent_thread_pool()?` only if sessions must not share the Environment global pool; confirm validity before relying on it.
- Verify at runtime: count threads spawned by a session (Linux `/proc/self/task`, or Windows thread count) after `commit_from_file`.
- Inference runs on the dedicated `brisvox-inference` pool, not tokio workers (engineering invariant, see CONTRIBUTING.md).

## Per-call allocation and IoBinding (docs.rs IoBinding + Session; https://ort.pyke.io/perf/io-binding)
Confirmed signatures (docs.rs 2.0.0-rc.13, non-wasm):
- `Session::create_binding(&self) -> Result<IoBinding>` — "Creates a new [`IoBinding`] for this session."
- `Session::run_binding<'b, 's: 'b>(&'s mut self, binding: &'b IoBinding) -> Result<SessionOutputs<'b>>` (CONFIRMED)
- `Session::run_binding_with_options<'r, 'b, 's: 'b>(&'s mut self, binding: &'b IoBinding, run_options: &'r RunOptions<NoSelectedOutputs>) -> Result<SessionOutputs<'b>>`
- `IoBinding::bind_input<T: ValueTypeMarker + ?Sized, S: Into<String>>(&mut self, name: S, ort_value: &Value<T>) -> Result<()>` — "The data is only copied upon invocation of this function."
- `IoBinding::bind_output<T, S: Into<String>>(&mut self, name: S, ort_value: Value<T>) -> Result<()>` — later runs "will reuse the same tensor to store the output instead of creating a new one each time."
- `IoBinding::bind_output_to_device<S: Into<String>>(&mut self, name: S, mem_info: &MemoryInfo<'_>) -> Result<()>` — for unknown output shapes; prefer `bind_output` when the shape is known.
- Guide: bindings apply to individual `IoBinding` instances, so create one per session worker, not per call. The guide's example uses `Tensor::<f32>::new(&Allocator::default(), ...)`, which is NOT the rc.13 `Tensor::from_array` API; the rc.13 equivalent is UNVERIFIED.

Still UNVERIFIED: whether `from_array_view` copies; whether ORT allocates internally per run (arena/memory pattern; effects of `with_memory_pattern` / `with_allocator` not documented on fetched pages); whether a warm `run_binding` performs zero heap allocations (only the docs' "reuse" intent is confirmed).

Partial result (review fixes, 2026-10-08): `SileroVad::infer` now lends its input and state to `session.run` as `TensorRef::from_array_view` (no brisvox-side copies) and writes the new state into a reused buffer. The counting-allocator test `a_call_makes_no_heap_copies_of_its_own` (`crates/brisvox-inference/tests/silero.rs`) measures 1320 bytes of Rust heap per call over 200 warm runs; that is the `ort` wrapper's own bookkeeping, and the test fails above one input copy (2304 bytes). Items (a) and (b) of the comparison below, and the `run_binding` item (c), are still NOT measured.

Required measurement (the comparison below is NOT DONE): counting global allocator test (same pattern as the chaos tests). Warm 10 runs, then count allocations over ~1000 runs with Silero-sized input (512 f32 samples plus state tensors) for (a) `Tensor::from_array` per call, (b) `from_array_view` per call, (c) `create_binding` once + `bind_input`/`bind_output` + `run_binding`. Invariant passes only if (c) is 0 per run, or a documented constant from ORT internals is recorded here.

## Alternatives (brief)
- tract (Sonos): `tract-onnx` 0.23.8 (2026-09-21), MIT OR Apache-2.0, rust_version 1.91 (crates.io + sonos/tract README). ONNX load supported (no save); no operator coverage list and no benchmarks in the README.
  - Silero VAD v5 on tract: NOT VERIFIED. All found Rust Silero wrappers (silero, silero-vad-rs, silero-vad-rust) use `ort`. Must be tested by loading the actual model with tract.
- candle: `candle-core` 0.11.0 (2026-06-26), `candle-onnx` 0.11.0 (2026-09-16), MIT OR Apache-2.0 (crates.io). Op coverage and speed: UNVERIFIED. Not evaluated further.
- Silero VAD license: the silero-vad repo shows conflicting signals: text "Published under permissive license (MIT)" but a badge reads "License: CC BY-NC 4.0" (github.com/snakers4/silero-vad, fetched 2026-10-08). UNVERIFIED until the primary LICENSE file is read.

## Open items (UNVERIFIED, to resolve by build test)
1. Runtime version print: rc.13 = ORT 1.28.0 per the tag's dist.tsv; the runtime print is NOT executed.
2. Silero opset 16 load test against the bundled runtime (not yet run).
3. Actual CI download/link per target, CPU-only Windows selection, libc++ in target images; no musl rows.
4. Whether pyke CDN binaries are OpenMP builds; ort default thread counts. Official ORT defaults are confirmed (see Threading section).
5. Measured 1-thread session latency and thread count on the target container: NOT DONE.
6. Concurrent `run` on one session: treat as unsupported (one session per thread).
7. Per-run allocations: whether `from_array_view` copies, and whether warm `run_binding` is allocation-free (counting-allocator test above).
8. Silero VAD v5 op support in tract.
