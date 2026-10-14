//! Integration tests of the Gemini adapter against a loopback mock of `streamGenerateContent`:
//! the provider contract suite, protocol-level tests and an opt-in live test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::disallowed_methods // the mock server and real sockets run on real time
)]

mod clock;
mod contract;
mod h2mock;
mod live;
mod mock;
mod protocol;
mod review;
