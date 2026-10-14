//! Tool calling through the real engine path: parallel calls in one response, a chain of
//! rounds, a booking next to a lookup, arguments that break the schema, a slow tool with its
//! filler, and a barge-in while the tools run.
//!
//! Every scenario is one function over a [`harness::Provider`] and runs against
//!
//! * scripted providers, in the normal suite (`mock.rs`): the simulator's `MockLlm` (calls one
//!   after the other, and with interleaved argument fragments) and the real adapters
//!   `OpenAiCompatibleLlm`, `AnthropicLlm` and `GeminiLlm` over loopback servers that speak each
//!   one's wire format for parallel calls (`wire.rs`). The script says what the model does, so
//!   the assertions are exact;
//! * real providers with `BRISVOX_LIVE_TESTS=1` and their keys in the environment (`live.rs`).
//!   A real model varies, so the assertions only pin what must hold whatever it chooses.
//!
//! The engine under test is the production one: `LlmRunner` with its `ToolEngine`, filler policy
//! and interruption handling, in the runner's pipeline (`tests/common/llm.rs`).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation,
    clippy::chunks_exact_to_as_chunks
)]

#[path = "../common/mod.rs"]
mod common;
mod harness;
mod live;
mod mock;
mod observed;
mod s2s;
mod scenarios;
mod wire;
mod world;
