//! `brisvox-bench hold` and `brisvox-bench container-stats`: the two helpers of the listening test
//! (`bench/railway/README.md`).
//!
//! `hold` keeps N synthetic calls running against each target until told otherwise: every slot
//! plays the fixture as one call (the `v2v` client: real-time 20 ms frames, V2V per turn), hangs
//! up, and dials again. A small page (`GET /`) sets N per target, shows what the synthetic calls
//! measure over the last 30 s and the CPU and memory of each target's container, and makes a real
//! call from the browser (`sdk/web/brisvox-call.js`, served at `/brisvox-call.js`, WebSocket
//! transport) to the target's public URL. Everything under `/api/` needs the access key
//! (`X-Hold-Key` header or `?key=`).
//!
//! `container-stats` serves `GET /stats`: the CPU (cores used over the last 2 s) and memory of the
//! container it runs in (cgroup v2 when readable, else the process tree of pid 1), as JSON.
//!
//! Both speak just enough HTTP/1.1 for a browser and a PaaS proxy: one request per connection,
//! heads up to 8 KiB, 64 connections at once, 10 s per connection.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_bench::BenchError;
use brisvox_bench::capacity::probe::{Scope, TreeProbe};
use brisvox_bench::capacity::stagger;
use brisvox_bench::fixture_load::pad_gaps;
use brisvox_bench::v2v::client::{ClientConfig, run_call};
use brisvox_bench::v2v::measure::Outcome;
use brisvox_core::{SharedClock, SystemClock};
use brisvox_sim::fixture::Fixture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const PAGE: &str = include_str!("hold_page.html");
const CALL_MODULE: &str = include_str!("../../../sdk/web/brisvox-call.js");

/// Longest request head read.
const MAX_HEAD: usize = 8 * 1024;
/// Connections served at once; the rest wait for a permit.
const MAX_CONNECTIONS: usize = 64;
/// Read, handle and write of one connection.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
/// Events kept per target. Older ones are dropped first (the page reads the last 30 s; at 350
/// calls a target makes about 150 events a second).
const MAX_EVENTS: usize = 100_000;
/// The window the page's numbers cover.
const WINDOW: Duration = Duration::from_secs(30);
/// Most synthetic calls per target.
const MAX_CALLS: usize = 2_000;

const HOLD_USAGE: &str = "brisvox-bench hold [--bind ADDR] [--fixtures-dir DIR] [--fixture NAME] \
    [--access-key-env VAR] --target 'NAME;LABEL;LOAD_WS_URL;STATS_URL;CALL_URL;CALL_TOKEN_VAR' ...\n\
    brisvox-bench container-stats [--bind ADDR]";

struct Target {
    name: String,
    label: String,
    client: ClientConfig,
    stats_url: String,
    call_url: String,
    call_token: String,
    slots: Mutex<Vec<CancellationToken>>,
    in_call: AtomicUsize,
    events: Mutex<VecDeque<Event>>,
    server: Mutex<String>,
}

#[derive(Clone)]
struct Event {
    at: Instant,
    kind: EventKind,
}

#[derive(Clone)]
enum EventKind {
    CallOk {
        late_frames: u64,
        frames: u64,
    },
    CallFailed(String),
    /// The server answered the dial with 503: full or protecting its live calls.
    CallRefused,
    Turn(Option<Duration>),
}

struct Hold {
    clock: SharedClock,
    fixture: Arc<Fixture>,
    targets: Vec<Arc<Target>>,
    access_key: String,
    cancel: CancellationToken,
}

fn arg(flag: &str, value: Option<String>) -> Result<String, BenchError> {
    value.ok_or_else(|| BenchError::Config(format!("{flag} needs a value\n{HOLD_USAGE}")))
}

fn env_var(name: &str) -> Result<String, BenchError> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| BenchError::Config(format!("environment variable {name} is not set")))
}

/// `--bind`, else `[::]:$PORT`, else `default`.
fn bind_addr(bind: Option<String>, default: &str) -> String {
    bind.unwrap_or_else(|| {
        std::env::var("PORT")
            .ok()
            .filter(|p| !p.is_empty())
            .map_or_else(|| default.to_owned(), |p| format!("[::]:{p}"))
    })
}

fn runtime() -> Result<tokio::runtime::Runtime, BenchError> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

/// `brisvox-bench hold`.
pub fn hold_cmd(mut args: impl Iterator<Item = String>) -> Result<bool, BenchError> {
    let mut bind = None;
    let mut fixtures_dir = brisvox_sim::fixture::fixtures_dir();
    let mut fixture_name = "en_two_utterances".to_owned();
    let mut key_var = "HOLD_ACCESS_KEY".to_owned();
    let mut specs = Vec::new();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--bind" => bind = Some(arg(&flag, args.next())?),
            "--fixtures-dir" => fixtures_dir = arg(&flag, args.next())?.into(),
            "--fixture" => fixture_name = arg(&flag, args.next())?,
            "--access-key-env" => key_var = arg(&flag, args.next())?,
            "--target" => specs.push(arg(&flag, args.next())?),
            _ => {
                return Err(BenchError::Config(format!(
                    "unknown flag {flag}\n{HOLD_USAGE}"
                )));
            }
        }
    }
    let access_key = env_var(&key_var)?;
    let mut targets = Vec::new();
    for spec in specs {
        let parts: Vec<&str> = spec.split(';').collect();
        let [name, label, load_url, stats_url, call_url, token_var] = parts.as_slice() else {
            return Err(BenchError::Config(format!(
                "bad --target {spec:?}\n{HOLD_USAGE}"
            )));
        };
        let mut client = ClientConfig::new(*load_url);
        client.response_timeout = Duration::from_secs(10);
        targets.push(Arc::new(Target {
            name: (*name).to_owned(),
            label: (*label).to_owned(),
            client,
            stats_url: (*stats_url).to_owned(),
            call_url: (*call_url).to_owned(),
            call_token: if token_var.is_empty() {
                String::new()
            } else {
                env_var(token_var)?
            },
            slots: Mutex::new(Vec::new()),
            in_call: AtomicUsize::new(0),
            events: Mutex::new(VecDeque::new()),
            server: Mutex::new("null".to_owned()),
        }));
    }
    if targets.is_empty() {
        return Err(BenchError::Config(format!("no --target\n{HOLD_USAGE}")));
    }
    let loaded = Fixture::load(&fixtures_dir, &fixture_name)
        .map_err(|e| BenchError::Config(format!("fixture {fixture_name}: {e}")))?;
    let fixture = Arc::new(pad_gaps(&loaded, Duration::from_millis(4_000)));
    let addr = bind_addr(bind, "[::]:8080");
    runtime()?.block_on(async move {
        let hold = Arc::new(Hold {
            clock: SystemClock::shared(),
            fixture,
            targets,
            access_key,
            cancel: CancellationToken::new(),
        });
        for target in &hold.targets {
            tokio::spawn(poll_server(Arc::clone(&hold), Arc::clone(target)));
        }
        let listener = TcpListener::bind(&addr).await?;
        eprintln!("hold: listening on {addr}");
        let h = Arc::clone(&hold);
        serve(listener, move |req| {
            let h = Arc::clone(&h);
            async move { h.handle(req) }
        })
        .await;
        Ok(true)
    })
}

/// Fetches the target's container stats every 2 s (the page shows the last answer).
async fn poll_server(hold: Arc<Hold>, target: Arc<Target>) {
    if target.stats_url.is_empty() {
        return;
    }
    loop {
        let body =
            match tokio::time::timeout(Duration::from_secs(3), http_get(&target.stats_url)).await {
                Ok(Ok(body)) if body.trim_start().starts_with('{') => body,
                Ok(Ok(_)) => "{\"error\":\"not JSON\"}".to_owned(),
                Ok(Err(e)) => format!("{{\"error\":{}}}", json_str(&e.to_string())),
                Err(_) => "{\"error\":\"timeout\"}".to_owned(),
            };
        if let Ok(mut server) = target.server.lock() {
            *server = body;
        }
        tokio::select! {
            () = hold.cancel.cancelled() => return,
            () = hold.clock.sleep(Duration::from_secs(2)) => {}
        }
    }
}

/// A plain `GET http://host:port/path`; the body.
async fn http_get(url: &str) -> std::io::Result<String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other("only http:// URLs"))?;
    let (host, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
    let mut stream = TcpStream::connect(host).await?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.take(1 << 20).read_to_end(&mut response).await?;
    let text = String::from_utf8_lossy(&response);
    Ok(text
        .split_once("\r\n\r\n")
        .map_or_else(String::new, |(_, body)| body.to_owned()))
}

impl Hold {
    fn handle(&self, req: Request) -> Response {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => Response::new(200, "text/html; charset=utf-8", PAGE),
            ("GET", "/brisvox-call.js") => {
                Response::new(200, "text/javascript; charset=utf-8", CALL_MODULE)
            }
            ("GET", "/healthz") => Response::new(200, "text/plain", "ok\n"),
            (_, path) if path.starts_with("/api/") => {
                let key = req
                    .header_key
                    .as_deref()
                    .or_else(|| req.query.get("key").map(String::as_str));
                if key != Some(self.access_key.as_str()) {
                    return Response::new(403, "application/json", "{\"error\":\"access key\"}");
                }
                match (req.method.as_str(), path) {
                    ("GET", "/api/config") => {
                        Response::new(200, "application/json", self.config_json())
                    }
                    ("GET", "/api/stats") => {
                        Response::new(200, "application/json", self.stats_json())
                    }
                    ("POST", "/api/load") => self.set_load(&req.query),
                    _ => Response::new(404, "application/json", "{\"error\":\"not found\"}"),
                }
            }
            _ => Response::new(404, "text/plain", "not found\n"),
        }
    }

    fn config_json(&self) -> String {
        let mut out = String::from("{\"targets\":[");
        for (i, t) in self.targets.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"name\":{},\"label\":{},\"call_url\":{},\"call_token\":{}}}",
                json_str(&t.name),
                json_str(&t.label),
                json_str(&t.call_url),
                json_str(&t.call_token)
            );
        }
        out.push_str("]}");
        out
    }

    fn set_load(&self, query: &BTreeMap<String, String>) -> Response {
        let Some(target) = query
            .get("target")
            .and_then(|name| self.targets.iter().find(|t| &t.name == name))
        else {
            return Response::new(400, "application/json", "{\"error\":\"unknown target\"}");
        };
        let Some(n) = query
            .get("n")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n <= MAX_CALLS)
        else {
            return Response::new(400, "application/json", "{\"error\":\"n: 0..=2000\"}");
        };
        let Ok(mut slots) = target.slots.lock() else {
            return Response::new(500, "application/json", "{\"error\":\"poisoned\"}");
        };
        while slots.len() > n {
            if let Some(slot) = slots.pop() {
                slot.cancel();
            }
        }
        let first = slots.len();
        let added = n - first;
        // New calls start spread out (20 ms apart, at most 10 s), as callers would.
        let spread = Duration::from_millis(20 * added as u64).min(Duration::from_secs(10));
        for i in 0..added {
            let token = self.cancel.child_token();
            tokio::spawn(slot(
                self.clock.clone(),
                Arc::clone(target),
                Arc::clone(&self.fixture),
                stagger(spread, i, added),
                token.clone(),
            ));
            slots.push(token);
        }
        eprintln!("hold: {} now holds {n} calls", target.name);
        Response::new(
            200,
            "application/json",
            format!("{{\"target\":{},\"n\":{n}}}", json_str(&target.name)),
        )
    }

    fn stats_json(&self) -> String {
        let now = self.clock.now();
        let mut out = format!("{{\"window_s\":{},\"targets\":[", WINDOW.as_secs());
        for (i, t) in self.targets.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let desired = t.slots.lock().map_or(0, |s| s.len());
            let in_call = t.in_call.load(Ordering::Relaxed);
            let (mut ok, mut failed, mut refused, mut late, mut frames, mut missed) =
                (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
            let mut v2v = Vec::new();
            let mut last_error = String::new();
            if let Ok(events) = t.events.lock() {
                for e in events
                    .iter()
                    .filter(|e| now.saturating_duration_since(e.at) <= WINDOW)
                {
                    match &e.kind {
                        EventKind::CallOk {
                            late_frames,
                            frames: f,
                        } => {
                            ok += 1;
                            late += late_frames;
                            frames += f;
                        }
                        EventKind::CallFailed(why) => {
                            failed += 1;
                            last_error.clone_from(why);
                        }
                        EventKind::CallRefused => refused += 1,
                        EventKind::Turn(Some(d)) => v2v.push(*d),
                        EventKind::Turn(None) => missed += 1,
                    }
                }
            }
            v2v.sort_unstable();
            let server = t
                .server
                .lock()
                .map_or_else(|_| "null".to_owned(), |s| s.clone());
            let _ = write!(
                out,
                "{{\"name\":{},\"desired\":{desired},\"in_call\":{in_call},\"calls_ok\":{ok},\
                 \"calls_failed\":{failed},\"calls_refused\":{refused},\"turns_answered\":{},\"turns_missed\":{missed},\
                 \"v2v_p50_ms\":{},\"v2v_p95_ms\":{},\"v2v_max_ms\":{},\"late_frames\":{late},\
                 \"frames\":{frames},\"last_error\":{},\"server\":{server}}}",
                json_str(&t.name),
                v2v.len(),
                pct_ms(&v2v, 50),
                pct_ms(&v2v, 95),
                pct_ms(&v2v, 100),
                json_str(&last_error),
            );
        }
        out.push_str("]}");
        out
    }
}

/// The `p`th percentile in ms (nearest rank), or `null`.
fn pct_ms(sorted: &[Duration], p: usize) -> String {
    if sorted.is_empty() {
        return "null".to_owned();
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1) - 1;
    sorted
        .get(rank.min(sorted.len() - 1))
        .map_or_else(|| "null".to_owned(), |d| d.as_millis().to_string())
}

/// One synthetic caller: dials, plays the fixture, hangs up, dials again, until cancelled.
async fn slot(
    clock: SharedClock,
    target: Arc<Target>,
    fixture: Arc<Fixture>,
    offset: Duration,
    cancel: CancellationToken,
) {
    tokio::select! {
        () = cancel.cancelled() => return,
        () = clock.sleep(offset) => {}
    }
    while !cancel.is_cancelled() {
        target.in_call.fetch_add(1, Ordering::Relaxed);
        let result = run_call(clock.clone(), &target.client, &fixture, &cancel).await;
        target.in_call.fetch_sub(1, Ordering::Relaxed);
        if cancel.is_cancelled() {
            // Hung up because N went down: not a failure.
            break;
        }
        let at = clock.now();
        let mut new = Vec::new();
        let pause = match result {
            Ok(outcome) => {
                let failure = outcome.failure.clone().or_else(|| {
                    (!outcome.server_errors.is_empty())
                        .then(|| format!("server error: {}", outcome.server_errors.join("; ")))
                });
                if let Some(why) = failure {
                    new.push(EventKind::CallFailed(why));
                    Duration::from_secs(1)
                } else {
                    for turn in &outcome.turns {
                        new.push(EventKind::Turn(match turn.outcome {
                            Outcome::NoResponse => None,
                            _ => Some(turn.v2v.unwrap_or_default()),
                        }));
                    }
                    new.push(EventKind::CallOk {
                        late_frames: outcome.late_frames,
                        frames: outcome.fixture_frames_sent,
                    });
                    Duration::from_millis(250)
                }
            }
            // A refused dial is retried after 2 s, like a caller who redials.
            Err(e) if e.to_string().contains("503") => {
                new.push(EventKind::CallRefused);
                Duration::from_secs(2)
            }
            Err(e) => {
                new.push(EventKind::CallFailed(e.to_string()));
                Duration::from_secs(1)
            }
        };
        if let Ok(mut events) = target.events.lock() {
            for kind in new {
                if events.len() >= MAX_EVENTS {
                    events.pop_front();
                }
                events.push_back(Event { at, kind });
            }
        }
        tokio::select! {
            () = cancel.cancelled() => break,
            () = clock.sleep(pause) => {}
        }
    }
}

/// `brisvox-bench container-stats`.
pub fn container_stats_cmd(mut args: impl Iterator<Item = String>) -> Result<bool, BenchError> {
    let mut bind = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--bind" => bind = Some(arg(&flag, args.next())?),
            _ => {
                return Err(BenchError::Config(format!(
                    "unknown flag {flag}\n{HOLD_USAGE}"
                )));
            }
        }
    }
    let addr = bind.unwrap_or_else(|| "[::]:9100".to_owned());
    runtime()?.block_on(async move {
        let clock = SystemClock::shared();
        let latest = Arc::new(Mutex::new("{}".to_owned()));
        tokio::spawn(sample_container(clock, Arc::clone(&latest)));
        let listener = TcpListener::bind(&addr).await?;
        eprintln!("container-stats: listening on {addr}");
        serve(listener, move |req| {
            let latest = Arc::clone(&latest);
            async move {
                match (req.method.as_str(), req.path.as_str()) {
                    ("GET", "/stats") => Response::new(
                        200,
                        "application/json",
                        latest
                            .lock()
                            .map_or_else(|_| "{}".to_owned(), |s| s.clone()),
                    ),
                    _ => Response::new(404, "text/plain", "not found\n"),
                }
            }
        })
        .await;
        Ok(true)
    })
}

/// cgroup v2 numbers of this container, when readable.
#[derive(Default)]
struct Cgroup {
    cpu_usage: Option<Duration>,
    memory: Option<u64>,
    cpu_limit: Option<f64>,
    memory_limit: Option<u64>,
}

fn read_cgroup() -> Cgroup {
    let read = |name: &str| std::fs::read_to_string(format!("/sys/fs/cgroup/{name}")).ok();
    let cpu_usage = read("cpu.stat").and_then(|text| {
        text.lines()
            .find_map(|l| l.strip_prefix("usage_usec "))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_micros)
    });
    let memory = read("memory.current").and_then(|v| v.trim().parse().ok());
    let cpu_limit = read("cpu.max").and_then(|v| {
        let mut parts = v.split_whitespace();
        let quota = parts.next()?.parse::<f64>().ok()?;
        let period = parts.next()?.parse::<f64>().ok()?;
        (period > 0.0).then(|| quota / period)
    });
    let memory_limit = read("memory.max").and_then(|v| v.trim().parse().ok());
    Cgroup {
        cpu_usage,
        memory,
        cpu_limit,
        memory_limit,
    }
}

/// Samples every second; CPU over the last 2 s.
async fn sample_container(clock: SharedClock, latest: Arc<Mutex<String>>) {
    let mut history: VecDeque<(Instant, Duration)> = VecDeque::new();
    let mut probe = TreeProbe::new(1, Scope::Tree);
    loop {
        let (cg, reading, probe_back) = match tokio::task::spawn_blocking(move || {
            let reading = probe.read();
            (read_cgroup(), reading, probe)
        })
        .await
        {
            Ok(v) => v,
            Err(_) => return,
        };
        probe = probe_back;
        let now = clock.now();
        let cpu_total = cg
            .cpu_usage
            .or_else(|| reading.map(|r| r.cpu_time))
            .unwrap_or_default();
        history.push_back((now, cpu_total));
        while history
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > Duration::from_secs(2))
            && history.len() > 2
        {
            history.pop_front();
        }
        let cores = history.front().map_or(0.0, |(at, cpu)| {
            let wall = now.saturating_duration_since(*at).as_secs_f64();
            if wall > 0.0 {
                cpu_total.saturating_sub(*cpu).as_secs_f64() / wall
            } else {
                0.0
            }
        });
        let opt = |v: Option<u64>| v.map_or_else(|| "null".to_owned(), |v| v.to_string());
        let json = format!(
            "{{\"cpu_cores\":{cores:.3},\"cpu_limit\":{},\"memory_bytes\":{},\"memory_limit\":{},\
             \"pss_bytes\":{},\"rss_bytes\":{},\"processes\":{},\"source\":{}}}",
            cg.cpu_limit
                .map_or_else(|| "null".to_owned(), |v| format!("{v:.2}")),
            opt(cg.memory.or_else(|| reading.and_then(|r| r.pss_bytes))),
            opt(cg.memory_limit),
            opt(reading.and_then(|r| r.pss_bytes)),
            opt(reading.map(|r| r.rss_bytes)),
            reading.map_or(0, |r| r.processes),
            json_str(if cg.cpu_usage.is_some() {
                "cgroup"
            } else {
                "processes"
            }),
        );
        if let Ok(mut l) = latest.lock() {
            *l = json;
        }
        clock.sleep(Duration::from_secs(1)).await;
    }
}

// --- the HTTP bits

struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    header_key: Option<String>,
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
        }
    }
}

async fn serve<F, Fut>(listener: TcpListener, handler: F)
where
    F: Fn(Request) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Response> + Send,
{
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            return;
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(CONNECTION_TIMEOUT, connection(stream, handler)).await;
        });
    }
}

async fn connection<F, Fut>(mut stream: TcpStream, handler: F) -> std::io::Result<()>
where
    F: Fn(Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        if n == 0 || head.len() + n > MAX_HEAD {
            return Ok(());
        }
        head.extend_from_slice(buf.get(..n).unwrap_or_default());
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_owned();
    let target = first.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let header_key = lines.find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("x-hold-key")
            .then(|| value.trim().to_owned())
    });
    let query = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let response = handler(Request {
        method,
        path: path.to_owned(),
        query,
        header_key,
    })
    .await;
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.shutdown().await
}

/// `s` as a JSON string literal.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let v: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(pct_ms(&v, 50), "50");
        assert_eq!(pct_ms(&v, 95), "95");
        assert_eq!(pct_ms(&v, 100), "100");
        assert_eq!(pct_ms(&[], 50), "null");
        assert_eq!(pct_ms(&[Duration::from_millis(7)], 95), "7");
    }
}
