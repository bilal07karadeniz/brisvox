//! Tests of the connection supervisor and the tool call path on the simulated clock: a fake
//! connector serves an in-memory MCP server, so no process, socket or real time is involved and
//! the delays are exact.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::context::ToolCallId;
use brisvox_agent::tools::{
    Confirmations, DEFAULT_CONFIRM_TTL, SessionId, Tool, ToolCallCtx, ToolOutput,
};
use brisvox_core::{SharedClock, SimClock, TurnEpoch};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool as McpToolDef,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{ClientServiceExt as _, ErrorData, RoleServer, ServerHandler, ServiceExt as _};
use serde_json::json;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::server::{
    Connect, McpLinkState, OpenFuture, Opened, Server, client_config, lifecycle, restart_delay,
};
use super::tool::{McpTool, SIDE_EFFECT_WAIT};

/// An MCP server with `echo` (answers `pong`) and `hang` (never answers).
#[derive(Clone)]
struct Handler;

impl ServerHandler for Handler {
    #[allow(deprecated)]
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = Arc::new(JsonObject::new());
        Ok(ListToolsResult::with_all_items(vec![
            McpToolDef::new("echo", "Answers pong.", Arc::clone(&schema)),
            McpToolDef::new("hang", "Never answers.", schema),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name == "hang" {
            std::future::pending::<()>().await;
        }
        Ok(CallToolResult::success(vec![ContentBlock::text("pong")]).into())
    }
}

/// The fake world: counts and times the connection attempts, and fails them on demand.
struct Fake {
    clock: SharedClock,
    opens: AtomicU32,
    /// Attempts numbered below this fail.
    fail_until: AtomicU32,
    /// Attempts never finish.
    hang: AtomicBool,
    open_times: Mutex<Vec<Instant>>,
    server_side: Mutex<Option<RunningService<RoleServer, Handler>>>,
}

impl Fake {
    fn new(clock: SharedClock) -> Arc<Self> {
        Arc::new(Self {
            clock,
            opens: AtomicU32::new(0),
            fail_until: AtomicU32::new(0),
            hang: AtomicBool::new(false),
            open_times: Mutex::new(Vec::new()),
            server_side: Mutex::new(None),
        })
    }

    /// The server side of the current connection goes away, as a crashed process does.
    async fn kill(&self) {
        let service = self
            .server_side
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(service) = service {
            let _ = service.cancel().await;
        }
    }

    fn times(&self) -> Vec<Instant> {
        self.open_times
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct FakeConnect(Arc<Fake>);

impl Connect for FakeConnect {
    fn connect(&self) -> OpenFuture<'_> {
        let fake = &self.0;
        Box::pin(async move {
            let attempt = fake.opens.fetch_add(1, Ordering::SeqCst);
            fake.open_times
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(fake.clock.now());
            if fake.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if attempt < fake.fail_until.load(Ordering::SeqCst) {
                return Err("connection refused".to_owned());
            }
            let (client_io, server_io) = tokio::io::duplex(1 << 16);
            // The server finishes its start when the first real request arrives, so the client
            // lists the tools while it starts.
            let client_side = async {
                let service = client_config()
                    .serve_with_lifecycle(client_io, lifecycle())
                    .await
                    .map_err(|e| e.to_string())?;
                let tools = service
                    .peer()
                    .list_all_tools()
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<_, String>((service, tools))
            };
            let (client, server) = tokio::join!(client_side, Handler.serve(server_io));
            let (service, tools) = client?;
            *fake
                .server_side
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(server.map_err(|e| e.to_string())?);
            Ok(Opened { service, tools })
        })
    }
}

struct World {
    sim: SimClock,
    clock: SharedClock,
    fake: Arc<Fake>,
    server: Arc<Server>,
    cancel: CancellationToken,
    supervisor: JoinHandle<()>,
}

/// A connected server on a simulated clock, with `max_in_flight` permits.
async fn world(max_in_flight: usize) -> World {
    let sim = SimClock::new();
    let clock: SharedClock = Arc::new(sim);
    let fake = Fake::new(clock.clone());
    let server = Arc::new(Server::with_connector(
        Arc::from("fake"),
        Box::new(FakeConnect(Arc::clone(&fake))),
        Duration::from_secs(30),
        vec!["echo".to_owned(), "hang".to_owned()],
        max_in_flight,
        clock.clone(),
    ));
    let opened = server.open().await.unwrap();
    server.mark_up(&opened.service);
    let cancel = CancellationToken::new();
    let supervisor = tokio::spawn(Arc::clone(&server).supervise(opened.service, cancel.clone()));
    World {
        sim,
        clock,
        fake,
        server,
        cancel,
        supervisor,
    }
}

fn tool(world: &World, name: &str, side_effect: bool, timeout: Duration) -> Arc<McpTool> {
    Arc::new(McpTool::new(
        Arc::clone(&world.server),
        name.to_owned(),
        side_effect,
        timeout,
        world.clock.clone(),
    ))
}

fn ctx(clock: &SharedClock) -> ToolCallCtx {
    let epoch = TurnEpoch::new(1);
    ToolCallCtx {
        session: SessionId::new(1),
        epoch,
        call: ToolCallId::new(epoch, 0),
        cancel: CancellationToken::new(),
        confirmations: Arc::new(Confirmations::new(clock.clone(), DEFAULT_CONFIRM_TTL)),
    }
}

async fn call(
    tool: &Arc<McpTool>,
    clock: &SharedClock,
) -> Result<ToolOutput, brisvox_agent::tools::ToolError> {
    tool.call(json!({}), ctx(clock)).await
}

#[tokio::test(start_paused = true)]
async fn a_lost_connection_is_retried_without_end_at_the_capped_delay() {
    let w = world(8).await;
    assert_eq!(w.server.state(), McpLinkState::Up);
    // Twelve refusals in a row, more than the ten attempts the supervisor once allowed itself.
    let opens = w.fake.opens.load(Ordering::SeqCst);
    w.fake.fail_until.store(opens + 12, Ordering::SeqCst);
    w.fake.kill().await;
    while w.server.state() != McpLinkState::Up {
        w.clock.sleep(Duration::from_secs(1)).await;
        assert!(
            w.sim.elapsed() < Duration::from_secs(600),
            "never came back"
        );
    }
    assert_eq!(w.server.restarts(), 1);
    // Attempt k comes restart_delay(k) after the one before it: 250 ms, 500 ms ... capped at 10 s.
    let times = w.fake.times();
    let attempts = times.get(1..).unwrap();
    assert_eq!(
        attempts.len(),
        13,
        "twelve refused, the thirteenth connected"
    );
    let gaps: Vec<Duration> = attempts
        .windows(2)
        .map(|pair| pair[1].saturating_duration_since(pair[0]))
        .collect();
    let expected: Vec<Duration> = (2..=13).map(restart_delay).collect();
    assert_eq!(gaps, expected);
    assert_eq!(*gaps.last().unwrap(), Duration::from_secs(10));
    // And the tools work again.
    let echo = tool(&w, "echo", false, Duration::from_secs(5));
    assert_eq!(
        call(&echo, &w.clock).await.unwrap(),
        ToolOutput::Text("pong".into())
    );
    w.cancel.cancel();
    w.supervisor.await.unwrap();
    assert_eq!(w.server.state(), McpLinkState::Down);
}

#[tokio::test(start_paused = true)]
async fn a_connection_that_never_answers_ends_at_the_startup_timeout() {
    let w = world(8).await;
    w.fake.hang.store(true, Ordering::SeqCst);
    let started = w.sim.elapsed();
    let error = w.server.open().await.err().unwrap();
    assert!(error.contains("startup timeout of 30000 ms"), "{error}");
    assert_eq!(w.sim.elapsed() - started, Duration::from_secs(30));
    w.cancel.cancel();
    w.supervisor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_call_that_is_never_answered_ends_as_an_unknown_outcome() {
    let w = world(8).await;
    let hang = tool(&w, "hang", true, Duration::from_secs(5));
    let started = w.sim.elapsed();
    let error = call(&hang, &w.clock).await.unwrap_err();
    assert!(
        error.message().contains("did not answer in time"),
        "{error}"
    );
    assert!(
        error.message().contains("the outcome is unknown"),
        "{error}"
    );
    // It waited past its slot's timeout, up to the side-effect wait.
    assert_eq!(w.sim.elapsed() - started, SIDE_EFFECT_WAIT);
    w.cancel.cancel();
    w.supervisor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_read_only_call_has_its_own_backstop_a_little_after_the_engines_timeout() {
    let w = world(8).await;
    let hang = tool(&w, "hang", false, Duration::from_secs(2));
    let started = w.sim.elapsed();
    let error = call(&hang, &w.clock).await.unwrap_err();
    assert!(
        error.message().contains("did not answer in time"),
        "{error}"
    );
    assert!(!error.message().contains("unknown"), "{error}");
    assert_eq!(
        w.sim.elapsed() - started,
        Duration::from_secs(2) + Duration::from_millis(500)
    );
    w.cancel.cancel();
    w.supervisor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn calls_over_the_in_flight_limit_are_refused_before_anything_is_sent() {
    let w = world(2).await;
    let hang = tool(&w, "hang", false, Duration::from_secs(10));
    let mut busy_calls = Vec::new();
    for _ in 0..2 {
        let (hang, clock) = (Arc::clone(&hang), w.clock.clone());
        busy_calls.push(tokio::spawn(async move { call(&hang, &clock).await }));
    }
    w.clock.sleep(Duration::from_millis(10)).await;

    // Both kinds of tool are refused at once, and the message says nothing ran.
    for side_effect in [false, true] {
        let echo = tool(&w, "echo", side_effect, Duration::from_secs(5));
        let before = w.sim.elapsed();
        let error = call(&echo, &w.clock).await.unwrap_err();
        assert!(
            error.message().contains("is busy (2 calls in flight)"),
            "{error}"
        );
        assert!(error.message().contains("the call was not made"), "{error}");
        assert!(!error.message().contains("unknown"), "{error}");
        assert_eq!(w.sim.elapsed(), before, "refused without waiting");
    }

    // A call that ends frees its permit; so does one whose future is dropped.
    busy_calls.pop().unwrap().abort();
    w.clock.sleep(Duration::from_millis(10)).await;
    let echo = tool(&w, "echo", false, Duration::from_secs(5));
    assert_eq!(
        call(&echo, &w.clock).await.unwrap(),
        ToolOutput::Text("pong".into())
    );
    // The other hangs until its backstop, then the server is idle again.
    let first = busy_calls.pop().unwrap().await.unwrap();
    assert!(first.is_err());
    for _ in 0..2 {
        assert!(call(&echo, &w.clock).await.is_ok());
    }
    w.cancel.cancel();
    w.supervisor.await.unwrap();
}
