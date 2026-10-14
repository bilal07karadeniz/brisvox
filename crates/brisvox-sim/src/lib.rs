//! Deterministic simulation support for brisvox: mock providers with latency profiles, a
//! paused-time simulator, protocol fixtures and leak checks. Used by tests and benchmarks only.

pub mod assertions;
pub mod contract;
pub mod error;
pub mod factory;
pub mod fixture;
pub mod input;
pub mod latency;
pub mod leak;
pub mod mock_common;
pub mod mock_llm;
pub mod mock_stt;
pub mod mock_tts;
pub mod outage;
pub mod output;
pub mod platform;
pub mod reference;
pub mod scenario;
pub mod session;
pub mod suite;
pub mod vad;

pub use error::{Error, Result};
