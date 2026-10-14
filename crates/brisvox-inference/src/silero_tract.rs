//! Silero VAD v6.2.3 on tract, pure Rust (feature `silero-tract`).
//!
//! tract cannot load the stock graph (shape inference fails inside its nested `If` subgraphs),
//! so it runs a copy prepared by `scripts/models/prepare_silero_tract.py`: the 16 kHz branch
//! inlined with static shapes and constant-folded by onnxruntime offline (no `If` left). Same
//! contract as the ort backend (`crate::silero`, feature `silero`): `input` f32 [1, 576] (64 context + 512 new samples), `state`
//! f32 [2, 1, 128], `sr` i64 (unused by the prepared graph but kept as an input); outputs
//! `output` f32 [1, 1] and `stateN` f32 [2, 1, 128].
//!
//! **Sharing**: a compiled tract plan runs through `&self`, so one [`SileroTractModel`] (the
//! weights and the optimized plan, loaded once) is shared by every stream behind an `Arc`;
//! each [`SileroTract`] stream carries only its recurrent state.

use std::path::Path;
use std::sync::Arc;

#[allow(clippy::wildcard_imports)] // tract's extension traits (into_runnable, ...)
use tract_onnx::prelude::*;

use crate::error::{Error, Result};
use crate::vad::SpeechModel;

/// Samples per model call at 16 kHz.
pub const WINDOW: usize = 512;
/// Context samples prepended to each call at 16 kHz.
pub const CONTEXT: usize = 64;
/// The model's sample rate.
pub const SAMPLE_RATE: u32 = 16_000;

/// Size of the prepared graph (`scripts/models/prepare_silero_tract.py` documents its SHA-256).
pub const PREPARED_BYTES: u64 = 1_251_444;

/// The prepared graph, embedded (SHA-256 9efbb701...accb8f, checked by a test).
static BUNDLED: &[u8] = include_bytes!("../../../assets/models/silero_vad_16k_tract.onnx");

const STATE_SHAPE: [usize; 3] = [2, 1, 128];

fn model_error(e: impl std::fmt::Display) -> Error {
    Error::Model(e.to_string())
}

type Plan = Arc<TypedRunnableModel>;

/// The loaded, optimized model, shared by all streams.
pub struct SileroTractModel {
    plan: Plan,
}

impl std::fmt::Debug for SileroTractModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SileroTractModel").finish_non_exhaustive()
    }
}

impl SileroTractModel {
    /// The prepared graph bundled into the binary (`assets/models/silero_vad_16k_tract.onnx`,
    /// MIT, see `assets/models/NOTICE`): no file or download needed at run time.
    ///
    /// # Errors
    /// [`Error::Model`] if tract rejects the graph (not expected: covered by tests).
    pub fn bundled() -> Result<Arc<Self>> {
        Self::from_bytes(BUNDLED)
    }

    /// Loads and optimizes a prepared graph from `path` (tens of ms: do it once at startup).
    ///
    /// # Errors
    /// [`Error::Model`] if the file is missing, has the wrong size, or tract rejects it.
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        let bytes =
            std::fs::read(path).map_err(|e| model_error(format!("{}: {e}", path.display())))?;
        Self::from_bytes(&bytes)
    }

    /// Loads and optimizes a prepared graph from memory.
    ///
    /// # Errors
    /// [`Error::Model`] if the size is not the prepared graph's or tract rejects it.
    pub fn from_bytes(bytes: &[u8]) -> Result<Arc<Self>> {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size != PREPARED_BYTES {
            return Err(model_error(format!(
                "{size} bytes, expected the prepared graph's {PREPARED_BYTES}                  (scripts/models/prepare_silero_tract.py)"
            )));
        }
        let samples = i64::try_from(CONTEXT + WINDOW).unwrap_or(576);
        let plan = tract_onnx::onnx()
            .model_for_read(&mut &bytes[..])
            .map_err(model_error)?
            .with_input_fact(
                0,
                InferenceFact::dt_shape(f32::datum_type(), tvec!(1, samples)),
            )
            .map_err(model_error)?
            .with_input_fact(
                1,
                InferenceFact::dt_shape(f32::datum_type(), tvec!(2, 1, 128)),
            )
            .map_err(model_error)?
            .with_input_fact(2, InferenceFact::from(tensor0(i64::from(SAMPLE_RATE))))
            .map_err(model_error)?
            .into_optimized()
            .map_err(model_error)?
            .into_runnable()
            .map_err(model_error)?;
        Ok(Arc::new(Self { plan }))
    }

    /// A new stream (zero state) over this model.
    pub fn stream(self: &Arc<Self>) -> Result<SileroTract> {
        Ok(SileroTract {
            model: Arc::clone(self),
            state: Tensor::zero::<f32>(&STATE_SHAPE).map_err(model_error)?,
        })
    }
}

/// One stream's Silero VAD on the shared tract model.
#[derive(Debug)]
pub struct SileroTract {
    model: Arc<SileroTractModel>,
    state: Tensor,
}

impl SpeechModel for SileroTract {
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
        let input = Tensor::from_shape(&[1, CONTEXT + WINDOW], input).map_err(model_error)?;
        let state = std::mem::replace(
            &mut self.state,
            Tensor::zero::<f32>(&STATE_SHAPE).map_err(model_error)?,
        );
        let inputs: TVec<_> = tvec!(
            input.into(),
            state.into(),
            tensor0(i64::from(SAMPLE_RATE)).into()
        );
        let mut outputs = self.model.plan.run(inputs).map_err(model_error)?;
        if outputs.len() < 2 {
            return Err(model_error("expected 2 outputs"));
        }
        let next = outputs.remove(1).into_tensor();
        let probability = outputs.remove(0).into_tensor();
        let probability = probability
            .as_plain_ram()
            .ok_or_else(|| model_error("output not in plain memory"))?
            .as_slice::<f32>()
            .map_err(model_error)?
            .first()
            .copied()
            .ok_or_else(|| model_error("empty output"))?;
        if next.shape() == STATE_SHAPE {
            self.state = next;
        }
        Ok(probability)
    }

    fn reset(&mut self) {
        if let Ok(zero) = Tensor::zero::<f32>(&STATE_SHAPE) {
            self.state = zero;
        }
    }
}
