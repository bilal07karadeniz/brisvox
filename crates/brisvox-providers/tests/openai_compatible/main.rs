//! Integration tests of the OpenAI-compatible adapter against a loopback mock of Chat
//! Completions, parameterised by the wire dialect of each preset: the provider contract suite,
//! protocol-level tests and an opt-in live test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods, // the mock server and real sockets run on real time
    dead_code
)]

mod contract;
mod live;
mod mock;
mod protocol;
mod wire;
