//! Integration tests of the OpenAI adapter against a loopback mock of the Responses API:
//! the provider contract suite, protocol-level tests and an opt-in live test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods // the mock server and real sockets run on real time
)]

mod contract;
mod live;
mod mock;
mod protocol;
