//! Tests of the tool engine: registry and schema validation, parallel execution, ordering,
//! timeouts, interruption, idempotency, unknown tools, the retry budget, speculation and per-request
//! LLM metrics. Paused time with `SimClock`, so every duration below is exact.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_agent::Assembled;
use brisvox_agent::assemble::{CallError, Delta, InvalidCall, ResponseAssembler};
use brisvox_agent::context::{Context, Message, ToolCall, ToolCallId};
use brisvox_agent::tools::{
    EngineConfig, Execution, IdempotencyKey, RegistryError, SessionId, Tool, ToolCallCtx,
    ToolEngine, ToolError, ToolFuture, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_core::{
    LlmRoundMetrics, SharedClock, SimClock, SystemClock, TimelineRecorder, TurnEpoch, TurnEvent,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

/// A tool that waits `delay` on the injected clock, records when it finished, and returns its
/// arguments. Counts its runs.
#[derive(Clone)]
struct Probe {
    clock: SharedClock,
    label: &'static str,
    delay: Duration,
    runs: Arc<AtomicUsize>,
    finished: Arc<Mutex<Vec<&'static str>>>,
    fail: bool,
    bulk: usize,
}

impl Probe {
    fn new(clock: &SharedClock, label: &'static str, delay: Duration) -> Self {
        Self {
            clock: Arc::clone(clock),
            label,
            delay,
            runs: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(Mutex::new(Vec::new())),
            fail: false,
            bulk: 0,
        }
    }

    /// Shares a completion log with other probes.
    fn logging_to(mut self, log: &Arc<Mutex<Vec<&'static str>>>) -> Self {
        self.finished = Arc::clone(log);
        self
    }

    /// A probe whose call fails.
    fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    /// A probe whose result is `bytes` long.
    fn bulky(mut self, bytes: usize) -> Self {
        self.bulk = bytes;
        self
    }

    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    fn finished(&self) -> Vec<&'static str> {
        self.finished.lock().unwrap().clone()
    }
}

impl Tool for Probe {
    fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.clock.sleep(self.delay).await;
            self.finished.lock().unwrap().push(self.label);
            if self.fail {
                return Err(ToolError::new("upstream said no"));
            }
            if self.bulk > 0 {
                return Ok(ToolOutput::Text("x".repeat(self.bulk)));
            }
            Ok(ToolOutput::Json(
                json!({ "label": self.label, "args": args, "ran": true }),
            ))
        })
    }
}

fn clock() -> SharedClock {
    Arc::new(SimClock::new())
}

fn epoch(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

fn weather_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "city": { "type": "string", "minLength": 1, "maxLength": 40 },
            "days": { "type": "integer", "minimum": 1, "maximum": 7 },
            "unit": { "enum": ["c", "f"] },
            "filter": {
                "type": "object",
                "properties": { "tags": { "type": "array", "items": { "type": "string" } } },
                "required": ["tags"],
                "additionalProperties": false
            }
        },
        "required": ["city"],
        "additionalProperties": false
    })
}

fn any_params() -> Value {
    json!({ "type": "object" })
}

fn book_params() -> Value {
    json!({
        "type": "object",
        "properties": { "table": { "type": "string" } },
        "required": ["table"],
        "additionalProperties": false
    })
}

fn spec(name: &str, parameters: Value, kind: ToolKind) -> ToolSpec {
    ToolSpec::new(name, format!("the {name} tool"), parameters, kind)
}

/// An engine over `tools`, each registered under its spec.
fn engine(clock: &SharedClock, tools: Vec<(ToolSpec, Probe)>, config: EngineConfig) -> ToolEngine {
    let mut registry = ToolRegistry::new();
    for (spec, probe) in tools {
        registry.register(spec, probe).unwrap();
    }
    ToolEngine::new(
        Arc::new(registry),
        SessionId::new(7),
        Arc::clone(clock),
        config,
    )
}

/// A complete call with the given sequence number in turn `turn`.
fn call(turn: u32, seq: u32, name: &str, arguments: Value) -> Assembled {
    Assembled::Complete(ToolCall {
        id: ToolCallId::new(epoch(turn), seq),
        provider_id: Some(Arc::from(format!("provider_{turn}_{seq}"))),
        name: Arc::from(name),
        arguments,
        provider_data: None,
    })
}

/// A call the assembler could not parse (the stream was cut off).
fn unparseable(turn: u32, seq: u32, name: &str) -> Assembled {
    Assembled::Invalid(InvalidCall {
        index: seq,
        id: ToolCallId::new(epoch(turn), seq),
        provider_id: None,
        name: Some(Arc::from(name)),
        raw_arguments: Arc::from("{\"city\": \"Anka"),
        provider_data: None,
        error: CallError::InvalidJson("EOF while parsing a string".to_owned()),
    })
}

/// The assistant message the response with these calls produces (as the assembler records it).
fn assistant_for(calls: &[Assembled]) -> Message {
    let tool_calls = calls
        .iter()
        .map(|call| match call {
            Assembled::Complete(call) => call.clone(),
            Assembled::Invalid(invalid) => ToolCall {
                id: invalid.id.clone(),
                provider_id: invalid.provider_id.clone(),
                name: invalid
                    .name
                    .clone()
                    .unwrap_or_else(|| Arc::from("unknown_tool")),
                arguments: json!({}),
                provider_data: None,
            },
        })
        .collect();
    Message::assistant_with_calls("", tool_calls)
}

fn fresh_context() -> Context {
    let mut context = Context::with_system("You are a voice assistant.");
    context.push(Message::user("Check the weather and book a table."));
    context
}

/// Runs `calls` for turn `turn` with a fresh cancellation token, in a committed run. The
/// execution counts as delivered (the test reads it as the LLM would); a test of a discarded
/// execution calls the engine itself.
async fn execute(engine: &ToolEngine, calls: Vec<Assembled>, turn: u32) -> Execution {
    let cancel = CancellationToken::new();
    let mut execution = tokio::time::timeout(
        Duration::from_secs(600),
        engine.execute(calls, epoch(turn), &cancel),
    )
    .await
    .expect("the engine must not hang");
    execution.mark_delivered();
    execution
}

/// One LLM round: pushes the response's assistant message, runs its calls, appends the results,
/// and checks that the context is still valid.
async fn round(
    engine: &ToolEngine,
    context: &mut Context,
    calls: Vec<Assembled>,
    turn: u32,
) -> Execution {
    context.push(assistant_for(&calls));
    let mut execution = execute(engine, calls, turn).await;
    execution.append_to(context);
    context.validate().expect("context must stay valid");
    execution
}

/// Parses the content of the `index`-th result: (is_error, JSON body).
fn result(execution: &Execution, index: usize) -> (bool, Value) {
    match &execution.results[index] {
        Message::Tool {
            content, is_error, ..
        } => (
            *is_error,
            serde_json::from_str(content).unwrap_or_else(|_| Value::String(content.to_string())),
        ),
        other => panic!("not a tool result: {other:?}"),
    }
}

fn error_kind(execution: &Execution, index: usize) -> String {
    let (is_error, body) = result(execution, index);
    assert!(is_error, "result {index} should be an error");
    body["error"].as_str().unwrap().to_owned()
}

fn ok_body(execution: &Execution, index: usize) -> Value {
    let (is_error, body) = result(execution, index);
    assert!(!is_error, "result {index} should succeed: {body}");
    body
}

#[tokio::test(start_paused = true)]
async fn registry_rejects_duplicates_bad_names_bad_schemas_and_zero_timeouts() {
    let clock = clock();
    let mut registry = ToolRegistry::new();
    let probe = || Probe::new(&clock, "p", Duration::ZERO);
    registry
        .register(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            probe(),
        )
        .unwrap();
    assert_eq!(
        registry.register(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            probe()
        ),
        Err(RegistryError::Duplicate("weather".to_owned()))
    );
    let long = "x".repeat(65);
    for bad in ["", "bad name", "dots.are.bad", long.as_str()] {
        assert!(
            matches!(
                registry.register(spec(bad, any_params(), ToolKind::ReadOnly), probe()),
                Err(RegistryError::InvalidName(_))
            ),
            "name {bad:?}"
        );
    }
    // Parameters must be an object schema, and must compile.
    for bad_schema in [
        json!({ "type": "array" }),
        json!({ "properties": {} }),
        json!("not a schema"),
        json!({ "type": "object", "properties": { "x": { "type": "nonsense" } } }),
    ] {
        assert!(
            matches!(
                registry.register(spec("bad", bad_schema.clone(), ToolKind::ReadOnly), probe()),
                Err(RegistryError::InvalidSchema { .. })
            ),
            "schema {bad_schema}"
        );
    }
    // A remote reference cannot be resolved without network features: rejected at registration.
    let remote = json!({
        "type": "object",
        "properties": { "x": { "$ref": "https://example.com/schema.json" } }
    });
    assert!(
        matches!(
            registry.register(spec("remote", remote, ToolKind::ReadOnly), probe()),
            Err(RegistryError::InvalidSchema { .. })
        ),
        "remote $ref must not compile offline"
    );
    assert_eq!(
        registry.register(
            spec("slow", any_params(), ToolKind::ReadOnly).with_timeout(Duration::ZERO),
            probe()
        ),
        Err(RegistryError::ZeroTimeout("slow".to_owned()))
    );
    // Nothing rejected was added; specs come out in name order.
    registry
        .register(spec("alpha", any_params(), ToolKind::ReadOnly), probe())
        .unwrap();
    let names: Vec<&str> = registry.specs().map(|s| &*s.name).collect();
    assert_eq!(names, ["alpha", "weather"]);
    assert_eq!(registry.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn valid_arguments_run_and_the_result_is_returned() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let exec = round(
        &engine,
        &mut context,
        vec![call(
            1,
            0,
            "weather",
            json!({ "city": "Ankara", "days": 2, "unit": "c" }),
        )],
        1,
    )
    .await;
    assert!(!exec.budget_exhausted);
    let body = ok_body(&exec, 0);
    assert_eq!(body["args"]["city"], "Ankara");
    assert_eq!(body["ran"], true);
    assert_eq!(weather.runs(), 1);
    assert_eq!(engine.invalid_rounds(epoch(1)), 0);
}

#[tokio::test(start_paused = true)]
async fn schema_validation_reports_missing_wrong_extra_and_nested_problems() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();

    // Missing required property: reported at the root.
    let exec = round(
        &engine,
        &mut context,
        vec![call(1, 0, "weather", json!({ "days": 2 }))],
        1,
    )
    .await;
    assert_eq!(error_kind(&exec, 0), "invalid_arguments");
    assert_eq!(
        engine.invalid_rounds(epoch(1)),
        1,
        "a schema failure spends a retry round"
    );
    let (_, body) = result(&exec, 0);
    assert_eq!(body["tool"], "weather");
    assert_eq!(body["issues"][0]["path"], "(root)");
    assert!(
        body["issues"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("city")
    );

    // Wrong type at a property.
    let args = json!({ "city": "Ankara", "days": "two" });
    let exec = round(&engine, &mut context, vec![call(2, 0, "weather", args)], 2).await;
    let (_, body) = result(&exec, 0);
    assert_eq!(body["issues"][0]["path"], "/days");
    assert!(
        body["issues"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not of type")
    );

    // Out of range and outside the enum.
    let args = json!({ "city": "Ankara", "days": 9, "unit": "k" });
    let exec = round(&engine, &mut context, vec![call(3, 0, "weather", args)], 3).await;
    let (_, body) = result(&exec, 0);
    let paths: Vec<&str> = body["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["/days", "/unit"]);

    // Extra property under additionalProperties: false.
    let args = json!({ "city": "Ankara", "zip": "06100" });
    let exec = round(&engine, &mut context, vec![call(4, 0, "weather", args)], 4).await;
    let (_, body) = result(&exec, 0);
    assert!(
        body["issues"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("zip")
    );

    // Nested: the second tag is a number.
    let args = json!({ "city": "Ankara", "filter": { "tags": ["a", 3] } });
    let exec = round(&engine, &mut context, vec![call(5, 0, "weather", args)], 5).await;
    let (_, body) = result(&exec, 0);
    assert_eq!(body["issues"][0]["path"], "/filter/tags/1");

    // Nested object with a missing required property.
    let args = json!({ "city": "Ankara", "filter": {} });
    let exec = round(&engine, &mut context, vec![call(6, 0, "weather", args)], 6).await;
    let (_, body) = result(&exec, 0);
    assert_eq!(body["issues"][0]["path"], "/filter");
    assert!(
        body["issues"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("tags")
    );

    // Invalid calls never reach the tool.
    assert_eq!(weather.runs(), 0);
}

#[tokio::test(start_paused = true)]
async fn validation_reports_are_bounded() {
    let clock = clock();
    let tags = Probe::new(&clock, "tags", Duration::ZERO);
    let params = json!({
        "type": "object",
        "properties": { "ids": { "type": "array", "items": { "type": "string" } } }
    });
    let engine = engine(
        &clock,
        vec![(spec("tags", params, ToolKind::ReadOnly), tags.clone())],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let huge: Vec<u32> = (0..50).collect();
    let exec = round(
        &engine,
        &mut context,
        vec![call(1, 0, "tags", json!({ "ids": huge }))],
        1,
    )
    .await;
    let (_, body) = result(&exec, 0);
    assert_eq!(
        body["issues"].as_array().unwrap().len(),
        5,
        "at most five issues"
    );
    assert_eq!(body["truncated"], true);
    assert!(
        body.to_string().len() < 2_048,
        "the report stays small: {body}"
    );
    assert_eq!(tags.runs(), 0);
}

#[tokio::test(start_paused = true)]
async fn parallel_calls_take_the_longest_duration_not_the_sum() {
    let clock = clock();
    let a = Probe::new(&clock, "a", 100 * MS);
    let b = Probe::new(&clock, "b", 100 * MS);
    let engine = engine(
        &clock,
        vec![
            (spec("a", any_params(), ToolKind::ReadOnly), a.clone()),
            (spec("b", any_params(), ToolKind::ReadOnly), b.clone()),
        ],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let start = clock.now();
    let exec = round(
        &engine,
        &mut context,
        vec![call(1, 0, "a", json!({})), call(1, 1, "b", json!({}))],
        1,
    )
    .await;
    assert_eq!(clock.now().duration_since(start), 100 * MS);
    assert_eq!(exec.results.len(), 2);
    assert_eq!((a.runs(), b.runs()), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn results_follow_the_issue_order_even_when_completion_order_differs() {
    let clock = clock();
    let log = Arc::new(Mutex::new(Vec::new()));
    let slow = Probe::new(&clock, "slow", 200 * MS).logging_to(&log);
    let fast = Probe::new(&clock, "fast", 50 * MS).logging_to(&log);
    let engine = engine(
        &clock,
        vec![
            (spec("slow", any_params(), ToolKind::ReadOnly), slow),
            (spec("fast", any_params(), ToolKind::ReadOnly), fast),
        ],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let exec = round(
        &engine,
        &mut context,
        vec![call(1, 0, "slow", json!({})), call(1, 1, "fast", json!({}))],
        1,
    )
    .await;
    // The fast call finished first ...
    assert_eq!(*log.lock().unwrap(), ["fast", "slow"]);
    // ... but its result is second, in the order the LLM issued the calls.
    assert_eq!(ok_body(&exec, 0)["label"], "slow");
    assert_eq!(ok_body(&exec, 1)["label"], "fast");
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_exceeds_its_timeout_yields_a_timeout_error() {
    let clock = clock();
    let slow = Probe::new(&clock, "slow", Duration::from_secs(10));
    let engine = engine(
        &clock,
        vec![(
            spec("slow", any_params(), ToolKind::ReadOnly).with_timeout(Duration::from_secs(1)),
            slow.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let start = clock.now();
    let exec = round(
        &engine,
        &mut context,
        vec![call(1, 0, "slow", json!({}))],
        1,
    )
    .await;
    assert_eq!(clock.now().duration_since(start), Duration::from_secs(1));
    assert_eq!(error_kind(&exec, 0), "timeout");
    assert_eq!(slow.runs(), 1);
    assert!(
        slow.finished().is_empty(),
        "the timed-out future was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_tool_yields_a_tool_failed_error_with_its_message() {
    let clock = clock();
    let flaky = Probe::new(&clock, "flaky", Duration::ZERO).failing();
    let engine = engine(
        &clock,
        vec![(spec("flaky", any_params(), ToolKind::ReadOnly), flaky)],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let exec = round(
        &engine,
        &mut context,
        vec![call(1, 0, "flaky", json!({}))],
        1,
    )
    .await;
    assert_eq!(error_kind(&exec, 0), "tool_failed");
    assert_eq!(result(&exec, 0).1["detail"], "upstream said no");
}

#[tokio::test(start_paused = true)]
async fn oversized_results_become_error_results() {
    let clock = clock();
    let big = Probe::new(&clock, "big", Duration::ZERO).bulky(1_000);
    let config = EngineConfig {
        max_result_bytes: 100,
        ..EngineConfig::default()
    };
    let engine = engine(
        &clock,
        vec![(spec("big", any_params(), ToolKind::ReadOnly), big)],
        config,
    );
    let mut context = fresh_context();
    let exec = round(&engine, &mut context, vec![call(1, 0, "big", json!({}))], 1).await;
    assert_eq!(error_kind(&exec, 0), "result_too_large");
}

/// Runs `calls` for turn `turn` with an interruption fired `after` on the injected clock.
async fn execute_interrupted_after(
    engine: &ToolEngine,
    calls: Vec<Assembled>,
    turn: u32,
    clock: &SharedClock,
    after: Duration,
) -> Execution {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let clock = Arc::clone(clock);
    let interrupter = async move {
        clock.sleep(after).await;
        trigger.cancel();
    };
    let (mut execution, ()) =
        tokio::join!(engine.execute(calls, epoch(turn), &cancel), interrupter);
    execution.mark_delivered();
    execution
}

/// A tool that panics when called, counting its runs.
#[derive(Clone)]
struct Panicker {
    runs: Arc<AtomicUsize>,
}

impl Tool for Panicker {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            if self.runs.fetch_add(1, Ordering::SeqCst) < usize::MAX {
                panic!("the tool panicked");
            }
            Ok(ToolOutput::Text(String::new()))
        })
    }
}

/// The `Tool` message ids of an execution, in order.
fn result_ids(execution: &Execution) -> Vec<String> {
    execution
        .results
        .iter()
        .map(|message| match message {
            Message::Tool { call_id, .. } => call_id.as_str().to_owned(),
            other => panic!("not a tool result: {other:?}"),
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_read_only_call_in_flight_is_dropped_by_an_interruption() {
    let clock = clock();
    let slow = Probe::new(&clock, "slow", Duration::from_secs(1));
    let engine = engine(
        &clock,
        vec![(spec("slow", any_params(), ToolKind::ReadOnly), slow.clone())],
        EngineConfig::default(),
    );
    let calls = vec![call(1, 0, "slow", json!({}))];
    let mut context = fresh_context();
    context.push(assistant_for(&calls));
    let start = clock.now();
    let mut execution = execute_interrupted_after(&engine, calls, 1, &clock, 100 * MS).await;
    assert_eq!(clock.now().duration_since(start), 100 * MS);
    assert_eq!(error_kind(&execution, 0), "interrupted");
    execution.append_to(&mut context);
    context.validate().unwrap();
    assert_eq!(slow.runs(), 1);
    assert!(
        slow.finished().is_empty(),
        "the dropped future never finished"
    );
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_call_in_flight_is_not_dropped_and_its_result_is_reported_later() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 1_000 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let calls = vec![call(1, 0, "book", json!({ "table": "ankara-4" }))];
    let mut context = fresh_context();
    context.push(assistant_for(&calls));
    let mut execution = execute_interrupted_after(&engine, calls, 1, &clock, 100 * MS).await;
    assert_eq!(error_kind(&execution, 0), "interrupted");
    execution.append_to(&mut context);
    context.validate().unwrap();
    assert!(book.finished().is_empty(), "the call is still running");

    // The booking completes after the interruption: its result is recorded and reported.
    clock.sleep(1_000 * MS).await;
    assert_eq!(book.finished(), vec!["book"], "the call was not dropped");
    assert_eq!(engine.idempotency_usage().entries, 1);
    let notes = engine.take_late_notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].text().contains("Late result of tool call t1.0"));
    context.push(notes[0].clone());
    context.validate().unwrap();
    assert!(
        engine.take_late_notes().is_empty(),
        "a note is delivered once"
    );
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_call_past_its_timeout_keeps_running_and_is_never_run_again() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 5_000 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect).with_timeout(1_000 * MS),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let calls = vec![call(1, 0, "book", json!({ "table": "ankara-4" }))];
    let start = clock.now();
    let timed_out = execute(&engine, calls.clone(), 1).await;
    assert_eq!(clock.now().duration_since(start), 1_000 * MS);
    assert_eq!(error_kind(&timed_out, 0), "timeout");
    let (_, body) = result(&timed_out, 0);
    assert!(
        body["detail"].as_str().unwrap().contains("keeps running"),
        "the LLM is told not to repeat the booking: {body}"
    );
    assert!(book.finished().is_empty(), "the future was not dropped");

    // A replay while the first run is still going is refused, not run.
    let replay = execute(&engine, calls.clone(), 1).await;
    assert_eq!(error_kind(&replay, 0), "duplicate_call");
    assert_eq!(book.runs(), 1);

    // The booking completes: the late result is reported, and a later replay gets it back.
    clock.sleep(5_000 * MS).await;
    assert_eq!(book.finished(), vec!["book"]);
    let late = engine.take_late_notes();
    assert_eq!(late.len(), 1, "the late result is reported once");
    let recorded = execute(&engine, calls, 1).await;
    assert!(!result(&recorded, 0).0, "the recorded result is a success");
    assert_eq!(book.runs(), 1, "the booking ran exactly once");
}

#[tokio::test(start_paused = true)]
async fn a_dropped_execute_future_does_not_leave_the_side_effect_in_flight() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 1_000 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let calls = vec![call(1, 0, "book", json!({ "table": "ankara-4" }))];
    let cancel = CancellationToken::new();
    let dropped =
        tokio::time::timeout(100 * MS, engine.execute(calls.clone(), epoch(1), &cancel)).await;
    // The call's own timeout is the 5 s default, so the outer 100 ms timeout drops the execute
    // future while the booking is still running.
    assert!(dropped.is_err(), "the outer timeout dropped execute");
    clock.sleep(1_000 * MS).await;
    assert_eq!(book.finished(), vec!["book"], "the task outlived execute");
    assert_eq!(engine.idempotency_usage().entries, 1);
    // Not stuck in flight: a replay gets the recorded result instead of duplicate_call.
    let replay = execute(&engine, calls, 1).await;
    assert!(!result(&replay, 0).0);
    assert_eq!(book.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_whose_task_panics_is_recorded_as_lost_and_never_run_again() {
    let clock = clock();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(
            spec("boom", any_params(), ToolKind::SideEffect),
            Panicker {
                runs: Arc::clone(&runs),
            },
        )
        .unwrap();
    let engine = ToolEngine::new(
        Arc::new(registry),
        SessionId::new(7),
        Arc::clone(&clock),
        EngineConfig::default(),
    );
    let calls = vec![call(1, 0, "boom", json!({}))];
    let first = execute(&engine, calls.clone(), 1).await;
    assert_eq!(error_kind(&first, 0), "unknown_outcome");
    let second = execute(&engine, calls, 1).await;
    assert_eq!(error_kind(&second, 0), "unknown_outcome");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "a lost call is not run again"
    );
    assert_eq!(engine.idempotency_usage().entries, 1);
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_call_that_has_not_started_is_not_run_after_an_interruption() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let calls = vec![call(1, 0, "book", json!({ "table": "ankara-4" }))];
    let mut context = fresh_context();
    context.push(assistant_for(&calls));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut execution = engine.execute(calls, epoch(1), &cancel).await;
    assert_eq!(error_kind(&execution, 0), "interrupted");
    assert_eq!(book.runs(), 0);
    execution.append_to(&mut context);
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_call_never_runs_twice_for_one_key() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let first = round(
        &engine,
        &mut context,
        vec![call(1, 0, "book", json!({ "table": "ankara-4" }))],
        1,
    )
    .await;
    context.validate().unwrap();
    // A replay of the same call id is not a new response, so it is checked without a context:
    // the context already holds that id.
    let again = execute(
        &engine,
        vec![call(1, 0, "book", json!({ "table": "ankara-4" }))],
        1,
    )
    .await;
    assert_eq!(book.runs(), 1, "one key, one execution");
    assert_eq!(
        ok_body(&first, 0),
        ok_body(&again, 0),
        "the recorded result"
    );
    // The same call id in another turn is another key, so it runs.
    round(
        &engine,
        &mut context,
        vec![call(2, 0, "book", json!({ "table": "ankara-4" }))],
        2,
    )
    .await;
    assert_eq!(book.runs(), 2);
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_same_call_id_twice_in_one_execution_runs_once() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 10 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let first = call(1, 0, "book", json!({ "table": "ankara-4" }));
    let execution = execute(&engine, vec![first.clone(), first.clone()], 1).await;
    assert_eq!(book.runs(), 1);
    assert!(!result(&execution, 0).0, "the first copy runs");
    assert_eq!(error_kind(&execution, 1), "duplicate_call");
    // Context::validate rejects two results for one call id, so the caller only ever keeps the
    // first copy: check the ordering of that valid pair.
    let mut context = fresh_context();
    context.push(assistant_for(&[first]));
    execution.results[..1]
        .iter()
        .for_each(|m| context.push(m.clone()));
    context.validate().unwrap();
}

// ---- the model repeats a side effect it already got the result of -------------------------

fn booking_engine(clock: &SharedClock, book: &Probe) -> ToolEngine {
    engine(
        clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    )
}

#[tokio::test(start_paused = true)]
async fn an_identical_side_effect_call_of_a_later_response_of_the_same_turn_is_refused() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 10 * MS);
    let engine = booking_engine(&clock, &book);
    let mut context = fresh_context();
    let table = json!({ "table": "ankara-4" });
    let first = round(
        &engine,
        &mut context,
        vec![call(1, 0, "book", table.clone())],
        1,
    )
    .await;
    assert!(!result(&first, 0).0);
    // The model read the confirmation and asks for the same booking again, in a new response
    // (a new call id): the effect already happened, so the tool does not run again.
    let again = round(
        &engine,
        &mut context,
        vec![call(1, 1, "book", table.clone())],
        1,
    )
    .await;
    assert_eq!(book.runs(), 1, "the booking ran twice");
    assert_eq!(error_kind(&again, 0), "already_done");
    assert_eq!(engine.stats().side_effect_repeats, 1);
    // The refusal reaches the model as an ordinary tool result, in a valid context.
    context.validate().unwrap();
    // And again, in a third response.
    let third = round(&engine, &mut context, vec![call(1, 2, "book", table)], 1).await;
    assert_eq!(error_kind(&third, 0), "already_done");
    assert_eq!(book.runs(), 1);
    assert_eq!(engine.stats().side_effect_repeats, 2);
}

#[tokio::test(start_paused = true)]
async fn a_repeat_with_other_arguments_a_new_turn_or_one_response_is_not_a_repeat() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 10 * MS);
    let engine = booking_engine(&clock, &book);
    let mut context = fresh_context();
    round(
        &engine,
        &mut context,
        vec![call(1, 0, "book", json!({ "table": "a" }))],
        1,
    )
    .await;
    // Another table is another booking.
    let other = round(
        &engine,
        &mut context,
        vec![call(1, 1, "book", json!({ "table": "b" }))],
        1,
    )
    .await;
    assert!(!result(&other, 0).0);
    assert_eq!(book.runs(), 2);
    // The caller's next turn may ask for the same thing again.
    let next = round(
        &engine,
        &mut context,
        vec![call(2, 0, "book", json!({ "table": "a" }))],
        2,
    )
    .await;
    assert!(!result(&next, 0).0);
    assert_eq!(book.runs(), 3);
    // Two identical calls of one response are two calls ("book two tables for Kaya").
    let both = round(
        &engine,
        &mut context,
        vec![
            call(3, 0, "book", json!({ "table": "c" })),
            call(3, 1, "book", json!({ "table": "c" })),
        ],
        3,
    )
    .await;
    assert!(!result(&both, 0).0 && !result(&both, 1).0);
    assert_eq!(book.runs(), 5);
    assert_eq!(engine.stats().side_effect_repeats, 0);
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_that_failed_may_be_retried_by_a_later_response() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 10 * MS).failing();
    let engine = booking_engine(&clock, &book);
    let mut context = fresh_context();
    let table = json!({ "table": "ankara-4" });
    let first = round(
        &engine,
        &mut context,
        vec![call(1, 0, "book", table.clone())],
        1,
    )
    .await;
    assert_eq!(error_kind(&first, 0), "tool_failed");
    // The recorded failure is not a success: the retry of the next response runs.
    let retry = round(&engine, &mut context, vec![call(1, 1, "book", table)], 1).await;
    assert_eq!(error_kind(&retry, 0), "tool_failed");
    assert_eq!(book.runs(), 2, "a failed booking can be retried");
    assert_eq!(engine.stats().side_effect_repeats, 0);
}

#[tokio::test(start_paused = true)]
async fn a_repeatable_side_effect_may_repeat_within_a_turn() {
    // Pressing the same digit twice in an IVR menu is two effects, not a model repeating itself.
    let clock = clock();
    let press = Probe::new(&clock, "press", 10 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("press", book_params(), ToolKind::SideEffect).with_repeatable(true),
            press.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let digit = json!({ "table": "1" });
    round(
        &engine,
        &mut context,
        vec![call(1, 0, "press", digit.clone())],
        1,
    )
    .await;
    let again = round(&engine, &mut context, vec![call(1, 1, "press", digit)], 1).await;
    assert!(!result(&again, 0).0);
    assert_eq!(press.runs(), 2);
    assert_eq!(engine.stats().side_effect_repeats, 0);
}

#[test]
fn repeatable_is_refused_on_a_read_only_tool() {
    // Like `confirm`: the flag only means something for a SideEffect tool.
    let clock = clock();
    let mut registry = ToolRegistry::new();
    let error = registry
        .register(
            spec("lookup", any_params(), ToolKind::ReadOnly).with_repeatable(true),
            Probe::new(&clock, "lookup", 10 * MS),
        )
        .unwrap_err();
    assert_eq!(
        error,
        RegistryError::RepeatableNeedsSideEffect("lookup".to_owned())
    );
    assert!(
        registry
            .register(
                spec("press", any_params(), ToolKind::SideEffect).with_repeatable(true),
                Probe::new(&clock, "press", 10 * MS),
            )
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn a_read_only_call_may_repeat_within_a_turn() {
    let clock = clock();
    let lookup = Probe::new(&clock, "lookup", 10 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("lookup", any_params(), ToolKind::ReadOnly),
            lookup.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    round(
        &engine,
        &mut context,
        vec![call(1, 0, "lookup", json!({ "q": 1 }))],
        1,
    )
    .await;
    let again = round(
        &engine,
        &mut context,
        vec![call(1, 1, "lookup", json!({ "q": 1 }))],
        1,
    )
    .await;
    assert!(!result(&again, 0).0);
    assert_eq!(lookup.runs(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_idempotency_store_is_bounded_and_evicts_the_oldest_settled_result() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let config = EngineConfig {
        idempotency_capacity: 2,
        ..EngineConfig::default()
    };
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        config,
    );
    let mut context = fresh_context();
    for seq in 0..3 {
        round(
            &engine,
            &mut context,
            vec![call(1, seq, "book", json!({ "table": format!("t{seq}") }))],
            1,
        )
        .await;
    }
    assert_eq!(engine.idempotency_usage().entries, 2);
    assert_eq!(book.runs(), 3);
    context.validate().unwrap();
    // Call 0 was the oldest settled entry, so it was evicted and a replay runs again (call ids
    // are unique within a turn, so this needs a retry of an old response). The replay is checked
    // without the context, which already holds the id.
    execute(
        &engine,
        vec![call(1, 0, "book", json!({ "table": "t0" }))],
        1,
    )
    .await;
    assert_eq!(book.runs(), 4);
    assert_eq!(engine.idempotency_usage().entries, 2);
}

#[tokio::test(start_paused = true)]
async fn the_idempotency_store_is_bounded_by_the_bytes_of_its_results() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO).bulky(400);
    let config = EngineConfig {
        max_result_bytes: 500,
        idempotency_max_bytes: 1_000,
        ..EngineConfig::default()
    };
    let engine = engine(
        &clock,
        vec![(spec("book", book_params(), ToolKind::SideEffect), book)],
        config,
    );
    for seq in 0..5 {
        execute(
            &engine,
            vec![call(1, seq, "book", json!({ "table": format!("t{seq}") }))],
            1,
        )
        .await;
    }
    let usage = engine.idempotency_usage();
    assert_eq!(usage.entries, 2, "two 400-byte results fit in 1000 bytes");
    assert!(usage.bytes <= 1_000, "{usage:?}");
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_result_over_the_limit_is_cut_and_still_recorded_as_done() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO).bulky(1_000);
    let config = EngineConfig {
        max_result_bytes: 100,
        ..EngineConfig::default()
    };
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        config,
    );
    let calls = vec![call(1, 0, "book", json!({ "table": "t" }))];
    let execution = execute(&engine, calls.clone(), 1).await;
    let Message::Tool {
        content, is_error, ..
    } = &execution.results[0]
    else {
        panic!("not a tool result");
    };
    assert!(
        !is_error,
        "the booking happened, so it is not reported as a failure"
    );
    assert!(content.len() <= 100, "{} bytes", content.len());
    assert!(content.ends_with("...[truncated]"));
    let again = execute(&engine, calls, 1).await;
    assert_eq!(
        again.results[0], execution.results[0],
        "the recorded result"
    );
    assert_eq!(book.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unknown_tool_yields_an_error_result_and_counts_as_a_malformed_round() {
    let clock = clock();
    let engine = engine(&clock, Vec::new(), EngineConfig::default());
    let mut context = fresh_context();
    let execution = round(
        &engine,
        &mut context,
        vec![call(1, 0, "teleport", json!({}))],
        1,
    )
    .await;
    assert_eq!(error_kind(&execution, 0), "unknown_tool");
    assert_eq!(engine.invalid_rounds(epoch(1)), 1);
}

#[tokio::test(start_paused = true)]
async fn the_retry_budget_is_spent_after_two_malformed_rounds_and_then_reported() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let first = round(&engine, &mut context, vec![unparseable(1, 0, "weather")], 1).await;
    assert_eq!(error_kind(&first, 0), "invalid tool call");
    assert!(!first.budget_exhausted);
    let second = round(&engine, &mut context, vec![unparseable(1, 1, "weather")], 1).await;
    assert!(!second.budget_exhausted, "two retries are allowed");
    let third = round(&engine, &mut context, vec![unparseable(1, 2, "weather")], 1).await;
    assert!(third.budget_exhausted);
    assert_eq!(error_kind(&third, 0), "budget_exhausted");
    // Every later call of the turn is refused, valid ones included, and nothing runs.
    let valid = json!({ "city": "Ankara" });
    let refused = round(&engine, &mut context, vec![call(1, 3, "weather", valid)], 1).await;
    assert!(refused.budget_exhausted);
    assert_eq!(error_kind(&refused, 0), "budget_exhausted");
    assert_eq!(weather.runs(), 0);
    // A new turn starts a new budget.
    let next = round(
        &engine,
        &mut context,
        vec![call(2, 0, "weather", json!({ "city": "Ankara" }))],
        2,
    )
    .await;
    assert!(!next.budget_exhausted);
    assert_eq!(weather.runs(), 1);
    assert_eq!(
        engine.invalid_rounds(epoch(1)),
        0,
        "turn 1 is no longer the newest"
    );
}

#[tokio::test(start_paused = true)]
async fn valid_rounds_do_not_refund_the_retry_budget() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    round(&engine, &mut context, vec![unparseable(1, 0, "weather")], 1).await;
    round(&engine, &mut context, vec![unparseable(1, 1, "weather")], 1).await;
    // Two malformed rounds are the allowed retries. A valid round is still admitted, but it does
    // not reset the count, so the next malformed round exhausts the budget.
    let valid = round(
        &engine,
        &mut context,
        vec![call(1, 2, "weather", json!({ "city": "Ankara" }))],
        1,
    )
    .await;
    assert!(!valid.budget_exhausted);
    assert_eq!(weather.runs(), 1);
    assert_eq!(
        engine.invalid_rounds(epoch(1)),
        2,
        "a valid round does not refund"
    );
    let later = round(&engine, &mut context, vec![unparseable(1, 3, "weather")], 1).await;
    assert!(later.budget_exhausted);
    assert_eq!(error_kind(&later, 0), "budget_exhausted");
    assert_eq!(weather.runs(), 1);
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_superseded_turn_cannot_reset_or_spend_the_budget_of_the_current_turn() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    // Turn 1 spends its budget.
    for seq in 0..2 {
        round(
            &engine,
            &mut context,
            vec![unparseable(1, seq, "weather")],
            1,
        )
        .await;
    }
    assert!(
        round(&engine, &mut context, vec![unparseable(1, 2, "weather")], 1)
            .await
            .budget_exhausted
    );
    // Turn 2 starts and makes a valid call.
    let turn_two = round(
        &engine,
        &mut context,
        vec![call(2, 0, "weather", json!({ "city": "Ankara" }))],
        2,
    )
    .await;
    assert!(!turn_two.budget_exhausted);
    assert_eq!(weather.runs(), 1);
    // A late turn-1 response arrives: it is stale, so nothing runs and no budget changes.
    let late = round(
        &engine,
        &mut context,
        vec![call(1, 3, "weather", json!({ "city": "Ankara" }))],
        1,
    )
    .await;
    assert_eq!(error_kind(&late, 0), "stale_turn");
    assert!(!late.budget_exhausted);
    assert_eq!(weather.runs(), 1);
    assert_eq!(engine.invalid_rounds(epoch(2)), 0);
    // Turn 2 still has its own full budget: two malformed rounds are allowed, the third is not.
    assert!(
        !round(&engine, &mut context, vec![unparseable(2, 1, "weather")], 2)
            .await
            .budget_exhausted
    );
    assert!(
        !round(&engine, &mut context, vec![unparseable(2, 2, "weather")], 2)
            .await
            .budget_exhausted
    );
    assert!(
        round(&engine, &mut context, vec![unparseable(2, 3, "weather")], 2)
            .await
            .budget_exhausted
    );
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_response_beyond_the_call_cap_is_refused_for_the_extra_calls() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let calls: Vec<Assembled> = (0..33)
        .map(|seq| call(1, seq, "weather", json!({ "city": "Ankara" })))
        .collect();
    let mut context = fresh_context();
    let execution = round(&engine, &mut context, calls, 1).await;
    assert_eq!(execution.results.len(), 33, "every call is answered");
    assert_eq!(weather.runs(), 32, "the first 32 run");
    assert_eq!(error_kind(&execution, 32), "too_many_calls");
    assert_eq!(
        engine.invalid_rounds(epoch(1)),
        1,
        "the extra calls spend a round"
    );
}

#[tokio::test(start_paused = true)]
async fn the_speculation_holds_side_effects_and_commit_returns_results_in_issue_order() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", 10 * MS);
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![
            (
                spec("weather", weather_params(), ToolKind::ReadOnly),
                weather.clone(),
            ),
            (
                spec("book", book_params(), ToolKind::SideEffect),
                book.clone(),
            ),
        ],
        EngineConfig::default(),
    );
    // Book first, weather second: the results must still come back in this order.
    let calls = vec![
        call(1, 0, "book", json!({ "table": "ankara-4" })),
        call(1, 1, "weather", json!({ "city": "Ankara" })),
    ];
    let mut context = fresh_context();
    context.push(assistant_for(&calls));
    let cancel = CancellationToken::new();
    let speculation = engine.speculate(calls, epoch(1), &cancel).await;
    assert_eq!(speculation.held_calls().count(), 1);
    assert_eq!(speculation.answered().count(), 1);
    assert_eq!(book.runs(), 0, "a held booking has not run");
    assert_eq!(weather.runs(), 1, "the read-only call ran speculatively");
    let mut committed = engine.commit(speculation, &cancel).await;
    assert_eq!(result_ids(&committed), ["t1.0", "t1.1"]);
    assert_eq!(book.runs(), 1);
    committed.append_to(&mut context);
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_discarded_speculation_never_runs_its_side_effects_and_spends_its_round() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![
            (
                spec("weather", weather_params(), ToolKind::ReadOnly),
                weather,
            ),
            (
                spec("book", book_params(), ToolKind::SideEffect),
                book.clone(),
            ),
        ],
        EngineConfig::default(),
    );
    let calls = vec![
        call(1, 0, "book", json!({ "table": "ankara-4" })),
        call(1, 1, "weather", json!({ "city": "Ankara" })),
    ];
    let mut context = fresh_context();
    context.push(assistant_for(&calls));
    let cancel = CancellationToken::new();
    let speculation = engine.speculate(calls, epoch(1), &cancel).await;
    assert_eq!(speculation.held_calls().count(), 1);
    // The speculation is discarded: the caller closes the open calls, so the context stays valid.
    drop(speculation);
    let closed = context.interrupt_pending_tool_calls();
    assert_eq!(closed.len(), 2);
    context.validate().unwrap();
    assert_eq!(book.runs(), 0, "a discarded booking never runs");

    // A discarded speculation with a malformed call still spends its round.
    let discarded = engine
        .speculate(vec![unparseable(1, 2, "book")], epoch(1), &cancel)
        .await;
    assert!(!discarded.budget_exhausted());
    drop(discarded);
    assert_eq!(engine.invalid_rounds(epoch(1)), 1);
}

#[tokio::test(start_paused = true)]
async fn a_speculation_of_a_superseded_turn_runs_nothing_on_commit() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![
            (
                spec("book", book_params(), ToolKind::SideEffect),
                book.clone(),
            ),
            (
                spec("weather", weather_params(), ToolKind::ReadOnly),
                weather.clone(),
            ),
        ],
        EngineConfig::default(),
    );
    let cancel = CancellationToken::new();
    let old = vec![call(1, 0, "book", json!({ "table": "ankara-4" }))];
    let mut context = fresh_context();
    let speculation = engine.speculate(old.clone(), epoch(1), &cancel).await;
    // Turn 2 starts before turn 1 commits.
    round(
        &engine,
        &mut context,
        vec![call(2, 0, "weather", json!({ "city": "Ankara" }))],
        2,
    )
    .await;
    let mut committed = engine.commit(speculation, &cancel).await;
    assert_eq!(error_kind(&committed, 0), "stale_turn");
    assert_eq!(book.runs(), 0);
    // The stale answer is valid in the turn it belongs to.
    let mut old_context = fresh_context();
    old_context.push(assistant_for(&old));
    committed.append_to(&mut old_context);
    old_context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_local_ref_compiles_offline_and_validates_nested_values() {
    let clock = clock();
    let params = json!({
        "type": "object",
        "properties": { "party": { "$ref": "#/$defs/party" } },
        "required": ["party"],
        "$defs": {
            "party": {
                "type": "object",
                "properties": { "size": { "type": "integer", "minimum": 1 } },
                "required": ["size"],
                "additionalProperties": false
            }
        }
    });
    let lookup = Probe::new(&clock, "lookup", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(spec("lookup", params, ToolKind::ReadOnly), lookup.clone())],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let bad = round(
        &engine,
        &mut context,
        vec![call(1, 0, "lookup", json!({ "party": { "size": 0 } }))],
        1,
    )
    .await;
    assert_eq!(error_kind(&bad, 0), "invalid_arguments");
    let (_, body) = result(&bad, 0);
    assert_eq!(body["issues"][0]["path"], "/party/size");
    assert_eq!(lookup.runs(), 0);
    let good = round(
        &engine,
        &mut context,
        vec![call(1, 1, "lookup", json!({ "party": { "size": 2 } }))],
        1,
    )
    .await;
    assert!(!good.results.is_empty() && !result(&good, 0).0);
    assert_eq!(lookup.runs(), 1);
}

/// One trace event of the tool engine (target `brisvox::tools`), as captured.
#[derive(Debug, Clone, Default)]
struct Event {
    session: u64,
    epoch: u64,
    outcome: String,
    retries: u64,
    duration_us: u64,
    message: String,
    tool: String,
    call: String,
    /// Every field as `name=value`, to prove that nothing else (arguments) is logged.
    all: String,
}

/// Events captured from every test of this binary; filter by session.
static CAPTURED: Mutex<Vec<Event>> = Mutex::new(Vec::new());

/// Records the `brisvox::tools` events into [`CAPTURED`].
struct Capture;

impl tracing::field::Visit for Event {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match field.name() {
            "session" => self.session = value,
            "epoch" => self.epoch = value,
            "retries" => self.retries = value,
            "duration_us" => self.duration_us = value,
            _ => {}
        }
        self.all.push_str(&format!("{}={value};", field.name()));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "outcome" => self.outcome = value.to_owned(),
            "tool" => self.tool = value.to_owned(),
            "call" => self.call = value.to_owned(),
            _ => {}
        }
        self.all.push_str(&format!("{}={value};", field.name()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        match field.name() {
            "message" => self.message.clone_from(&text),
            "tool" => self.tool.clone_from(&text),
            "call" => self.call.clone_from(&text),
            _ => {}
        }
        self.all.push_str(&format!("{}={text};", field.name()));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != "brisvox::tools" {
            return;
        }
        let mut fields = Event::default();
        event.record(&mut fields);
        if let Ok(mut captured) = CAPTURED.lock() {
            captured.push(fields);
        }
    }
}

/// The captured events of `session`, in order.
fn events_of(session: u64) -> Vec<Event> {
    CAPTURED
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.session == session)
        .cloned()
        .collect()
}

/// Installs the capture layer once for this test binary.
fn install_capture() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber =
            tracing_subscriber::layer::SubscriberExt::with(tracing_subscriber::registry(), Capture);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

#[tokio::test(start_paused = true)]
async fn every_call_is_traced_with_its_outcome_and_retry_count() {
    install_capture();
    const SESSION: u64 = 4242;
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let mut registry = ToolRegistry::new();
    registry
        .register(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )
        .unwrap();
    let engine = ToolEngine::new(
        Arc::new(registry),
        SessionId::new(SESSION),
        Arc::clone(&clock),
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    // Round 1: an unknown tool (malformed). Round 2: an unparseable call (malformed).
    round(
        &engine,
        &mut context,
        vec![call(1, 0, "teleport", json!({}))],
        1,
    )
    .await;
    round(&engine, &mut context, vec![unparseable(1, 1, "weather")], 1).await;
    // Round 3 spends the budget: nothing runs, every call is traced as budget_exhausted.
    round(&engine, &mut context, vec![unparseable(1, 2, "weather")], 1).await;
    round(
        &engine,
        &mut context,
        vec![call(1, 3, "weather", json!({ "city": "Ankara" }))],
        1,
    )
    .await;
    let events: Vec<(String, u64)> = events_of(SESSION)
        .into_iter()
        .map(|event| (event.outcome, event.retries))
        .collect();
    assert_eq!(
        events,
        [
            ("unknown_tool".to_owned(), 1),
            ("malformed".to_owned(), 2),
            ("budget_exhausted".to_owned(), 3),
            ("budget_exhausted".to_owned(), 3),
        ]
    );
    assert_eq!(weather.runs(), 0);
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn each_request_of_a_three_round_tool_loop_gets_its_own_llm_metrics() {
    let clock = clock();
    let lookup = Probe::new(&clock, "lookup", 50 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("lookup", any_params(), ToolKind::ReadOnly),
            lookup.clone(),
        )],
        EngineConfig::default(),
    );
    let timeline = TimelineRecorder::new();
    let turn = epoch(1);
    let mut context = fresh_context();
    // (time to first token, time from first token to completion) per LLM request, in ms.
    let shapes: [(u32, u32); 3] = [(40, 60), (10, 20), (25, 0)];
    let mut rounds = Vec::new();

    for (index, (to_first, to_done)) in shapes.into_iter().enumerate() {
        let round = timeline.llm_request(turn, clock.now()).unwrap();
        clock.sleep(to_first * MS).await;
        timeline.llm_first_token(turn, round, clock.now());
        clock.sleep(to_done * MS).await;
        timeline.llm_done(turn, round, clock.now());
        rounds.push(LlmRoundMetrics {
            round: index,
            time_to_first_token: Some(to_first * MS),
            duration: Some((to_first + to_done) * MS),
        });

        // The response: a lookup in the first two rounds, the spoken answer in the last.
        let mut assembler = ResponseAssembler::new();
        if index < 2 {
            assembler
                .push(Delta::ToolCall {
                    index: 0,
                    id: Some(Arc::from(format!("call_{index}"))),
                    name: Some(Arc::from("lookup")),
                    arguments: Arc::from(format!("{{\"q\":\"{index}\"}}")),
                    provider_data: None,
                })
                .unwrap();
        } else {
            assembler
                .push(Delta::Text(Arc::from("It is sunny.")))
                .unwrap();
        }
        let response = assembler.finish(turn, &context);
        if let Some(message) = response.assistant_message() {
            context.push(message);
        }
        let (_, calls, _) = response.into_parts();
        if !calls.is_empty() {
            let mut exec = execute(&engine, calls, 1).await;
            exec.append_to(&mut context);
        }
        context.validate().unwrap();
    }

    timeline.finish();
    assert_eq!(lookup.runs(), 2);
    let finished = timeline.history();
    assert_eq!(finished.len(), 1);
    let record = finished[0];
    assert_eq!(record.llm_round_count(), 3);
    assert_eq!(record.llm_rounds().collect::<Vec<_>>(), rounds);
    // The turn-level marks still describe the first request, as before.
    assert_eq!(record.metrics().llm_time_to_first_token, Some(40 * MS));
    assert!(record.at(TurnEvent::LlmRequest).is_some());
    // Only the first request is marked at turn level; the second and third are per-round only.
    assert_eq!(record.llm_round(2).unwrap().round, 2);
}

/// Compile-time check: the engine is shared across tasks and its execution future can be spawned.
#[tokio::test(start_paused = true)]
async fn the_engine_and_its_execution_future_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>(_: &T) {}
    assert_send_sync::<ToolEngine>();
    assert_send_sync::<ToolRegistry>();
    let clock = clock();
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            Probe::new(&clock, "weather", Duration::ZERO),
        )],
        EngineConfig::default(),
    );
    let cancel = CancellationToken::new();
    let future = engine.execute(
        vec![call(1, 0, "weather", json!({ "city": "Ankara" }))],
        epoch(1),
        &cancel,
    );
    assert_send(&future);
    let exec = future.await;
    assert_eq!(exec.results.len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Review fixes: late results that are never lost, panics, backstop and teardown, the
// argument cap, and the contract tests that were missing.
// ---------------------------------------------------------------------------------------------

/// Sets its flag when dropped; lets a tool prove that its future was dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A tool whose future never completes. Counts its runs and flags when its future is dropped.
#[derive(Clone)]
struct Hang {
    runs: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl Hang {
    fn new() -> Self {
        Self {
            runs: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn was_dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }
}

impl Tool for Hang {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let _flag = DropFlag(Arc::clone(&self.dropped));
            self.runs.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(ToolOutput::Text(String::new()))
        })
    }
}

/// A tool that returns a fixed output, counting its runs.
#[derive(Clone)]
struct Fixed {
    output: ToolOutput,
    runs: Arc<AtomicUsize>,
}

impl Fixed {
    fn new(output: ToolOutput) -> Self {
        Self {
            output,
            runs: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Tool for Fixed {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(self.output.clone())
        })
    }
}

/// A tool that fails with a fixed message, counting its runs.
#[derive(Clone)]
struct Failing {
    message: String,
    runs: Arc<AtomicUsize>,
}

impl Failing {
    fn new(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            runs: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Tool for Failing {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Err(ToolError::new(self.message.clone()))
        })
    }
}

/// A tool that panics in `call` itself, before it returns a future.
struct SyncPanicker;

impl Tool for SyncPanicker {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        panic!("the tool panicked before its future");
    }
}

/// A tool that records the context of every call and can wait for the turn's cancellation.
#[derive(Clone)]
struct CtxProbe {
    seen: Arc<Mutex<Vec<ToolCallCtx>>>,
    watch_cancel: bool,
    saw_cancel: Arc<AtomicBool>,
}

impl CtxProbe {
    fn new(watch_cancel: bool) -> Self {
        Self {
            seen: Arc::new(Mutex::new(Vec::new())),
            watch_cancel,
            saw_cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Tool for CtxProbe {
    fn call<'a>(&'a self, _args: Value, ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(ctx.clone());
            if self.watch_cancel {
                ctx.cancel.cancelled().await;
                self.saw_cancel.store(true, Ordering::SeqCst);
            }
            Ok(ToolOutput::Text("ok".to_owned()))
        })
    }
}

/// An engine for session `session`, with the tools registered by `register`.
fn engine_with(
    clock: &SharedClock,
    session: u64,
    config: EngineConfig,
    register: impl FnOnce(&mut ToolRegistry),
) -> ToolEngine {
    let mut registry = ToolRegistry::new();
    register(&mut registry);
    ToolEngine::new(
        Arc::new(registry),
        SessionId::new(session),
        Arc::clone(clock),
        config,
    )
}

/// The text content of the `index`-th result.
fn content_of(execution: &Execution, index: usize) -> String {
    match &execution.results[index] {
        Message::Tool { content, .. } => content.to_string(),
        other => panic!("not a tool result: {other:?}"),
    }
}

/// Lets spawned tasks run (aborted ones are dropped when they are next scheduled).
async fn settle_tasks() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_result_sent_before_execute_is_dropped_comes_back_as_a_late_note() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 100 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let calls = vec![call(1, 0, "book", json!({ "table": "ankara-4" }))];
    let cancel = CancellationToken::new();
    let mut execute = Box::pin(engine.execute(calls.clone(), epoch(1), &cancel));
    assert!(futures::poll!(execute.as_mut()).is_pending());
    // The booking completes and its task sends the result while `execute` is not being polled.
    clock.sleep(200 * MS).await;
    assert_eq!(book.finished(), vec!["book"]);
    // The caller (a barge-in `select!`) drops the future before it reads the result.
    drop(execute);
    let notes = engine.take_late_notes();
    assert_eq!(notes.len(), 1, "the result must not vanish with the future");
    assert!(notes[0].text().contains("Late result of tool call t1.0"));
    assert!(engine.take_late_notes().is_empty(), "reported once");
    // The booking is recorded, so a retry of the same call does not book again.
    let replay = execute_calls(&engine, calls).await;
    assert!(!result(&replay, 0).0);
    assert_eq!(book.runs(), 1);
}

/// Runs `calls` in turn 1 with a fresh token.
async fn execute_calls(engine: &ToolEngine, calls: Vec<Assembled>) -> Execution {
    execute(engine, calls, 1).await
}

#[tokio::test(start_paused = true)]
async fn a_result_already_taken_by_a_dropped_execute_is_reported_as_a_late_note() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 100 * MS);
    let search = Probe::new(&clock, "search", Duration::from_secs(10));
    let engine = engine(
        &clock,
        vec![
            (
                spec("book", book_params(), ToolKind::SideEffect),
                book.clone(),
            ),
            (
                spec("search", any_params(), ToolKind::ReadOnly)
                    .with_timeout(Duration::from_secs(30)),
                search.clone(),
            ),
        ],
        EngineConfig::default(),
    );
    let calls = vec![
        call(1, 0, "book", json!({ "table": "ankara-4" })),
        call(1, 1, "search", json!({})),
    ];
    let cancel = CancellationToken::new();
    let mut execute = Box::pin(engine.execute(calls, epoch(1), &cancel));
    assert!(futures::poll!(execute.as_mut()).is_pending());
    clock.sleep(200 * MS).await;
    // This poll resolves the booking's slot inside `join_all`; the search is still running, so
    // the execution as a whole is not finished.
    assert!(futures::poll!(execute.as_mut()).is_pending());
    drop(execute);
    let notes = engine.take_late_notes();
    assert_eq!(notes.len(), 1, "the resolved slot must not be discarded");
    assert!(notes[0].text().contains("t1.0"));
    assert!(
        search.finished().is_empty(),
        "the read-only call was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn a_result_that_arrives_with_the_timeout_is_delivered_once() {
    // The tool finishes exactly at its timeout. Whatever the poll order, the call is answered by
    // the real result or by a timeout plus a note, never by both and never by neither.
    let clock = clock();
    let book = Probe::new(&clock, "book", 100 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect).with_timeout(100 * MS),
            book,
        )],
        EngineConfig::default(),
    );
    let exec = execute(
        &engine,
        vec![call(1, 0, "book", json!({ "table": "t" }))],
        1,
    )
    .await;
    clock.sleep(Duration::from_secs(1)).await;
    let answered_ok = !result(&exec, 0).0;
    let notes = exec.late_notes.len() + engine.take_late_notes().len();
    assert_eq!(usize::from(answered_ok) + notes, 1, "exactly one report");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn side_effect_results_racing_the_timeout_are_answered_or_noted_never_lost() {
    const CALLS: u32 = 120;
    let clock = SystemClock::shared();
    let book = Probe::new(&clock, "book", 2 * MS);
    let config = EngineConfig {
        idempotency_capacity: 1_000,
        ..EngineConfig::default()
    };
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect).with_timeout(2 * MS),
            book.clone(),
        )],
        config,
    );
    let (mut delivered, mut noted) = (0_usize, 0_usize);
    for seq in 0..CALLS {
        let exec = execute(
            &engine,
            // Distinct arguments: an identical call after a timed-out one would be suppressed as
            // a repeat of an orphan, and this test is about every call running.
            vec![call(1, seq, "book", json!({ "table": format!("t{seq}") }))],
            1,
        )
        .await;
        delivered += usize::from(!result(&exec, 0).0);
        noted += exec.late_notes.len();
    }
    // Stragglers: the tasks that outlived their slot report themselves.
    for _ in 0..500 {
        noted += engine.take_late_notes().len();
        if delivered + noted >= CALLS as usize {
            break;
        }
        clock.sleep(10 * MS).await;
    }
    assert_eq!(book.runs(), CALLS as usize);
    assert_eq!(
        delivered + noted,
        CALLS as usize,
        "every result is answered or noted exactly once ({delivered} answered, {noted} noted)"
    );
}

#[tokio::test(start_paused = true)]
async fn a_panicking_read_only_tool_is_answered_and_does_not_take_its_siblings_down() {
    let clock = clock();
    let runs = Arc::new(AtomicUsize::new(0));
    let weather = Probe::new(&clock, "weather", 10 * MS);
    let engine = engine_with(&clock, 7, EngineConfig::default(), |registry| {
        registry
            .register(
                spec("boom", any_params(), ToolKind::ReadOnly),
                Panicker {
                    runs: Arc::clone(&runs),
                },
            )
            .unwrap();
        registry
            .register(
                spec("sync_boom", any_params(), ToolKind::ReadOnly),
                SyncPanicker,
            )
            .unwrap();
        registry
            .register(
                spec("weather", weather_params(), ToolKind::ReadOnly),
                weather.clone(),
            )
            .unwrap();
    });
    let mut context = fresh_context();
    let exec = round(
        &engine,
        &mut context,
        vec![
            call(1, 0, "boom", json!({})),
            call(1, 1, "weather", json!({ "city": "Ankara" })),
            call(1, 2, "sync_boom", json!({})),
        ],
        1,
    )
    .await;
    assert_eq!(exec.results.len(), 3, "every call is answered");
    assert_eq!(error_kind(&exec, 0), "tool_failed");
    assert_eq!(result(&exec, 0).1["detail"], "the tool panicked");
    assert_eq!(ok_body(&exec, 1)["args"]["city"], "Ankara");
    assert_eq!(error_kind(&exec, 2), "tool_failed");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    // The engine is still usable.
    let again = round(
        &engine,
        &mut context,
        vec![call(1, 3, "weather", json!({ "city": "Izmir" }))],
        1,
    )
    .await;
    assert_eq!(ok_body(&again, 0)["args"]["city"], "Izmir");
}

#[tokio::test(start_paused = true)]
async fn a_hung_side_effect_is_stopped_at_the_backstop_and_recorded_as_lost() {
    let clock = clock();
    let hang = Hang::new();
    let config = EngineConfig {
        max_side_effect_runtime: Duration::from_secs(10),
        ..EngineConfig::default()
    };
    let engine = engine_with(&clock, 7, config, |registry| {
        registry
            .register(
                spec("book", any_params(), ToolKind::SideEffect)
                    .with_timeout(Duration::from_secs(1)),
                hang.clone(),
            )
            .unwrap();
    });
    let calls = vec![call(1, 0, "book", json!({}))];
    let first = execute(&engine, calls.clone(), 1).await;
    assert_eq!(error_kind(&first, 0), "timeout");
    assert!(
        !hang.was_dropped(),
        "the future outlives the call's timeout"
    );
    let running = execute(&engine, calls.clone(), 1).await;
    assert_eq!(error_kind(&running, 0), "duplicate_call");

    // The backstop passes: the future is dropped and the outcome is reported as unknown.
    clock.sleep(Duration::from_secs(10)).await;
    assert!(hang.was_dropped(), "the backstop dropped the hung future");
    let notes = engine.take_late_notes();
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].text().contains("outcome is unknown"),
        "{}",
        notes[0].text()
    );
    // Never run again, and not stuck in flight.
    let replay = execute(&engine, calls, 1).await;
    assert_eq!(error_kind(&replay, 0), "unknown_outcome");
    assert_eq!(hang.runs.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn the_backstop_never_undercuts_the_tools_own_timeout() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::from_secs(3));
    let config = EngineConfig {
        max_side_effect_runtime: Duration::from_secs(1),
        ..EngineConfig::default()
    };
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect).with_timeout(Duration::from_secs(5)),
            book.clone(),
        )],
        config,
    );
    let exec = execute(
        &engine,
        vec![call(1, 0, "book", json!({ "table": "t" }))],
        1,
    )
    .await;
    assert!(!result(&exec, 0).0, "the 3 s booking fits its 5 s timeout");
    assert_eq!(book.finished(), vec!["book"]);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_engine_aborts_running_side_effects() {
    let clock = clock();
    let hang = Hang::new();
    let engine = engine_with(&clock, 7, EngineConfig::default(), |registry| {
        registry
            .register(
                spec("book", any_params(), ToolKind::SideEffect),
                hang.clone(),
            )
            .unwrap();
    });
    let cancel = CancellationToken::new();
    let dropped = tokio::time::timeout(
        100 * MS,
        engine.execute(vec![call(1, 0, "book", json!({}))], epoch(1), &cancel),
    )
    .await;
    assert!(dropped.is_err());
    settle_tasks().await;
    assert!(
        !hang.was_dropped(),
        "the engine still owns the running call"
    );
    drop(engine);
    settle_tasks().await;
    assert!(hang.was_dropped(), "the session's end aborts the call");
}

#[tokio::test(start_paused = true)]
async fn finished_side_effect_tasks_are_reaped() {
    // Many sequential calls must not grow the engine's task set without bound: it is reaped on
    // every spawn. (Observable through the store: nothing stays in flight.)
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig {
            idempotency_capacity: 4,
            ..EngineConfig::default()
        },
    );
    for seq in 0..50 {
        let exec = execute(
            &engine,
            vec![call(1, seq, "book", json!({ "table": format!("t{seq}") }))],
            1,
        )
        .await;
        assert!(!result(&exec, 0).0);
    }
    assert_eq!(book.runs(), 50);
    assert_eq!(engine.idempotency_usage().entries, 4);
}

#[tokio::test(start_paused = true)]
async fn arguments_over_the_cap_are_refused_without_validation_or_a_run() {
    let clock = clock();
    let tool = Probe::new(&clock, "tool", Duration::ZERO);
    let config = EngineConfig {
        max_argument_bytes: 64,
        ..EngineConfig::default()
    };
    let engine = engine(
        &clock,
        vec![(spec("tool", any_params(), ToolKind::ReadOnly), tool.clone())],
        config,
    );
    let mut context = fresh_context();
    // {"a":"<n x>"} serializes to 8 + n bytes: 64 fits, 65 does not.
    let at_limit = round(
        &engine,
        &mut context,
        vec![call(1, 0, "tool", json!({ "a": "x".repeat(56) }))],
        1,
    )
    .await;
    assert!(!result(&at_limit, 0).0, "exactly at the cap runs");
    assert_eq!(tool.runs(), 1);
    assert_eq!(engine.invalid_rounds(epoch(1)), 0);
    let over = round(
        &engine,
        &mut context,
        vec![call(1, 1, "tool", json!({ "a": "x".repeat(57) }))],
        1,
    )
    .await;
    assert_eq!(error_kind(&over, 0), "arguments_too_large");
    assert!(
        result(&over, 0).1["detail"]
            .as_str()
            .unwrap()
            .contains("64 bytes")
    );
    assert_eq!(tool.runs(), 1, "refused arguments never reach the tool");
    assert_eq!(
        engine.invalid_rounds(epoch(1)),
        1,
        "it spends a retry round"
    );
}

#[tokio::test(start_paused = true)]
async fn a_hostile_argument_at_the_default_cap_is_refused_quickly_and_a_smaller_one_validates() {
    let clock = clock();
    let params = json!({
        "type": "object",
        "properties": { "ids": { "type": "array", "uniqueItems": true, "items": { "type": "integer" } } }
    });
    let tool = Probe::new(&clock, "tool", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(spec("tool", params, ToolKind::ReadOnly), tool.clone())],
        EngineConfig::default(),
    );
    // 200 KiB of distinct integers: far over the 16 KiB cap, so not validated at all.
    let huge: Vec<u32> = (0..40_000).collect();
    let exec = execute(&engine, vec![call(1, 0, "tool", json!({ "ids": huge }))], 1).await;
    assert_eq!(error_kind(&exec, 0), "arguments_too_large");
    // A few thousand integers under the cap validate normally.
    let small: Vec<u32> = (0..1_500).collect();
    let exec = execute(
        &engine,
        vec![call(1, 1, "tool", json!({ "ids": small }))],
        1,
    )
    .await;
    assert!(!result(&exec, 0).0);
    assert_eq!(tool.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn schema_failures_spend_the_retry_budget_and_then_refuse_even_valid_calls() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let bad = |seq| call(1, seq, "weather", json!({ "city": 42 }));
    let first = round(&engine, &mut context, vec![bad(0)], 1).await;
    assert_eq!(error_kind(&first, 0), "invalid_arguments");
    assert_eq!(engine.invalid_rounds(epoch(1)), 1);
    let second = round(&engine, &mut context, vec![bad(1)], 1).await;
    assert_eq!(error_kind(&second, 0), "invalid_arguments");
    assert!(!second.budget_exhausted);
    assert_eq!(engine.invalid_rounds(epoch(1)), 2);
    let third = round(&engine, &mut context, vec![bad(2)], 1).await;
    assert!(
        third.budget_exhausted,
        "the third bad round exhausts the turn"
    );
    assert_eq!(error_kind(&third, 0), "budget_exhausted");
    let valid = json!({ "city": "Ankara" });
    let after = round(&engine, &mut context, vec![call(1, 3, "weather", valid)], 1).await;
    assert!(after.budget_exhausted);
    assert_eq!(error_kind(&after, 0), "budget_exhausted");
    assert_eq!(weather.runs(), 0, "nothing ran in the whole turn");
}

#[tokio::test(start_paused = true)]
async fn an_in_flight_entry_is_never_evicted_and_a_full_store_refuses_new_calls() {
    let clock = clock();
    let hold = Probe::new(&clock, "hold", Duration::from_secs(10));
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let register = |registry: &mut ToolRegistry| {
        registry
            .register(
                spec("hold", any_params(), ToolKind::SideEffect).with_timeout(100 * MS),
                hold.clone(),
            )
            .unwrap();
        registry
            .register(
                spec("book", any_params(), ToolKind::SideEffect),
                book.clone(),
            )
            .unwrap();
    };

    // Capacity 2: A (in flight, the oldest), B (done), C evicts B and never A.
    let config = EngineConfig {
        idempotency_capacity: 2,
        ..EngineConfig::default()
    };
    let engine = engine_with(&clock, 7, config, register);
    let a = call(1, 0, "hold", json!({}));
    assert_eq!(
        error_kind(&execute(&engine, vec![a.clone()], 1).await, 0),
        "timeout"
    );
    assert!(
        !result(
            &execute(&engine, vec![call(1, 1, "book", json!({}))], 1).await,
            0
        )
        .0
    );
    assert!(
        !result(
            &execute(&engine, vec![call(1, 2, "book", json!({ "n": 2 }))], 1).await,
            0
        )
        .0
    );
    assert_eq!(engine.idempotency_usage().entries, 2);
    let replay = execute(&engine, vec![a], 1).await;
    assert_eq!(
        error_kind(&replay, 0),
        "duplicate_call",
        "A is still recorded as running"
    );
    assert_eq!(hold.runs(), 1);

    // Capacity 1: while A runs, every new call is refused and does not run.
    let config = EngineConfig {
        idempotency_capacity: 1,
        ..EngineConfig::default()
    };
    let engine = engine_with(&clock, 8, config, register);
    let a = call(2, 0, "hold", json!({}));
    assert_eq!(
        error_kind(&execute(&engine, vec![a.clone()], 2).await, 0),
        "timeout"
    );
    let runs_before = book.runs();
    let refused = execute(&engine, vec![call(2, 1, "book", json!({}))], 2).await;
    assert_eq!(error_kind(&refused, 0), "store_full");
    assert_eq!(book.runs(), runs_before, "a refused call never runs");
    assert_eq!(
        error_kind(&execute(&engine, vec![a.clone()], 2).await, 0),
        "duplicate_call"
    );
    // Once A settles it can be evicted, so the store has room again.
    clock.sleep(Duration::from_secs(11)).await;
    let _ = engine.take_late_notes();
    let accepted = execute(&engine, vec![call(2, 2, "book", json!({}))], 2).await;
    assert!(!result(&accepted, 0).0);
    assert_eq!(book.runs(), runs_before + 1);
}

#[tokio::test(start_paused = true)]
async fn the_byte_budget_is_raised_to_hold_one_result_and_a_zero_capacity_still_records() {
    let clock = clock();
    // A byte budget smaller than max_result_bytes is raised, so the result stays recorded.
    let book = Probe::new(&clock, "book", Duration::ZERO).bulky(400);
    let config = EngineConfig {
        max_result_bytes: 500,
        idempotency_max_bytes: 100,
        ..EngineConfig::default()
    };
    let small_budget = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        config,
    );
    let calls = vec![call(1, 0, "book", json!({ "table": "t" }))];
    let first = execute(&small_budget, calls.clone(), 1).await;
    let again = execute(&small_budget, calls, 1).await;
    assert_eq!(book.runs(), 1, "a recorded result is not evicted at once");
    assert_eq!(first.results[0], again.results[0]);
    assert_eq!(small_budget.idempotency_usage().entries, 1);

    // Capacity 0 behaves as capacity 1.
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let config = EngineConfig {
        idempotency_capacity: 0,
        ..EngineConfig::default()
    };
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        config,
    );
    let calls = vec![call(1, 0, "book", json!({ "table": "t" }))];
    execute(&engine, calls.clone(), 1).await;
    execute(&engine, calls, 1).await;
    assert_eq!(book.runs(), 1);
    assert_eq!(engine.idempotency_usage().entries, 1);
}

#[tokio::test(start_paused = true)]
async fn a_late_result_is_drained_by_the_next_execute_after_the_results() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 1_000 * MS);
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![
            (
                spec("book", book_params(), ToolKind::SideEffect).with_timeout(100 * MS),
                book,
            ),
            (
                spec("weather", weather_params(), ToolKind::ReadOnly),
                weather,
            ),
        ],
        EngineConfig::default(),
    );
    let mut context = fresh_context();
    let first = round(
        &engine,
        &mut context,
        vec![call(1, 0, "book", json!({ "table": "t" }))],
        1,
    )
    .await;
    assert_eq!(error_kind(&first, 0), "timeout");
    assert!(first.late_notes.is_empty());
    clock.sleep(2_000 * MS).await;

    // The next round drains the note into its own execution, after its results.
    let second = round(
        &engine,
        &mut context,
        vec![call(1, 1, "weather", json!({ "city": "Ankara" }))],
        1,
    )
    .await;
    assert_eq!(second.late_notes.len(), 1);
    assert!(
        second.late_notes[0]
            .text()
            .contains("Late result of tool call t1.0")
    );
    let last = context.messages().last().unwrap();
    assert!(
        matches!(last, Message::System { .. }) && last.text().contains("t1.0"),
        "the note follows the tool results: {last:?}"
    );
    assert!(engine.take_late_notes().is_empty(), "drained exactly once");
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_mailbox_keeps_64_late_notes_and_counts_the_rest() {
    let clock = clock();
    let book = Probe::new(&clock, "book", 1_000 * MS);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect).with_timeout(10 * MS),
            book,
        )],
        EngineConfig::default(),
    );
    let mut seq = 0;
    for chunk in [32, 32, 1] {
        let calls: Vec<Assembled> = (0..chunk)
            .map(|_| {
                seq += 1;
                // Distinct arguments, so no call repeats an orphan of an earlier chunk.
                call(1, seq, "book", json!({ "table": format!("t{seq}") }))
            })
            .collect();
        let exec = execute(&engine, calls, 1).await;
        assert!(exec.late_notes.is_empty());
    }
    clock.sleep(Duration::from_secs(5)).await;
    assert_eq!(engine.late_notes_dropped(), 1, "the 65th note does not fit");
    assert_eq!(engine.take_late_notes().len(), 64);
    assert!(engine.take_late_notes().is_empty());
}

#[tokio::test(start_paused = true)]
async fn commit_spends_no_budget_and_a_held_call_is_recorded_for_replays() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let cancel = CancellationToken::new();
    let held = call(1, 0, "book", json!({ "table": "t" }));
    let speculation = engine
        .speculate(
            vec![held.clone(), unparseable(1, 1, "book")],
            epoch(1),
            &cancel,
        )
        .await;
    assert_eq!(
        engine.invalid_rounds(epoch(1)),
        1,
        "the speculation spent its round"
    );
    let committed = engine.commit(speculation, &cancel).await;
    assert_eq!(
        engine.invalid_rounds(epoch(1)),
        1,
        "commit does not spend the round again"
    );
    assert!(!committed.budget_exhausted);
    assert!(!result(&committed, 0).0);
    assert_eq!(error_kind(&committed, 1), "invalid tool call");
    // A replay of the committed call returns the recorded result.
    let replay = execute(&engine, vec![held], 1).await;
    assert_eq!(replay.results[0], committed.results[0]);
    assert_eq!(book.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn commit_answers_held_calls_budget_exhausted_when_the_budget_ran_out_since() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![
            (
                spec("book", book_params(), ToolKind::SideEffect),
                book.clone(),
            ),
            (
                spec("weather", weather_params(), ToolKind::ReadOnly),
                weather,
            ),
        ],
        EngineConfig::default(),
    );
    let cancel = CancellationToken::new();
    let calls = vec![
        call(1, 0, "book", json!({ "table": "t" })),
        call(1, 1, "weather", json!({ "city": "Ankara" })),
    ];
    let speculation = engine.speculate(calls, epoch(1), &cancel).await;
    assert!(!speculation.budget_exhausted());
    // Three malformed rounds of the same turn spend the budget meanwhile.
    for seq in 2..5 {
        execute(&engine, vec![unparseable(1, seq, "book")], 1).await;
    }
    let committed = engine.commit(speculation, &cancel).await;
    assert!(committed.budget_exhausted);
    assert_eq!(error_kind(&committed, 0), "budget_exhausted");
    assert!(
        !result(&committed, 1).0,
        "the answered ReadOnly result is kept"
    );
    assert_eq!(book.runs(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_speculation_under_an_exhausted_budget_commits_as_exhausted() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    for seq in 0..3 {
        execute(&engine, vec![unparseable(1, seq, "book")], 1).await;
    }
    let cancel = CancellationToken::new();
    let speculation = engine
        .speculate(
            vec![call(1, 3, "book", json!({ "table": "t" }))],
            epoch(1),
            &cancel,
        )
        .await;
    assert!(speculation.budget_exhausted());
    assert_eq!(speculation.held_calls().count(), 0, "nothing is held");
    let committed = engine.commit(speculation, &cancel).await;
    assert!(committed.budget_exhausted, "the flag passes through commit");
    assert_eq!(error_kind(&committed, 0), "budget_exhausted");
    assert_eq!(book.runs(), 0);
}

#[tokio::test(start_paused = true)]
async fn commit_with_a_cancelled_token_answers_interrupted_and_does_not_book() {
    let clock = clock();
    let book = Probe::new(&clock, "book", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("book", book_params(), ToolKind::SideEffect),
            book.clone(),
        )],
        EngineConfig::default(),
    );
    let cancel = CancellationToken::new();
    let calls = vec![call(1, 0, "book", json!({ "table": "t" }))];
    let mut context = fresh_context();
    context.push(assistant_for(&calls));
    let speculation = engine.speculate(calls, epoch(1), &cancel).await;
    assert_eq!(speculation.held_calls().count(), 1);
    cancel.cancel();
    let mut committed = engine.commit(speculation, &cancel).await;
    assert_eq!(error_kind(&committed, 0), "interrupted");
    assert_eq!(book.runs(), 0, "the held booking never ran");
    committed.append_to(&mut context);
    context.validate().unwrap();
}

#[tokio::test(start_paused = true)]
async fn every_outcome_is_traced_with_tool_call_and_duration_and_never_the_arguments() {
    install_capture();
    const SESSION: u64 = 4343;
    const SECRET: &str = "secret-token-xyz";
    let clock = clock();
    let engine = engine_with(&clock, SESSION, EngineConfig::default(), |registry| {
        let probe = |label, delay| Probe::new(&clock, label, delay);
        registry
            .register(
                spec("ro_ok", any_params(), ToolKind::ReadOnly),
                probe("ro_ok", Duration::ZERO),
            )
            .unwrap();
        registry
            .register(
                spec("ro_slow", any_params(), ToolKind::ReadOnly)
                    .with_timeout(Duration::from_secs(1)),
                probe("ro_slow", Duration::from_secs(10)),
            )
            .unwrap();
        registry
            .register(
                spec("ro_fail", any_params(), ToolKind::ReadOnly),
                probe("ro_fail", Duration::ZERO).failing(),
            )
            .unwrap();
        registry
            .register(
                spec("ro_boom", any_params(), ToolKind::ReadOnly),
                Panicker {
                    runs: Arc::new(AtomicUsize::new(0)),
                },
            )
            .unwrap();
        registry
            .register(
                spec("se_slow", any_params(), ToolKind::SideEffect)
                    .with_timeout(Duration::from_secs(1)),
                probe("se_slow", Duration::from_secs(3)),
            )
            .unwrap();
        registry
            .register(
                spec("se_fast", any_params(), ToolKind::SideEffect),
                probe("se_fast", Duration::ZERO),
            )
            .unwrap();
        registry
            .register(
                spec("se_boom", any_params(), ToolKind::SideEffect),
                Panicker {
                    runs: Arc::new(AtomicUsize::new(0)),
                },
            )
            .unwrap();
    });
    let secret_args = json!({ "token": SECRET });

    execute(&engine, vec![call(1, 0, "ro_ok", secret_args.clone())], 1).await;
    execute(&engine, vec![call(1, 1, "ro_slow", secret_args.clone())], 1).await;
    execute(&engine, vec![call(1, 2, "ro_fail", secret_args.clone())], 1).await;
    execute(&engine, vec![call(1, 3, "ro_boom", secret_args.clone())], 1).await;
    // A SideEffect call past its timeout, a duplicate while it runs, its late result, a replay.
    let slow = call(1, 4, "se_slow", secret_args.clone());
    execute(&engine, vec![slow.clone()], 1).await;
    execute(&engine, vec![slow.clone()], 1).await;
    clock.sleep(Duration::from_secs(3)).await;
    execute(&engine, vec![slow], 1).await;
    // A speculative SideEffect call: held, then committed.
    let cancel = CancellationToken::new();
    let speculation = engine
        .speculate(
            vec![call(1, 5, "se_fast", secret_args.clone())],
            epoch(1),
            &cancel,
        )
        .await;
    engine.commit(speculation, &cancel).await.mark_delivered();
    // Interruptions of a ReadOnly and of a SideEffect call.
    execute_interrupted_after(
        &engine,
        vec![call(1, 6, "ro_slow", secret_args.clone())],
        1,
        &clock,
        100 * MS,
    )
    .await;
    execute_interrupted_after(
        &engine,
        // Other arguments than call 4, which is an orphan: an identical call would be answered
        // from its record.
        vec![call(
            1,
            7,
            "se_slow",
            json!({ "token": SECRET, "attempt": 2 }),
        )],
        1,
        &clock,
        100 * MS,
    )
    .await;
    clock.sleep(Duration::from_secs(3)).await;
    // A SideEffect call whose task panics, and its replay.
    let boom = call(1, 8, "se_boom", secret_args);
    execute(&engine, vec![boom.clone()], 1).await;
    execute(&engine, vec![boom], 1).await;

    let events = events_of(SESSION);
    let seen: Vec<(&str, &str, &str, u64)> = events
        .iter()
        .map(|e| {
            (
                e.outcome.as_str(),
                e.tool.as_str(),
                e.call.as_str(),
                e.duration_us,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("ok", "ro_ok", "t1.0", 0),
            ("timed_out", "ro_slow", "t1.1", 1_000_000),
            ("failed", "ro_fail", "t1.2", 0),
            ("failed", "ro_boom", "t1.3", 0),
            ("timed_out_running", "se_slow", "t1.4", 1_000_000),
            ("duplicate", "se_slow", "t1.4", 0),
            ("late_result", "se_slow", "t1.4", 3_000_000),
            ("recorded", "se_slow", "t1.4", 0),
            ("held", "se_fast", "t1.5", 0),
            ("ok", "se_fast", "t1.5", 0),
            ("interrupted", "ro_slow", "t1.6", 100_000),
            ("interrupted_running", "se_slow", "t1.7", 100_000),
            ("late_result", "se_slow", "t1.7", 3_000_000),
            ("lost", "se_boom", "t1.8", 0),
            ("lost", "se_boom", "t1.8", 0),
        ]
    );
    for event in &events {
        assert_eq!(event.epoch, 1);
        assert!(
            !event.all.contains(SECRET),
            "the arguments must never be traced: {}",
            event.all
        );
        let expected = if event.outcome == "held" {
            "tool call held"
        } else {
            "tool call settled"
        };
        assert_eq!(event.message, expected);
    }
}

#[tokio::test(start_paused = true)]
async fn error_fields_are_bounded_per_field_and_cut_at_character_boundaries() {
    let clock = clock();
    let params = json!({
        "type": "object",
        "properties": { "mode": { "enum": ["a", "b"] } },
        "additionalProperties": { "type": "string" }
    });
    let engine = engine_with(&clock, 7, EngineConfig::default(), |registry| {
        registry
            .register(
                spec("probe", params, ToolKind::ReadOnly),
                Probe::new(&clock, "probe", Duration::ZERO),
            )
            .unwrap();
        registry
            .register(
                spec("fails", any_params(), ToolKind::ReadOnly),
                Failing::new(&"ğ".repeat(1_000)),
            )
            .unwrap();
    });
    let mut context = fresh_context();
    // A 5000-byte property name (the issue path) and a 5000-byte string (the issue reason), both
    // made of two-byte characters so that a cut can land inside a character.
    let key = "ğ".repeat(2_500);
    let value = "ş".repeat(2_500);
    let args = json!({ key: 1, "mode": value });
    let exec = round(&engine, &mut context, vec![call(1, 0, "probe", args)], 1).await;
    assert_eq!(error_kind(&exec, 0), "invalid_arguments");
    let (_, body) = result(&exec, 0);
    let issues = body["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 2);
    let mut long_paths = 0;
    let mut long_reasons = 0;
    for issue in issues {
        let path = issue["path"].as_str().unwrap();
        let reason = issue["reason"].as_str().unwrap();
        assert!(path.len() <= 128, "path is {} bytes", path.len());
        assert!(reason.len() <= 200, "reason is {} bytes", reason.len());
        if path.len() > 100 {
            assert!(path.ends_with("..."));
            long_paths += 1;
        }
        if reason.len() > 150 {
            assert!(reason.ends_with("..."));
            long_reasons += 1;
        }
    }
    assert_eq!((long_paths, long_reasons), (1, 1), "{body}");
    assert!(content_of(&exec, 0).len() < 1_024);

    // An unknown tool with a 10 KB name.
    let name = "ü".repeat(5_000);
    let exec = round(&engine, &mut context, vec![call(1, 1, &name, json!({}))], 1).await;
    assert_eq!(error_kind(&exec, 0), "unknown_tool");
    let tool = result(&exec, 0).1["tool"].as_str().unwrap().to_owned();
    assert!(
        tool.len() <= 64 && tool.ends_with("..."),
        "{} bytes",
        tool.len()
    );

    // A tool failure with a long message: the detail is cut at 512 bytes.
    let exec = round(
        &engine,
        &mut context,
        vec![call(1, 2, "fails", json!({}))],
        1,
    )
    .await;
    assert_eq!(error_kind(&exec, 0), "tool_failed");
    let detail = result(&exec, 0).1["detail"].as_str().unwrap().to_owned();
    assert!(
        detail.len() <= 512 && detail.ends_with("..."),
        "{} bytes",
        detail.len()
    );
    assert!(detail.starts_with('ğ'));
}

/// A tool with a fixed JSON or text output, under a registry sized for the size tests.
fn sized_engine(
    clock: &SharedClock,
    kind: ToolKind,
    output: ToolOutput,
    max_result_bytes: usize,
) -> ToolEngine {
    engine_with(
        clock,
        7,
        EngineConfig {
            max_result_bytes,
            ..EngineConfig::default()
        },
        |registry| {
            registry
                .register(spec("out", any_params(), kind), Fixed::new(output))
                .unwrap();
        },
    )
}

#[tokio::test(start_paused = true)]
async fn json_and_text_results_are_limited_exactly_at_max_result_bytes() {
    let clock = clock();
    // A JSON string of n characters serializes to n + 2 bytes.
    let cases = [
        (ToolOutput::Json(json!("x".repeat(97))), true),
        (ToolOutput::Json(json!("x".repeat(98))), true),
        (ToolOutput::Json(json!("x".repeat(99))), false),
        (ToolOutput::Text("x".repeat(99)), true),
        (ToolOutput::Text("x".repeat(100)), true),
        (ToolOutput::Text("x".repeat(101)), false),
    ];
    for (n, (output, fits)) in cases.into_iter().enumerate() {
        let engine = sized_engine(&clock, ToolKind::ReadOnly, output.clone(), 100);
        let exec = execute(&engine, vec![call(1, 0, "out", json!({}))], 1).await;
        if fits {
            assert!(!result(&exec, 0).0, "case {n} ({output:?}) fits");
        } else {
            assert_eq!(error_kind(&exec, 0), "result_too_large", "case {n}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_side_effect_result_over_the_limit_is_cut_to_exactly_the_limit_keeping_its_prefix() {
    let clock = clock();
    let marker = "...[truncated]";
    // Json: serialized `"xxx...` (102 bytes), cut to 100 with the marker.
    let engine = sized_engine(
        &clock,
        ToolKind::SideEffect,
        ToolOutput::Json(json!("x".repeat(100))),
        100,
    );
    let exec = execute(&engine, vec![call(1, 0, "out", json!({}))], 1).await;
    let content = content_of(&exec, 0);
    assert!(!result(&exec, 0).0);
    assert_eq!(content.len(), 100);
    assert_eq!(content, format!("\"{}{marker}", "x".repeat(85)));

    // Text.
    let engine = sized_engine(
        &clock,
        ToolKind::SideEffect,
        ToolOutput::Text("y".repeat(1_000)),
        100,
    );
    let exec = execute(&engine, vec![call(1, 0, "out", json!({}))], 1).await;
    assert_eq!(content_of(&exec, 0), format!("{}{marker}", "y".repeat(86)));

    // Exactly at the limit: not cut.
    let engine = sized_engine(
        &clock,
        ToolKind::SideEffect,
        ToolOutput::Text("y".repeat(100)),
        100,
    );
    let exec = execute(&engine, vec![call(1, 0, "out", json!({}))], 1).await;
    assert_eq!(content_of(&exec, 0), "y".repeat(100));

    // Multi-byte text cut inside a character: valid UTF-8, within the limit, original prefix.
    let text = format!("a{}", "ş".repeat(100));
    let engine = sized_engine(&clock, ToolKind::SideEffect, ToolOutput::Text(text), 100);
    let exec = execute(&engine, vec![call(1, 0, "out", json!({}))], 1).await;
    let content = content_of(&exec, 0);
    assert!(content.len() <= 100, "{} bytes", content.len());
    assert_eq!(content, format!("a{}{marker}", "ş".repeat(42)));

    // Multi-byte JSON: the capped writer cuts inside a character too.
    let engine = sized_engine(
        &clock,
        ToolKind::SideEffect,
        ToolOutput::Json(json!("ş".repeat(100))),
        100,
    );
    let exec = execute(&engine, vec![call(1, 0, "out", json!({}))], 1).await;
    let content = content_of(&exec, 0);
    assert!(content.len() <= 100, "{} bytes", content.len());
    assert!(content.starts_with("\"ş") && content.ends_with(marker));
}

#[tokio::test(start_paused = true)]
async fn tools_receive_their_session_turn_call_and_the_turns_own_token() {
    let clock = clock();
    let read = CtxProbe::new(false);
    let write = CtxProbe::new(true);
    let engine = engine_with(&clock, 7, EngineConfig::default(), |registry| {
        registry
            .register(spec("read", any_params(), ToolKind::ReadOnly), read.clone())
            .unwrap();
        registry
            .register(
                spec("write", any_params(), ToolKind::SideEffect),
                write.clone(),
            )
            .unwrap();
    });
    let cancel = CancellationToken::new();
    let calls = vec![
        call(3, 5, "read", json!({})),
        call(3, 6, "write", json!({})),
    ];
    let trigger = cancel.clone();
    let clock2 = Arc::clone(&clock);
    let interrupter = async move {
        clock2.sleep(100 * MS).await;
        trigger.cancel();
    };
    let (exec, ()) = tokio::join!(engine.execute(calls, epoch(3), &cancel), interrupter);
    assert_eq!(exec.results.len(), 2);

    let seen = read.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].session, SessionId::new(7));
    assert_eq!(seen[0].epoch, epoch(3));
    assert_eq!(seen[0].call.as_str(), "t3.5");
    assert!(
        seen[0].cancel.is_cancelled(),
        "it is the turn's token, not a fresh one"
    );
    assert_eq!(
        seen[0].idempotency_key(),
        IdempotencyKey::new(SessionId::new(7), epoch(3), ToolCallId::new(epoch(3), 5))
    );

    settle_tasks().await;
    let seen = write.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].call.as_str(), "t3.6");
    assert!(
        write.saw_cancel.load(Ordering::SeqCst),
        "a SideEffect tool watching the turn's token sees the interruption"
    );
}

#[test]
fn the_idempotency_digest_is_stable_and_depends_on_every_part_of_the_key() {
    let key = |session, turn, seq| {
        IdempotencyKey::new(
            SessionId::new(session),
            epoch(turn),
            ToolCallId::new(epoch(turn), seq),
        )
    };
    // Pinned: providers see this value as an idempotency header, so it must not change.
    assert_eq!(key(7, 1, 0).digest(), 0xc6ad_cffd_0ae7_b855);
    assert_eq!(key(7, 1, 0).digest(), key(7, 1, 0).digest());
    let base = key(7, 1, 0).digest();
    assert_ne!(base, key(8, 1, 0).digest(), "session");
    assert_ne!(base, key(7, 2, 0).digest(), "epoch");
    assert_ne!(base, key(7, 1, 1).digest(), "call id");
    // The epoch of the key and the epoch inside the call id are separate inputs.
    let other = IdempotencyKey::new(SessionId::new(7), epoch(2), ToolCallId::new(epoch(1), 0));
    assert_ne!(base, other.digest());
}

#[tokio::test(start_paused = true)]
async fn a_failing_side_effect_is_recorded_and_a_replay_returns_the_error_without_retrying() {
    let clock = clock();
    let failing = Failing::new("the restaurant is closed");
    let engine = engine_with(&clock, 7, EngineConfig::default(), |registry| {
        registry
            .register(
                spec("book", any_params(), ToolKind::SideEffect),
                failing.clone(),
            )
            .unwrap();
    });
    let calls = vec![call(1, 0, "book", json!({}))];
    let first = execute(&engine, calls.clone(), 1).await;
    assert_eq!(error_kind(&first, 0), "tool_failed");
    assert_eq!(result(&first, 0).1["detail"], "the restaurant is closed");
    assert_eq!(failing.runs.load(Ordering::SeqCst), 1);
    let replay = execute(&engine, calls, 1).await;
    assert_eq!(replay.results[0], first.results[0], "the recorded error");
    assert_eq!(
        failing.runs.load(Ordering::SeqCst),
        1,
        "a failure is not retried"
    );
    // A retry needs a new call id (a new response).
    let retry = execute(&engine, vec![call(1, 1, "book", json!({}))], 1).await;
    assert_eq!(error_kind(&retry, 0), "tool_failed");
    assert_eq!(failing.runs.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn the_registry_accepts_valid_names_and_keeps_the_first_tool_on_a_duplicate() {
    let clock = clock();
    let mut registry = ToolRegistry::new();
    let probe = || Probe::new(&clock, "p", Duration::ZERO);
    let longest = "n".repeat(64);
    for name in ["get_weather", "book-table", "Table2", longest.as_str()] {
        registry
            .register(spec(name, any_params(), ToolKind::ReadOnly), probe())
            .unwrap_or_else(|error| panic!("{name:?} must be accepted: {error}"));
    }
    let too_long = "n".repeat(65);
    for bad in [
        too_long.as_str(),
        "rezervasyon_ı",
        "tablo\u{e7}",
        "a b",
        "a/b",
    ] {
        assert!(
            matches!(
                registry.register(spec(bad, any_params(), ToolKind::ReadOnly), probe()),
                Err(RegistryError::InvalidName(_))
            ),
            "{bad:?}"
        );
    }
    // A duplicate neither replaces the spec nor the tool.
    let original = registry.spec("get_weather").unwrap().clone();
    let replacement = ToolSpec::new(
        "get_weather",
        "something else",
        weather_params(),
        ToolKind::SideEffect,
    );
    assert_eq!(
        registry.register(replacement, probe()),
        Err(RegistryError::Duplicate("get_weather".to_owned()))
    );
    assert_eq!(registry.spec("get_weather").unwrap(), &original);
    assert_eq!(registry.len(), 4);
}

#[tokio::test(start_paused = true)]
async fn a_refunded_speculation_gives_its_malformed_round_back_exactly_once() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::ZERO);
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let cancel = CancellationToken::new();
    // A valid speculation spent nothing, so there is nothing to give back.
    let valid = engine
        .speculate(
            vec![call(1, 0, "weather", json!({ "city": "Ankara" }))],
            epoch(1),
            &cancel,
        )
        .await;
    assert!(!valid.spent_round());
    engine.refund_speculation(&valid);
    assert_eq!(engine.invalid_rounds(epoch(1)), 0);

    // A malformed one spent a round; refunding it restores the budget a plain run would have.
    let malformed = engine
        .speculate(vec![unparseable(1, 1, "weather")], epoch(1), &cancel)
        .await;
    assert!(malformed.spent_round());
    assert_eq!(engine.invalid_rounds(epoch(1)), 1);
    engine.refund_speculation(&malformed);
    assert_eq!(engine.invalid_rounds(epoch(1)), 0);
    // The budget never goes below zero.
    engine.refund_speculation(&malformed);
    assert_eq!(engine.invalid_rounds(epoch(1)), 0);

    // A newer turn is not charged for an older speculation's refund.
    let old = engine
        .speculate(vec![unparseable(1, 2, "weather")], epoch(1), &cancel)
        .await;
    let newer = engine
        .speculate(vec![unparseable(2, 0, "weather")], epoch(2), &cancel)
        .await;
    assert_eq!(engine.invalid_rounds(epoch(2)), 1);
    engine.refund_speculation(&old);
    assert_eq!(
        engine.invalid_rounds(epoch(2)),
        1,
        "a refund of turn 1 does not touch turn 2"
    );
    drop(newer);
}

#[tokio::test(start_paused = true)]
async fn a_speculative_run_cancelled_in_flight_spends_no_round() {
    let clock = clock();
    let weather = Probe::new(&clock, "weather", Duration::from_secs(5));
    let engine = engine(
        &clock,
        vec![(
            spec("weather", weather_params(), ToolKind::ReadOnly),
            weather.clone(),
        )],
        EngineConfig::default(),
    );
    let cancel = CancellationToken::new();
    // A malformed call next to a slow ReadOnly call: the future is dropped while the tool runs.
    let calls = vec![
        unparseable(1, 0, "weather"),
        call(1, 1, "weather", json!({ "city": "Ankara" })),
    ];
    let outcome = tokio::time::timeout(
        Duration::from_millis(100),
        engine.speculate(calls, epoch(1), &cancel),
    )
    .await;
    assert!(outcome.is_err(), "the speculative run was still in flight");
    assert_eq!(
        engine.invalid_rounds(epoch(1)),
        0,
        "a run that never finished gave its round back"
    );
}
