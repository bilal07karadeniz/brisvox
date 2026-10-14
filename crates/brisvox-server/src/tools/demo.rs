//! Demo tools (cargo feature `demo-tools`, off by default): `lookup_order` and
//! `book_appointment`, the application tools the example agents name (`examples/agents`), with
//! fixed answers so the examples run end to end without an application.
//!
//! They touch nothing outside the call: `lookup_order` answers from a canned table and
//! `book_appointment` confirms without booking anything. Every answer says it is demo data, so
//! a model never presents it as real. Never ship a build with this feature to callers.

use serde_json::{Value, json};

use brisvox_agent::tools::{
    RegistryError, Tool, ToolCallCtx, ToolError, ToolFuture, ToolKind, ToolOutput, ToolRegistry,
    ToolSpec,
};
use brisvox_core::SharedClock;

use super::AppTools;

/// Longest order number or name the demo tools accept (their schemas say so too).
const MAX_ARG_CHARS: usize = 64;

/// The demo tools as an [`AppTools`] plug-in.
#[derive(Debug, Clone, Copy, Default)]
pub struct DemoTools;

impl AppTools for DemoTools {
    fn register(
        &self,
        registry: &mut ToolRegistry,
        _clock: &SharedClock,
    ) -> Result<(), RegistryError> {
        registry.register(
            ToolSpec::new(
                "lookup_order",
                "Looks up an order by its order number and returns its status (DEMO DATA: \
                 the answer is canned).",
                json!({
                    "type": "object",
                    "properties": {
                        "order_id": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MAX_ARG_CHARS,
                            "description": "The order number the caller gave."
                        }
                    },
                    "required": ["order_id"],
                    "additionalProperties": false
                }),
                ToolKind::ReadOnly,
            ),
            LookupOrder,
        )?;
        registry.register(
            ToolSpec::new(
                "book_appointment",
                "Books an appointment for the caller (DEMO: nothing is booked, the \
                 confirmation is canned). Ask for the name and the preferred day first.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MAX_ARG_CHARS,
                            "description": "The caller's name."
                        },
                        "day": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MAX_ARG_CHARS,
                            "description": "The day the caller asked for, as they said it."
                        }
                    },
                    "required": ["name", "day"],
                    "additionalProperties": false
                }),
                ToolKind::SideEffect,
            ),
            BookAppointment,
        )?;
        Ok(())
    }
}

/// The text of a string argument the schema made required.
fn arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new(format!("missing `{name}`")))
}

/// `lookup_order`: a canned status, the same for every order number.
struct LookupOrder;

impl Tool for LookupOrder {
    fn call<'a>(&'a self, args: Value, _ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let order_id = arg(&args, "order_id")?;
            Ok(ToolOutput::Json(json!({
                "demo": true,
                "order_id": order_id,
                "status": "shipped",
                "estimated_delivery": "in two days"
            })))
        })
    }
}

/// `book_appointment`: a canned confirmation; nothing is booked.
struct BookAppointment;

impl Tool for BookAppointment {
    fn call<'a>(&'a self, args: Value, ctx: ToolCallCtx) -> ToolFuture<'a> {
        Box::pin(async move {
            let name = arg(&args, "name")?;
            let day = arg(&args, "day")?;
            Ok(ToolOutput::Json(json!({
                "demo": true,
                "booked": true,
                "name": name,
                "day": day,
                "time": "10:00",
                "confirmation": format!("DEMO-{:08X}", ctx.idempotency_key().digest() & 0xFFFF_FFFF)
            })))
        })
    }
}

#[cfg(test)]
mod tests {
    use brisvox_agent::ToolCallId;
    use brisvox_agent::tools::SessionId;
    use brisvox_core::{SystemClock, TurnEpoch};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn ctx() -> ToolCallCtx {
        ToolCallCtx {
            session: SessionId::new(7),
            epoch: TurnEpoch::ZERO,
            call: ToolCallId::new(TurnEpoch::ZERO, 1),
            cancel: CancellationToken::new(),
            confirmations: std::sync::Arc::new(brisvox_agent::tools::Confirmations::new(
                brisvox_core::SystemClock::shared(),
                brisvox_agent::tools::DEFAULT_CONFIRM_TTL,
            )),
        }
    }

    #[tokio::test]
    async fn the_demo_tools_answer_with_labelled_canned_data() {
        let mut registry = ToolRegistry::new();
        DemoTools
            .register(&mut registry, &SystemClock::shared())
            .unwrap();
        let names: Vec<&str> = registry.specs().map(|s| &*s.name).collect();
        // In name order.
        assert_eq!(names, ["book_appointment", "lookup_order"]);
        let ToolOutput::Json(order) = LookupOrder
            .call(json!({"order_id": "A-17"}), ctx())
            .await
            .unwrap()
        else {
            panic!("json expected");
        };
        assert_eq!(order["demo"], true);
        assert_eq!(order["order_id"], "A-17");
        let ToolOutput::Json(booking) = BookAppointment
            .call(json!({"name": "Ada", "day": "Monday"}), ctx())
            .await
            .unwrap()
        else {
            panic!("json expected");
        };
        assert_eq!(booking["demo"], true);
        assert!(
            booking["confirmation"]
                .as_str()
                .unwrap()
                .starts_with("DEMO-")
        );
        assert!(LookupOrder.call(json!({}), ctx()).await.is_err());
    }
}
