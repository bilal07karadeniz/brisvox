//! Audio utilities for brisvox: μ-law/A-law/PCM16 codecs, resampling, buffer management and
//! real-time output pacing.

pub mod analysis;
pub mod codec;
pub mod error;
pub mod pacer;
pub mod resample;

pub use codec::Codec;
pub use error::{Error, Result};
pub use pacer::OutputPacer;
pub use resample::StreamResampler;
