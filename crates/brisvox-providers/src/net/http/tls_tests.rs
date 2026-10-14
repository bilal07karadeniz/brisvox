//! The settings of the shared client that only show on a real TLS connection: ALPN
//! `h2`, one TLS handshake for `warm()` plus the first stream, HTTP/2 PING keep-alive while
//! idle, and "no system proxy".
//!
//! A loopback server speaks TLS (rustls, blocking, one thread per connection) with a leaf
//! certificate signed by a throw-away test root (`testdata/`, valid for 100 years, its private
//! key was discarded; the leaf key is test-only and protects nothing). Behind the TLS it is a
//! minimal HTTP/2 server written on raw frames, so that the tests can SEE the PING frames: it
//! answers every request with an empty `200`, acknowledges SETTINGS and PINGs, and counts
//! handshakes, negotiated ALPN protocols, requests and PINGs.
//!
//! The clients are built by `build_with`, the function behind the production `build`, with a
//! base TLS config that trusts the test root, so deleting the ALPN line, `http2_keep_alive_*` or
//! `no_proxy` there fails these tests. Not observable from the peer, and so not pinned by any
//! test: the TCP keepalive socket option (`SO_KEEPALIVE` is a property of the client's own
//! socket).

// These tests drive real sockets and the real timers of hyper (HTTP/2 PING intervals), so they wait on
// the wall clock; the injected `Clock` cannot move those.
#![allow(clippy::disallowed_methods)]

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use super::{HttpClientSpec, Timings, build_with};

const CA_PEM: &str = include_str!("../testdata/ca.pem");
const LEAF_PEM: &str = include_str!("../testdata/leaf.pem");
const LEAF_KEY_PEM: &str = include_str!("../testdata/leaf.key.pem");

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_HEADERS: u8 = 0x1;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_PING: u8 = 0x6;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM_AND_HEADERS: u8 = 0x5;
/// HPACK: the indexed header field `:status: 200`.
const HPACK_STATUS_200: u8 = 0x88;

fn ring() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A client TLS config that trusts only the test root.
fn client_tls() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(CA_PEM.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    rustls::ClientConfig::builder_with_provider(ring())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

fn server_tls() -> Arc<rustls::ServerConfig> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(LEAF_PEM.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(LEAF_KEY_PEM.as_bytes()).unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(ring())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

#[derive(Default)]
struct Counts {
    handshakes: AtomicUsize,
    requests: AtomicUsize,
    pings: AtomicUsize,
    /// The ALPN protocol negotiated on each completed handshake.
    alpns: Mutex<Vec<Option<Vec<u8>>>>,
    stop: AtomicBool,
}

struct TlsServer {
    addr: SocketAddr,
    counts: Arc<Counts>,
}

impl TlsServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let counts = Arc::new(Counts::default());
        let config = server_tls();
        let shared = Arc::clone(&counts);
        thread::spawn(move || {
            for tcp in listener.incoming() {
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(tcp) = tcp else { break };
                let config = Arc::clone(&config);
                let counts = Arc::clone(&shared);
                thread::spawn(move || serve(tcp, config, &counts));
            }
        });
        Self { addr, counts }
    }

    fn url(&self) -> String {
        format!("https://{}", self.addr)
    }

    fn handshakes(&self) -> usize {
        self.counts.handshakes.load(Ordering::SeqCst)
    }

    fn requests(&self) -> usize {
        self.counts.requests.load(Ordering::SeqCst)
    }

    fn pings(&self) -> usize {
        self.counts.pings.load(Ordering::SeqCst)
    }

    fn alpns(&self) -> Vec<Option<Vec<u8>>> {
        self.counts
            .alpns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        // Stops the accept loop (a connection wakes it) and the connection threads (their next
        // read timeout).
        self.counts.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

/// Reads exactly `buf.len()` bytes, waiting through read timeouts until the server stops.
/// `false` on EOF, error or stop.
fn read_full(tls: &mut impl Read, buf: &mut [u8], counts: &Counts) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        if counts.stop.load(Ordering::SeqCst) {
            return false;
        }
        match tls.read(&mut buf[filled..]) {
            Ok(0) => return false,
            Ok(n) => filled += n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return false,
        }
    }
    true
}

fn write_frame(tls: &mut impl Write, kind: u8, flags: u8, stream: u32, payload: &[u8]) -> bool {
    let len = u32::try_from(payload.len()).unwrap();
    let mut frame = Vec::with_capacity(9 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes()[1..]);
    frame.push(kind);
    frame.push(flags);
    frame.extend_from_slice(&stream.to_be_bytes());
    frame.extend_from_slice(payload);
    tls.write_all(&frame).and_then(|()| tls.flush()).is_ok()
}

/// One connection: the TLS handshake, then (when `h2` was negotiated) the HTTP/2 frames.
fn serve(mut tcp: TcpStream, config: Arc<rustls::ServerConfig>, counts: &Counts) {
    let Ok(mut conn) = rustls::ServerConnection::new(config) else {
        return;
    };
    while conn.is_handshaking() {
        if conn.complete_io(&mut tcp).is_err() {
            return;
        }
    }
    let alpn = conn.alpn_protocol().map(<[u8]>::to_vec);
    let is_h2 = alpn.as_deref() == Some(b"h2".as_slice());
    counts
        .alpns
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(alpn);
    counts.handshakes.fetch_add(1, Ordering::SeqCst);
    if !is_h2
        || tcp
            .set_read_timeout(Some(Duration::from_millis(50)))
            .is_err()
    {
        return;
    }
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    if !write_frame(&mut tls, FRAME_SETTINGS, 0, 0, &[]) {
        return;
    }
    let mut preface = [0_u8; 24];
    if !read_full(&mut tls, &mut preface, counts) || preface != H2_PREFACE {
        return;
    }
    loop {
        let mut head = [0_u8; 9];
        if !read_full(&mut tls, &mut head, counts) {
            return;
        }
        let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
        let (kind, flags) = (head[3], head[4]);
        let stream = u32::from_be_bytes([head[5] & 0x7f, head[6], head[7], head[8]]);
        let mut payload = vec![0_u8; len];
        if !read_full(&mut tls, &mut payload, counts) {
            return;
        }
        let ok = match kind {
            FRAME_SETTINGS if flags & FLAG_ACK == 0 => {
                write_frame(&mut tls, FRAME_SETTINGS, FLAG_ACK, 0, &[])
            }
            FRAME_PING if flags & FLAG_ACK == 0 => {
                counts.pings.fetch_add(1, Ordering::SeqCst);
                write_frame(&mut tls, FRAME_PING, FLAG_ACK, 0, &payload)
            }
            FRAME_HEADERS => {
                counts.requests.fetch_add(1, Ordering::SeqCst);
                write_frame(
                    &mut tls,
                    FRAME_HEADERS,
                    FLAG_END_STREAM_AND_HEADERS,
                    stream,
                    &[HPACK_STATUS_200],
                )
            }
            _ => true,
        };
        if !ok {
            return;
        }
    }
}

fn spec(url: &str) -> HttpClientSpec<'_> {
    HttpClientSpec {
        provider: "tls-test",
        base_url: url,
        connect_timeout: Duration::from_secs(5),
        pool_idle_timeout: Some(Duration::from_secs(600)),
        http2_prior_knowledge: false,
    }
}

fn client(url: &str, timings: Timings) -> reqwest::Client {
    build_with(&spec(url), client_tls(), timings).unwrap()
}

async fn status(request: reqwest::RequestBuilder) -> reqwest::StatusCode {
    tokio::time::timeout(Duration::from_secs(5), request.send())
        .await
        .unwrap()
        .unwrap()
        .status()
}

#[tokio::test]
async fn alpn_negotiates_h2_and_warm_plus_the_first_stream_share_one_tls_handshake() {
    let server = TlsServer::start();
    let url = server.url();
    let client = client(&url, Timings::PRODUCTION);
    // What `warm()` does (a HEAD), then what the first turn does (a request on the same client).
    assert_eq!(status(client.head(&url)).await, 200);
    assert_eq!(server.handshakes(), 1, "the warm-up opened the connection");
    assert_eq!(status(client.get(&url)).await, 200);
    assert_eq!(status(client.get(&url)).await, 200);
    assert_eq!(server.requests(), 3);
    assert_eq!(
        server.handshakes(),
        1,
        "the first stream must reuse the warm connection, not handshake again"
    );
    assert_eq!(
        server.alpns(),
        vec![Some(b"h2".to_vec())],
        "ALPN must settle on h2"
    );
}

#[tokio::test]
async fn an_idle_h2_connection_is_kept_alive_with_pings() {
    let server = TlsServer::start();
    let url = server.url();
    let timings = Timings {
        h2_keep_alive_interval: Duration::from_millis(100),
        h2_keep_alive_timeout: Duration::from_secs(2),
        ..Timings::PRODUCTION
    };
    let client = client(&url, timings);
    assert_eq!(status(client.head(&url)).await, 200);
    // Idle: no stream is open, yet the client must keep pinging (`keep_alive_while_idle`).
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.pings() < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        server.pings() >= 3,
        "an idle connection must send HTTP/2 PINGs, saw {}",
        server.pings()
    );
    // The answered pings keep the one connection: the next request needs no new handshake.
    assert_eq!(status(client.get(&url)).await, 200);
    assert_eq!(server.handshakes(), 1);
}

#[tokio::test]
async fn the_production_interval_does_not_ping_within_a_second() {
    // The control of the test above: with the production 20 s interval an idle connection stays
    // quiet for a while, so the PINGs seen above come from the interval and nothing else.
    let server = TlsServer::start();
    let url = server.url();
    let client = client(&url, Timings::PRODUCTION);
    assert_eq!(status(client.head(&url)).await, 200);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(server.pings(), 0);
}

/// The environment variable that turns [`proxy_child`] from a no-op into the real check.
const CHILD_ENV: &str = "BRISVOX_TLS_TEST_PROXY_CHILD";

/// Run as a child process of [`a_system_proxy_setting_is_ignored`] with proxy variables set:
/// reaches the loopback TLS server directly. (Setting environment variables in-process is
/// `unsafe` in edition 2024 and the crate denies `unsafe_code`.) Without the variable it does
/// nothing.
#[tokio::test]
async fn proxy_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let server = TlsServer::start();
    let url = server.url();
    let client = client(&url, Timings::PRODUCTION);
    assert_eq!(status(client.head(&url)).await, 200);
    assert_eq!(server.handshakes(), 1);
    println!("PROXY_CHILD_OK");
}

#[test]
fn a_system_proxy_setting_is_ignored() {
    // Port 9 (discard) on loopback: nothing listens there, so a client that honoured the proxy
    // variables could not reach the server.
    let proxy = "http://127.0.0.1:9";
    let exe = std::env::current_exe().unwrap();
    let output = Command::new(exe)
        .args([
            "--exact",
            "net::http::tls_tests::proxy_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env("HTTP_PROXY", proxy)
        .env("HTTPS_PROXY", proxy)
        .env("ALL_PROXY", proxy)
        .env("http_proxy", proxy)
        .env("https_proxy", proxy)
        .env("all_proxy", proxy)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("PROXY_CHILD_OK"),
        "the client went through the proxy: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
