//! Voice activity detection on a speech-probability model.
//!
//! Two runtime-independent pieces:
//!
//! - [`VadDetector`]: turns one speech probability per model window into start/stop events with
//!   hysteresis, like the reference Silero `VADIterator` plus a minimum-speech rule:
//!   - speech **starts** once probabilities stay `>= threshold` for `min_speech` (a single loud
//!     window is not speech); the start is backdated to the first window of that run, minus
//!     `speech_pad`;
//!   - speech **continues** while probabilities stay `>= neg_threshold` (the band between the two
//!     thresholds keeps the current state: hysteresis);
//!   - speech **stops** once probabilities stay `< neg_threshold` for `min_silence` (the
//!     hangover); the stop is backdated to the end of the last speech window, plus `speech_pad`.
//!     A band window that interrupts the hangover neither confirms nor cancels it: only a window
//!     `>= threshold` cancels a pending stop (as in the reference `VADIterator`);
//!   - events are never reported out of time order: a start backdated before the previous event
//!     is dated to that event (possible only when `speech_pad` is large against `min_silence`).
//! - [`VadStream`]: feeds PCM16 into a [`SpeechModel`] in the model's windows (with the model's
//!   context samples prepended) and the probabilities into a detector. A stream that closes while
//!   speech is in progress reports the stop with [`VadStream::finish`].
//!
//! Times are the audio's own timestamps (frame times), so the detector needs no clock and is
//! deterministic. Model calls are CPU work: the engine stage runs [`VadStream`] on the inference
//! pool (`crate::pool`), never on a tokio worker.

use std::time::Duration;

use tokio::time::Instant;

use crate::denoise::BoxedDenoiser;
use crate::error::{Error, Result};

/// A model that maps one window of audio to a speech probability, keeping its own recurrent
/// state between calls (e.g. Silero VAD).
pub trait SpeechModel: Send {
    /// New samples per call.
    fn window(&self) -> usize;
    /// Samples of the previous call prepended to each input (0 if none).
    fn context(&self) -> usize;
    /// Sample rate the model expects.
    fn sample_rate(&self) -> u32;
    /// Speech probability (0..=1) of `input` = `context()` previous samples followed by
    /// `window()` new samples, as `f32` in -1..1.
    ///
    /// # Errors
    /// Model runtime failures.
    fn infer(&mut self, input: &[f32]) -> Result<f32>;
    /// Forgets the recurrent state (a new, unrelated stream follows).
    fn reset(&mut self);
    /// Loads whatever the calling THREAD needs to run [`Self::infer`] without a cold start (the
    /// ONNX Runtime Silero keeps one session per thread and loads it on the first call, about
    /// 80 ms). The engine calls it once on each inference pool thread when a session starts.
    /// The default does nothing (models without per-thread state).
    ///
    /// # Errors
    /// Model runtime failures.
    fn warm(&self) -> Result<()> {
        Ok(())
    }

    /// What the per-thread state [`Self::warm`] prepares belongs to, as a key the engine claims
    /// the warm-up under (once per pool for the whole process): the model file for a model that
    /// keeps one runtime session per thread and file (the ONNX Runtime Silero). `None` (the
    /// default): the engine keys the claim on the model type, so two setups of one type share
    /// one warm-up.
    fn warm_key(&self) -> Option<String> {
        None
    }
}

/// Detector settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadParams {
    /// Probability at or above which a window counts as speech.
    pub threshold: f32,
    /// Probability below which a window counts as silence (hysteresis band in between).
    pub neg_threshold: f32,
    /// Speech must last this long before a start is reported.
    pub min_speech: Duration,
    /// Silence must last this long before a stop is reported (hangover).
    pub min_silence: Duration,
    /// Margin added before a start and after a stop.
    pub speech_pad: Duration,
}

impl Default for VadParams {
    /// Silero `VADIterator` defaults (threshold 0.5, neg 0.35, speech pad 30 ms) with a 100 ms
    /// silence hangover and 64 ms minimum speech (two 32 ms windows).
    fn default() -> Self {
        Self {
            threshold: 0.5,
            neg_threshold: 0.35,
            min_speech: Duration::from_millis(64),
            min_silence: Duration::from_millis(100),
            speech_pad: Duration::from_millis(30),
        }
    }
}

impl VadParams {
    /// Whether a falling hint ([`VadDetector::with_falling_hint`]) with delay `after` can ever
    /// precede a stop of these parameters, for windows of `window` duration. It cannot when
    ///
    /// - `speech_pad` exceeds `min_silence`: the stop is then dated `min(last speech end + pad,
    ///   end of the confirming window)`, and the confirming window is not known when the hint
    ///   is made, so the hint could not carry the stop's exact time (the detector sends none);
    /// - the first window that satisfies the delay (a whole number of windows of silence, at
    ///   least one) already reaches `min_silence`: it confirms the stop itself.
    ///
    /// Callers that switch hints on use this to notice (and report) a configuration in which
    /// they would silently get none.
    pub fn falling_hint_possible(&self, after: Duration, window: Duration) -> bool {
        if self.speech_pad > self.min_silence || window.is_zero() {
            return false;
        }
        let windows = after.as_nanos().div_ceil(window.as_nanos()).max(1);
        windows.saturating_mul(window.as_nanos()) < self.min_silence.as_nanos()
    }
}

/// A detected boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// Speech started at `at` (backdated: first speech window minus the pad).
    Start {
        /// Start time.
        at: Instant,
    },
    /// Speech stopped at `at` (backdated: end of the last speech window plus the pad).
    Stop {
        /// Stop time.
        at: Instant,
    },
}

/// What a stream reports when falling hints are on ([`VadStream::with_falling_hint`]): the
/// events of [`VadEvent`] and the hint, in time order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadSignal {
    /// A confirmed boundary.
    Event(VadEvent),
    /// The probability fell and stayed low: a stop will follow after the minimum silence unless
    /// speech resumes first (see [`VadDetector::with_falling_hint`]).
    Falling {
        /// The time the stop will be dated to if it is confirmed: the end of the last speech
        /// window plus the speech padding.
        at: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Silence,
    /// Probabilities above threshold since `since`, not yet `min_speech` long.
    Rising {
        since: Instant,
    },
    /// Speech; `last_speech` is the end of the last window that was not below `neg_threshold`.
    Speech {
        last_speech: Instant,
    },
    /// Speech, but below `neg_threshold` since the end of the last speech window `last_speech`.
    Falling {
        last_speech: Instant,
    },
}

/// Hysteresis state machine over per-window speech probabilities. See the module docs.
#[derive(Debug, Clone)]
pub struct VadDetector {
    params: VadParams,
    window: Duration,
    state: State,
    /// End of the last window fed to [`VadDetector::push`].
    last_end: Option<Instant>,
    /// Time of the last event reported; later events are never dated before it.
    last_event: Option<Instant>,
    /// Opt-in falling hint: report (once per fall) when the silence of a pending stop reaches
    /// this long. `None` (the default) never reports one.
    falling_hint: Option<Duration>,
    /// The hint of the current fall was already produced.
    hinted: bool,
    /// A hint produced by the last push, not yet taken: the time the stop would be dated to.
    pending_hint: Option<Instant>,
}

impl VadDetector {
    /// A detector for windows of `window` duration.
    pub fn new(params: VadParams, window: Duration) -> Self {
        Self {
            params,
            window,
            state: State::Silence,
            last_end: None,
            last_event: None,
            falling_hint: None,
            hinted: false,
            pending_hint: None,
        }
    }

    /// Opt in to falling hints: once per fall, on the first window below
    /// `neg_threshold` after the silence of a pending stop has lasted `after` (zero: the first
    /// such window), the detector records the time the stop will carry if it is confirmed (the
    /// end of the last speech window plus `speech_pad`, as [`VadEvent::Stop`] is dated), which
    /// [`VadDetector::take_falling_hint`] returns, so a consumer can tell the stop of this fall
    /// from the stop of a later one by comparing times. A hint always comes BEFORE the stop it
    /// belongs to (never in the window that confirms the stop), and speech (`>= threshold`)
    /// that cancels the pending stop makes the next fall eligible for a new hint. `None`
    /// switches hints off. Events are unchanged either way.
    ///
    /// **Exact time.** The hint's time equals the time of the stop of the same fall exactly,
    /// which holds when `speech_pad <= min_silence` (the stop is then always dated
    /// `last speech end + speech_pad`, however many band windows delay its confirmation). The
    /// time can be later than the hint window's own end (a pad longer than the silence so
    /// far): it is the time the stop will carry, not the time of the observation. With
    /// `speech_pad > min_silence` the stop's time depends on the confirming window, so no
    /// hint is sent (see [`VadParams::falling_hint_possible`]).
    ///
    /// **Band windows.** A window between `neg_threshold` and `threshold` after the hint
    /// neither cancels the pending stop nor restarts its silence timer, so the stop is still
    /// confirmed, with the hint's time, after such windows; the hint cannot tell the consumer
    /// that they happened.
    #[must_use]
    pub fn with_falling_hint(mut self, after: Option<Duration>) -> Self {
        self.falling_hint = after;
        self
    }

    /// The hint produced by the last [`VadDetector::push`], if any (see
    /// [`VadDetector::with_falling_hint`]): the time the pending stop will be dated to.
    pub fn take_falling_hint(&mut self) -> Option<Instant> {
        self.pending_hint.take()
    }

    /// Whether speech is currently in progress (started and not yet stopped).
    pub fn in_speech(&self) -> bool {
        matches!(self.state, State::Speech { .. } | State::Falling { .. })
    }

    /// Back to silence (a new stream). Discards speech in progress without reporting a stop;
    /// call [`VadDetector::finish`] first to close it with one.
    pub fn reset(&mut self) {
        *self = Self::new(self.params, self.window).with_falling_hint(self.falling_hint);
    }

    /// Feeds the probability of the window starting at `start`; returns an event if a boundary
    /// was confirmed by this window.
    pub fn push(&mut self, probability: f32, start: Instant) -> Option<VadEvent> {
        let end = start + self.window;
        self.last_end = Some(end);
        self.pending_hint = None;
        let p = self.params;
        match self.state {
            State::Silence => {
                if probability >= p.threshold {
                    self.state = State::Rising { since: start };
                    return self.confirm_start(start, end);
                }
                None
            }
            State::Rising { since } => {
                if probability >= p.threshold {
                    self.confirm_start(since, end)
                } else {
                    // Too short: a click or a blip, not speech.
                    self.state = State::Silence;
                    None
                }
            }
            State::Speech { last_speech } => {
                if probability < p.neg_threshold {
                    self.state = State::Falling { last_speech };
                    self.hinted = false;
                    let event = self.confirm_stop(last_speech, end);
                    if event.is_none() {
                        self.hint_fall(last_speech, end);
                    }
                    return event;
                }
                self.state = State::Speech { last_speech: end };
                None
            }
            State::Falling { last_speech } => {
                if probability >= p.threshold {
                    // Speech again: this cancels the pending stop.
                    self.state = State::Speech { last_speech: end };
                    None
                } else if probability >= p.neg_threshold {
                    // Band window: neither confirms nor cancels the stop; the silence timer
                    // keeps running from `last_speech` (as in the reference VADIterator).
                    None
                } else {
                    let event = self.confirm_stop(last_speech, end);
                    if event.is_none() {
                        self.hint_fall(last_speech, end);
                    }
                    event
                }
            }
        }
    }

    /// In `Falling` with the stop not confirmed by the window ending at `end`: produces the
    /// opt-in hint once the silence since `last_speech` has lasted long enough.
    fn hint_fall(&mut self, last_speech: Instant, end: Instant) {
        if let Some(after) = self.falling_hint
            && !self.hinted
            && self.params.speech_pad <= self.params.min_silence
            && end.saturating_duration_since(last_speech) >= after
        {
            self.hinted = true;
            // Exactly the time `confirm_stop` will give the stop: the confirming window ends at
            // least `min_silence` after `last_speech`, which is not before the pad, so its
            // `min(.., end)` clamp never bites. (Not clamped to this window's end: with a pad
            // longer than the silence so far the stop is dated later than this window.)
            self.pending_hint = Some(last_speech + self.params.speech_pad);
        }
    }

    /// Closes the stream: if speech is in progress, reports its stop, dated to the end of the
    /// last speech window plus `speech_pad` but never past the end of the last window fed (no
    /// silence has been observed beyond it). Returns to silence either way; `None` when no
    /// speech was in progress.
    pub fn finish(&mut self) -> Option<VadEvent> {
        let (State::Speech { last_speech } | State::Falling { last_speech }) = self.state else {
            self.state = State::Silence;
            return None;
        };
        self.state = State::Silence;
        let last_end = self.last_end.unwrap_or(last_speech);
        let at = (last_speech + self.params.speech_pad).min(last_end);
        Some(self.report(VadEvent::Stop { at }))
    }

    /// From `Rising`: reports the start once the run reaches `min_speech`.
    fn confirm_start(&mut self, since: Instant, end: Instant) -> Option<VadEvent> {
        if end.saturating_duration_since(since) < self.params.min_speech {
            return None;
        }
        self.state = State::Speech { last_speech: end };
        let at = since.checked_sub(self.params.speech_pad).unwrap_or(since);
        Some(self.report(VadEvent::Start { at }))
    }

    /// From `Falling`: reports the stop once the silence reaches `min_silence`.
    fn confirm_stop(&mut self, last_speech: Instant, end: Instant) -> Option<VadEvent> {
        if end.saturating_duration_since(last_speech) < self.params.min_silence {
            return None;
        }
        self.state = State::Silence;
        // Never later than now (`end`): the pad only extends into silence already observed.
        let at = (last_speech + self.params.speech_pad).min(end);
        Some(self.report(VadEvent::Stop { at }))
    }

    /// Records `event` as the latest one. A start backdated before the previous event (a long
    /// `speech_pad` against a short `min_silence`) is dated to that event instead, so the
    /// events a stream reports are in time order.
    fn report(&mut self, event: VadEvent) -> VadEvent {
        let (is_start, at) = match event {
            VadEvent::Start { at } => (true, at),
            VadEvent::Stop { at } => (false, at),
        };
        let at = match self.last_event {
            Some(last) if last > at => last,
            _ => at,
        };
        self.last_event = Some(at);
        if is_start {
            VadEvent::Start { at }
        } else {
            VadEvent::Stop { at }
        }
    }
}

/// Streams PCM16 through a [`SpeechModel`] window by window and a [`VadDetector`].
pub struct VadStream<M: SpeechModel> {
    model: M,
    detector: VadDetector,
    /// `context + window` samples: the context, then the window being filled.
    buffer: Vec<f32>,
    filled: usize,
    /// Timestamp of the first sample of the window being filled.
    window_start: Option<Instant>,
    sample_period_ns: u64,
    last_probability: f32,
    /// Noise suppression in front of the model ([`VadStream::with_denoiser`]).
    denoise: Option<Denoise>,
}

/// The audio a stream's noise suppressor produced for the last push (see
/// [`VadStream::denoised`]).
#[derive(Debug, Clone, Copy)]
pub struct DenoisedChunk<'a> {
    /// The enhanced samples, in the order of the input. Fewer or more than the push brought
    /// in, since the suppressor works in blocks.
    pub pcm: &'a [i16],
    /// Time of the first sample, aligned with the input timeline: a sample dated `t` is the
    /// enhanced version of the input at `t`. The suppressor's fade-in (its first
    /// [`Denoiser::delay`](crate::Denoiser::delay) of output, which belongs to the time before the
    /// stream began) is dropped, so chunks of one stream are contiguous from the stream's start.
    pub start: Instant,
}

/// Samples of a suppressor's delay at a sample period.
fn preroll_samples(delay: Duration, period_ns: u64) -> usize {
    let ns = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
    usize::try_from((ns + period_ns / 2) / period_ns.max(1)).unwrap_or(usize::MAX)
}

/// Noise suppression state of a [`VadStream`].
struct Denoise {
    denoiser: BoxedDenoiser,
    /// Output of the last push.
    chunk: Vec<i16>,
    chunk_start: Option<Instant>,
    /// Time of the first input sample since the last reset.
    origin: Option<Instant>,
    /// Enhanced samples handed on since the last reset (the fade-in not counted).
    produced: u64,
    /// Fade-in samples of the suppressor's output still to discard.
    preroll: usize,
    /// Pushes the suppressor failed (their audio went to the model as it was).
    errors: u64,
    /// The suppressor is switched off: audio goes to the model as it is (see
    /// [`VadStream::set_denoise_bypass`]).
    bypass: bool,
}

impl<M: SpeechModel> std::fmt::Debug for VadStream<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VadStream")
            .field("denoising", &self.denoise.is_some())
            .field("detector", &self.detector)
            .field("filled", &self.filled)
            .finish_non_exhaustive()
    }
}

impl<M: SpeechModel> VadStream<M> {
    /// The same stream with falling hints on or off (see [`VadDetector::with_falling_hint`]).
    /// Hints are only visible through [`VadStream::push_signals`]; [`VadStream::push`] reports
    /// the same events as without them.
    #[must_use]
    pub fn with_falling_hint(mut self, after: Option<Duration>) -> Self {
        self.detector = self.detector.with_falling_hint(after);
        self
    }

    /// A stream over `model` with detector `params`.
    pub fn new(model: M, params: VadParams) -> Self {
        let window = model.window();
        let rate = u64::from(model.sample_rate().max(1));
        let window_duration =
            Duration::from_nanos(u64::try_from(window).unwrap_or(512) * 1_000_000_000 / rate);
        Self {
            buffer: vec![0.0; model.context() + window],
            detector: VadDetector::new(params, window_duration),
            model,
            filled: 0,
            window_start: None,
            sample_period_ns: 1_000_000_000 / rate,
            last_probability: 0.0,
            denoise: None,
        }
    }

    /// The same stream with noise suppression in front of the model: every push goes through
    /// `denoiser` first, the model hears the enhanced audio, and [`VadStream::denoised`] hands
    /// the enhanced samples out. A denoiser that fails on a push is reset and that push reaches
    /// the model unprocessed (counted by [`VadStream::denoise_errors`]): audio is never lost to
    /// the suppressor. [`VadStream::finish`] and [`VadStream::reset`] reset it too.
    ///
    /// # Errors
    /// [`Error::Model`] if the denoiser's sample rate is not the model's.
    pub fn with_denoiser(mut self, denoiser: BoxedDenoiser) -> Result<Self> {
        if denoiser.sample_rate() != self.model.sample_rate() {
            return Err(Error::Model(format!(
                "denoiser works at {} Hz, the speech model at {} Hz",
                denoiser.sample_rate(),
                self.model.sample_rate()
            )));
        }
        let preroll = preroll_samples(denoiser.delay(), self.sample_period_ns);
        self.denoise = Some(Denoise {
            denoiser,
            chunk: Vec::new(),
            chunk_start: None,
            origin: None,
            produced: 0,
            preroll,
            errors: 0,
            bypass: false,
        });
        Ok(self)
    }

    /// Whether noise suppression is on.
    pub fn is_denoising(&self) -> bool {
        self.denoise.is_some()
    }

    /// What the suppressor produced for the last push (`None` without one, or before any push).
    pub fn denoised(&self) -> Option<DenoisedChunk<'_>> {
        let d = self.denoise.as_ref()?;
        Some(DenoisedChunk {
            pcm: &d.chunk,
            start: d.chunk_start?,
        })
    }

    /// Switches the suppressor off (`true`) or back on: while off, audio reaches the model as it
    /// is and [`VadStream::denoised`] reports nothing. The engine switches it off for audio the
    /// suppressor cannot handle (a model trained on wideband speech on a band-limited 8 kHz call).
    /// Switching resets the suppressor's state, since the stretches on either side are not
    /// contiguous for it.
    pub fn set_denoise_bypass(&mut self, bypass: bool) {
        if let Some(d) = self.denoise.as_mut()
            && d.bypass != bypass
        {
            d.bypass = bypass;
            d.denoiser.reset();
            d.chunk.clear();
            d.chunk_start = None;
            d.origin = None;
            d.produced = 0;
            d.preroll = preroll_samples(d.denoiser.delay(), self.sample_period_ns);
        }
    }

    /// Pushes the suppressor failed on since the stream was built.
    pub fn denoise_errors(&self) -> u64 {
        self.denoise.as_ref().map_or(0, |d| d.errors)
    }

    /// Whether a falling hint with delay `after` can ever precede a stop on this stream (see
    /// [`VadParams::falling_hint_possible`]); `false` means [`VadStream::with_falling_hint`]
    /// would be inert.
    pub fn falling_hint_possible(&self, after: Duration) -> bool {
        self.detector
            .params
            .falling_hint_possible(after, self.detector.window)
    }

    /// [`SpeechModel::warm`] of the stream's model, on the calling thread.
    ///
    /// # Errors
    /// Model runtime failures.
    pub fn warm(&self) -> Result<()> {
        self.model.warm()
    }

    /// [`SpeechModel::warm_key`] of the stream's model.
    pub fn warm_key(&self) -> Option<String> {
        self.model.warm_key()
    }

    /// The probability of the last complete window.
    pub fn last_probability(&self) -> f32 {
        self.last_probability
    }

    /// Whether speech is in progress.
    pub fn in_speech(&self) -> bool {
        self.detector.in_speech()
    }

    /// Feeds PCM16 samples whose first sample is at `start` (model rate); events confirmed by
    /// the windows completed in this call are appended to `events`.
    ///
    /// A failing window is dropped from detection (it reports no probability and no event), but
    /// it still advances the context, and the rest of `pcm` is processed as usual: the first
    /// error is returned after the whole batch, with the events of the other windows already in
    /// `events`. The caller therefore never loses audio to a model error. The model's own
    /// recurrent state is not advanced by a failed window, so the next window runs on a
    /// state one window behind (it converges within a few windows).
    ///
    /// # Errors
    /// The first model failure of this call (see [`SpeechModel::infer`]).
    pub fn push(&mut self, pcm: &[i16], start: Instant, events: &mut Vec<VadEvent>) -> Result<()> {
        self.push_with(pcm, start, &mut |signal| {
            if let VadSignal::Event(event) = signal {
                events.push(event);
            }
        })
    }

    /// Like [`VadStream::push`], but appends the falling hints as well (when the stream was
    /// built with [`VadStream::with_falling_hint`]), interleaved with the events in time order.
    /// Without hints on it appends exactly the events of [`VadStream::push`].
    ///
    /// # Errors
    /// As [`VadStream::push`].
    pub fn push_signals(
        &mut self,
        pcm: &[i16],
        start: Instant,
        signals: &mut Vec<VadSignal>,
    ) -> Result<()> {
        self.push_with(pcm, start, &mut |signal| signals.push(signal))
    }

    /// Runs `pcm` through the noise suppressor, if any, then through the model.
    fn push_with(
        &mut self,
        pcm: &[i16],
        start: Instant,
        sink: &mut dyn FnMut(VadSignal),
    ) -> Result<()> {
        let Some(mut d) = self.denoise.take() else {
            return self.feed(pcm, start, sink);
        };
        if d.bypass {
            let result = self.feed(pcm, start, sink);
            self.denoise = Some(d);
            return result;
        }
        // The enhanced audio is a continuous stream counted from the first sample since the
        // last reset (the engine closes the stream at every hole). Its output lags the input by
        // the suppressor's delay, so the first `preroll` output samples are fade-in from before
        // the stream began and are dropped: what remains lines up with the input timeline.
        let origin = *d.origin.get_or_insert(start);
        d.chunk.clear();
        let at = match d.denoiser.process(pcm, &mut d.chunk) {
            Ok(()) => {
                let fade = d.preroll.min(d.chunk.len());
                d.chunk.drain(..fade);
                d.preroll -= fade;
                let offset = Duration::from_nanos(d.produced.saturating_mul(self.sample_period_ns));
                d.produced = d
                    .produced
                    .saturating_add(u64::try_from(d.chunk.len()).unwrap_or(0));
                origin + offset
            }
            Err(_) => {
                d.errors = d.errors.saturating_add(1);
                d.denoiser.reset();
                d.origin = None;
                d.produced = 0;
                d.preroll = preroll_samples(d.denoiser.delay(), self.sample_period_ns);
                d.chunk.clear();
                d.chunk.extend_from_slice(pcm);
                start
            }
        };
        d.chunk_start = Some(at);
        let result = self.feed(&d.chunk, at, sink);
        self.denoise = Some(d);
        result
    }

    fn feed(&mut self, pcm: &[i16], start: Instant, sink: &mut dyn FnMut(VadSignal)) -> Result<()> {
        let context = self.model.context();
        let window = self.model.window();
        let mut first_error = None;
        for (i, sample) in pcm.iter().enumerate() {
            if self.filled == 0 {
                let offset = u64::try_from(i).unwrap_or(0) * self.sample_period_ns;
                self.window_start = Some(start + Duration::from_nanos(offset));
            }
            if let Some(slot) = self.buffer.get_mut(context + self.filled) {
                *slot = f32::from(*sample) / 32_768.0;
            }
            self.filled += 1;
            if self.filled < window {
                continue;
            }
            self.filled = 0;
            match self.model.infer(&self.buffer) {
                Ok(probability) => {
                    self.last_probability = probability;
                    if let Some(at) = self.window_start {
                        if let Some(event) = self.detector.push(probability, at) {
                            sink(VadSignal::Event(event));
                        } else if let Some(at) = self.detector.take_falling_hint() {
                            sink(VadSignal::Falling { at });
                        }
                    }
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            // The next context is the tail of what was just sent, whether or not the model
            // succeeded: the audio is real, so the following window must see it.
            self.buffer.copy_within(window.., 0);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Closes the stream: returns the stop of speech still in progress (see
    /// [`VadDetector::finish`]), then forgets everything as [`VadStream::reset`] does. The
    /// partial window, if any, is dropped.
    pub fn finish(&mut self) -> Option<VadEvent> {
        let event = self.detector.finish();
        self.reset();
        event
    }

    /// Forgets model state, context, partial window and detector state (a new stream). Speech
    /// in progress is discarded without a stop; use [`VadStream::finish`] to close a stream.
    pub fn reset(&mut self) {
        if let Some(d) = self.denoise.as_mut() {
            d.denoiser.reset();
            d.chunk.clear();
            d.chunk_start = None;
            d.origin = None;
            d.produced = 0;
            d.preroll = preroll_samples(d.denoiser.delay(), self.sample_period_ns);
        }
        self.model.reset();
        self.detector.reset();
        self.buffer.fill(0.0);
        self.filled = 0;
        self.window_start = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    const WINDOW: Duration = Duration::from_millis(32);

    /// A fixed time origin (paused clock; time does not advance in these tests).
    fn origin() -> Instant {
        use brisvox_core::Clock;
        brisvox_core::SimClock::new().now()
    }

    /// Feeds probabilities window by window from `t0`; returns (window index, event).
    fn run(params: VadParams, probs: &[f32]) -> Vec<(usize, VadEvent)> {
        let t0 = origin();
        let mut d = VadDetector::new(params, WINDOW);
        probs
            .iter()
            .enumerate()
            .filter_map(|(i, p)| d.push(*p, t0 + WINDOW * i as u32).map(|e| (i, e)))
            .collect()
    }

    fn at(e: VadEvent) -> Instant {
        match e {
            VadEvent::Start { at } | VadEvent::Stop { at } => at,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn start_needs_min_speech_and_is_backdated_with_the_pad() {
        let p = VadParams::default(); // min speech 64 ms = 2 windows
        // One loud window is a blip.
        assert!(run(p, &[0.0, 0.9, 0.0, 0.0]).is_empty());
        // Two windows start speech, reported on the second, dated to the first minus 30 ms.
        let events = run(p, &[0.0, 0.0, 0.9, 0.9, 0.9]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, 3);
        let t0 = origin();
        let start = at(events[0].1);
        assert!(matches!(events[0].1, VadEvent::Start { .. }));
        assert_eq!(start, t0 + WINDOW * 2 - p.speech_pad);
    }

    #[tokio::test(start_paused = true)]
    async fn hysteresis_keeps_speech_between_the_thresholds() {
        let p = VadParams::default();
        // 0.4 is below threshold but above neg_threshold: speech continues, no stop.
        let probs = [0.9, 0.9, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.9];
        let events = run(p, &probs);
        assert_eq!(events.len(), 1, "{events:?}");
        // From silence, 0.4 never starts speech.
        assert!(run(p, &[0.4; 10]).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stop_needs_min_silence_and_is_dated_to_the_last_speech_plus_pad() {
        let p = VadParams::default(); // min silence 100 ms = 4 windows of 32 ms (128 ms)
        let t0 = origin();
        // Speech in windows 0-3, silence from window 4.
        let mut probs = vec![0.9; 4];
        probs.extend([0.1; 8]);
        let events = run(p, &probs);
        assert_eq!(events.len(), 2, "{events:?}");
        let (i, stop) = events[1];
        assert!(matches!(stop, VadEvent::Stop { .. }));
        // Silence measured from the start of window 4; 100 ms needs four 32 ms windows (4-7).
        assert_eq!(i, 7);
        assert_eq!(at(stop), t0 + WINDOW * 4 + p.speech_pad);
        // A short dip below neg_threshold does not stop speech.
        let mut dip = vec![0.9; 4];
        dip.extend([0.1, 0.1, 0.9, 0.9]);
        assert_eq!(run(p, &dip).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_utterances_alternate_start_and_stop() {
        let p = VadParams::default();
        let mut probs = Vec::new();
        for _ in 0..3 {
            probs.extend([0.95; 10]);
            probs.extend([0.05; 10]);
        }
        let events = run(p, &probs);
        let kinds: Vec<bool> = events
            .iter()
            .map(|(_, e)| matches!(e, VadEvent::Start { .. }))
            .collect();
        assert_eq!(kinds, [true, false, true, false, true, false]);
        // Times strictly increase.
        assert!(events.windows(2).all(|w| at(w[0].1) < at(w[1].1)));
    }

    /// A model that returns scripted probabilities and records its inputs. The calls whose
    /// (0-based) index is in `fail` return an error instead of consuming a probability.
    struct Scripted {
        probs: std::vec::IntoIter<f32>,
        inputs: Vec<Vec<f32>>,
        fail: Vec<usize>,
    }

    impl Scripted {
        fn new(probs: Vec<f32>) -> Self {
            Self {
                probs: probs.into_iter(),
                inputs: Vec::new(),
                fail: Vec::new(),
            }
        }
    }

    impl SpeechModel for Scripted {
        fn window(&self) -> usize {
            4
        }
        fn context(&self) -> usize {
            2
        }
        fn sample_rate(&self) -> u32 {
            1_000
        }
        fn infer(&mut self, input: &[f32]) -> Result<f32> {
            let call = self.inputs.len();
            self.inputs.push(input.to_vec());
            if self.fail.contains(&call) {
                return Err(Error::Model(format!("scripted failure at call {call}")));
            }
            Ok(self.probs.next().unwrap_or(0.0))
        }
        fn reset(&mut self) {}
    }

    #[tokio::test(start_paused = true)]
    async fn the_stream_prepends_the_previous_context_and_times_windows() {
        let model = Scripted::new(vec![0.9, 0.9, 0.9]);
        let params = VadParams {
            min_speech: Duration::from_millis(8),
            speech_pad: Duration::ZERO,
            ..VadParams::default()
        };
        let mut stream = VadStream::new(model, params);
        let t0 = origin();
        let mut events = Vec::new();
        // 1 kHz model, 4-sample windows; feed 10 samples in uneven pieces.
        let samples: Vec<i16> = (1..=10).map(|i| i * 1_000).collect();
        stream.push(&samples[..3], t0, &mut events).unwrap();
        stream
            .push(&samples[3..10], t0 + Duration::from_millis(3), &mut events)
            .unwrap();
        let inputs = &stream.model.inputs;
        assert_eq!(inputs.len(), 2, "two complete windows");
        let scale = |v: i16| f32::from(v) / 32_768.0;
        // First call: zero context + samples 1-4; second: samples 3-4 as context + 5-8.
        assert_eq!(
            inputs[0],
            [
                0.0,
                0.0,
                scale(1_000),
                scale(2_000),
                scale(3_000),
                scale(4_000)
            ]
        );
        assert_eq!(
            inputs[1],
            [
                scale(3_000),
                scale(4_000),
                scale(5_000),
                scale(6_000),
                scale(7_000),
                scale(8_000)
            ]
        );
        // Start after two windows (8 ms), dated to the first window's first sample.
        assert_eq!(events, [VadEvent::Start { at: t0 }]);
        assert!(stream.in_speech());
        stream.reset();
        assert!(!stream.in_speech());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_window_does_not_lose_the_rest_of_the_batch() {
        // Three 4-sample windows in one call; the second one fails.
        let mut model = Scripted::new(vec![0.9, 0.9]);
        model.fail = vec![1];
        let params = VadParams {
            min_speech: Duration::from_millis(8),
            speech_pad: Duration::ZERO,
            ..VadParams::default()
        };
        let mut stream = VadStream::new(model, params);
        let t0 = origin();
        let samples: Vec<i16> = (1..=12).map(|i| i * 1_000).collect();
        let mut events = Vec::new();
        let result = stream.push(&samples, t0, &mut events);
        assert!(matches!(result, Err(Error::Model(_))), "{result:?}");
        // Every window of the batch ran, the failing one included.
        let inputs = &stream.model.inputs;
        assert_eq!(inputs.len(), 3);
        let scale = |v: i16| f32::from(v) / 32_768.0;
        // The window after the failure takes its context from the failed window's tail (7, 8).
        assert_eq!(inputs[2][..2], [scale(7_000), scale(8_000)]);
        // The probabilities of windows 0 and 2 still count: 8 ms of speech starts at t0.
        assert_eq!(events, [VadEvent::Start { at: t0 }]);
        assert!(stream.in_speech());
    }

    #[tokio::test(start_paused = true)]
    async fn finish_closes_open_speech_with_a_stop() {
        let t0 = origin();
        let p = VadParams::default();
        // Speech in windows 0-3: no silence yet, so the stop is the end of the speech.
        let mut d = VadDetector::new(p, WINDOW);
        for i in 0..4 {
            d.push(0.9, t0 + WINDOW * i);
        }
        assert!(d.in_speech());
        assert_eq!(
            d.finish(),
            Some(VadEvent::Stop {
                at: t0 + WINDOW * 4
            })
        );
        assert!(!d.in_speech());
        assert_eq!(d.finish(), None, "nothing left to close");

        // Speech in windows 0-3, one low window (4): the pad counts from the last speech and is
        // capped at the end of the last window fed (t0 + 160 ms): min(128 + 30, 160) = 158 ms.
        let mut d = VadDetector::new(p, WINDOW);
        let mut events = Vec::new();
        for (i, prob) in [0.9, 0.9, 0.9, 0.9, 0.1].into_iter().enumerate() {
            events.extend(d.push(prob, t0 + WINDOW * i as u32));
        }
        assert!(
            matches!(events.as_slice(), [VadEvent::Start { .. }]),
            "{events:?}"
        );
        assert_eq!(
            d.finish(),
            Some(VadEvent::Stop {
                at: t0 + WINDOW * 4 + p.speech_pad
            })
        );

        // Silence does not report anything when closed.
        let mut d = VadDetector::new(p, WINDOW);
        assert_eq!(d.push(0.1, t0), None);
        assert_eq!(d.finish(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_finish_reports_the_open_segment_and_resets() {
        let params = VadParams {
            min_speech: Duration::from_millis(8),
            speech_pad: Duration::ZERO,
            ..VadParams::default()
        };
        let mut stream = VadStream::new(Scripted::new(vec![0.9, 0.9]), params);
        let t0 = origin();
        let mut events = Vec::new();
        // Two 4-sample windows: speech from t0, and still open when the stream closes.
        stream.push(&[1_000; 8], t0, &mut events).unwrap();
        assert_eq!(events, [VadEvent::Start { at: t0 }]);
        assert_eq!(
            stream.finish(),
            Some(VadEvent::Stop {
                at: t0 + Duration::from_millis(8)
            })
        );
        assert!(!stream.in_speech());
        assert_eq!(stream.finish(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn events_never_go_backwards_in_time() {
        // A pad longer than half the silence backdates the second start before the stop before
        // it (stop at 188 ms, start at 192 - 60 = 132 ms without the clamp).
        let long_pad = VadParams {
            min_silence: Duration::from_millis(50),
            speech_pad: Duration::from_millis(60),
            ..VadParams::default()
        };
        let mut probs = vec![0.9; 4];
        probs.extend([0.1; 2]);
        probs.extend([0.9; 4]);
        let events = run(long_pad, &probs);
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(
            events.windows(2).all(|w| at(w[0].1) <= at(w[1].1)),
            "{events:?}"
        );
        assert_eq!(
            at(events[2].1),
            at(events[1].1),
            "start is dated to the stop it follows"
        );

        // No hangover: the stop lands 30 ms after the last speech, and the next start (backdated
        // 30 ms from the first window after it) is clamped to that stop.
        let no_hangover = VadParams {
            min_silence: Duration::ZERO,
            speech_pad: Duration::from_millis(30),
            ..VadParams::default()
        };
        let mut probs = vec![0.9; 4];
        probs.push(0.1);
        probs.extend([0.9; 4]);
        let events = run(no_hangover, &probs);
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(
            events.windows(2).all(|w| at(w[0].1) <= at(w[1].1)),
            "{events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_band_window_does_not_restart_the_hangover() {
        let p = VadParams::default(); // 100 ms silence = four 32 ms windows
        let t0 = origin();
        // Speech in windows 0-3; silence from window 4, with one band window (0.4) at window 7.
        // The band window does not cancel the stop, so 100 ms of silence (window 8) stops speech.
        let mut probs = vec![0.9; 4];
        probs.extend([0.1, 0.1, 0.1, 0.4, 0.1]);
        let events = run(p, &probs);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[1].0, 8);
        assert_eq!(at(events[1].1), t0 + WINDOW * 4 + p.speech_pad);
    }

    #[tokio::test(start_paused = true)]
    async fn window_stamps_follow_the_sample_clock_across_calls() {
        let model = Scripted::new(vec![0.9, 0.9, 0.1]);
        let params = VadParams {
            min_speech: Duration::from_millis(8),
            min_silence: Duration::from_millis(4),
            speech_pad: Duration::ZERO,
            ..VadParams::default()
        };
        let mut stream = VadStream::new(model, params);
        let t0 = origin();
        let samples: Vec<i16> = (1..=12).map(|i| i * 1_000).collect();
        let mut events = Vec::new();
        // 4 ms windows; the second call starts at t0 + 3 ms, so window 1 starts 1 ms into it and
        // window 2 (silence) starts 5 ms into it.
        stream.push(&samples[..3], t0, &mut events).unwrap();
        stream
            .push(&samples[3..], t0 + Duration::from_millis(3), &mut events)
            .unwrap();
        // Speech from t0; the silence starts at window 2 (t0 + 8 ms) and lasts 4 ms by the end of
        // window 2 (t0 + 12 ms), so the stop is dated to t0 + 8 ms.
        assert_eq!(
            events,
            [
                VadEvent::Start { at: t0 },
                VadEvent::Stop {
                    at: t0 + Duration::from_millis(8)
                }
            ]
        );
    }

    /// Feeds probabilities with falling hints on; returns (window index, signal).
    fn run_signals(
        params: VadParams,
        after: Option<Duration>,
        probs: &[f32],
    ) -> Vec<(usize, VadSignal)> {
        let t0 = origin();
        let mut d = VadDetector::new(params, WINDOW).with_falling_hint(after);
        let mut out = Vec::new();
        for (i, p) in probs.iter().enumerate() {
            let start = t0 + WINDOW * i as u32;
            if let Some(event) = d.push(*p, start) {
                out.push((i, VadSignal::Event(event)));
            } else if let Some(at) = d.take_falling_hint() {
                out.push((i, VadSignal::Falling { at }));
            }
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn falling_hints_are_off_by_default_and_never_change_the_events() {
        let p = VadParams::default(); // 100 ms silence = four 32 ms windows
        let mut probs = vec![0.9; 4];
        probs.extend([0.1; 6]);
        probs.extend([0.9; 3]);
        probs.extend([0.1; 6]);
        let plain = run(p, &probs);
        assert_eq!(plain.len(), 4, "{plain:?}");
        let off = run_signals(p, None, &probs);
        assert!(off.iter().all(|(_, s)| matches!(s, VadSignal::Event(_))));
        for after in [Some(Duration::ZERO), Some(WINDOW), Some(WINDOW * 2)] {
            let events: Vec<(usize, VadEvent)> = run_signals(p, after, &probs)
                .into_iter()
                .filter_map(|(i, s)| match s {
                    VadSignal::Event(e) => Some((i, e)),
                    VadSignal::Falling { .. } => None,
                })
                .collect();
            assert_eq!(events, plain, "hints do not move the events ({after:?})");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hint_comes_once_per_fall_before_the_stop_and_dates_the_silence_start() {
        let p = VadParams::default(); // min silence 100 ms: the stop confirms at window 4 + 3
        let t0 = origin();
        let mut probs = vec![0.9; 4];
        probs.extend([0.1; 6]);
        // Zero delay: the first window below the negative threshold (window 4) hints.
        let signals = run_signals(p, Some(Duration::ZERO), &probs);
        assert_eq!(signals.len(), 3, "start, hint, stop: {signals:?}");
        assert_eq!(signals[1].0, 4);
        assert_eq!(
            signals[1].1,
            VadSignal::Falling {
                at: t0 + WINDOW * 4 + p.speech_pad
            },
            "dated like the stop that follows"
        );
        let VadSignal::Event(VadEvent::Stop { at: stop_at }) = signals[2].1 else {
            panic!("a stop follows: {signals:?}");
        };
        assert_eq!(signals[1].1, VadSignal::Falling { at: stop_at });
        assert!(signals[1].0 < signals[2].0, "the hint precedes the stop");
        assert!(matches!(
            signals[2].1,
            VadSignal::Event(VadEvent::Stop { .. })
        ));
        // A 64 ms delay waits for the second silent window (6), still before the stop (7).
        let delayed = run_signals(p, Some(WINDOW * 2), &probs);
        assert_eq!(delayed.len(), 3, "{delayed:?}");
        assert_eq!(delayed[1].0, 5, "windows 4 and 5 make 64 ms of silence");
        assert_eq!(delayed[1].1, signals[1].1);
        // A delay as long as the minimum silence never wins against the stop: no hint at all.
        let late = run_signals(p, Some(Duration::from_millis(128)), &probs);
        assert_eq!(late.len(), 2, "start and stop only: {late:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn speech_that_cancels_the_pending_stop_allows_a_new_hint() {
        let p = VadParams::default();
        // Speech, a dip of three windows (hinted), speech again (cancels), then a real fall.
        let mut probs = vec![0.9; 4];
        probs.extend([0.1; 3]);
        probs.extend([0.9; 3]);
        probs.extend([0.1; 6]);
        let signals = run_signals(p, Some(Duration::ZERO), &probs);
        let hints: Vec<usize> = signals
            .iter()
            .filter(|(_, s)| matches!(s, VadSignal::Falling { .. }))
            .map(|(i, _)| *i)
            .collect();
        assert_eq!(hints, [4, 10], "one hint per fall: {signals:?}");
        let events = signals
            .iter()
            .filter(|(_, s)| matches!(s, VadSignal::Event(_)))
            .count();
        assert_eq!(events, 2, "the dip never became a stop: {signals:?}");
        // A band window (0.4) neither hints nor cancels.
        let band = run_signals(p, Some(Duration::ZERO), &[0.9, 0.9, 0.9, 0.4, 0.4, 0.4]);
        assert!(
            band.iter()
                .all(|(_, s)| !matches!(s, VadSignal::Falling { .. })),
            "{band:?}"
        );
    }

    /// The (hint, stop) times of the single fall in `signals`, which must have both.
    fn hint_and_stop(signals: &[(usize, VadSignal)]) -> (Instant, Instant) {
        let hint = signals.iter().find_map(|(_, s)| match s {
            VadSignal::Falling { at } => Some(*at),
            VadSignal::Event(_) => None,
        });
        let stop = signals.iter().find_map(|(_, s)| match s {
            VadSignal::Event(VadEvent::Stop { at }) => Some(*at),
            _ => None,
        });
        (hint.expect("a hint"), stop.expect("a stop"))
    }

    #[tokio::test(start_paused = true)]
    async fn the_hint_carries_the_stops_exact_time_even_when_the_pad_exceeds_the_silence_so_far() {
        let ms = Duration::from_millis;
        // Pad 100 ms, hint after 64 ms of silence, stop after 300 ms: the pad is longer than
        // the silence at the hint plus a window, so the hint is dated in the future of its own
        // window and must still equal the stop's time.
        for (pad, min_silence, after) in [
            (100, 300, 64),
            (100, 100, 64),
            (70, 128, 0),
            (96, 200, 32),
            (0, 100, 0),
            (30, 100, 64),
        ] {
            let p = VadParams {
                speech_pad: ms(pad),
                min_silence: ms(min_silence),
                ..VadParams::default()
            };
            let mut probs = vec![0.9; 4];
            probs.extend([0.1; 14]);
            let signals = run_signals(p, Some(ms(after)), &probs);
            let (hint, stop) = hint_and_stop(&signals);
            assert_eq!(
                hint, stop,
                "pad {pad} silence {min_silence} after {after}: {signals:?}"
            );
            assert_eq!(stop, origin() + WINDOW * 4 + ms(pad));
            let hint_index = signals
                .iter()
                .position(|(_, s)| matches!(s, VadSignal::Falling { .. }))
                .expect("hint position");
            assert!(hint_index < signals.len() - 1, "the hint precedes the stop");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_pad_longer_than_the_minimum_silence_sends_no_hint() {
        let ms = Duration::from_millis;
        let p = VadParams {
            speech_pad: ms(150),
            min_silence: ms(100),
            ..VadParams::default()
        };
        let mut probs = vec![0.9; 4];
        probs.extend([0.1; 10]);
        let signals = run_signals(p, Some(Duration::ZERO), &probs);
        assert_eq!(signals.len(), 2, "start and stop only: {signals:?}");
        assert!(!p.falling_hint_possible(Duration::ZERO, WINDOW));
    }

    #[test]
    fn falling_hint_possible_says_whether_a_hint_can_precede_the_stop() {
        let ms = Duration::from_millis;
        let p = VadParams::default(); // pad 30, min silence 100: silence windows 32, 64, 96
        assert!(p.falling_hint_possible(Duration::ZERO, WINDOW));
        assert!(p.falling_hint_possible(ms(64), WINDOW));
        assert!(p.falling_hint_possible(ms(96), WINDOW));
        assert!(
            !p.falling_hint_possible(ms(97), WINDOW),
            "128 ms confirms the stop"
        );
        assert!(!p.falling_hint_possible(ms(128), WINDOW));
        assert!(!p.falling_hint_possible(ms(0), Duration::ZERO));
        let one_window_silence = VadParams {
            min_silence: WINDOW,
            ..p
        };
        assert!(!one_window_silence.falling_hint_possible(Duration::ZERO, WINDOW));
    }

    #[tokio::test(start_paused = true)]
    async fn band_windows_after_the_hint_do_not_move_the_stop_away_from_the_hint() {
        // Pins the documented behaviour: a window between the thresholds (0.4) neither cancels
        // the pending stop nor restarts its silence, so the stop of the fall still carries the
        // hint's time even though the audio in between was not clean silence.
        let p = VadParams::default();
        let mut probs = vec![0.9; 4];
        probs.extend([0.1, 0.4, 0.4, 0.1, 0.1, 0.1]);
        let signals = run_signals(p, Some(Duration::ZERO), &probs);
        let (hint, stop) = hint_and_stop(&signals);
        assert_eq!(hint, stop, "{signals:?}");
        // Speech (>= threshold) in between does cancel it: the stop is dated to a later pause.
        let mut probs = vec![0.9; 4];
        probs.extend([0.1, 0.9, 0.9, 0.1, 0.1, 0.1, 0.1, 0.1]);
        let signals = run_signals(p, Some(Duration::ZERO), &probs);
        let hints: Vec<Instant> = signals
            .iter()
            .filter_map(|(_, s)| match s {
                VadSignal::Falling { at } => Some(*at),
                VadSignal::Event(_) => None,
            })
            .collect();
        let (_, stop) = hint_and_stop(&signals);
        assert_eq!(hints.len(), 2, "{signals:?}");
        assert_ne!(hints[0], stop, "the first fall never became the stop");
        assert_eq!(hints[1], stop);
    }

    #[tokio::test(start_paused = true)]
    async fn the_stream_interleaves_hints_with_events_and_reset_keeps_the_setting() {
        let p = VadParams {
            min_speech: Duration::from_millis(8),
            min_silence: Duration::from_millis(12),
            speech_pad: Duration::ZERO,
            ..VadParams::default()
        };
        // 4 ms windows: speech, speech, then silence for three windows (12 ms) = the stop.
        let model = Scripted::new(vec![0.9, 0.9, 0.1, 0.1, 0.1, 0.1, 0.1]);
        let mut stream = VadStream::new(model, p).with_falling_hint(Some(Duration::ZERO));
        let t0 = origin();
        let samples: Vec<i16> = (1..=28).map(|i| i * 100).collect();
        let mut signals = Vec::new();
        stream.push_signals(&samples, t0, &mut signals).unwrap();
        let ms = Duration::from_millis;
        assert_eq!(
            signals,
            [
                VadSignal::Event(VadEvent::Start { at: t0 }),
                VadSignal::Falling { at: t0 + ms(8) },
                VadSignal::Event(VadEvent::Stop { at: t0 + ms(8) }),
            ]
        );
        // `push` on a stream with hints reports the plain events.
        let model_again = Scripted::new(vec![0.9, 0.9, 0.1, 0.1, 0.1, 0.1, 0.1]);
        let mut plain = VadStream::new(model_again, p).with_falling_hint(Some(Duration::ZERO));
        let mut events = Vec::new();
        plain.push(&samples, t0, &mut events).unwrap();
        assert_eq!(
            events,
            [
                VadEvent::Start { at: t0 },
                VadEvent::Stop { at: t0 + ms(8) }
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reset_detector_keeps_its_hint_setting_and_forgets_the_fall() {
        let p = VadParams::default();
        let t0 = origin();
        let mut d = VadDetector::new(p, WINDOW).with_falling_hint(Some(Duration::ZERO));
        let feed = |d: &mut VadDetector, probs: &[f32]| {
            let mut hints = 0;
            for (i, prob) in probs.iter().enumerate() {
                d.push(*prob, t0 + WINDOW * i as u32);
                hints += usize::from(d.take_falling_hint().is_some());
            }
            hints
        };
        assert_eq!(feed(&mut d, &[0.9, 0.9, 0.1, 0.1]), 1);
        d.reset();
        assert_eq!(
            feed(&mut d, &[0.9, 0.9, 0.1, 0.1]),
            1,
            "a new fall hints again"
        );
        // A hint nobody took is gone once the next window is pushed.
        d.push(0.9, t0);
        d.push(0.9, t0 + WINDOW);
        d.push(0.1, t0 + WINDOW * 2);
        d.push(0.1, t0 + WINDOW * 3);
        assert_eq!(
            d.take_falling_hint(),
            None,
            "the hint of window 2 was not taken and the next window dropped it"
        );
    }

    /// A suppressor of 3-sample blocks that returns each block one block later (zeros first):
    /// the model hears the input delayed by 3 samples, like a real model's algorithmic delay.
    struct Lagged {
        held: Vec<i16>,
        previous: [i16; 3],
        fail: bool,
        resets: usize,
    }

    impl Lagged {
        fn new(fail: bool) -> Self {
            Self {
                held: Vec::new(),
                previous: [0; 3],
                fail,
                resets: 0,
            }
        }
    }

    impl crate::denoise::Denoiser for Lagged {
        fn sample_rate(&self) -> u32 {
            1_000
        }
        fn delay(&self) -> Duration {
            Duration::from_millis(3)
        }
        fn process(&mut self, input: &[i16], output: &mut Vec<i16>) -> Result<()> {
            if self.fail {
                return Err(Error::Model("scripted denoiser failure".into()));
            }
            self.held.extend_from_slice(input);
            while self.held.len() >= 3 {
                output.extend_from_slice(&self.previous);
                self.previous.copy_from_slice(&self.held[..3]);
                self.held.drain(..3);
            }
            Ok(())
        }
        fn reset(&mut self) {
            self.held.clear();
            self.previous = [0; 3];
            self.resets += 1;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_model_hears_the_enhanced_audio_and_it_is_dated_on_the_input_timeline() {
        let mut stream = VadStream::new(Scripted::new(vec![0.0; 8]), VadParams::default())
            .with_denoiser(Box::new(Lagged::new(false)))
            .unwrap();
        assert!(stream.is_denoising());
        let t0 = origin();
        let mut events = Vec::new();
        let pcm: Vec<i16> = (1..=12).collect();
        let mut chunks = Vec::new();
        for (i, piece) in pcm.chunks(5).enumerate() {
            let at = t0 + Duration::from_millis(5 * i as u64);
            stream.push(piece, at, &mut events).unwrap();
            let c = stream.denoised().unwrap();
            chunks.push((c.pcm.to_vec(), c.start));
        }
        // 5 + 5 + 2 samples in; blocks of 3 come out one block late: 0,0,0 | 1..6 | 7,8,9. The
        // first three are the fade-in (the denoiser's 3 ms delay) and are dropped.
        let all: Vec<i16> = chunks.iter().flat_map(|c| c.0.clone()).collect();
        assert_eq!(all, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
        // What is left starts with the stream, and chunks continue where the last one ended.
        assert!(chunks[0].0.is_empty());
        assert_eq!(chunks[1].1, t0);
        assert_eq!(chunks[1].0.len(), 6);
        assert_eq!(chunks[2].1, t0 + Duration::from_millis(6));
        assert_eq!(stream.denoise_errors(), 0);
        stream.reset();
        assert!(stream.denoised().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_denoiser_costs_no_audio() {
        let mut stream = VadStream::new(Scripted::new(vec![0.0; 8]), VadParams::default())
            .with_denoiser(Box::new(Lagged::new(true)))
            .unwrap();
        let t0 = origin();
        let mut events = Vec::new();
        stream.push(&[7, 8, 9, 10], t0, &mut events).unwrap();
        // The push reached the model as it was, and is what the consumer gets back.
        let c = stream.denoised().unwrap();
        assert_eq!(c.pcm, [7, 8, 9, 10]);
        assert_eq!(c.start, t0);
        assert_eq!(stream.denoise_errors(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_bypassed_denoiser_leaves_the_audio_alone_and_comes_back_clean() {
        let mut stream = VadStream::new(Scripted::new(vec![0.0; 16]), VadParams::default())
            .with_denoiser(Box::new(Lagged::new(false)))
            .unwrap();
        let t0 = origin();
        let mut events = Vec::new();
        stream.set_denoise_bypass(true);
        stream.push(&[1, 2, 3, 4, 5, 6], t0, &mut events).unwrap();
        assert!(
            stream.denoised().is_none(),
            "nothing is produced while bypassed"
        );
        // Back on: the suppressor starts from silence, its fade-in dropped again.
        stream.set_denoise_bypass(false);
        let at = t0 + Duration::from_millis(6);
        stream
            .push(&[7, 8, 9, 10, 11, 12], at, &mut events)
            .unwrap();
        let chunk = stream.denoised().unwrap();
        assert_eq!(chunk.pcm, [7, 8, 9], "{:?}", chunk.pcm);
        assert_eq!(chunk.start, at);
    }

    #[tokio::test(start_paused = true)]
    async fn a_denoiser_of_another_rate_is_refused() {
        struct Wrong;
        impl crate::denoise::Denoiser for Wrong {
            fn sample_rate(&self) -> u32 {
                16_000
            }
            fn delay(&self) -> Duration {
                Duration::ZERO
            }
            fn process(&mut self, _: &[i16], _: &mut Vec<i16>) -> Result<()> {
                Ok(())
            }
            fn reset(&mut self) {}
        }
        let stream = VadStream::new(Scripted::new(vec![]), VadParams::default());
        assert!(stream.with_denoiser(Box::new(Wrong)).is_err());
    }
}
