//! Silero VAD v6.2.3 on ONNX Runtime (feature `silero`).
//!
//! Model contract (confirmed from the pinned file, docs/providers/silero-vad.md): inputs `input`
//! f32 [1, 64 + 512] (64 samples of context, then 512 new samples at 16 kHz), `state` f32
//! [2, 1, 128], `sr` i64 = 16000; outputs `output` f32 [1, 1] (speech probability) and `stateN`
//! f32 [2, 1, 128] (the next state).
//!
//! **Sharing**: an ONNX Runtime session holds the weights and arenas and `run` needs `&mut`, so
//! sessions are per *thread*, loaded lazily into thread-local storage on first use (the
//! inference pool's few threads end up with one each), while each call stream only carries its
//! own recurrent state ([`SileroVad`], 2 KiB: the state and the buffer the next state lands in).
//! Each session uses one intra-op and one inter-op thread: parallelism comes from the pool, not
//! from ORT.
//!
//! The model file is fetched by `scripts/models/fetch_models.py` (SHA-256 pinned) into
//! `models/`; [`SileroVad::load`] verifies the size before use.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use ort::session::Session;
use ort::value::TensorRef;

use crate::error::{Error, Result};
use crate::vad::SpeechModel;

/// Samples per model call at 16 kHz.
pub const WINDOW: usize = 512;
/// Context samples prepended to each call at 16 kHz.
pub const CONTEXT: usize = 64;
/// The model's sample rate.
pub const SAMPLE_RATE: u32 = 16_000;
/// Size of the pinned model file (scripts/models/fetch_models.py also checks the SHA-256).
pub const MODEL_BYTES: u64 = 2_327_524;

const STATE_LEN: usize = 2 * 128;

thread_local! {
    /// This thread's session, with the path it was loaded from.
    static SESSION: RefCell<Option<(PathBuf, Session)>> = const { RefCell::new(None) };
}

fn model_error(e: impl std::fmt::Display) -> Error {
    Error::Model(e.to_string())
}

fn new_session(path: &Path) -> Result<Session> {
    Session::builder()
        .map_err(model_error)?
        .with_intra_threads(1)
        .map_err(model_error)?
        .with_inter_threads(1)
        .map_err(model_error)?
        .with_parallel_execution(false)
        .map_err(model_error)?
        .with_independent_thread_pool()
        .map_err(model_error)?
        .commit_from_file(path)
        .map_err(model_error)
}

/// One stream's Silero VAD: its recurrent state, run on the calling thread's shared session.
///
/// A call lends the input and the current state to the runtime as borrowed tensors and writes
/// the new state into a second buffer that is swapped in on success. The call therefore makes no
/// heap copies of its own; the runtime still allocates its output tensors.
#[derive(Debug, Clone)]
pub struct SileroVad {
    model: PathBuf,
    /// The recurrent state the next call runs on.
    state: Vec<f32>,
    /// Receives the state a call produces; swapped with `state` when the call succeeds.
    next_state: Vec<f32>,
    /// The `sr` input: the sample rate as one `i64`.
    sample_rate: [i64; 1],
}

impl SileroVad {
    /// A stream over the model at `path`, checked to exist with the pinned size. Loading the
    /// session itself happens on the first [`SpeechModel::infer`] of each thread (~80 ms;
    /// warm the pool threads at startup with [`SileroVad::warm`]).
    ///
    /// # Errors
    /// [`Error::Model`] if the file is missing or has the wrong size.
    pub fn load(path: &Path) -> Result<Self> {
        let size = std::fs::metadata(path)
            .map_err(|e| model_error(format!("{}: {e}", path.display())))?
            .len();
        if size != MODEL_BYTES {
            return Err(model_error(format!(
                "{}: {size} bytes, expected {MODEL_BYTES} (run scripts/models/fetch_models.py)",
                path.display()
            )));
        }
        Ok(Self {
            model: path.to_path_buf(),
            state: vec![0.0; STATE_LEN],
            next_state: vec![0.0; STATE_LEN],
            sample_rate: [i64::from(SAMPLE_RATE)],
        })
    }

    /// Loads this thread's session now (so the first real window does not pay for it).
    ///
    /// # Errors
    /// [`Error::Model`] if the session cannot be created.
    pub fn warm(&self) -> Result<()> {
        with_session(&self.model, |_| Ok(()))
    }
}

/// Runs `f` on this thread's session for `model`, loading it on first use (or if the thread's
/// session was loaded from another file).
fn with_session<T>(model: &Path, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
    SESSION.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().is_none_or(|(path, _)| path != model) {
            *slot = Some((model.to_path_buf(), new_session(model)?));
        }
        match slot.as_mut() {
            Some((_, session)) => f(session),
            None => Err(model_error("no session")),
        }
    })
}

/// Checks one call's outputs. `output` must be a single probability in `0..=1` (which also
/// rejects NaN) and `stateN` must have the state's length. Anything else is a malformed model:
/// it is reported as an error, never read as silence or carried on as a stale state.
fn check_outputs(probability: &[f32], next: &[f32]) -> Result<f32> {
    let [p] = probability else {
        return Err(model_error(format!(
            "output has {} elements, expected 1",
            probability.len()
        )));
    };
    if !(0.0..=1.0).contains(p) {
        return Err(model_error(format!(
            "output probability {p} is outside 0..=1"
        )));
    }
    if next.len() != STATE_LEN {
        return Err(model_error(format!(
            "stateN has {} elements, expected {STATE_LEN}",
            next.len()
        )));
    }
    Ok(*p)
}

impl SpeechModel for SileroVad {
    fn warm(&self) -> Result<()> {
        // The inherent `warm` (an associated function of the type wins over the trait's).
        SileroVad::warm(self)
    }

    fn warm_key(&self) -> Option<String> {
        Some(format!("silero-ort:{}", self.model.display()))
    }

    fn window(&self) -> usize {
        WINDOW
    }

    fn context(&self) -> usize {
        CONTEXT
    }

    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn infer(&mut self, input: &[f32]) -> Result<f32> {
        if input.len() != CONTEXT + WINDOW {
            return Err(model_error(format!(
                "input of {} samples, expected {}",
                input.len(),
                CONTEXT + WINDOW
            )));
        }
        let Self {
            model,
            state,
            next_state,
            sample_rate,
        } = self;
        let probability = with_session(model.as_path(), |session| {
            let input = TensorRef::from_array_view(([1_i64, (CONTEXT + WINDOW) as i64], input))
                .map_err(model_error)?;
            let state_in = TensorRef::from_array_view(([2_i64, 1, 128], state.as_slice()))
                .map_err(model_error)?;
            let rate =
                TensorRef::from_array_view(((), sample_rate.as_slice())).map_err(model_error)?;
            let outputs = session
                .run(ort::inputs!["input" => input, "state" => state_in, "sr" => rate])
                .map_err(model_error)?;
            let (_, probability) = outputs
                .get("output")
                .ok_or_else(|| model_error("no output tensor"))?
                .try_extract_tensor::<f32>()
                .map_err(model_error)?;
            let (_, next) = outputs
                .get("stateN")
                .ok_or_else(|| model_error("no stateN tensor"))?
                .try_extract_tensor::<f32>()
                .map_err(model_error)?;
            let probability = check_outputs(probability, next)?;
            // Capacity is STATE_LEN from `load`, so this copy does not reallocate.
            next_state.clear();
            next_state.extend_from_slice(next);
            Ok(probability)
        })?;
        std::mem::swap(state, next_state);
        Ok(probability)
    }

    fn reset(&mut self) {
        self.state.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_outputs_accepts_one_probability_and_a_full_state() {
        let state = vec![0.0; STATE_LEN];
        assert_eq!(check_outputs(&[0.25], &state).ok(), Some(0.25));
        assert_eq!(check_outputs(&[0.0], &state).ok(), Some(0.0));
        assert_eq!(check_outputs(&[1.0], &state).ok(), Some(1.0));
    }

    #[test]
    fn check_outputs_rejects_malformed_outputs() {
        let state = vec![0.0; STATE_LEN];
        assert!(check_outputs(&[], &state).is_err(), "empty output");
        assert!(check_outputs(&[0.1, 0.2], &state).is_err(), "two outputs");
        assert!(check_outputs(&[f32::NAN], &state).is_err(), "NaN");
        assert!(check_outputs(&[1.5], &state).is_err(), "above 1");
        assert!(check_outputs(&[-0.1], &state).is_err(), "below 0");
        assert!(
            check_outputs(&[0.5], &state[..STATE_LEN - 1]).is_err(),
            "short state"
        );
        assert!(
            check_outputs(&[0.5], &vec![0.0; STATE_LEN + 1]).is_err(),
            "long state"
        );
    }
}
