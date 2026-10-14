//! The tools of the scenarios and what they did.
//!
//! Every tool sleeps on the injected clock (simulated time on a paused runtime, real time
//! otherwise), records when it started and finished (or that its future was dropped before it
//! finished: a cancelled ReadOnly call), and answers from a small fixed world.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_agent::tools::{
    EngineConfig as ToolEngineConfig, SessionId, Tool, ToolCallCtx, ToolEngine, ToolError,
    ToolFuture, ToolKind, ToolOutput, ToolRegistry, ToolSpec,
};
use brisvox_core::{Clock, SharedClock, SimClock};
use serde_json::{Value, json};

/// One run of a tool.
#[derive(Debug, Clone)]
pub struct ToolEvent {
    pub tool: String,
    pub args: Value,
    /// Since the world was created.
    pub started: Duration,
    /// `None` while running, or when the future was dropped.
    pub finished: Option<Duration>,
    /// The future was dropped before it finished (the engine cancelled a ReadOnly call).
    pub dropped: bool,
}

/// Which tools a scenario offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Weather,
    Chain,
    Booking,
    Forecast,
    /// Three independent lookups about one sentence (the speech-to-speech scenarios).
    Geo,
    /// A chain from a continent to the weather in a city (the speech-to-speech scenarios).
    GeoChain,
}

/// How long the tools take.
#[derive(Debug, Clone)]
pub struct Delays {
    /// `get_weather`, by city (`default` for any other).
    pub weather: Vec<(&'static str, Duration)>,
    pub default: Duration,
    /// `get_order`, `get_customer`, `get_forecast`.
    pub lookup: Duration,
    /// `book_table`.
    pub booking: Duration,
}

impl Delays {
    pub fn flat(d: Duration) -> Self {
        Self {
            weather: Vec::new(),
            default: d,
            lookup: d,
            booking: d,
        }
    }
}

pub struct World {
    clock: SimClock,
    delays: Delays,
    events: Mutex<Vec<ToolEvent>>,
    /// `book_table` bodies that ran.
    pub bookings: AtomicUsize,
}

impl World {
    pub fn new(clock: SimClock, delays: Delays) -> Arc<Self> {
        Arc::new(Self {
            clock,
            delays,
            events: Mutex::default(),
            bookings: AtomicUsize::new(0),
        })
    }

    pub fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    pub fn events(&self) -> Vec<ToolEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn events_of(&self, tool: &str) -> Vec<ToolEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.tool == tool)
            .collect()
    }

    pub fn started(&self, tool: &str) -> usize {
        self.events_of(tool).len()
    }

    pub fn finished(&self, tool: &str) -> usize {
        self.events_of(tool)
            .iter()
            .filter(|e| e.finished.is_some())
            .count()
    }

    fn begin(&self, tool: &str, args: &Value) -> usize {
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        events.push(ToolEvent {
            tool: tool.to_owned(),
            args: args.clone(),
            started: self.now(),
            finished: None,
            dropped: false,
        });
        events.len() - 1
    }

    fn delay(&self, tool: &str, args: &Value) -> Duration {
        match tool {
            "get_weather" => {
                let city = args["city"].as_str().unwrap_or_default();
                self.delays
                    .weather
                    .iter()
                    .find(|(c, _)| *c == city)
                    .map_or(self.delays.default, |(_, d)| *d)
            }
            "book_table" => self.delays.booking,
            _ => self.delays.lookup,
        }
    }

    fn answer(&self, tool: &str, args: &Value) -> Result<Value, String> {
        let text = |key: &str| args[key].as_str().unwrap_or_default().to_owned();
        match tool {
            "get_weather" => match text("city").as_str() {
                "Paris" => Ok(json!({"city": "Paris", "temp_c": 14, "sky": "rainy"})),
                "Tokyo" => Ok(json!({"city": "Tokyo", "temp_c": 22, "sky": "clear"})),
                "Cairo" => Ok(json!({"city": "Cairo", "temp_c": 33, "sky": "sunny"})),
                "Izmir" => Ok(json!({"city": "Izmir", "temp_c": 27, "sky": "sunny"})),
                "Algiers" => Ok(json!({"city": "Algiers", "temp_c": 25, "sky": "sunny"})),
                other => Err(format!("no weather station in {other}")),
            },
            "get_order" => match text("order_id").as_str() {
                "A-1042" => Ok(json!({"order_id": "A-1042", "customer_id": "C-77",
                                      "item": "espresso machine", "status": "shipped"})),
                other => Err(format!("no order {other}")),
            },
            "get_customer" => match text("customer_id").as_str() {
                "C-77" => Ok(json!({"customer_id": "C-77", "name": "Ayse Demir",
                                    "city": "Izmir"})),
                other => Err(format!("no customer {other}")),
            },
            "lookup_region" => Ok(json!({"region": text("name"),
                "countries": ["Madagascar", "Comoros", "Seychelles", "Mauritius"]})),
            "lookup_ocean" => Ok(json!({"ocean": text("name"), "area_million_km2": 70.5,
                "deepest_point": "Java Trench"})),
            "lookup_continent" => Ok(json!({"continent": text("name"),
                "population_billion": 1.5, "countries": 54})),
            "get_largest_country" => Ok(json!({"continent": text("continent"),
                "largest_country": "Algeria"})),
            "get_capital" => Ok(json!({"country": text("country"), "capital": "Algiers"})),
            "get_forecast" => Ok(json!({"city": text("city"), "days": args["days"],
                                        "forecast": "sunny, 25 degrees every day"})),
            "book_table" => {
                let n = self.bookings.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(json!({"confirmation": format!("BK-{}", 1000 + n),
                          "name": text("name"), "party_size": args["party_size"],
                          "time": text("time")}))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }
}

/// Marks the event as dropped when the tool future is dropped before it finished.
struct Run {
    world: Arc<World>,
    index: usize,
    done: bool,
}

impl Drop for Run {
    fn drop(&mut self) {
        if !self.done {
            let mut events = self
                .world
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(event) = events.get_mut(self.index) {
                event.dropped = true;
            }
        }
    }
}

struct WorldTool {
    world: Arc<World>,
    name: &'static str,
}

impl Tool for WorldTool {
    fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let index = self.world.begin(self.name, &args);
            let mut run = Run {
                world: Arc::clone(&self.world),
                index,
                done: false,
            };
            let delay = self.world.delay(self.name, &args);
            self.world.clock.sleep(delay).await;
            let result = self.world.answer(self.name, &args);
            {
                let mut events = self
                    .world
                    .events
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(event) = events.get_mut(index) {
                    event.finished = Some(self.world.now());
                }
            }
            run.done = true;
            result.map(ToolOutput::Json).map_err(ToolError::new)
        })
    }
}

fn spec(name: &'static str, description: &str, parameters: Value, kind: ToolKind) -> ToolSpec {
    ToolSpec::new(name, description, parameters, kind).with_timeout(Duration::from_secs(30))
}

fn specs(which: Which) -> Vec<(&'static str, ToolSpec)> {
    let object = |properties: Value, required: &[&str]| {
        json!({"type": "object", "properties": properties, "required": required,
               "additionalProperties": false})
    };
    let weather = spec(
        "get_weather",
        "Current weather of one city. Returns temperature in Celsius and the sky.",
        object(
            json!({"city": {"type": "string", "description": "City name in English, e.g. Paris"}}),
            &["city"],
        ),
        ToolKind::ReadOnly,
    );
    match which {
        Which::Weather => vec![("get_weather", weather)],
        Which::Chain => vec![
            (
                "get_order",
                spec(
                    "get_order",
                    "Looks up an order by its id (like A-1042). Returns the customer id, the item and the status.",
                    object(json!({"order_id": {"type": "string"}}), &["order_id"]),
                    ToolKind::ReadOnly,
                ),
            ),
            (
                "get_customer",
                spec(
                    "get_customer",
                    "Looks up a customer by customer id (like C-77). Returns the name and the city they live in.",
                    object(json!({"customer_id": {"type": "string"}}), &["customer_id"]),
                    ToolKind::ReadOnly,
                ),
            ),
            ("get_weather", weather),
        ],
        Which::Booking => vec![
            (
                "book_table",
                spec(
                    "book_table",
                    "Books a restaurant table. This really reserves it: call it once per booking. Returns a confirmation code.",
                    object(
                        json!({"name": {"type": "string"},
                               "party_size": {"type": "integer", "minimum": 1},
                               "time": {"type": "string", "description": "24-hour HH:MM"}}),
                        &["name", "party_size", "time"],
                    ),
                    ToolKind::SideEffect,
                ),
            ),
            ("get_weather", weather),
        ],
        Which::Geo => ["lookup_region", "lookup_ocean", "lookup_continent"]
            .into_iter()
            .map(|name| {
                (
                    name,
                    spec(
                        name,
                        "Facts about a place the caller named. Takes the name as the caller said it.",
                        object(json!({"name": {"type": "string"}}), &["name"]),
                        ToolKind::ReadOnly,
                    ),
                )
            })
            .collect(),
        Which::GeoChain => vec![
            (
                "get_largest_country",
                spec(
                    "get_largest_country",
                    "The largest country of a continent, by name (like Africa).",
                    object(json!({"continent": {"type": "string"}}), &["continent"]),
                    ToolKind::ReadOnly,
                ),
            ),
            (
                "get_capital",
                spec(
                    "get_capital",
                    "The capital city of a country.",
                    object(json!({"country": {"type": "string"}}), &["country"]),
                    ToolKind::ReadOnly,
                ),
            ),
            ("get_weather", weather),
        ],
        Which::Forecast => vec![(
            "get_forecast",
            spec(
                "get_forecast",
                "Daily weather forecast of a city.",
                object(
                    json!({"city": {"type": "string"},
                           "days": {"type": "integer", "minimum": 1, "maximum": 3}}),
                    &["city", "days"],
                ),
                ToolKind::ReadOnly,
            ),
        )],
    }
}

/// The registry and engine of `which`.
pub fn tools(
    world: &Arc<World>,
    which: Which,
    timeout: Option<Duration>,
) -> (Arc<ToolRegistry>, Arc<ToolEngine>) {
    let mut registry = ToolRegistry::new();
    for (name, spec) in specs(which) {
        let spec = match timeout {
            Some(timeout) => spec.with_timeout(timeout),
            None => spec,
        };
        registry
            .register(
                spec,
                WorldTool {
                    world: Arc::clone(world),
                    name,
                },
            )
            .unwrap();
    }
    let registry = Arc::new(registry);
    let clock: SharedClock = Arc::new(world.clock);
    let engine = Arc::new(ToolEngine::new(
        Arc::clone(&registry),
        SessionId::new(1),
        clock,
        ToolEngineConfig::default(),
    ));
    (registry, engine)
}
