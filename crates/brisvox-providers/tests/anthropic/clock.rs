//! Proof that every deadline of the adapter that is driven by the injected `Clock` really runs on it.
//!
//! The mock server speaks over real sockets, so tokio's paused time (and `SimClock`) cannot be
//! used. Instead the adapter gets a [`FastClock`]: a clock that runs `FACTOR` times faster than
//! real time. The deadlines are configured in "virtual" seconds (10 s, 20 s, 30 s, 40 s) and must
//! fire after a hundredth of that in real time. An adapter that read `Instant::now` or slept on
//! tokio's own timer directly would wait the full virtual time and fail these tests (and trip the
//! five-second guard of `collect`).
//!
//! Covered here: the response-headers deadline (`first byte`), the first-output, idle, tool-idle,
//! thinking and total-stream deadlines, the warm-up probe (`anthropic warm`), the bound on
//! reading an error body (`ERROR_BODY_TIMEOUT`, 1 s) and the bound on draining a finished
//! response (`DRAIN_TIMEOUT`, 0.5 s). The connect timeout is the one deadline not driven by the
//! clock: reqwest enforces it on a real socket, so it is checked against the refused connection
//! in `protocol.rs` instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use brisvox_core::clock::Clock;
use brisvox_core::{Error as CoreError, Secret, SharedClock, SystemClock};
use brisvox_providers::anthropic::{AnthropicConfig, AnthropicLlm};
use brisvox_providers::{Error, LlmEvent, LlmProvider, LlmRequest, LlmStream, Message, Role};
use tokio::time::{Instant, Sleep};

use crate::mock::{Mock, Script, Step};
use crate::wire;

/// How many times faster than real time the clock runs.
const FACTOR: u32 = 100;

/// A [`Clock`] that runs [`FACTOR`] times faster than real time and counts its uses.
#[derive(Debug)]
struct FastClock {
    real: SharedClock,
    origin: Instant,
    nows: AtomicUsize,
    sleeps: AtomicUsize,
}

impl FastClock {
    fn new() -> Arc<Self> {
        let real = SystemClock::shared();
        Arc::new(Self {
            origin: real.now(),
            real,
            nows: AtomicUsize::new(0),
            sleeps: AtomicUsize::new(0),
        })
    }
}

impl Clock for FastClock {
    fn now(&self) -> Instant {
        self.nows.fetch_add(1, Ordering::SeqCst);
        let elapsed = self.real.now().saturating_duration_since(self.origin);
        self.origin + elapsed * FACTOR
    }

    fn wall(&self) -> SystemTime {
        self.real.wall()
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        self.sleeps.fetch_add(1, Ordering::SeqCst);
        let virtual_left = deadline.saturating_duration_since(self.origin);
        self.real.sleep_until(self.origin + virtual_left / FACTOR)
    }
}

/// An adapter on a [`FastClock`], with all deadlines in virtual time: 10 s to the response
/// headers, 20 s to the first output, 10 s idle, 30 s idle inside a tool call, 40 s per response.
fn fast(mock: &Mock, tweak: impl FnOnce(&mut AnthropicConfig)) -> (AnthropicLlm, Arc<FastClock>) {
    let clock = FastClock::new();
    let mut config =
        AnthropicConfig::new(Secret::new("sk-ant-test".to_owned())).with_base_url(mock.base_url());
    config.connect_timeout = Duration::from_secs(3);
    config.first_byte_timeout = Duration::from_secs(10);
    config.first_output_timeout = Duration::from_secs(20);
    config.idle_timeout = Duration::from_secs(10);
    config.tool_idle_timeout = Duration::from_secs(30);
    config.max_stream_duration = Duration::from_secs(40);
    tweak(&mut config);
    let shared: SharedClock = clock.clone();
    (AnthropicLlm::new(config, shared).expect("provider"), clock)
}

fn user() -> LlmRequest {
    LlmRequest {
        messages: vec![Message::new(Role::User, "hi")],
        tools: Vec::new(),
    }
}

/// The events of a response; the whole response must take under 2.5 s of real time.
async fn collect_fast(mut stream: LlmStream) -> (Vec<LlmEvent>, Duration) {
    let start = std::time::Instant::now();
    let events = tokio::time::timeout(Duration::from_millis(2500), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("the deadline fired on the injected clock, not on real time");
    (events, start.elapsed())
}

fn timeout_of(events: &[LlmEvent]) -> Option<&'static str> {
    match events.last() {
        Some(LlmEvent::Error(Error::Core(CoreError::Timeout { what, .. }))) => Some(what),
        _ => None,
    }
}

#[tokio::test]
async fn the_first_output_deadline_runs_on_the_injected_clock() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(wire::message_start(10)),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let (llm, clock) = fast(&mock, |_| {});
    let (events, took) = collect_fast(llm.stream(user()).await.expect("accepted")).await;
    assert_eq!(timeout_of(&events), Some("anthropic first output"));
    // 20 virtual seconds are 200 ms of real time.
    assert!(took >= Duration::from_millis(150), "{took:?}");
    assert!(clock.sleeps.load(Ordering::SeqCst) > 0 && clock.nows.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn the_idle_deadline_runs_on_the_injected_clock() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(wire::text_start(0)),
        Step::event(wire::text_delta(0, "a")),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let (llm, _) = fast(&mock, |_| {});
    let (events, took) = collect_fast(llm.stream(user()).await.expect("accepted")).await;
    assert_eq!(timeout_of(&events), Some("anthropic idle"));
    // 10 virtual seconds are 100 ms of real time.
    assert!(took >= Duration::from_millis(70), "{took:?}");
}

#[tokio::test]
async fn the_tool_idle_deadline_runs_on_the_injected_clock() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(wire::tool_start(0, "toolu_1", "get_weather")),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let (llm, _) = fast(&mock, |_| {});
    let (events, took) = collect_fast(llm.stream(user()).await.expect("accepted")).await;
    assert_eq!(timeout_of(&events), Some("anthropic idle in a tool call"));
    // 30 virtual seconds are 300 ms: longer than the 100 ms of plain idle.
    assert!(took >= Duration::from_millis(250), "{took:?}");
}

#[tokio::test]
async fn the_thinking_allowance_runs_on_the_injected_clock() {
    let mock = Mock::start(false).await;
    mock.script(Script::stream(vec![
        Step::event(wire::thinking_start(0)),
        Step::Sleep(Duration::from_secs(30)),
    ]));
    let (llm, _) = fast(&mock, |_| {});
    let (events, took) = collect_fast(llm.stream(user()).await.expect("accepted")).await;
    assert_eq!(timeout_of(&events), Some("anthropic idle in thinking"));
    // 20 virtual seconds are 200 ms: longer than the 100 ms of plain idle.
    assert!(took >= Duration::from_millis(150), "{took:?}");
}

#[tokio::test]
async fn the_total_duration_cap_runs_on_the_injected_clock() {
    let mock = Mock::start(false).await;
    let mut steps = vec![Step::event(wire::text_start(0))];
    for _ in 0..60 {
        steps.push(Step::event(wire::text_delta(0, "x")));
        steps.push(Step::Sleep(Duration::from_millis(30)));
    }
    mock.script(Script::stream(steps));
    let (llm, _) = fast(&mock, |_| {});
    let (events, took) = collect_fast(llm.stream(user()).await.expect("accepted")).await;
    assert_eq!(timeout_of(&events), Some("anthropic stream"));
    // 40 virtual seconds are 400 ms; the content kept the idle deadline (100 ms) satisfied.
    assert!(took >= Duration::from_millis(300), "{took:?}");
}

#[tokio::test]
async fn the_response_headers_deadline_runs_on_the_injected_clock() {
    let mock = Mock::start(false).await;
    let mut script = Script::stream(Vec::new());
    script.pre_delay = Duration::from_secs(30);
    mock.script(script);
    let (llm, clock) = fast(&mock, |_| {});
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(2500), llm.stream(user()))
        .await
        .expect("the deadline fired on the injected clock, not on real time");
    assert!(matches!(
        result,
        Err(Error::Core(CoreError::Timeout {
            what: "anthropic first byte",
            ..
        }))
    ));
    // 10 virtual seconds are 100 ms.
    assert!(start.elapsed() >= Duration::from_millis(70));
    assert!(clock.sleeps.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn the_warm_up_deadline_runs_on_the_injected_clock() {
    // The server accepts the HEAD and never answers it.
    let mock = Mock::start(false).await;
    mock.delay_probes(Duration::from_secs(30));
    let (llm, clock) = fast(&mock, |_| {});
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(2500), llm.warm())
        .await
        .expect("the deadline fired on the injected clock, not on real time");
    assert!(matches!(
        result,
        Err(Error::Core(CoreError::Timeout {
            what: "anthropic warm",
            ..
        }))
    ));
    // 10 virtual seconds are 100 ms.
    assert!(start.elapsed() >= Duration::from_millis(70));
    assert!(clock.sleeps.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn the_error_body_bound_runs_on_the_injected_clock() {
    // The head announces 4 KiB, 60 bytes arrive, then silence. The 1 s bound is 10 ms on the
    // fast clock; on a real timer `stream()` would take a full second.
    let mock = Mock::start(false).await;
    let body = r#"{"type":"error","error":{"type":"overloaded_error"}}"#;
    mock.script(Script::status_then_stall(529, body, 4096));
    let (llm, clock) = fast(&mock, |_| {});
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(2500), llm.stream(user()))
        .await
        .expect("the stalled body was cut off");
    assert!(
        matches!(result, Err(Error::Rejected { status: 529, .. })),
        "{result:?}"
    );
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "the stalled body was read for {:?}",
        start.elapsed()
    );
    assert!(clock.sleeps.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn the_drain_bound_runs_on_the_injected_clock() {
    // After `message_stop` the server trickles bytes forever. The 0.5 s drain bound is 5 ms on
    // the fast clock; on a real timer the connection would stay open for half a second.
    let mock = Mock::start(true).await;
    let mut steps = vec![
        Step::event(wire::message_delta("end_turn", 7)),
        Step::event(wire::message_stop()),
    ];
    for _ in 0..80 {
        steps.push(Step::Sleep(Duration::from_millis(100)));
        steps.push(Step::counted("{}"));
    }
    mock.script(Script::stream(steps));
    let (llm, _) = fast(&mock, |_| {});
    let (events, _) = collect_fast(llm.stream(user()).await.expect("accepted")).await;
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));
    let start = std::time::Instant::now();
    while mock.closed_connections() == 0 && start.elapsed() < Duration::from_millis(2000) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mock.closed_connections(), 1, "the server was hung up on");
    assert!(
        start.elapsed() < Duration::from_millis(350),
        "the drain took {:?}",
        start.elapsed()
    );
}
