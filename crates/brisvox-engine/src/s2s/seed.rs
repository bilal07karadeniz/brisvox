//! The history a NEW provider session is seeded with (after a reconnect or a rotation), taken
//! from the stage's local mirror of the conversation (`SessionHandles::context`).
//!
//! The mirror holds what was said and, for the assistant, what was actually played (cut at a
//! barge-in), so seeding from it keeps the invariant "after an interruption the context contains
//! only what was played" across sessions. Caller turns are in the mirror only when the provider
//! transcribes them (`S2sOptions::input_transcription`); without it the new session knows what
//! the assistant said but not what the caller said.

use std::collections::HashMap;
use std::sync::Arc;

use brisvox_agent::Message;
use brisvox_providers::s2s::{S2S_MAX_HISTORY_ITEMS, S2sHistoryItem};

/// Most characters of text seeded in total (the newest are kept).
pub(super) const MAX_SEED_CHARS: usize = 32_000;

/// Longest single text; a longer one is cut (tool results can be large).
const MAX_ITEM_CHARS: usize = 4_000;

/// `text` cut to [`MAX_ITEM_CHARS`] characters (on a character boundary).
fn clip(text: &str) -> Arc<str> {
    match text.char_indices().nth(MAX_ITEM_CHARS) {
        Some((end, _)) => Arc::from(text.get(..end).unwrap_or_default()),
        None => Arc::from(text),
    }
}

fn size(item: &S2sHistoryItem) -> usize {
    match item {
        S2sHistoryItem::User(t) | S2sHistoryItem::Assistant(t) | S2sHistoryItem::Note(t) => t.len(),
        S2sHistoryItem::ToolCall {
            name, arguments, ..
        } => name.len().saturating_add(arguments.len()),
        S2sHistoryItem::ToolResult { output, .. } => output.len(),
        _ => 0,
    }
}

/// Converts the mirror into seed items, oldest first, within [`S2S_MAX_HISTORY_ITEMS`] items and
/// [`MAX_SEED_CHARS`] characters (older items are dropped first).
///
/// - A tool call with no result (still running, or interrupted) is left out, and a result whose
///   call is not in the list is left out: providers refuse an unanswered call or an orphan answer.
/// - Calls carry the id the provider gave them when there is one.
pub(super) fn seed_history(messages: &[Message]) -> Vec<S2sHistoryItem> {
    let answered: HashMap<&str, ()> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool { call_id, .. } => Some((call_id.as_str(), ())),
            _ => None,
        })
        .collect();
    // Internal call id -> the id used in the seed.
    let mut ids: HashMap<String, Arc<str>> = HashMap::new();
    let mut items = Vec::new();
    // The leading system messages are the agent's instructions, which every session gets in its
    // configuration; later ones are notes.
    let mut leading = true;
    for message in messages {
        if !matches!(message, Message::System { .. }) {
            leading = false;
        }
        match message {
            Message::System { text } => {
                if !leading && !text.trim().is_empty() {
                    items.push(S2sHistoryItem::Note(clip(text)));
                }
            }
            Message::User { text } => {
                if !text.trim().is_empty() {
                    items.push(S2sHistoryItem::User(clip(text)));
                }
            }
            Message::Assistant {
                text, tool_calls, ..
            } => {
                if !text.trim().is_empty() {
                    items.push(S2sHistoryItem::Assistant(clip(text)));
                }
                for call in tool_calls {
                    if !answered.contains_key(call.id.as_str()) {
                        continue;
                    }
                    let id: Arc<str> = call
                        .provider_id
                        .clone()
                        .unwrap_or_else(|| Arc::from(call.id.as_str()));
                    let mut arguments = call.arguments.to_string();
                    if arguments.len() > MAX_ITEM_CHARS {
                        // A cut JSON object is worse than none.
                        arguments = "{}".to_owned();
                    }
                    ids.insert(call.id.as_str().to_owned(), Arc::clone(&id));
                    items.push(S2sHistoryItem::ToolCall {
                        call_id: id,
                        name: Arc::clone(&call.name),
                        arguments: Arc::from(arguments),
                    });
                }
            }
            Message::Tool {
                call_id, content, ..
            } => {
                if let Some(id) = ids.get(call_id.as_str()) {
                    items.push(S2sHistoryItem::ToolResult {
                        call_id: Arc::clone(id),
                        output: clip(content),
                    });
                }
            }
        }
    }
    // Keep the newest items within both bounds.
    let mut start = items.len();
    let mut chars = 0_usize;
    while start > 0 {
        let Some(item) = items.get(start - 1) else {
            break;
        };
        let next = chars.saturating_add(size(item));
        if items.len() - start >= S2S_MAX_HISTORY_ITEMS || next > MAX_SEED_CHARS {
            break;
        }
        chars = next;
        start -= 1;
    }
    let kept = items.split_off(start);
    // A result whose call fell off the front is an orphan.
    let mut calls: HashMap<Arc<str>, ()> = HashMap::new();
    kept.into_iter()
        .filter(|item| match item {
            S2sHistoryItem::ToolCall { call_id, .. } => {
                calls.insert(Arc::clone(call_id), ());
                true
            }
            S2sHistoryItem::ToolResult { call_id, .. } => calls.contains_key(call_id),
            _ => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brisvox_agent::{ToolCall, ToolCallId};
    use brisvox_core::TurnEpoch;

    fn call(seq: u32, provider: Option<&str>) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(TurnEpoch::ZERO, seq),
            provider_id: provider.map(Arc::from),
            name: Arc::from("lookup"),
            arguments: serde_json::json!({"q": "x"}),
            provider_data: None,
        }
    }

    #[test]
    fn the_mirror_becomes_items_with_provider_call_ids() {
        let c = call(1, Some("prov-1"));
        let messages = vec![
            Message::system("the instructions"),
            Message::user("hello"),
            Message::assistant_with_calls("", vec![c.clone()]),
            Message::tool_result(c.id.clone(), "found"),
            Message::assistant("here it is"),
            Message::system("late note"),
        ];
        let items = seed_history(&messages);
        assert_eq!(items.len(), 5);
        assert_eq!(items[0], S2sHistoryItem::User(Arc::from("hello")));
        assert!(
            matches!(&items[1], S2sHistoryItem::ToolCall { call_id, name, arguments }
            if &**call_id == "prov-1" && &**name == "lookup" && &**arguments == r#"{"q":"x"}"#)
        );
        assert!(
            matches!(&items[2], S2sHistoryItem::ToolResult { call_id, output }
            if &**call_id == "prov-1" && &**output == "found")
        );
        assert_eq!(items[3], S2sHistoryItem::Assistant(Arc::from("here it is")));
        assert_eq!(items[4], S2sHistoryItem::Note(Arc::from("late note")));
    }

    #[test]
    fn unanswered_calls_and_orphan_results_are_left_out() {
        let answered = call(1, None);
        let pending = call(2, None);
        let messages = vec![
            Message::assistant_with_calls("", vec![answered.clone(), pending]),
            Message::tool_result(answered.id.clone(), "ok"),
            // A result whose call is not in the mirror.
            Message::tool_result(ToolCallId::new(TurnEpoch::ZERO, 9), "stray"),
        ];
        let items = seed_history(&messages);
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], S2sHistoryItem::ToolCall { call_id, .. }
            if **call_id == *answered.id.as_str()));
        assert!(matches!(items[1], S2sHistoryItem::ToolResult { .. }));
    }

    #[test]
    fn the_newest_items_are_kept_within_the_bounds() {
        let messages: Vec<Message> = (0..S2S_MAX_HISTORY_ITEMS + 20)
            .map(|n| Message::user(format!("turn {n}")))
            .collect();
        let items = seed_history(&messages);
        assert_eq!(items.len(), S2S_MAX_HISTORY_ITEMS);
        assert_eq!(
            items.last(),
            Some(&S2sHistoryItem::User(Arc::from(format!(
                "turn {}",
                S2S_MAX_HISTORY_ITEMS + 19
            ))))
        );
        // The character bound: 10 messages of 4000 characters exceed 32000.
        let big: Vec<Message> = (0..10)
            .map(|n| Message::user(format!("{n}{}", "x".repeat(5_000))))
            .collect();
        let items = seed_history(&big);
        assert_eq!(items.len(), 8);
        assert!(matches!(&items[7], S2sHistoryItem::User(t) if t.starts_with('9')));
        // A single text is cut at 4000 characters.
        assert!(matches!(&items[0], S2sHistoryItem::User(t) if t.chars().count() == 4_000));
    }

    #[test]
    fn a_result_whose_call_fell_off_the_front_is_dropped() {
        let c = call(1, None);
        let mut messages = vec![
            Message::assistant_with_calls("", vec![c.clone()]),
            Message::tool_result(c.id.clone(), "ok"),
        ];
        // 128 later turns push the call (and with it the result's partner) out of the window.
        messages.extend((0..S2S_MAX_HISTORY_ITEMS - 1).map(|n| Message::user(format!("t{n}"))));
        let items = seed_history(&messages);
        assert_eq!(items.len(), S2S_MAX_HISTORY_ITEMS - 1);
        assert!(items.iter().all(|i| matches!(i, S2sHistoryItem::User(_))));
    }
}
