//! The Gladia init call ignores system proxy settings, so the `x-gladia-key` header can never be
//! routed through a proxy picked up from the environment.
//!
//! This is its own test binary because it sets process-wide environment variables, which the
//! other Gladia tests (running in parallel threads of one process) must not see.
#![cfg(feature = "provider-gladia")]
#![allow(
    unsafe_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use brisvox_core::{Secret, SystemClock};
use brisvox_providers::gladia::{GladiaConfig, GladiaStt};
use brisvox_providers::{Error, SttConfig, SttProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A listener that counts connections and answers every request with `status`.
async fn counting_server(status: u16) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0_u8; 4096];
                let _ = stream.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn the_init_call_does_not_go_through_a_proxy_from_the_environment() {
    let (proxy_url, via_proxy) = counting_server(418).await;
    let (target_url, at_target) = counting_server(401).await;
    // SAFETY: this binary holds a single test, so no other thread reads the environment while it
    // is modified, and the variables are set before the HTTP client is built.
    unsafe {
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::set_var(name, &proxy_url);
        }
        for name in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(name);
        }
    }
    let mut cfg = GladiaConfig::new(Secret::new("gla-test-key-0123456789".to_owned()));
    cfg.base_url = Some(target_url);
    cfg.io_timeout = Duration::from_secs(2);
    let stt = GladiaStt::new(cfg, SystemClock::shared()).unwrap();
    let err = stt
        .connect(SttConfig {
            sample_rate: SampleRate::Hz16000,
            language: None,
        })
        .await
        .unwrap_err();
    // The request reached the target itself (which refuses it) and not the proxy.
    assert!(
        matches!(err, Error::Rejected { status: 401, .. }),
        "{err:?}"
    );
    assert_eq!(at_target.load(Ordering::SeqCst), 1);
    assert_eq!(
        via_proxy.load(Ordering::SeqCst),
        0,
        "the init call used the environment's proxy"
    );
}
