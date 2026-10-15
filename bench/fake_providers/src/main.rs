//! `brisvox-fake-providers`: protocol-faithful fake Deepgram (live STT, Aura TTS) and
//! OpenAI-compatible chat servers, for load tests whose synthetic calls must go through the real
//! provider client code of brisvox and of the Pipecat baseline (README.md).
//!
//! Test tooling: it keeps the real clock and prints to stderr; it is not part of the engine.

#![allow(
    clippy::disallowed_methods,
    reason = "a real-time test server: it paces its output on the wall clock"
)]

mod config;
mod llm;
mod metrics;
mod script;
mod selftest;
mod server;
mod stt;
mod tls;
mod tts;

use std::process::ExitCode;
use std::sync::Arc;

use tokio::sync::{Semaphore, watch};
use tokio_rustls::TlsAcceptor;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("selftest") {
        return run_selftest(args.into_iter().skip(1));
    }
    let port = std::env::var("PORT").ok();
    let config = match config::Config::parse(args, port.as_deref()) {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprint!("{}", config::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("error: {message}\n{}", config::USAGE);
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// `brisvox-fake-providers selftest --url ws://HOST:PORT [--ca FILE] [--calls N] [--seconds S]`:
/// a load generator against a running server (see `selftest.rs`).
fn run_selftest(args: impl Iterator<Item = String>) -> ExitCode {
    let options = match selftest::Options::parse(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(selftest::run(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(config: config::Config) -> Result<(), String> {
    let script = script::load(&config.fixtures_dir, &config.fixture)?;
    let material = tls::load_or_create(config.ca_dir.as_deref(), &config.sans)?;
    let acceptor = TlsAcceptor::from(tls::server_config(&material)?);
    let config = Arc::new(config);
    let shared = Arc::new(server::Shared::new(
        Arc::clone(&config),
        Arc::new(script),
        material.ca_pem.clone(),
    ));
    let limit = Arc::new(Semaphore::new(config.max_connections));
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    if let Some(addr) = config.tls_addr {
        let listener = server::bind(addr).await?;
        eprintln!("tls  (wss://, https://) listening on {addr}");
        tasks.push(tokio::spawn(server::serve(
            listener,
            Some(acceptor),
            Arc::clone(&shared),
            Arc::clone(&limit),
            stop_rx.clone(),
        )));
    }
    if let Some(addr) = config.plain_addr {
        let listener = server::bind(addr).await?;
        eprintln!("plain (ws://, http://, /metrics, /healthz, /ca.pem) listening on {addr}");
        tasks.push(tokio::spawn(server::serve(
            listener,
            None,
            Arc::clone(&shared),
            Arc::clone(&limit),
            stop_rx,
        )));
    }
    if tasks.is_empty() {
        return Err("both listeners are off".to_owned());
    }
    eprintln!(
        "fixture {} ({} utterances); sans {:?}",
        config.fixture,
        shared.script.len(),
        config.sans
    );
    tokio::signal::ctrl_c()
        .await
        .map_err(|e| format!("cannot wait for ctrl-c: {e}"))?;
    let _ = stop_tx.send(true);
    for task in tasks {
        let _ = task.await;
    }
    Ok(())
}
