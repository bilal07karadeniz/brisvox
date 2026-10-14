//! The OTLP/HTTP trace exporter (feature `otlp`, off by default).
//!
//! Turn traces are posted as OTLP/HTTP with the JSON encoding (`POST /v1/traces`,
//! `Content-Type: application/json`) to a collector: the OpenTelemetry Collector, Grafana
//! Alloy, Jaeger, Tempo and most vendors' agents accept it on port 4318.
//!
//! # Why no OpenTelemetry SDK
//! The SDK crates pull in an HTTP/gRPC client stack of their own and a second `tracing`
//! bridge, to export spans that the engine's timeline already holds as finished intervals.
//! The wire format is a small JSON document, so the exporter is a few hundred lines on the
//! sockets and `serde_json` the server already has.
//!
//! # Delivery
//! [`TraceExporter::export`] only queues: a bounded channel (`BRISVOX_OTLP_QUEUE` traces; when
//! it is full the new trace is dropped and counted in `brisvox_traces_total{outcome="dropped"}`).
//! A background task batches up to `BRISVOX_OTLP_BATCH` traces or `BRISVOX_OTLP_FLUSH`, posts
//! them with one `BRISVOX_OTLP_TIMEOUT` deadline for connect, request and response, and drops
//! the batch when that fails (counted, logged at `warn`; telemetry never retries into a growing
//! backlog). It stops with its cancellation token after a last bounded flush.
//!
//! # Limits
//! Plain `http://` endpoints only: run a collector or agent next to the server (the usual
//! deployment) and let it carry the TLS and credentials to the backend. An `https://` endpoint is
//! a startup error.

use std::sync::Arc;
use std::time::Duration;

use brisvox_core::SharedClock;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::registry::TraceCounters;
use super::trace::{AttrValue, TraceExporter, TurnTrace};
use crate::config::Env;
use crate::settings::{count_var, duration_var, var};
use crate::{Error, Result};

/// Environment variable: the collector, `http://host:port[/path]` (default
/// `http://127.0.0.1:4318/v1/traces`).
pub const ENV_OTLP_ENDPOINT: &str = "BRISVOX_OTLP_ENDPOINT";
/// Environment variable: traces waiting for the exporter task (default 1024).
pub const ENV_OTLP_QUEUE: &str = "BRISVOX_OTLP_QUEUE";
/// Environment variable: most traces per request (default 64).
pub const ENV_OTLP_BATCH: &str = "BRISVOX_OTLP_BATCH";
/// Environment variable: longest wait before a partial batch is sent (default 2s).
pub const ENV_OTLP_FLUSH: &str = "BRISVOX_OTLP_FLUSH";
/// Environment variable: deadline of one request (default 5s).
pub const ENV_OTLP_TIMEOUT: &str = "BRISVOX_OTLP_TIMEOUT";
/// Environment variable: the `service.name` resource attribute (default `brisvox-server`).
pub const ENV_OTLP_SERVICE: &str = "BRISVOX_OTLP_SERVICE";

/// Largest response head read from the collector.
const MAX_RESPONSE_HEAD: usize = 8192;

/// Settings of an [`OtlpExporter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpConfig {
    /// Collector host.
    pub host: String,
    /// Collector port.
    pub port: u16,
    /// Request path.
    pub path: String,
    /// Traces waiting for the exporter task.
    pub queue: usize,
    /// Most traces per request.
    pub batch: usize,
    /// Longest wait before a partial batch is sent.
    pub flush: Duration,
    /// Deadline of one request.
    pub timeout: Duration,
    /// The `service.name` resource attribute.
    pub service: String,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 4318,
            path: "/v1/traces".to_owned(),
            queue: 1024,
            batch: 64,
            flush: Duration::from_secs(2),
            timeout: Duration::from_secs(5),
            service: "brisvox-server".to_owned(),
        }
    }
}

impl OtlpConfig {
    /// Sets host, port and path from `http://host[:port][/path]`.
    ///
    /// # Errors
    /// [`Error::Config`] for another scheme (`https` included, see the module docs), a missing
    /// host or a bad port.
    pub fn with_endpoint(mut self, endpoint: &str) -> Result<Self> {
        let bad = |why: &str| Error::Config(format!("{ENV_OTLP_ENDPOINT}: {endpoint:?} {why}"));
        let rest = endpoint.strip_prefix("http://").ok_or_else(|| {
            bad("must start with http:// (TLS is not supported: use a local collector)")
        })?;
        let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
        let port_of = |text: &str| text.parse::<u16>().map_err(|_| bad("has a bad port"));
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (host, after) = v6.split_once(']').ok_or_else(|| bad("has an unclosed ["))?;
            match after.strip_prefix(':') {
                Some(port) => (host, port_of(port)?),
                None if after.is_empty() => (host, 4318),
                None => return Err(bad("has text after the address")),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, port_of(port)?),
                None => (authority, 4318),
            }
        };
        if host.is_empty() {
            return Err(bad("has no host"));
        }
        self.host = host.to_owned();
        self.port = port;
        self.path = if path.is_empty() {
            "/v1/traces".to_owned()
        } else {
            path.to_owned()
        };
        Ok(self)
    }

    /// The `BRISVOX_OTLP_*` variables over the defaults.
    ///
    /// # Errors
    /// [`Error::Config`] naming the variable.
    pub fn from_env(env: &dyn Env) -> Result<Self> {
        let mut config = Self::default();
        if let Some(endpoint) = var(env, ENV_OTLP_ENDPOINT)? {
            config = config.with_endpoint(&endpoint)?;
        }
        if let Some(n) = count_var(env, ENV_OTLP_QUEUE, 1, 1_000_000)? {
            config.queue = n;
        }
        if let Some(n) = count_var(env, ENV_OTLP_BATCH, 1, 10_000)? {
            config.batch = n;
        }
        for (name, slot) in [
            (ENV_OTLP_FLUSH, &mut config.flush),
            (ENV_OTLP_TIMEOUT, &mut config.timeout),
        ] {
            if let Some(d) = duration_var(env, name)? {
                if d.is_zero() {
                    return Err(Error::Config(format!("{name}: must be longer than zero")));
                }
                *slot = d;
            }
        }
        if let Some(service) = var(env, ENV_OTLP_SERVICE)? {
            config.service = service;
        }
        Ok(config)
    }
}

/// Queues traces for the exporter task (see the [module docs](self)).
#[derive(Debug)]
pub struct OtlpExporter {
    tx: mpsc::Sender<TurnTrace>,
    counters: Arc<TraceCounters>,
}

impl OtlpExporter {
    /// Starts the exporter task on the current runtime.
    ///
    /// # Errors
    /// [`Error::Config`] outside a tokio runtime.
    pub fn spawn(
        config: &OtlpConfig,
        clock: SharedClock,
        cancel: CancellationToken,
        counters: Arc<TraceCounters>,
    ) -> Result<Arc<Self>> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Config("the OTLP exporter needs a tokio runtime".into()))?;
        let (tx, rx) = mpsc::channel(config.queue.max(1));
        let task = Task {
            config: config.clone(),
            clock,
            cancel,
            counters: Arc::clone(&counters),
        };
        runtime.spawn(task.run(rx));
        tracing::info!(
            target: "brisvox::telemetry",
            host = %config.host,
            port = config.port,
            path = %config.path,
            "OTLP trace exporter started"
        );
        Ok(Arc::new(Self { tx, counters }))
    }
}

impl TraceExporter for OtlpExporter {
    fn export(&self, trace: TurnTrace) {
        if self.tx.try_send(trace).is_err() {
            // Full (overflow policy: drop the newest) or the task has ended.
            self.counters.dropped.inc();
        }
    }
}

struct Task {
    config: OtlpConfig,
    clock: SharedClock,
    cancel: CancellationToken,
    counters: Arc<TraceCounters>,
}

impl Task {
    async fn run(self, mut rx: mpsc::Receiver<TurnTrace>) {
        let mut batch: Vec<TurnTrace> = Vec::with_capacity(self.config.batch);
        let mut deadline = None;
        loop {
            let wait = async {
                match deadline {
                    Some(at) => self.clock.sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                trace = rx.recv() => match trace {
                    Some(trace) => {
                        if batch.is_empty() {
                            deadline = Some(brisvox_core::clock::deadline_after(
                                self.clock.now(),
                                self.config.flush,
                            ));
                        }
                        batch.push(trace);
                        if batch.len() >= self.config.batch {
                            self.send(&mut batch).await;
                            deadline = None;
                        }
                    }
                    None => break,
                },
                () = wait => {
                    self.send(&mut batch).await;
                    deadline = None;
                }
            }
        }
        // Last flush: whatever is queued, once, under the request deadline.
        while let Ok(trace) = rx.try_recv() {
            batch.push(trace);
            if batch.len() >= self.config.batch {
                self.send(&mut batch).await;
            }
        }
        self.send(&mut batch).await;
    }

    /// Posts `batch` (emptying it); a failure drops it.
    async fn send(&self, batch: &mut Vec<TurnTrace>) {
        if batch.is_empty() {
            return;
        }
        let traces = std::mem::take(batch);
        let body = encode_batch(&self.config.service, &traces);
        let posted = tokio::select! {
            biased;
            posted = post(&self.config, body.as_bytes()) => posted,
            () = self.clock.sleep(self.config.timeout) => Err(format!(
                "no answer within {} ms",
                self.config.timeout.as_millis()
            )),
        };
        match posted {
            Ok(()) => self.counters.batches_sent.inc(),
            Err(why) => {
                self.counters.batches_failed.inc();
                self.counters
                    .dropped
                    .add(u64::try_from(traces.len()).unwrap_or(u64::MAX));
                tracing::warn!(
                    target: "brisvox::telemetry",
                    traces = traces.len(),
                    error = %why,
                    "OTLP trace batch dropped"
                );
            }
        }
    }
}

/// One `POST` with `Connection: close`; `Ok` for a 2xx answer.
async fn post(config: &OtlpConfig, body: &[u8]) -> std::result::Result<(), String> {
    let mut stream = TcpStream::connect((config.host.as_str(), config.port))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: brisvox-server/{}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        config.path,
        config.host,
        config.port,
        crate::VERSION,
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    stream
        .write_all(body)
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut answer = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    while answer.len() < MAX_RESPONSE_HEAD && !answer.windows(2).any(|w| w == b"\r\n") {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        answer.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
    let line = String::from_utf8_lossy(&answer);
    let status = line
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "no HTTP status line".to_owned())?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("collector answered {status}"))
    }
}

fn attribute(key: &str, value: &AttrValue) -> Value {
    let value = match value {
        AttrValue::Str(s) => json!({ "stringValue": &**s }),
        // OTLP/JSON writes 64-bit integers as strings.
        AttrValue::Int(i) => json!({ "intValue": i.to_string() }),
        AttrValue::Bool(b) => json!({ "boolValue": b }),
    };
    json!({ "key": key, "value": value })
}

/// The OTLP/JSON `ExportTraceServiceRequest` for `traces`.
pub fn encode_batch(service: &str, traces: &[TurnTrace]) -> String {
    let spans: Vec<Value> = traces
        .iter()
        .flat_map(|trace| &trace.spans)
        .map(|span| {
            let mut v = json!({
                "traceId": span.trace_id.to_hex(),
                "spanId": span.span_id.to_hex(),
                "name": span.name,
                // SPAN_KIND_INTERNAL
                "kind": 1,
                "startTimeUnixNano": span.start_unix_nanos.to_string(),
                "endTimeUnixNano": span.end_unix_nanos.to_string(),
                "attributes": span.attributes.iter().map(|(k, v)| attribute(k, v)).collect::<Vec<_>>(),
            });
            if let (Some(parent), Some(obj)) = (span.parent, v.as_object_mut()) {
                obj.insert("parentSpanId".to_owned(), json!(parent.to_hex()));
            }
            v
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [attribute("service.name", &AttrValue::Str(Arc::from(service)))]
            },
            "scopeSpans": [{
                "scope": { "name": "brisvox", "version": crate::VERSION },
                "spans": spans,
            }],
        }],
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing() {
        let c = OtlpConfig::default()
            .with_endpoint("http://collector.internal:4319/custom/traces")
            .unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.path.as_str()),
            ("collector.internal", 4319, "/custom/traces")
        );
        let c = OtlpConfig::default().with_endpoint("http://otel").unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.path.as_str()),
            ("otel", 4318, "/v1/traces")
        );
        let c = OtlpConfig::default()
            .with_endpoint("http://[::1]:4318")
            .unwrap();
        assert_eq!(c.host, "::1");
        for bad in [
            "https://otel:4318",
            "otel:4318",
            "http://",
            "http://h:port",
            "http://h:99999",
        ] {
            assert!(OtlpConfig::default().with_endpoint(bad).is_err(), "{bad}");
        }
    }
}
