//! The six scenarios. Each is one function over a [`Provider`]: scripted (exact assertions: the
//! script says what the model does, so everything the engine does is pinned) or live (loose
//! assertions: a real model varies, so only the properties that must hold whatever it chooses).

use std::collections::BTreeSet;
use std::time::Duration;

use crate::harness::{Env, FillerSetup, MS, Provider, Setup, check_context, span};
use crate::wire::{SIGNATURE, Turn, result_groups_on_wire, tool_results_on_wire};
use crate::world::{Delays, ToolEvent, Which};

const SYSTEM: &str = "You are a phone assistant for a small business. Speak in short plain sentences without markdown, lists or emoji. Use the tools to look things up instead of guessing. When several lookups do not depend on each other, request all of them in the same step.";

const SEC: Duration = Duration::from_secs(1);

fn norm(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn city(event: &ToolEvent) -> String {
    event.args["city"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase()
}

fn overlap(a: &ToolEvent, b: &ToolEvent) -> bool {
    let end = |e: &ToolEvent| e.finished.unwrap_or(Duration::MAX);
    a.started < end(b) && b.started < end(a)
}

/// The allowed distance between a time the test predicts and the one it sees.
fn tolerance(env: &Env) -> Duration {
    if env.paused { 30 * MS } else { 300 * MS }
}

/// The tool results the second and later requests carried on the provider's own wire, in order.
fn wire_results(env: &Env, request: usize) -> Vec<String> {
    let (Some(server), Some(dialect)) = (&env.server, env.dialect) else {
        return Vec::new();
    };
    server
        .requests()
        .get(request)
        .map(|body| tool_results_on_wire(dialect, body))
        .unwrap_or_default()
}

/// How the results of request `request` are grouped on the provider's wire (see
/// `wire::result_groups_on_wire`).
fn wire_groups(env: &Env, request: usize) -> Vec<usize> {
    let (Some(server), Some(dialect)) = (&env.server, env.dialect) else {
        return Vec::new();
    };
    server
        .requests()
        .get(request)
        .map(|body| result_groups_on_wire(dialect, body))
        .unwrap_or_default()
}

fn requests_made(env: &Env) -> usize {
    env.requests().len()
}

// ---- S1: three independent lookups in one response ------------------------------------------------

pub fn s1_setup() -> Setup {
    Setup {
        system: SYSTEM,
        which: Which::Weather,
        delays: Delays {
            weather: vec![
                ("Paris", 600 * MS),
                ("Tokyo", 400 * MS),
                ("Cairo", 200 * MS),
            ],
            default: 300 * MS,
            lookup: 300 * MS,
            booking: 300 * MS,
        },
        filler: FillerSetup::Default,
        tool_timeout: None,
    }
}

pub fn s1_script() -> Vec<Turn> {
    vec![
        Turn::calls(&[
            ("get_weather", r#"{"city":"Paris"}"#),
            ("get_weather", r#"{"city":"Tokyo"}"#),
            ("get_weather", r#"{"city":"Cairo"}"#),
        ]),
        Turn::say(
            "Paris is 14 degrees and rainy, Tokyo is 22 and clear, and Cairo is 33 and sunny.",
        ),
    ]
}

pub async fn s1(provider: Provider) {
    let env = Env::start(provider, &s1_setup()).await;
    env.ask(
        1,
        "What's the weather like in Paris, Tokyo and Cairo right now?",
        1,
        env.limit(),
    )
    .await;
    env.report("S1 three lookups at once");
    if env.limited() {
        return;
    }
    check_context(&env);
    let events = env.world.events_of("get_weather");
    let in_one_round = env
        .requests()
        .iter()
        .map(|r| r.tool_calls)
        .max()
        .unwrap_or(0);

    // Whatever the provider: the calls of one response run at the same time.
    if in_one_round >= 2 {
        assert!(events.len() >= 2, "{events:?}");
        assert!(
            events
                .iter()
                .enumerate()
                .any(|(i, a)| events.iter().skip(i + 1).any(|b| overlap(a, b))),
            "no two lookups overlapped: {events:?}"
        );
    }

    if env.live {
        let cities: BTreeSet<_> = events.iter().map(city).collect();
        for want in ["paris", "tokyo", "cairo"] {
            assert!(
                cities.contains(want),
                "{want} was never looked up: {events:?}"
            );
        }
        assert!(
            in_one_round >= 2,
            "the model asked for its lookups one by one ({in_one_round} in its largest response)"
        );
        let said = env.spoken_text().to_lowercase();
        for want in ["paris", "tokyo", "cairo"] {
            assert!(said.contains(want), "{want} missing from: {said}");
        }
        assert!(env.ended() >= 1 && requests_made(&env) <= 4);
        return;
    }

    // Exact.
    assert_eq!(requests_made(&env), 2);
    assert_eq!(env.ended(), 1, "one spoken answer: {:?}", env.rig.labels());
    assert_eq!(env.world.started("get_weather"), 3);
    assert_eq!(
        events.iter().map(city).collect::<Vec<_>>(),
        ["paris", "tokyo", "cairo"],
        "started in issue order"
    );
    // All three were running before the first finished: concurrent, not one after the other.
    let last_start = events.iter().map(|e| e.started).max().unwrap();
    let first_end = events.iter().filter_map(|e| e.finished).min().unwrap();
    assert!(last_start < first_end, "{events:?}");
    // Wall time of the tool phase is the longest lookup (600 ms), not the sum (1200 ms).
    let wall = span(&events);
    assert!(
        wall >= 600 * MS && wall < 600 * MS + tolerance(&env),
        "{wall:?}"
    );
    assert!(wall < 1_000 * MS);
    // The results reach the context in issue order although Cairo finished first.
    let results = env.tool_messages();
    assert_eq!(results.len(), 3, "{results:?}");
    for (result, want) in results.iter().zip(["Paris", "Tokyo", "Cairo"]) {
        assert!(!result.0 && result.1.contains(want), "{results:?}");
    }
    assert_eq!(env.call_groups(), [vec!["get_weather"; 3]]);
    let stats = env.rig.stats.snapshot();
    assert_eq!(
        (stats.tool_rounds, stats.tool_calls, stats.fillers),
        (1, 3, 0)
    );
    assert_eq!(
        norm(&env.spoken_text()),
        "Paris is 14 degrees and rainy, Tokyo is 22 and clear, and Cairo is 33 and sunny."
    );
    // The second request carried the three results, in order, on the provider's own wire.
    let second = &env.requests()[1].request.messages;
    assert_eq!(
        second.iter().filter(|m| m.tool_call_id.is_some()).count(),
        3
    );
    if env.dialect.is_some() {
        let wire = wire_results(&env, 1);
        assert_eq!(wire.len(), 3, "{wire:?}");
        for (result, want) in wire.iter().zip(["Paris", "Tokyo", "Cairo"]) {
            assert!(result.contains(want), "{wire:?}");
        }
        // The three results of the one response sit together, as every provider requires.
        assert_eq!(wire_groups(&env, 1), [3]);
    }
    if env.dialect == Some(crate::wire::Dialect::Gemini) {
        // The thought signature of the calls is echoed with them.
        let body = env.server.as_ref().unwrap().requests()[1].to_string();
        assert!(body.contains(SIGNATURE), "{body}");
    }
}

// ---- S2: a chain of three rounds ------------------------------------------------------------------

pub fn s2_setup() -> Setup {
    Setup {
        system: SYSTEM,
        which: Which::Chain,
        delays: Delays::flat(200 * MS),
        filler: FillerSetup::Default,
        tool_timeout: None,
    }
}

pub fn s2_script() -> Vec<Turn> {
    vec![
        Turn::default()
            .and_say("Let me look that up. ")
            .and_call("get_order", r#"{"order_id":"A-1042"}"#),
        Turn::call("get_customer", r#"{"customer_id":"C-77"}"#),
        Turn::call("get_weather", r#"{"city":"Izmir"}"#),
        Turn::say(
            "Your espresso machine, order A-1042, is going to Ayse Demir in Izmir, where it is 27 degrees and sunny.",
        ),
    ]
}

pub async fn s2(provider: Provider) {
    let env = Env::start(provider, &s2_setup()).await;
    env.ask(
        1,
        "Where is order A-1042 going, and what's the weather like there?",
        1,
        env.limit(),
    )
    .await;
    env.report("S2 three-round chain");
    if env.limited() {
        return;
    }
    check_context(&env);
    let events = env.world.events();
    let order: Vec<&str> = events.iter().map(|e| e.tool.as_str()).collect();
    let stats = env.rig.stats.snapshot();

    if env.live {
        // The chain happened in order, each step fed by the one before.
        assert_eq!(
            order.first().copied(),
            Some("get_order"),
            "the order is looked up first: {events:?}"
        );
        let customer = events
            .iter()
            .find(|e| e.tool == "get_customer")
            .expect("get_customer was called");
        assert_eq!(customer.args["customer_id"], "C-77", "{events:?}");
        let weather = events
            .iter()
            .find(|e| e.tool == "get_weather")
            .expect("get_weather was called");
        assert_eq!(city(weather), "izmir", "{events:?}");
        assert!(
            stats.tool_rounds >= 3 && stats.tool_rounds <= 5,
            "{stats:?}"
        );
        assert!(env.spoken_text().to_lowercase().contains("izmir"));
        assert!(requests_made(&env) <= 7);
        return;
    }

    assert_eq!(order, ["get_order", "get_customer", "get_weather"]);
    assert_eq!(requests_made(&env), 4, "three tool rounds and the answer");
    assert_eq!(
        (stats.tool_rounds, stats.tool_calls, stats.fillers),
        (3, 3, 0)
    );
    // Each step started after the one before had finished: a chain, not a fan-out.
    for pair in events.windows(2) {
        assert!(pair[1].started >= pair[0].finished.unwrap(), "{events:?}");
    }
    assert_eq!(
        env.call_groups(),
        [vec!["get_order"], vec!["get_customer"], vec!["get_weather"]]
    );
    // Every request carried the results so far.
    let carried: Vec<usize> = env
        .requests()
        .iter()
        .map(|r| {
            r.request
                .messages
                .iter()
                .filter(|m| m.tool_call_id.is_some())
                .count()
        })
        .collect();
    assert_eq!(carried, [0, 1, 2, 3]);
    if env.dialect.is_some() {
        let lens: Vec<usize> = (0..4).map(|i| wire_results(&env, i).len()).collect();
        assert_eq!(lens, [0, 1, 2, 3]);
        assert_eq!(wire_groups(&env, 3), [1, 1, 1], "one result per round");
        let last = wire_results(&env, 3);
        assert!(last[0].contains("C-77") && last[1].contains("Izmir") && last[2].contains("27"));
    }
    assert_eq!(
        norm(&env.spoken_text()),
        "Let me look that up. Your espresso machine, order A-1042, is going to Ayse Demir in Izmir, where it is 27 degrees and sunny."
    );
    assert_eq!(env.ended(), 1);
}

// ---- S3: a booking next to a lookup, and the model repeats the booking ----------------------------

pub fn s3_setup() -> Setup {
    Setup {
        system: SYSTEM,
        which: Which::Booking,
        delays: Delays {
            weather: Vec::new(),
            default: 200 * MS,
            lookup: 200 * MS,
            booking: 400 * MS,
        },
        filler: FillerSetup::Default,
        tool_timeout: None,
    }
}

const BOOKING: &str = r#"{"name":"Kaya","party_size":2,"time":"19:30"}"#;

pub fn s3_script() -> Vec<Turn> {
    vec![
        Turn::default()
            .and_say("Booking that now. ")
            .and_call("book_table", BOOKING)
            .and_call("get_weather", r#"{"city":"Paris"}"#),
        // The model forgets it has the confirmation and books again.
        Turn::call("book_table", BOOKING),
        Turn::say(
            "Your table for two at 19:30 is booked under Kaya, confirmation BK-1001. Paris is 14 degrees and rainy.",
        ),
    ]
}

pub async fn s3(provider: Provider) {
    let env = Env::start(provider, &s3_setup()).await;
    env.ask(
        1,
        "Book a table for two at 7:30 pm under the name Kaya, and tell me the weather in Paris.",
        1,
        env.limit(),
    )
    .await;
    env.report("S3 booking next to a lookup");
    if env.limited() {
        return;
    }
    check_context(&env);
    let results = env.tool_messages();
    let context = format!("{:?}", env.rig.conversation());

    // Whatever the model does: the booking happens at most once, and not twice for one request.
    assert_eq!(
        env.world.bookings.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the table was booked more than once: {context}"
    );
    assert!(!context.contains("BK-1002"), "{context}");

    if env.live {
        assert!(env.world.started("get_weather") >= 1);
        let said = env.spoken_text().to_lowercase();
        assert!(
            said.contains("1001") || said.contains("booked") || said.contains("confirm"),
            "{said}"
        );
        assert!(requests_made(&env) <= 6);
        return;
    }

    assert_eq!(requests_made(&env), 3);
    assert_eq!(env.world.started("book_table"), 1);
    assert_eq!(env.world.started("get_weather"), 1);
    // The three results, in issue order: the booking, the weather, and the refusal of the repeat.
    assert_eq!(results.len(), 3, "{results:?}");
    assert!(
        !results[0].0 && results[0].1.contains("BK-1001"),
        "{results:?}"
    );
    assert!(
        !results[1].0 && results[1].1.contains("Paris"),
        "{results:?}"
    );
    assert!(
        results[2].0 && results[2].1.contains("already_done"),
        "the repeat is refused, not run: {results:?}"
    );
    assert_eq!(
        env.call_groups(),
        [vec!["book_table", "get_weather"], vec!["book_table"]]
    );
    assert_eq!(env.tool_stats().side_effect_repeats, 1);
    assert_eq!(env.ended(), 1);
    assert!(env.spoken_text().contains("BK-1001"));
    if env.dialect.is_some() {
        let wire = wire_results(&env, 2);
        assert_eq!(wire.len(), 3, "{wire:?}");
        assert!(wire[2].contains("already_done"), "{wire:?}");
        assert_eq!(wire_groups(&env, 2), [2, 1], "{wire:?}");
    }
}

// ---- S4: arguments that break the schema ------------------------------------------------------------

pub fn s4_setup() -> Setup {
    Setup {
        system: SYSTEM,
        which: Which::Forecast,
        delays: Delays::flat(100 * MS),
        filler: FillerSetup::Default,
        tool_timeout: None,
    }
}

pub fn s4_script() -> Vec<Turn> {
    vec![
        // Five days: the schema allows three.
        Turn::call("get_forecast", r#"{"city":"Izmir","days":5}"#),
        Turn::call("get_forecast", r#"{"city":"Izmir","days":3}"#),
        Turn::say("Izmir will be sunny at 25 degrees for the next three days."),
    ]
}

pub async fn s4(provider: Provider) {
    let env = Env::start(provider, &s4_setup()).await;
    env.ask(
        1,
        "Give me the forecast for Izmir for the next five days.",
        1,
        env.limit(),
    )
    .await;
    env.report("S4 invalid arguments, then a retry");
    if env.limited() {
        return;
    }
    check_context(&env);
    let results = env.tool_messages();
    let events = env.world.events_of("get_forecast");

    // A call that breaks the schema never reaches the tool, whatever the model sends.
    for e in &events {
        let days = e.args["days"].as_i64().unwrap_or(0);
        assert!((1..=3).contains(&days), "an invalid call ran: {events:?}");
    }
    if env.live {
        let invalid = results
            .iter()
            .filter(|r| r.0 && r.1.contains("invalid_arguments"))
            .count();
        eprintln!("  live: {invalid} invalid call(s) refused by the engine");
        assert!(
            !events.is_empty(),
            "no valid forecast call ever ran: {results:?}"
        );
        assert!(requests_made(&env) <= 6);
        assert!(
            env.spoken_text().to_lowercase().contains("izmir")
                || env.spoken_text().to_lowercase().contains("sunny")
        );
        return;
    }

    assert_eq!(requests_made(&env), 3);
    assert_eq!(events.len(), 1, "only the valid retry ran: {events:?}");
    assert_eq!(results.len(), 2);
    assert!(results[0].0, "{results:?}");
    let error: serde_json::Value = serde_json::from_str(&results[0].1).unwrap();
    assert_eq!(error["error"], "invalid_arguments");
    assert_eq!(error["tool"], "get_forecast");
    let issues = error["issues"].as_array().unwrap();
    assert!(
        issues
            .iter()
            .any(|i| i["path"] == "/days" && i["reason"].as_str().unwrap().contains("maximum")),
        "{error}"
    );
    assert!(
        !results[1].0 && results[1].1.contains("sunny"),
        "{results:?}"
    );
    assert_eq!(
        env.engine.invalid_rounds(brisvox_core::TurnEpoch::new(1)),
        1
    );
    assert_eq!(env.rig.stats.snapshot().fallbacks, 0);
    assert_eq!(env.ended(), 1);
    if env.dialect.is_some() {
        let wire = wire_results(&env, 1);
        assert!(
            wire.len() == 1 && wire[0].contains("invalid_arguments"),
            "{wire:?}"
        );
    }
}

/// S4b: the model never gets it right. The retries are bounded and the caller hears a fallback.
pub async fn s4b(provider: Provider) {
    let env = Env::start(provider, &s4_setup()).await;
    env.ask(
        1,
        "Give me the forecast for Izmir for the next five days.",
        1,
        env.limit(),
    )
    .await;
    env.report("S4b invalid arguments forever");
    check_context(&env);
    assert_eq!(requests_made(&env), 3, "the third refusal ends the loop");
    assert_eq!(env.world.started("get_forecast"), 0, "nothing invalid ran");
    let stats = env.rig.stats.snapshot();
    assert_eq!((stats.tool_budget_exhausted, stats.fallbacks), (1, 1));
    let results = env.tool_messages();
    assert_eq!(results.len(), 3);
    assert!(
        results[0].1.contains("invalid_arguments") && results[1].1.contains("invalid_arguments")
    );
    assert!(results[2].1.contains("budget_exhausted"), "{results:?}");
    assert_eq!(env.ended(), 1);
    assert!(
        !env.spoken_text().trim().is_empty(),
        "the caller hears something"
    );
}

pub fn s4b_script() -> Vec<Turn> {
    vec![
        Turn::call("get_forecast", r#"{"city":"Izmir","days":5}"#),
        Turn::call("get_forecast", r#"{"city":"Izmir","days":9}"#),
        Turn::call("get_forecast", r#"{"city":"Izmir","days":7}"#),
        Turn::say("This answer must never be asked for."),
    ]
}

// ---- S5: a slow tool and the filler -------------------------------------------------------------------

/// Slow tool and filler delay: the paused runs use the numbers of the design (2 s tool, 1.2 s
/// filler); real-time runs are scaled down so the suite stays fast.
fn slow(paused: bool) -> (Duration, Duration) {
    if paused {
        (2 * SEC, 1_200 * MS)
    } else {
        (1_000 * MS, 400 * MS)
    }
}

pub fn s5_setup(paused: bool) -> Setup {
    let (tool, filler) = slow(paused);
    Setup {
        system: SYSTEM,
        which: Which::Weather,
        delays: Delays::flat(tool),
        filler: FillerSetup::Fixed(filler),
        tool_timeout: None,
    }
}

pub fn s5_script() -> Vec<Turn> {
    vec![
        Turn::default()
            .and_say("Let me check. ")
            .and_call("get_weather", r#"{"city":"Izmir"}"#),
        Turn::say("It is 27 degrees and sunny in Izmir."),
    ]
}

pub async fn s5(provider: Provider) {
    let paused = provider.shape.is_some_and(crate::harness::Shape::paused);
    let setup = s5_setup(paused);
    let env = Env::start(provider, &setup).await;
    let (tool, filler) = slow(paused);
    env.ask(1, "What's the weather in Izmir?", 1, env.limit())
        .await;
    env.report("S5 slow tool, then the answer");
    if env.limited() {
        return;
    }
    check_context(&env);
    let events = env.world.events_of("get_weather");
    let started = events.first().expect("the tool ran").started;
    let spoken = env.spoken();
    let at = spoken
        .iter()
        .position(|t| t.contains("One moment, please."));
    let filler_at = env.first_seen("text:One moment, please.");
    let stats = env.rig.stats.snapshot();

    // Exactly one filler for the slow round, spoken before the answer and before the tool ended.
    assert_eq!(stats.fillers, 1, "{spoken:?}");
    let at = at.expect("the filler was spoken");
    assert!(
        at + 1 < spoken.len(),
        "the answer follows the filler: {spoken:?}"
    );
    let filler_at = filler_at.unwrap();
    assert!(
        filler_at + 10 * MS >= started + filler,
        "{filler_at:?} vs {started:?}"
    );
    assert!(
        filler_at < started + filler + tolerance(&env),
        "{filler_at:?} vs {started:?}"
    );
    let finished = events[0].finished.unwrap();
    assert!(
        filler_at < finished && finished >= started + tool,
        "{events:?}"
    );

    if env.live {
        assert!(
            env.spoken_text().to_lowercase().contains("izmir") || env.spoken_text().contains("27")
        );
        assert!(requests_made(&env) <= 4);
        return;
    }
    assert_eq!(requests_made(&env), 2);
    assert_eq!(
        norm(&env.spoken_text()),
        "Let me check. One moment, please.It is 27 degrees and sunny in Izmir.",
        "{spoken:?}"
    );
    assert_eq!(events.len(), 1);
    assert_eq!(env.ended(), 1);
}

/// S5b: three slow calls of one response get one filler, not three.
pub fn s5b_setup(paused: bool) -> Setup {
    let (tool, filler) = slow(paused);
    Setup {
        system: SYSTEM,
        which: Which::Weather,
        delays: Delays {
            weather: vec![
                ("Paris", tool),
                ("Tokyo", tool + tool / 4),
                ("Cairo", tool + tool / 2),
            ],
            default: tool,
            lookup: tool,
            booking: tool,
        },
        filler: FillerSetup::Fixed(filler),
        tool_timeout: None,
    }
}

pub async fn s5b(provider: Provider) {
    let paused = provider.shape.is_some_and(crate::harness::Shape::paused);
    let env = Env::start(provider, &s5b_setup(paused)).await;
    let (tool, filler) = slow(paused);
    env.ask(
        1,
        "What's the weather like in Paris, Tokyo and Cairo right now?",
        1,
        env.limit(),
    )
    .await;
    env.report("S5b three slow calls");
    check_context(&env);
    let events = env.world.events_of("get_weather");
    assert_eq!(events.len(), 3);
    assert_eq!(env.rig.stats.snapshot().fillers, 1, "{:?}", env.spoken());
    let spoken = env.spoken();
    assert_eq!(
        spoken
            .iter()
            .filter(|t| t.contains("One moment, please."))
            .count(),
        1
    );
    // Concurrent: the phase lasts as long as the slowest call, not the sum.
    let wall = span(&events);
    assert!(
        wall >= tool + tool / 2 && wall < tool + tool / 2 + tolerance(&env),
        "{wall:?}"
    );
    let filler_at = env.first_seen("text:One moment, please.").unwrap();
    let begun = events.iter().map(|e| e.started).min().unwrap();
    assert!(filler_at + 10 * MS >= begun + filler && filler_at < begun + filler + tolerance(&env));
    assert_eq!(env.ended(), 1);
}

pub fn s5b_script() -> Vec<Turn> {
    s1_script()
}

// ---- S6: the caller cuts in while the tools run ----------------------------------------------------------

pub fn s6_setup() -> Setup {
    Setup {
        system: SYSTEM,
        which: Which::Booking,
        delays: Delays {
            // The lookup is slow enough to still be running when the caller cuts in.
            weather: Vec::new(),
            default: 5 * SEC,
            lookup: 5 * SEC,
            booking: 600 * MS,
        },
        filler: FillerSetup::Default,
        tool_timeout: None,
    }
}

pub fn s6_script() -> Vec<Turn> {
    vec![
        Turn::default()
            .and_say("On it. ")
            .and_call("book_table", BOOKING)
            .and_call("get_weather", r#"{"city":"Paris"}"#),
        Turn::say("Yes, your table is booked, confirmation BK-1001."),
    ]
}

pub async fn s6(provider: Provider) {
    let env = Env::start(provider, &s6_setup()).await;
    env.say(
        1,
        "Book a table for two at 7:30 pm under the name Kaya, and check the weather in Paris.",
    )
    .await;
    // Wait until the model's calls are running, then cut in.
    // (The calls of one response start together, so one start in a live run means its siblings
    // have started too.)
    env.wait("the tools to start", env.limit(), |e| {
        let running = e.world.events().len();
        if e.live { running >= 1 } else { running >= 2 }
    })
    .await;
    if env.limited() {
        return;
    }
    env.pass(250 * MS).await;
    env.rig.interrupt(2).await;
    env.pass(50 * MS).await;
    let booking_started = env.world.started("book_table") >= 1;
    if booking_started {
        // The booking finishes by itself; the lookup does not (cancelled).
        env.wait("the booking to finish", env.limit(), |e| {
            e.world.finished("book_table") >= 1
        })
        .await;
    }
    env.pass(200 * MS).await;
    env.ask(2, "Sorry, did the table get booked?", 1, env.limit())
        .await;
    env.report("S6 barge-in during the tools");
    if env.limited() {
        return;
    }
    check_context(&env);
    let weather = env.world.events_of("get_weather");
    let bookings = env.world.bookings.load(std::sync::atomic::Ordering::SeqCst);
    let context = env.rig.conversation();
    let joined = format!("{context:?}");

    // Whatever the model did: the booking ran at most once, and a cancelled lookup never finished.
    assert!(bookings <= 1, "{joined}");
    for e in &weather {
        assert!(
            e.finished.is_none() && e.dropped,
            "the lookup was not cancelled: {weather:?}"
        );
    }
    if booking_started {
        assert_eq!(bookings, 1, "{joined}");
        let notes = context
            .iter()
            .filter(|m| m.starts_with("system:") && m.contains("BK-1001"))
            .count();
        assert_eq!(notes, 1, "the result is reported once, as a note: {joined}");
        let note = context
            .iter()
            .position(|m| m.starts_with("system:") && m.contains("BK-1001"))
            .unwrap();
        let user2 = context
            .iter()
            .position(|m| m == "user:Sorry, did the table get booked?")
            .unwrap();
        assert!(
            note < user2,
            "the note comes before the caller's next words: {joined}"
        );
        // ... and the model saw it in its next request.
        let seen = env
            .requests()
            .last()
            .unwrap()
            .request
            .messages
            .iter()
            .any(|m: &brisvox_providers::Message| m.content.contains("BK-1001"));
        assert!(seen, "the next request did not carry the booking's result");
    }
    if env.live {
        eprintln!("  live: answer after the barge-in: {:?}", env.spoken_text());
        assert!(env.ended() >= 1 && requests_made(&env) <= 5);
        return;
    }
    assert_eq!(requests_made(&env), 2);
    assert_eq!(env.world.started("book_table"), 1);
    assert_eq!(weather.len(), 1);
    let tools = env.tool_stats();
    assert_eq!(
        (
            tools.read_only_cancelled,
            tools.side_effect_interrupted,
            tools.side_effect_late_results
        ),
        (1, 1, 1),
        "{tools:?}"
    );
    assert_eq!(env.rig.stats.snapshot().late_notes, 1);
    let closed = env
        .tool_messages()
        .iter()
        .filter(|m| m.0 && m.1.contains("interrupted"))
        .count();
    assert_eq!(
        closed,
        2,
        "both open calls were closed: {:?}",
        env.tool_messages()
    );
    // The history the model sees: the interrupted turn with both calls closed, the note, the
    // caller's new words, the answer.
    let shape: Vec<&str> = context
        .iter()
        .map(|m| {
            if m.starts_with("system:Late result") {
                "note"
            } else if m.starts_with("system:") {
                "system"
            } else if m.starts_with("user:") {
                "user"
            } else if m.starts_with("assistant+calls") {
                "calls"
            } else if m.starts_with("tool!:") {
                "closed"
            } else {
                "assistant"
            }
        })
        .collect();
    assert_eq!(
        shape,
        [
            "system",
            "user",
            "calls",
            "closed",
            "closed",
            "note",
            "user",
            "assistant"
        ],
        "{joined}"
    );
    if env.dialect.is_some() {
        let body = env.server.as_ref().unwrap().requests()[1].to_string();
        assert!(body.contains("BK-1001"));
    }
}

// ---- S4c: some calls of a response are broken, the others still run ------------------------------------

pub fn s4c_script() -> Vec<Turn> {
    vec![
        Turn::calls(&[
            ("get_weather", r#"{"city":"Paris"}"#),
            // `town` is not a property of the tool.
            ("get_weather", r#"{"town":"Tokyo"}"#),
            ("no_such_tool", "{}"),
            ("get_weather", r#"{"city":"Cairo"}"#),
        ]),
        Turn::say(
            "Paris is 14 degrees and rainy and Cairo is 33 and sunny. I could not look up Tokyo.",
        ),
    ]
}

pub async fn s4c(provider: Provider) {
    let env = Env::start(provider, &s1_setup()).await;
    env.ask(
        1,
        "What's the weather like in Paris, Tokyo and Cairo right now?",
        1,
        env.limit(),
    )
    .await;
    env.report("S4c broken calls among good ones");
    check_context(&env);
    let results = env.tool_messages();
    let events = env.world.events_of("get_weather");

    assert_eq!(requests_made(&env), 2);
    assert_eq!(results.len(), 4, "{results:?}");
    // In issue order: good, schema violation, unknown tool, good.
    assert!(
        !results[0].0 && results[0].1.contains("Paris"),
        "{results:?}"
    );
    assert!(
        results[1].0 && results[1].1.contains("invalid_arguments"),
        "{results:?}"
    );
    assert!(
        results[2].0 && results[2].1.contains("unknown_tool"),
        "{results:?}"
    );
    assert!(
        !results[3].0 && results[3].1.contains("Cairo"),
        "{results:?}"
    );
    // The two good calls ran, together; the broken ones never did.
    let cities: Vec<_> = events.iter().map(city).collect();
    assert_eq!(cities, ["paris", "cairo"], "{events:?}");
    assert!(overlap(&events[0], &events[1]), "{events:?}");
    // One round with broken calls spends one unit of the retry budget, however many are broken.
    assert_eq!(
        env.engine.invalid_rounds(brisvox_core::TurnEpoch::new(1)),
        1
    );
    assert_eq!(env.rig.stats.snapshot().fallbacks, 0);
    assert_eq!(env.ended(), 1);
    if env.dialect.is_some() {
        let wire = wire_results(&env, 1);
        assert_eq!(wire.len(), 4, "{wire:?}");
        assert!(
            wire[1].contains("invalid_arguments") && wire[2].contains("unknown_tool"),
            "{wire:?}"
        );
        assert_eq!(wire_groups(&env, 1), [4]);
    }
}

// ---- S5c: a tool that does not answer in time ---------------------------------------------------------------

pub fn s5c_setup() -> Setup {
    Setup {
        system: SYSTEM,
        which: Which::Weather,
        // Takes far longer than the tool is allowed to.
        delays: Delays::flat(5 * SEC),
        // The filler comes before the timeout.
        filler: FillerSetup::Fixed(300 * MS),
        tool_timeout: Some(600 * MS),
    }
}

pub fn s5c_script() -> Vec<Turn> {
    vec![
        Turn::default()
            .and_say("Let me check. ")
            .and_call("get_weather", r#"{"city":"Izmir"}"#),
        Turn::say("Sorry, the weather service is not answering right now."),
    ]
}

pub async fn s5c(provider: Provider) {
    let env = Env::start(provider, &s5c_setup()).await;
    env.ask(1, "What's the weather in Izmir?", 1, env.limit())
        .await;
    env.report("S5c tool timeout");
    check_context(&env);
    let events = env.world.events_of("get_weather");
    let results = env.tool_messages();
    assert_eq!(requests_made(&env), 2);
    assert_eq!(events.len(), 1);
    // The future was dropped at the timeout; the tool never answered.
    assert!(
        events[0].dropped && events[0].finished.is_none(),
        "{events:?}"
    );
    assert_eq!(results.len(), 1);
    assert!(
        results[0].0 && results[0].1.contains("timeout"),
        "{results:?}"
    );
    assert_eq!(env.rig.stats.snapshot().fillers, 1);
    assert_eq!(env.rig.stats.snapshot().fallbacks, 0);
    // The model is told, and says so; the round took the timeout, not the tool's 5 s.
    assert!(env.spoken_text().contains("Sorry"), "{:?}", env.spoken());
    let second = env.requests()[1].sent;
    assert!(
        second >= 600 * MS && second < 600 * MS + tolerance(&env),
        "{second:?}"
    );
    assert_eq!(env.ended(), 1);
}
