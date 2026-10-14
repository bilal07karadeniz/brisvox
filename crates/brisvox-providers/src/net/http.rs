//! The process-wide HTTP client pool: one `reqwest::Client` per distinct client
//! configuration, shared by every session and every adapter instance of the process.
//!
//! A `reqwest::Client` owns the connection pool (the TCP + TLS handshakes and the HTTP/2
//! connection with its keep-alive pings live there). An adapter that built its own client per
//! call paid a full handshake on every call's first request and held one idle connection per
//! call. Clients built through [`shared_client`] are keyed by the request origin (scheme, host,
//! port) plus every setting that changes how the client connects, and handed out as cheap clones
//! of one `Arc`-backed handle, so the connection a previous call left in the pool (and the
//! connection the `warm()` HEAD request opened) serves the next call's first request.
//!
//! Credentials are NOT part of the key and not part of the client: adapters put them in
//! per-request headers, so calls with different API keys can share the connection. The pool
//! holds at most [`MAX_SHARED_CLIENTS`] distinct configurations; when a new one arrives at a
//! full pool the least recently used entry is evicted (counted in [`PoolStats::evicted`], and
//! logged once at `warn`), so a deployment with unbounded per-tenant base URLs cannot grow the
//! map without limit and a new configuration always gets sharing. Callers that still hold the
//! evicted client keep it alive and keep using its connections; only the next caller of the
//! evicted configuration builds a fresh client.
//!
//! The clients are runtime-independent: a connection opened under one tokio runtime is
//! discarded when that runtime is gone and the next request connects again under the current
//! one (a library host that restarts its runtime pays one handshake, nothing fails).
//!
//! # Concurrency limit
//! HTTP/2 multiplexes the streams of every call to one origin onto few connections, and a
//! server advertises `SETTINGS_MAX_CONCURRENT_STREAMS` (commonly 100). Beyond it `hyper` queues
//! the request until a stream frees up, which on the LLM path is latency. This has not been
//! measured against the real providers (see `docs/RISKS.md` R16); if it proves a limit the key
//! gets a shard index.
//!
//! # Tests
//! `tls_tests` runs the clients of `build_with` (the function behind `build`) against a TLS
//! loopback server and pins ALPN `h2`, one handshake for `warm()` plus the first stream, HTTP/2
//! PING keep-alive while idle and "no system proxy". The TCP keepalive option is not observable
//! from the peer and has no test.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use crate::error::{Error, Result};

/// How many distinct client configurations the pool keeps. Past it the least recently used one
/// is evicted (see the module docs).
pub const MAX_SHARED_CLIENTS: usize = 64;

/// Interval of the HTTP/2 PING frames that keep an idle shared connection alive (and detect a
/// dead one before the next request does).
const H2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// How long a PING may stay unanswered before the connection is considered dead.
const H2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// TCP keepalive probes on the underlying sockets.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// The keepalive timings of a built client. Production uses [`Timings::PRODUCTION`]; the tests
/// of `tls_tests` shorten them to watch the HTTP/2 PING frames of an idle connection.
#[derive(Debug, Clone, Copy)]
struct Timings {
    tcp_keepalive: Duration,
    h2_keep_alive_interval: Duration,
    h2_keep_alive_timeout: Duration,
}

impl Timings {
    const PRODUCTION: Self = Self {
        tcp_keepalive: TCP_KEEPALIVE,
        h2_keep_alive_interval: H2_KEEP_ALIVE_INTERVAL,
        h2_keep_alive_timeout: H2_KEEP_ALIVE_TIMEOUT,
    };
}

/// Everything that distinguishes one shared client from another.
#[derive(Debug, Clone, Copy)]
pub struct HttpClientSpec<'a> {
    /// Provider name for errors (`"openai"`, ...).
    pub provider: &'static str,
    /// The adapter's base URL: only its origin (scheme, host, port) is used.
    pub base_url: &'a str,
    /// TCP + TLS connect deadline.
    pub connect_timeout: Duration,
    /// How long an idle connection stays in the pool (`None`: until the server closes it).
    pub pool_idle_timeout: Option<Duration>,
    /// Speak HTTP/2 without negotiating it (cleartext HTTP/2 test servers).
    pub http2_prior_knowledge: bool,
}

/// The key: the origin plus the connection-affecting settings (the provider name is not part
/// of it: two adapters talking to the same origin the same way share the connections).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    origin: String,
    connect_timeout: Duration,
    pool_idle_timeout: Option<Duration>,
    http2_prior_knowledge: bool,
}

impl Key {
    fn of(spec: &HttpClientSpec<'_>) -> Self {
        Self {
            origin: origin_of(spec.base_url),
            connect_timeout: spec.connect_timeout,
            pool_idle_timeout: spec.pool_idle_timeout,
            http2_prior_knowledge: spec.http2_prior_knowledge,
        }
    }
}

/// Counters of the pool, for evidence that sessions share clients.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Clients built and put in the pool.
    pub built: u64,
    /// Requests for a client that were served by one already in the pool.
    pub reused: u64,
    /// Entries evicted (least recently used) to make room for a new configuration.
    pub evicted: u64,
    /// Clients in the pool now.
    pub entries: usize,
}

#[derive(Debug)]
struct Entry {
    client: reqwest::Client,
    /// Value of the pool's use counter at the last request (the LRU order).
    last_used: u64,
}

#[derive(Debug, Default)]
struct Entries {
    map: HashMap<Key, Entry>,
    /// Incremented on every request; stamped into the entry it served.
    uses: u64,
}

/// A bounded map from client configuration to shared client. The process uses one
/// ([`shared_client`]); the type is separate so that tests can use a private one.
#[derive(Debug)]
struct ClientPool {
    entries: Mutex<Entries>,
    capacity: usize,
    built: AtomicU64,
    reused: AtomicU64,
    evicted: AtomicU64,
    /// The first eviction was logged.
    warned: AtomicBool,
}

impl ClientPool {
    fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(Entries::default()),
            capacity: capacity.max(1),
            built: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            warned: AtomicBool::new(false),
        }
    }

    fn get_or_build(&self, spec: &HttpClientSpec<'_>) -> Result<reqwest::Client> {
        let key = Key::of(spec);
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.uses = entries.uses.wrapping_add(1);
        let now = entries.uses;
        if let Some(entry) = entries.map.get_mut(&key) {
            entry.last_used = now;
            self.reused.fetch_add(1, Ordering::Relaxed);
            return Ok(entry.client.clone());
        }
        // Built under the lock: a miss happens once per configuration and the build does no I/O.
        let client = build(spec)?;
        if entries.map.len() >= self.capacity {
            let oldest = entries
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.map.remove(&oldest);
                self.evicted.fetch_add(1, Ordering::Relaxed);
                if !self.warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        target: "brisvox::providers",
                        capacity = self.capacity,
                        "the shared HTTP client pool is full: evicting the least recently used \
                         client (many distinct provider origins or timeouts?)"
                    );
                }
            }
        }
        self.built.fetch_add(1, Ordering::Relaxed);
        entries.map.insert(
            key,
            Entry {
                client: client.clone(),
                last_used: now,
            },
        );
        Ok(client)
    }

    fn stats(&self) -> PoolStats {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .len();
        PoolStats {
            built: self.built.load(Ordering::Relaxed),
            reused: self.reused.load(Ordering::Relaxed),
            evicted: self.evicted.load(Ordering::Relaxed),
            entries,
        }
    }
}

fn pool() -> &'static ClientPool {
    static POOL: OnceLock<ClientPool> = OnceLock::new();
    POOL.get_or_init(|| ClientPool::new(MAX_SHARED_CLIENTS))
}

/// The origin of `base_url` (`scheme://host:port`), or the URL itself when it does not parse
/// (the adapter's own URL check reports that case; the key only has to be stable).
fn origin_of(base_url: &str) -> String {
    reqwest::Url::parse(base_url).map_or_else(
        |_| base_url.to_owned(),
        |url| url.origin().ascii_serialization(),
    )
}

fn build(spec: &HttpClientSpec<'_>) -> Result<reqwest::Client> {
    build_with(spec, (*super::tls_config()?).clone(), Timings::PRODUCTION)
}

/// The client builder behind [`build`]: `tls` is the base TLS config (the shared one in
/// production, one that trusts a test root in `tls_tests`), everything else is what every shared
/// client gets, so a test through this function pins the real settings.
fn build_with(
    spec: &HttpClientSpec<'_>,
    mut tls: rustls::ClientConfig,
    timings: Timings,
) -> Result<reqwest::Client> {
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let mut builder = reqwest::Client::builder().tls_backend_preconfigured(tls);
    if spec.http2_prior_knowledge {
        builder = builder.http2_prior_knowledge();
    }
    // Dual-stack hosts: reqwest 0.13.5 builds its hyper-util 0.1.21 `HttpConnector`, whose
    // default `happy_eyeballs_timeout` is 300 ms (RFC 6555 style: the second address family
    // starts 300 ms after the first and the first socket to connect wins). Nothing here sets a
    // `local_address`, which would switch that race off, so a blackholed IPv6 route does not
    // eat `connect_timeout`. No builder switch needed.
    builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("brisvox/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(spec.connect_timeout)
        .tcp_keepalive(Some(timings.tcp_keepalive))
        .pool_idle_timeout(spec.pool_idle_timeout)
        .http2_keep_alive_interval(Some(timings.h2_keep_alive_interval))
        .http2_keep_alive_timeout(timings.h2_keep_alive_timeout)
        .http2_keep_alive_while_idle(true)
        .build()
        .map_err(|e| Error::Protocol {
            provider: spec.provider,
            detail: format!("cannot build the HTTP client: {e}"),
        })
}

/// The shared client for `spec`: the one already in the process-wide pool, or a new one that is
/// put there. Cloning a `reqwest::Client` shares its connection pool, so every caller of the
/// same spec talks over the same connections.
///
/// The client has the shared TLS config ([`tls_config`](super::tls_config)) with ALPN `h2` then
/// `http/1.1`, no system proxy, no redirects (so a key never follows one), TCP keepalive and
/// HTTP/2 PING keep-alive while idle.
///
/// # Errors
/// [`Error::Protocol`] if the TLS config or the client cannot be built.
pub fn shared_client(spec: &HttpClientSpec<'_>) -> Result<reqwest::Client> {
    pool().get_or_build(spec)
}

/// The process-wide pool's counters now.
pub fn pool_stats() -> PoolStats {
    pool().stats()
}

#[cfg(test)]
mod tls_tests;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn spec(base_url: &str) -> HttpClientSpec<'_> {
        HttpClientSpec {
            provider: "test",
            base_url,
            connect_timeout: Duration::from_millis(1_234),
            pool_idle_timeout: Some(Duration::from_secs(600)),
            http2_prior_knowledge: false,
        }
    }

    #[test]
    fn the_origin_ignores_path_and_trailing_slash_but_not_port_or_scheme() {
        assert_eq!(
            origin_of("https://api.example.com/v1/"),
            "https://api.example.com"
        );
        assert_eq!(
            origin_of("https://api.example.com"),
            origin_of("https://api.example.com/openai/v1")
        );
        assert_ne!(
            origin_of("https://api.example.com:8443"),
            origin_of("https://api.example.com")
        );
        assert_ne!(
            origin_of("http://127.0.0.1:9"),
            origin_of("https://127.0.0.1:9")
        );
        assert_eq!(origin_of("not a url"), "not a url");
    }

    #[test]
    fn the_same_spec_is_served_from_the_pool_and_counted_once() {
        let pool = ClientPool::new(8);
        pool.get_or_build(&spec("https://a.test.invalid/v1"))
            .unwrap();
        pool.get_or_build(&spec("https://a.test.invalid/other/path"))
            .unwrap();
        pool.get_or_build(&spec("https://a.test.invalid")).unwrap();
        assert_eq!(
            pool.stats(),
            PoolStats {
                built: 1,
                reused: 2,
                evicted: 0,
                entries: 1
            }
        );
    }

    #[test]
    fn every_connection_affecting_field_is_part_of_the_key() {
        let pool = ClientPool::new(16);
        let base = spec("https://b.test.invalid");
        let mut other_timeout = base;
        other_timeout.connect_timeout = Duration::from_millis(4_321);
        let mut other_idle = base;
        other_idle.pool_idle_timeout = None;
        let mut other_h2 = base;
        other_h2.http2_prior_knowledge = true;
        let other_origin = spec("https://b2.test.invalid");
        let other_port = spec("https://b.test.invalid:8443");
        let other_scheme = spec("http://b.test.invalid");
        for s in [
            base,
            other_timeout,
            other_idle,
            other_h2,
            other_origin,
            other_port,
            other_scheme,
        ] {
            pool.get_or_build(&s).unwrap();
        }
        let stats = pool.stats();
        assert_eq!((stats.built, stats.reused, stats.entries), (7, 0, 7));
        // The provider name is not part of it: the same connection settings share the client.
        let mut renamed = base;
        renamed.provider = "someone-else";
        pool.get_or_build(&renamed).unwrap();
        assert_eq!(pool.stats().reused, 1);
        assert_eq!(pool.stats().entries, 7);
    }

    #[test]
    fn a_full_pool_evicts_the_least_recently_used_entry_and_keeps_sharing() {
        let pool = ClientPool::new(3);
        let urls = [
            "https://e0.test.invalid",
            "https://e1.test.invalid",
            "https://e2.test.invalid",
        ];
        for url in urls {
            pool.get_or_build(&spec(url)).unwrap();
        }
        // e0 is the oldest, but it is used again: e1 becomes the least recently used.
        pool.get_or_build(&spec(urls[0])).unwrap();
        pool.get_or_build(&spec("https://e3.test.invalid")).unwrap();
        let stats = pool.stats();
        assert_eq!((stats.built, stats.evicted, stats.entries), (4, 1, 3));
        // e0, e2 and e3 are still shared; e1 was evicted and is built again.
        for url in [urls[0], urls[2], "https://e3.test.invalid"] {
            let before = pool.stats().reused;
            pool.get_or_build(&spec(url)).unwrap();
            assert_eq!(pool.stats().reused, before + 1, "{url}");
        }
        let before = pool.stats();
        pool.get_or_build(&spec(urls[1])).unwrap();
        let after = pool.stats();
        assert_eq!(after.built, before.built + 1);
        assert_eq!(after.evicted, before.evicted + 1);
        assert_eq!(after.entries, 3);
    }

    #[test]
    fn many_more_configurations_than_the_capacity_never_grow_the_pool() {
        let pool = ClientPool::new(4);
        for i in 0..40 {
            let url = format!("https://tenant{i}.test.invalid");
            pool.get_or_build(&spec(&url)).unwrap();
            // The newest entry is always shared.
            let before = pool.stats().reused;
            pool.get_or_build(&spec(&url)).unwrap();
            assert_eq!(pool.stats().reused, before + 1);
        }
        let stats = pool.stats();
        assert_eq!((stats.entries, stats.built, stats.evicted), (4, 40, 36));
    }

    /// A minimal keep-alive HTTP/1.1 server on a runtime of its own; counts connections.
    struct Server {
        addr: std::net::SocketAddr,
        connections: Arc<AtomicUsize>,
        runtime: Option<tokio::runtime::Runtime>,
    }

    impl Server {
        fn start() -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let listener = runtime
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let connections = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&connections);
            runtime.spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let mut buf = vec![0_u8; 4096];
                        while let Ok(n) = socket.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            let reply = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
                            if socket.write_all(reply).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            });
            Self {
                addr,
                connections,
                runtime: Some(runtime),
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            if let Some(runtime) = self.runtime.take() {
                runtime.shutdown_background();
            }
        }
    }

    /// One GET on a runtime of its own, dropped on return: the connection driver dies with it.
    fn get_on_new_runtime(client: &reqwest::Client, url: &str) -> reqwest::StatusCode {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), client.get(url).send())
                .await
                .unwrap()
                .unwrap()
                .status()
        })
    }

    #[test]
    fn a_client_whose_connection_died_with_its_runtime_reconnects_under_the_next_one() {
        let server = Server::start();
        let pool = ClientPool::new(4);
        let url = server.url();
        let client = pool.get_or_build(&spec(&url)).unwrap();
        assert_eq!(get_on_new_runtime(&client, &url), 200);
        assert_eq!(server.connections(), 1);
        // The first runtime is gone; the same pooled client is used from a second one.
        let again = pool.get_or_build(&spec(&url)).unwrap();
        assert_eq!(get_on_new_runtime(&again, &url), 200);
        assert_eq!(pool.stats().reused, 1, "it is the pooled client");
        assert_eq!(server.connections(), 2, "one transparent reconnect");
    }

    #[test]
    fn requests_under_one_runtime_share_a_connection() {
        let server = Server::start();
        let pool = ClientPool::new(4);
        let url = server.url();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for _ in 0..3 {
                let client = pool.get_or_build(&spec(&url)).unwrap();
                let status = client.get(&url).send().await.unwrap().status();
                assert_eq!(status, 200);
            }
        });
        assert_eq!(server.connections(), 1);
    }
}
