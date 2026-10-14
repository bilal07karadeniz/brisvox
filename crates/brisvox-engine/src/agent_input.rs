//! Commands an application gives a running call: lines the bot speaks and edits of the
//! conversation, the engine side of the out-of-process agent protocol (`docs/AGENT_PROTOCOL.md`).
//!
//! Both are bounded channels whose sending end stays with the application and whose receiving
//! end goes to the call's [`SessionBuilder`](crate::SessionBuilder):
//!
//! - [`say_input`]: [`BotLine`]s for the turn driver, the epoch authority
//!   ([`SessionBuilder::with_say_input`](crate::SessionBuilder::with_say_input)). A line is a
//!   *bot turn*: when the machine is idle (nobody speaks, no response runs) it starts a new
//!   epoch, the LLM runner speaks the text verbatim as that epoch's response (no LLM request)
//!   and puts it in the context as an assistant message, cut to what was heard like any
//!   response. The caller can barge in on it. A line waits (in order, at most
//!   [`MAX_HELD_LINES`]) while the user speaks or a response runs, unless it is
//!   [`BotLine::interrupt`]ing: such a line drops the lines that wait before it and cuts a
//!   running response (never the user) like a barge-in.
//! - [`conversation_edits`]: [`ConversationEdit`]s for the LLM runner
//!   ([`SessionBuilder::with_conversation_edits`](crate::SessionBuilder::with_conversation_edits)).
//!   They are applied at the runner's next safe point, when no round task can touch the
//!   context: right before the next response starts (a user turn's end, a bot line, a
//!   speculative start). So an edit made while the bot speaks shapes the *next* request, never
//!   the running one, and the context stays valid (tool calls stay paired with their results).
//!
//! # Bounds
//! [`SaySender::try_send`] and [`EditSender::try_send`] never wait and fail when the channel is
//! full or the call is over. A line longer than [`MAX_LINE_BYTES`] is refused at the sender; an
//! edit whose text is longer than [`MAX_EDIT_TEXT_BYTES`] too. The runner applies at most
//! [`MAX_EDITS_PER_SETTLE`] edits per safe point (the rest wait for the next one).

use std::sync::Arc;

use brisvox_agent::context::Message;
use tokio::sync::mpsc;

/// Largest capacity of a line channel; a larger request is clamped to it.
pub const MAX_SAY_CAPACITY: usize = 64;

/// Largest capacity of an edit channel; a larger request is clamped to it.
pub const MAX_EDIT_CAPACITY: usize = 256;

/// Longest line the bot may be given, in bytes (about a minute of speech).
pub const MAX_LINE_BYTES: usize = 4 * 1024;

/// Longest text of one edit (a system prompt or an appended message), in bytes.
pub const MAX_EDIT_TEXT_BYTES: usize = 64 * 1024;

/// Lines the turn driver holds while they wait for their turn; a newer line is refused (counted
/// as `lines_rejected`) when this many wait.
pub const MAX_HELD_LINES: usize = 8;

/// Edits the runner applies at one safe point; more wait for the next one.
pub const MAX_EDITS_PER_SETTLE: usize = 64;

/// A line the bot speaks verbatim (see the [module docs](self)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotLine {
    /// What the bot says.
    pub text: Arc<str>,
    /// `true`: drop the lines still waiting and cut a response that is running (thinking or
    /// speaking) for this line; `false`: wait, after the lines before it, until the bot is idle.
    /// A line never cuts the user: while the user speaks every line waits.
    pub interrupt: bool,
}

impl BotLine {
    /// A line spoken once the bot is idle.
    pub fn new(text: impl Into<Arc<str>>) -> Self {
        Self {
            text: text.into(),
            interrupt: false,
        }
    }

    /// A line that cuts a running response.
    pub fn interrupting(text: impl Into<Arc<str>>) -> Self {
        Self {
            text: text.into(),
            interrupt: true,
        }
    }
}

/// Why a line or an edit was not queued. Hands the value back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError<T> {
    /// The channel is full: the call is not keeping up (or the application floods it).
    #[error("the call's input queue is full")]
    Full(T),
    /// The call is over.
    #[error("the call is over")]
    Closed(T),
    /// The value breaks a bound or a rule (too long, an empty line, a message the context
    /// cannot take).
    #[error("rejected: {1}")]
    Invalid(T, &'static str),
}

impl<T> InputError<T> {
    /// The value that was not queued.
    pub fn into_inner(self) -> T {
        match self {
            Self::Full(value) | Self::Closed(value) | Self::Invalid(value, _) => value,
        }
    }
}

fn send<T>(tx: &mpsc::Sender<T>, value: T) -> Result<(), InputError<T>> {
    tx.try_send(value).map_err(|error| match error {
        mpsc::error::TrySendError::Full(value) => InputError::Full(value),
        mpsc::error::TrySendError::Closed(value) => InputError::Closed(value),
    })
}

/// The application's end of a call's bot lines. Cheap to clone.
#[derive(Debug, Clone)]
pub struct SaySender {
    tx: mpsc::Sender<BotLine>,
}

impl SaySender {
    /// Queues `line` without waiting.
    ///
    /// # Errors
    /// [`InputError::Invalid`] for an empty (blank) line or one longer than
    /// [`MAX_LINE_BYTES`]; [`InputError::Full`] / [`InputError::Closed`] otherwise.
    pub fn try_send(&self, line: BotLine) -> Result<(), InputError<BotLine>> {
        if line.text.trim().is_empty() {
            return Err(InputError::Invalid(line, "the line is empty"));
        }
        if line.text.len() > MAX_LINE_BYTES {
            return Err(InputError::Invalid(line, "the line is too long"));
        }
        send(&self.tx, line)
    }

    /// `true` once the call dropped its end.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// The call's end of the bot lines: give it to
/// [`SessionBuilder::with_say_input`](crate::SessionBuilder::with_say_input) (or
/// [`TurnDriver::with_say_input`](crate::TurnDriver::with_say_input)).
#[derive(Debug)]
pub struct SayReceiver {
    pub(crate) rx: mpsc::Receiver<BotLine>,
}

impl SayReceiver {
    /// Waits for the next line; `None` once every sender is dropped and the queue is empty.
    /// For an application that merges several line sources into one (the engine takes a single
    /// receiver). Cancel-safe.
    pub async fn recv(&mut self) -> Option<BotLine> {
        self.rx.recv().await
    }
}

/// A bounded channel of bot lines holding at most `capacity` (clamped to
/// `1..=`[`MAX_SAY_CAPACITY`]).
pub fn say_input(capacity: usize) -> (SaySender, SayReceiver) {
    let (tx, rx) = mpsc::channel(capacity.clamp(1, MAX_SAY_CAPACITY));
    (SaySender { tx }, SayReceiver { rx })
}

/// A change to the conversation, applied at the runner's next safe point (see the
/// [module docs](self)).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConversationEdit {
    /// Replaces the leading system message (or inserts one). With a flow, the flow's node prompt
    /// is that message and the next transition replaces it again.
    SetSystemPrompt(Arc<str>),
    /// Appends a system, user or assistant (text only) message. Tool messages and assistant tool
    /// calls are refused at the sender: only the engine pairs calls with results.
    Append(Message),
    /// Offers only the named tools to the LLM from the next request on (`None`: every registered
    /// tool again). Names that are not registered are ignored. A call to a tool that is not
    /// offered is answered with an `unknown_tool` error result and does not run. With a flow,
    /// the tools offered are those of the current node that are also in this set.
    SetTools(Option<Arc<[Arc<str>]>>),
}

impl ConversationEdit {
    /// Why this edit cannot be applied, if it cannot.
    fn invalid(&self) -> Option<&'static str> {
        match self {
            Self::SetSystemPrompt(text) if text.len() > MAX_EDIT_TEXT_BYTES => {
                Some("the system prompt is too long")
            }
            Self::Append(message) => match message {
                Message::System { text } | Message::User { text } if text.is_empty() => {
                    Some("the message is empty")
                }
                Message::System { text } | Message::User { text }
                    if text.len() > MAX_EDIT_TEXT_BYTES =>
                {
                    Some("the message is too long")
                }
                Message::System { .. } | Message::User { .. } => None,
                Message::Assistant {
                    text, tool_calls, ..
                } => {
                    if !tool_calls.is_empty() {
                        Some("an appended assistant message cannot carry tool calls")
                    } else if text.is_empty() {
                        Some("the message is empty")
                    } else if text.len() > MAX_EDIT_TEXT_BYTES {
                        Some("the message is too long")
                    } else {
                        None
                    }
                }
                Message::Tool { .. } => Some("tool results cannot be appended"),
            },
            _ => None,
        }
    }
}

/// The application's end of a call's conversation edits. Cheap to clone.
#[derive(Debug, Clone)]
pub struct EditSender {
    tx: mpsc::Sender<ConversationEdit>,
}

impl EditSender {
    /// Queues `edit` without waiting.
    ///
    /// # Errors
    /// [`InputError::Invalid`] for an edit the context cannot take (see [`ConversationEdit`]),
    /// [`InputError::Full`] / [`InputError::Closed`] otherwise.
    pub fn try_send(&self, edit: ConversationEdit) -> Result<(), InputError<ConversationEdit>> {
        if let Some(why) = edit.invalid() {
            return Err(InputError::Invalid(edit, why));
        }
        send(&self.tx, edit)
    }

    /// Free slots in the channel now (an application that sends several edits as one change
    /// checks this first, so the change is applied whole or not at all).
    pub fn capacity(&self) -> usize {
        self.tx.capacity()
    }

    /// `true` once the call dropped its end.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// The call's end of the conversation edits: give it to
/// [`SessionBuilder::with_conversation_edits`](crate::SessionBuilder::with_conversation_edits)
/// (or [`LlmRunnerBuilder::with_conversation_edits`](crate::LlmRunnerBuilder::with_conversation_edits)).
#[derive(Debug)]
pub struct EditReceiver {
    pub(crate) rx: mpsc::Receiver<ConversationEdit>,
}

/// A bounded channel of conversation edits holding at most `capacity` (clamped to
/// `1..=`[`MAX_EDIT_CAPACITY`]).
pub fn conversation_edits(capacity: usize) -> (EditSender, EditReceiver) {
    let (tx, rx) = mpsc::channel(capacity.clamp(1, MAX_EDIT_CAPACITY));
    (EditSender { tx }, EditReceiver { rx })
}

#[cfg(test)]
mod tests {
    use brisvox_agent::context::{ToolCall, ToolCallId};
    use brisvox_core::TurnEpoch;
    use serde_json::Value;

    use super::*;

    #[test]
    fn lines_are_bounded_and_validated_at_the_sender() {
        let (tx, rx) = say_input(1);
        assert!(matches!(
            tx.try_send(BotLine::new("  ")),
            Err(InputError::Invalid(..))
        ));
        let long = "a".repeat(MAX_LINE_BYTES + 1);
        assert!(matches!(
            tx.try_send(BotLine::new(long)),
            Err(InputError::Invalid(..))
        ));
        tx.try_send(BotLine::new("hello")).unwrap();
        assert_eq!(
            tx.try_send(BotLine::interrupting("again")),
            Err(InputError::Full(BotLine::interrupting("again")))
        );
        drop(rx);
        assert!(tx.is_closed());
        assert!(matches!(
            tx.try_send(BotLine::new("late")),
            Err(InputError::Closed(_))
        ));
        let (tx, _rx) = say_input(usize::MAX);
        for n in 0..MAX_SAY_CAPACITY {
            tx.try_send(BotLine::new(n.to_string())).unwrap();
        }
        assert!(tx.try_send(BotLine::new("one too many")).is_err());
    }

    #[test]
    fn edits_the_context_cannot_take_are_refused() {
        let (tx, _rx) = conversation_edits(8);
        let call = ToolCall {
            id: ToolCallId::new(TurnEpoch::ZERO, 0),
            provider_id: None,
            name: Arc::from("x"),
            arguments: Value::Null,
            provider_data: None,
        };
        for bad in [
            ConversationEdit::Append(Message::tool_result(
                ToolCallId::new(TurnEpoch::ZERO, 0),
                "{}",
            )),
            ConversationEdit::Append(Message::assistant_with_calls("hi", vec![call])),
            ConversationEdit::Append(Message::user("")),
            ConversationEdit::Append(Message::assistant("")),
            ConversationEdit::SetSystemPrompt(Arc::from("p".repeat(MAX_EDIT_TEXT_BYTES + 1))),
        ] {
            assert!(
                matches!(tx.try_send(bad.clone()), Err(InputError::Invalid(..))),
                "{bad:?}"
            );
        }
        for good in [
            ConversationEdit::Append(Message::user("typed")),
            ConversationEdit::Append(Message::system("note")),
            ConversationEdit::Append(Message::assistant("said")),
            ConversationEdit::SetSystemPrompt(Arc::from("new prompt")),
            ConversationEdit::SetTools(None),
        ] {
            tx.try_send(good).unwrap();
        }
    }
}
