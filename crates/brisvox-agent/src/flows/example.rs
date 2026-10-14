//! An example flow: a restaurant reservation, in English and Turkish, over a small in-memory
//! restaurant. It exercises every feature of the flow module (tool-result transitions with rules,
//! a global tool, state capture and placeholders, all three context strategies, pre and post
//! actions) and is what the tests and the documentation use.
//!
//! ```text
//! greeting --check_availability--> confirm (ResetWithSummary) --make_reservation--> done (Reset, hang up)
//!    |   \                           ^   \
//!    |    '--not available--> alternatives (Append) --check_availability--'
//!    '-- end_call (any node) --> farewell (Append, hang up)
//! ```

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use super::{
    Action, Condition, ContextStrategy, Flow, FlowBuilder, FlowDefError, Node, Rule, ToolBinding,
};
use crate::segment::Language;
use crate::tools::{
    RegistryError, Tool, ToolCallCtx, ToolError, ToolFuture, ToolKind, ToolOutput, ToolRegistry,
    ToolSpec,
};

/// Times the in-memory restaurant offers, in order.
const SLOTS: [&str; 6] = ["18:00", "18:30", "19:00", "19:30", "20:00", "20:30"];
/// Largest party the restaurant seats.
const MAX_PARTY: u64 = 10;
/// Alternatives offered when a slot is not free.
const MAX_ALTERNATIVES: usize = 3;

/// A tool that does nothing and answers `{"ok": true}`: the body of a pure transition function
/// (the transition itself is the [`ToolBinding`]'s).
#[derive(Debug, Clone, Copy, Default)]
pub struct Ack;

impl Tool for Ack {
    fn call<'a>(&'a self, _args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async { Ok(ToolOutput::Json(json!({ "ok": true }))) })
    }
}

/// A booking the restaurant took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Booking {
    /// Confirmation code, `R-0001` and so on.
    pub code: String,
    /// Name the table is held under.
    pub name: String,
    /// Number of guests.
    pub party_size: u64,
    /// Date as the caller gave it.
    pub date: String,
    /// Time, `HH:MM`.
    pub time: String,
}

#[derive(Debug, Default)]
struct RestaurantState {
    /// Times that are always full.
    full_times: Vec<String>,
    bookings: Vec<Booking>,
}

/// A tiny restaurant: one table per (date, time) slot, some times always full. Deterministic,
/// no IO.
#[derive(Debug, Default)]
pub struct Restaurant {
    inner: Mutex<RestaurantState>,
}

impl Restaurant {
    /// A restaurant whose `full_times` (for example `"19:00"`) are never free.
    pub fn new(full_times: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(RestaurantState {
                full_times: full_times.iter().map(|time| (*time).to_owned()).collect(),
                bookings: Vec::new(),
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, RestaurantState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The bookings taken so far.
    pub fn bookings(&self) -> Vec<Booking> {
        self.lock().bookings.clone()
    }

    fn is_free(state: &RestaurantState, date: &str, time: &str) -> bool {
        SLOTS.contains(&time)
            && !state.full_times.iter().any(|full| full == time)
            && !state
                .bookings
                .iter()
                .any(|booking| booking.date == date && booking.time == time)
    }

    fn alternatives(state: &RestaurantState, date: &str) -> Vec<&'static str> {
        SLOTS
            .iter()
            .copied()
            .filter(|time| Self::is_free(state, date, time))
            .take(MAX_ALTERNATIVES)
            .collect()
    }

    fn check(&self, party_size: u64, date: &str, time: &str) -> Value {
        let state = self.lock();
        let available = party_size <= MAX_PARTY && Self::is_free(&state, date, time);
        json!({
            "available": available,
            "alternatives": if available { Vec::new() } else { Self::alternatives(&state, date) },
        })
    }

    fn book(&self, name: &str, party_size: u64, date: &str, time: &str) -> Value {
        let mut state = self.lock();
        if party_size > MAX_PARTY || !Self::is_free(&state, date, time) {
            return json!({
                "status": "unavailable",
                "alternatives": Self::alternatives(&state, date),
            });
        }
        let code = format!("R-{:04}", state.bookings.len().saturating_add(1));
        state.bookings.push(Booking {
            code: code.clone(),
            name: name.to_owned(),
            party_size,
            date: date.to_owned(),
            time: time.to_owned(),
        });
        json!({ "status": "confirmed", "code": code })
    }
}

/// `check_availability`: ReadOnly.
struct CheckAvailability(Arc<Restaurant>);

impl Tool for CheckAvailability {
    fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let (party, date, time) = booking_args(&args)?;
            Ok(ToolOutput::Json(self.0.check(party, date, time)))
        })
    }
}

/// `make_reservation`: SideEffect.
struct MakeReservation(Arc<Restaurant>);

impl Tool for MakeReservation {
    fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let (party, date, time) = booking_args(&args)?;
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::new("missing name"))?;
            Ok(ToolOutput::Json(self.0.book(name, party, date, time)))
        })
    }
}

/// The party size, date and time of validated arguments.
fn booking_args(args: &Value) -> Result<(u64, &str, &str), ToolError> {
    let party = args.get("party_size").and_then(Value::as_u64);
    let date = args.get("date").and_then(Value::as_str);
    let time = args.get("time").and_then(Value::as_str);
    match (party, date, time) {
        (Some(party), Some(date), Some(time)) => Ok((party, date, time)),
        _ => Err(ToolError::new("missing party_size, date or time")),
    }
}

fn booking_properties() -> Value {
    json!({
        "party_size": { "type": "integer", "minimum": 1, "maximum": 20 },
        "date": { "type": "string", "minLength": 1, "maxLength": 32 },
        "time": { "type": "string", "pattern": "^[0-2][0-9]:[0-5][0-9]$" },
    })
}

/// The registry of the example: `check_availability` (ReadOnly), `make_reservation` (SideEffect)
/// and `end_call` (ReadOnly, [`Ack`]), all over `restaurant`.
///
/// # Errors
/// [`RegistryError`] (not expected: the specs are fixed).
pub fn restaurant_registry(restaurant: &Arc<Restaurant>) -> Result<ToolRegistry, RegistryError> {
    let mut registry = ToolRegistry::new();
    registry.register(
        ToolSpec::new(
            "check_availability",
            "Checks whether a table is free for a party size, date and time (HH:MM).",
            json!({
                "type": "object",
                "properties": booking_properties(),
                "required": ["party_size", "date", "time"],
                "additionalProperties": false,
            }),
            ToolKind::ReadOnly,
        ),
        CheckAvailability(Arc::clone(restaurant)),
    )?;
    let mut properties = booking_properties();
    if let Some(map) = properties.as_object_mut() {
        map.insert(
            "name".to_owned(),
            json!({ "type": "string", "minLength": 1, "maxLength": 80 }),
        );
    }
    registry.register(
        ToolSpec::new(
            "make_reservation",
            "Books the table for the caller. Only call it after the caller confirmed the details.",
            json!({
                "type": "object",
                "properties": properties,
                "required": ["name", "party_size", "date", "time"],
                "additionalProperties": false,
            }),
            ToolKind::SideEffect,
        ),
        MakeReservation(Arc::clone(restaurant)),
    )?;
    registry.register(
        ToolSpec::new(
            "end_call",
            "Ends the call when the caller is done or wants to hang up.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            ToolKind::ReadOnly,
        ),
        Ack,
    )?;
    Ok(registry)
}

/// Why [`restaurant_flow`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExampleError {
    /// The example's tools did not register.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The example's flow did not validate.
    #[error(transparent)]
    Flow(#[from] FlowDefError),
}

/// The prompts of one language.
struct Texts {
    role: &'static str,
    greeting: &'static str,
    alternatives: &'static str,
    confirm: &'static str,
    confirm_say: &'static str,
    done: &'static str,
    farewell: &'static str,
}

const EN: Texts = Texts {
    role: "You are the phone receptionist of the restaurant Bella Tavola. Speak briefly and \
           naturally: this is a phone call. Never invent availability; use the tools.",
    greeting: "Greet the caller and ask for the party size, the date and the time they want. \
               As soon as you have all three, call check_availability.",
    alternatives: "The requested time is not free. Offer the caller these times: \
                   {availability.alternatives}. When the caller picks one, call check_availability \
                   again with it.",
    confirm: "A table is free. Read the details back: {party_size} guests on {date} at {time}. \
              Ask for the name for the booking. When the caller confirms and gives a name, call \
              make_reservation.",
    confirm_say: "One moment please.",
    done: "The booking is made: code {reservation.code} for {name}, {party_size} guests on {date} \
           at {time}. Tell the caller the code, thank them and say goodbye.",
    farewell: "Say a short goodbye.",
};

const TR: Texts = Texts {
    role: "Bella Tavola restoranının telefon görevlisisin. Kısa ve doğal konuş: bu bir telefon \
           görüşmesi. Müsaitlik uydurma; araçları kullan.",
    greeting: "Arayanı karşıla; kişi sayısını, tarihi ve saati sor. Üçünü de öğrenir öğrenmez \
               check_availability aracını çağır.",
    alternatives: "İstenen saat dolu. Arayana şu saatleri öner: {availability.alternatives}. \
                   Arayan birini seçince o saatle check_availability aracını yeniden çağır.",
    confirm: "Masa müsait. Ayrıntıları tekrar et: {date} tarihinde saat {time}, {party_size} kişi. \
              Rezervasyon için adı sor. Arayan onaylayıp adını verince make_reservation aracını \
              çağır.",
    confirm_say: "Bir saniye lütfen.",
    done: "Rezervasyon yapıldı: kod {reservation.code}, {name} adına, {date} tarihinde saat \
           {time}, {party_size} kişi. Arayana kodu söyle, teşekkür et ve vedalaş.",
    farewell: "Kısa bir veda cümlesi söyle.",
};

/// The `check_availability` binding shared by the nodes that check: remembers the request and the
/// answer, goes to `confirm` when the table is free and to `alternatives` when it is not.
fn check_binding() -> ToolBinding {
    ToolBinding::new("check_availability")
        .rules(vec![
            Rule::new(
                Condition::result_equals("/available", json!(true)),
                "confirm",
            ),
            Rule::new(
                Condition::result_equals("/available", json!(false)),
                "alternatives",
            ),
        ])
        .capture_args()
        .capture_result("availability")
}

/// The restaurant reservation flow in `language`, over `restaurant`. Its registry
/// ([`Flow::registry`]) is what the tool engine must run.
///
/// # Errors
/// [`ExampleError`] (not expected: the definition is fixed and tested).
pub fn restaurant_flow(
    language: Language,
    restaurant: &Arc<Restaurant>,
) -> Result<Flow, ExampleError> {
    let texts = match language {
        Language::En => &EN,
        Language::Tr => &TR,
    };
    let registry = Arc::new(restaurant_registry(restaurant)?);
    let flow = FlowBuilder::new("restaurant")
        .role(texts.role)
        .global_tool(ToolBinding::new("end_call").goto("farewell"))
        .node(
            Node::new("greeting")
                .task(texts.greeting)
                .tool(check_binding()),
        )
        .node(
            Node::new("alternatives")
                .task(texts.alternatives)
                .tool(check_binding()),
        )
        .node(
            Node::new("confirm")
                .task(texts.confirm)
                .with_strategy(ContextStrategy::ResetWithSummary)
                .with_keep_last_user(true)
                .pre_action(Action::Say(texts.confirm_say.into()))
                .tool(check_binding())
                .tool(
                    ToolBinding::new("make_reservation")
                        .rules(vec![
                            Rule::new(
                                Condition::result_equals("/status", json!("confirmed")),
                                "done",
                            ),
                            Rule::new(
                                Condition::result_equals("/status", json!("unavailable")),
                                "alternatives",
                            ),
                        ])
                        .capture_args()
                        .capture_result("reservation")
                        .capture_result("availability"),
                ),
        )
        .node(
            Node::new("done")
                .task(texts.done)
                .with_strategy(ContextStrategy::Reset)
                .with_keep_last_user(true)
                .post_action(Action::EndCall),
        )
        .node(
            Node::new("farewell")
                .task(texts.farewell)
                .post_action(Action::EndCall),
        )
        .build(registry)?;
    Ok(flow)
}
