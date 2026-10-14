# rubato 5.0.1 — audio resampling crate (not a network provider)

- Fetched 2026-10-08 from:
  - https://docs.rs/rubato/5.0.1/rubato/ (+ struct/trait/enum pages: Async, Fft, Resampler, Indexing, FixedSync, FixedAsync, SincInterpolationParameters, SincInterpolationType, WindowFunction, Sample)
  - https://docs.rs/rubato/5.0.1/src/rubato/synchro.rs.html and .../asynchro.rs.html (source)
  - https://docs.rs/crate/rubato/5.0.1 (deps, MSRV)
  - https://raw.githubusercontent.com/HEnquist/rubato/master/README.md (README + changelog), https://github.com/HEnquist/rubato
  - https://raw.githubusercontent.com/HEnquist/rubato/master/examples/process_raw.rs
  - https://docs.rs/audioadapter/5.0.0/audioadapter/
  - https://docs.rs/audioadapter-buffers/5.1.0/audioadapter_buffers/ (+ `direct`, `number_to_float`, `InterleavedNumbers`)
- Versions: rubato 5.0.1 (released 2026-10-01 per docs.rs); audioadapter 5.0.0 (2026-10-07 per docs.rs); audioadapter-buffers ^5.1 required (5.1.0 page exists; newest patch UNVERIFIED).
- License: MIT OR Apache-2.0 (rubato, audioadapter, audioadapter-buffers). MSRV: rustc 1.87.
- Dependencies (rubato 5.0.1): audioadapter ^5.0, audioadapter-buffers ^5.1, num-integer, num-traits, visibility, windowfunctions ^0.1.1; optional: realfft ^3.5.0 + num-complex (feature `fft_resampler`, default on), log ^0.4.18 (feature `log`, off by default; keep off in real time, it allocates).
- no_std: rubato docs do not mention it (UNVERIFIED, assume std). audioadapter core traits and audioadapter-buffers work without `std` (audioadapter-buffers: `alloc` feature for owned buffers).
- SIMD: Async sinc has AVX/SSE3 (x86_64) and Neon (aarch64) with runtime detection and scalar fallback; Fft gets SIMD through RustFFT. No feature flag needed (none listed).

## 1. Resampler types in 5.x
- `Fft<T>` (synchronous, fixed ratio from two integer rates, needs `fft_resampler`). Exact rational ratio via gcd of the rates. Best fit for fixed-ratio voice.
- `Async<T>` (asynchronous: `new_sinc` = high quality/CPU heavy, `new_poly` = fast/lower quality). Ratio is f64 and adjustable at runtime (`Adjustable`); built for clock drift. Output frames per call may vary.
- `Slip<T>`: only for nearly equal rates (insert/drop frames with crossfade); NOT for 48k->16k.
- Old 3.x names (SincFixedIn, FftFixedIn, ...) are gone; replaced by `Async`/`Fft` + `FixedAsync`/`FixedSync` (4.0.0 changelog and current docs).

Constructors (exact, verbatim from docs.rs):
```rust
// Fft<T: Sample>
pub fn new(sample_rate_input: usize, sample_rate_output: usize, chunk_size: usize,
           nbr_channels: usize, fixed: FixedSync) -> Result<Self, ResamplerConstructionError>
pub fn new_custom(sample_rate_input: usize, sample_rate_output: usize, chunk_size: usize,
           sub_chunks: usize, nbr_channels: usize, window: WindowFunction,
           fixed: FixedSync) -> Result<Self, ResamplerConstructionError>
// Async<T: Sample>
pub fn new_sinc(resample_ratio: f64, max_resample_ratio_relative: f64,
           parameters: &SincInterpolationParameters, chunk_size: usize,
           nbr_channels: usize, fixed: FixedAsync) -> Result<Self, ResamplerConstructionError>
pub fn new_poly(resample_ratio: f64, max_resample_ratio_relative: f64,
           interpolation_type: PolynomialDegree, chunk_size: usize,
           nbr_channels: usize, fixed: FixedAsync) -> Result<Self, ResamplerConstructionError>
```
- `FixedSync { Input, Output, Both }`; `FixedAsync { Input, Output }`. `chunk_size` is "size of input data in frames" for Async (docs), "desired chunk size in frames" for Fft. `PolynomialDegree`: Cubic, Quadratic, Linear.
- `Fft::new` uses BlackmanHarris2 window and targets sub-chunks of ~256 frames (`sub_chunks = (chunk_size/256).max(1)` in source), "which keeps the resampler delay low". `Fft` also has `fft_size_in()`, `fft_size_out()`, `cutoff()`.
- Fft sizing (from source): `gcd = gcd(in, out)`; `fft_size_out = fft_chunks*out/gcd`, `fft_size_in = fft_chunks*in/gcd`; invalid `chunk_size` is NOT an error, it is rounded UP to a multiple of the minimum block. So the actual per-call size may differ from the requested chunk_size: always read `input_frames_next()` after construction; do not assume it equals 160/320/480. (Exact rounding for 20 ms frames UNVERIFIED; test in the resampler tests.)
- Async errors: `chunk_size == 0` -> `InvalidChunkSize`; ratio must be finite and > 0; `max_resample_ratio_relative` finite and >= 1.0 (use 1.0 for fixed ratio).
- Recommendation for Brisvox (fixed ratios, mono, 20 ms): `Fft` with `FixedSync::Input` (or `Both`; which one gives exact 20 ms in/out is UNVERIFIED), one instance per session per direction. Rates 8k/16k/24k/44.1k/48k are all integer rationals, so Fft gives exact ratios (e.g. 44100->16000: gcd 100, block 441 in / 160 out; 882-sample 20 ms frame = 2 blocks). Use `Async::new_sinc` only if the Fft delay or quality turns out worse in the resampler benchmark.

## 2. Process API (5.x)
```rust
// trait Resampler<T>
fn process_into_buffer(&mut self, buffer_in: &dyn Adapter<T>, buffer_out: &mut dyn AdapterMut<T>,
                       indexing: Option<&Indexing>) -> ResampleResult<(usize, usize)>; // (frames_read, frames_written)
fn input_frames_next(&self) -> usize;   // exact frames needed by the next call
fn input_frames_max(&self) -> usize;
fn output_frames_next(&self) -> usize;  // frames the next call produces
fn output_frames_max(&self) -> usize;   // size output buffers with this
fn output_delay(&self) -> usize;        // in OUTPUT frames
fn nbr_channels(&self) -> usize;  fn resample_ratio(&self) -> f64;
fn reset(&mut self);                    // clears state, restores initial conditions
fn as_adjustable(&mut self) -> Option<&mut dyn Adjustable<T>>;  // Some for Async only
fn as_resizable(&mut self) -> Option<&mut dyn Resizable<T>>;
// provided: process (ALLOCATES, returns InterleavedOwned<T>), process_all, process_all_into_buffer, process_all_needed_output_len, is_adjustable, is_resizable
```
- Buffers are `audioadapter::{Adapter, AdapterMut}` trait objects (not Vec<Vec<T>> as in 3.x). Wrappers come from `audioadapter-buffers`.
- `Indexing { input_offset: usize, output_offset: usize, partial_len: Option<usize>, active_channels_mask: Option<Vec<bool>> }`, `Indexing::new()` = zero offsets, None, all channels; `Default` implemented; fluent setters exist (4.0.0 changelog; exact names UNVERIFIED). Passing `None` instead of `Some(&indexing)` is allowed by the signature.
- Fixed input chunk: feed exactly `input_frames_next()` frames per call. Partial last chunk: set `indexing.partial_len = Some(n)`; the rest is treated as silence and the output buffer must still hold a full output chunk. `Some(0)` pushes pure silence (flush the delay). Default `None` = read the full required count.
- Zero-alloc steady state: `process_into_buffer` does not allocate (Async: preallocated buffers, `copy_within`; Fft: scratch buffers preallocated in the constructor; README states real-time safe). Adapter wrappers are thin views (no heap). `Indexing::new()` does not allocate unless `active_channels_mask` is set. Never use `process`/`process_all` on the hot path. Construction allocates (sinc tables, FFT plans): construct at session setup, off the tokio workers (CONTRIBUTING.md: >50us goes to the inference pool).
- CPU: processing is synchronous and CPU-bound; measure whether a 20 ms frame stays under the 50 us budget, else run it on the inference pool.

## 3. Delay and parameters
- `output_delay()` returns delay in output frames. Fft: `fft_size_out / 2` (source). Async: `(nbr_points * resample_ratio / 2.0) as usize` (source; nbr_points presumably = sinc_len, UNVERIFIED). Convert: `delay_ms = output_delay() * 1000 / out_rate`. Example from the formula only (not measured): Async sinc_len 128, 48k->16k (ratio 1/3) ~21 output frames ~1.3 ms; Fft delay depends on the block size, measure in the resampler benchmark.
- `SincInterpolationParameters` fields: `sinc_len` ("256 is a good starting point"), `f_cutoff: Option<f32>` (None = auto via `calculate_cutoff`, preferred), `oversampling_factor` ("Start at 128"; lower keeps tables in CPU cache), `interpolation: SincInterpolationType`, `window: WindowFunction`. Defaults: 256 / None / 128 / Cubic / BlackmanHarris2.
- 5.x builder: `SincInterpolationParameters::new(sinc_len: usize, window: WindowFunction)` then `.sinc_len() .window() .f_cutoff(f32) .oversampling_factor(usize) .interpolation(SincInterpolationType)`.
- `SincInterpolationType`: `Nearest` (only for ratios with small-denominator fractions), `Linear` (fastest for 1-2 ch, needs larger oversampling), `Quadratic`, `Cubic` (best quality per oversampling).
- `WindowFunction`: `Blackman`, `Blackman2`, `BlackmanHarris`, `BlackmanHarris2` (slower rolloff, better attenuation), `Hann`, `Hann2`. Repo example uses `new(128, Blackman2).oversampling_factor(256).interpolation(Quadratic)`.
- Voice starting point (UNVERIFIED quality, tune with the SNR sweep): sinc_len 64-128, BlackmanHarris2 or Blackman2, oversampling 128, Cubic; shorter sinc_len lowers delay but lowers cutoff/roll-off near Nyquist.

## 4. Sample types and i16
- `Sample` is implemented only for `f32` and `f64`. No direct i16 processing.
- Convert via audioadapter-buffers `number_to_float::InterleavedNumbers` (also `SequentialNumbers`): wraps `&[i16]` / `&mut [i16]` and exposes `Adapter<f32>` / `AdapterMut<f32>` with on-the-fly conversion:
  `pub fn new(buf: &'a [U], channels: usize, frames: usize) -> Result<Self, SizeError>`; `new_mut(buf: &'a mut [U], ...)`; `new_from_bytes(_mut)` for raw byte buffers.
  Docs example (`data: Vec<i16>`): `let buffer = InterleavedNumbers::<_, f32>::new(&data, 2, 3).unwrap();`
  Writing via `AdapterMut` uses `write_sample_unchecked()` returning a `bool` (clipping flag). Exact i16<->f32 scaling (expected 32768) UNVERIFIED; verify with a unit test (full-scale sine round trip).
- Alternative: convert i16->f32 by hand into a preallocated scratch and use `direct::InterleavedSlice<f32>` (generic over `T: Clone`; `new(&[T], channels, frames)`, `new_mut(&mut [T], channels, frames)`). Choose by benchmark.
- Mono: `channels = 1`; interleaved == sequential.

## 5. Examples
Quoted from the docs.rs / README (Async poly, f64, stereo):
```rust
use rubato::{Resampler, Async, FixedAsync, PolynomialDegree, Indexing};
use audioadapter_buffers::direct::InterleavedSlice;
let channels = 2; let chunk_size = 1024;
let mut resampler = Async::<f64>::new_poly(48000.0 / 44100.0, 1.1, PolynomialDegree::Cubic,
    chunk_size, channels, FixedAsync::Input).unwrap();
let mut indata = vec![0.0; channels * chunk_size];
let mut outdata = vec![0.0; channels * resampler.output_frames_max()];
let indexing = Indexing::new();
let input_adapter = InterleavedSlice::new(&indata, channels, chunk_size).unwrap();
let mut output_adapter = InterleavedSlice::new_mut(&mut outdata, channels, outdata.len() / channels).unwrap();
let (_frames_read, frames_written) = resampler
    .process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing)).unwrap();
```
From `examples/process_raw.rs` (loop and last partial chunk):
```rust
while input_frames_left >= input_frames_next {
    let (nbr_in, nbr_out) = resampler.process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing)).unwrap();
    indexing.input_offset += nbr_in; indexing.output_offset += nbr_out;
    input_frames_left -= nbr_in; input_frames_next = resampler.input_frames_next();
}
indexing.partial_len = Some(input_frames_left);   // last partial chunk
```
The example trims `resampler.output_delay()` frames from the start of the (offline) output.

COMPOSED by me from the verified signatures, NOT compiled or run (verify in the resampler tests), 48k -> 16k mono, 20 ms (960 -> 320 expected, UNVERIFIED rounding):
```rust
let mut rs = Fft::<f32>::new(48000, 16000, 960, 1, FixedSync::Input)?; // 960 = 20 ms @48k
let need = rs.input_frames_next();                 // read it; may differ from 960
let mut out = vec![0f32; rs.output_frames_max()];
// per frame (inp: &[f32] of len `need`):
let ia = InterleavedSlice::new(inp, 1, need)?;
let mut oa = InterleavedSlice::new_mut(&mut out, 1, rs.output_frames_max())?;
let (_read, written) = rs.process_into_buffer(&ia, &mut oa, None)?;
```

## Gotchas / open items for the resampler
- Fft rounds chunk_size up; 20 ms frames may not map 1:1 to resampler chunks. Keep a per-session accumulator if `input_frames_next()` != frame size.
- Resampler adds `output_delay()` frames of delay; the first output contains the startup transient/silence. Do not trim in streaming; account for it in the latency budget (docs/LATENCY.md).
- `reset()` if state continuity is not wanted (e.g. new stream).
- Async output frame count varies per call for non-integer ratios; size buffers with `output_frames_max()`.
- Send/Sync: `Fft` lists Send + Sync auto traits (docs); `Async` UNVERIFIED. `Sample` requires `Send`.
- Not fetched: audioadapter 5.0.0 trait method signatures in detail, `SizeError` / `ResamplerConstructionError` variants, benchmark numbers (none in fetched pages).
