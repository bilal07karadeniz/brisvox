//! Typed user messages: text that reaches the conversation without speech, such as an
//! RTVI `send-text` from a Pipecat client.
//!
//! [`text_turns`] makes a bounded channel: the [`TextTurnSender`] stays with the application
//! (the RTVI bridge holds one), the [`TextTurnReceiver`] goes to the call
//! ([`SessionBuilder::with_text_input`](crate::SessionBuilder::with_text_input)), where the turn
//! driver, the epoch authority, turns each message into a user turn:
//!
//! - **The bot is idle, thinking or speaking** (`run_immediately`): the message is a user turn
//!   that ends at once (`TurnInput::UserText`). While the bot thinks or speaks it is a barge-in
//!   first: the transport is cleared and the epoch moves on, exactly as for speech. Then the
//!   text goes downstream as the turn's final transcript and `UserTurnEnded`, and the LLM runner
//!   appends it to the context as a user message and answers it.
//! - **The user is speaking** (a turn is open, or a barge-in candidate is pending): the text
//!   joins that turn and is answered together with the speech when the turn ends.
//! - **`run_immediately: false`**: nothing is interrupted and nothing runs now; the text is held
//!   and joins the next user turn (spoken or typed), so the LLM sees it with that turn.
//! - **After the hang-up**, or an empty text: dropped (counted).
//!
//! # Bounds
//! The channel holds at most the capacity given to [`text_turns`] (clamped to
//! [`MAX_TEXT_CAPACITY`]); [`TextTurnSender::try_send`]
//! never waits and fails when it is full. Between the channel and the driver a small event slot
//! (8 messages, newest rejected and counted as `text_rejected`) carries the messages with a
//! `Wake`, the driver's usual way in. A message longer than the driver's text bound (64 KiB) is
//! dropped and counted.

use std::sync::Arc;

use tokio::sync::mpsc;

/// Largest capacity of a typed-message channel; a larger request is clamped to it (a person
/// types a few messages a minute, and each can hold 64 KiB).
pub const MAX_TEXT_CAPACITY: usize = 256;

/// A typed user message for the turn driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextTurn {
    /// The text the user typed.
    pub text: Arc<str>,
    /// `true`: answer now (interrupting the bot); `false`: hold the text for the next turn.
    pub run_immediately: bool,
}

impl TextTurn {
    /// A message answered now.
    pub fn now(text: impl Into<Arc<str>>) -> Self {
        Self {
            text: text.into(),
            run_immediately: true,
        }
    }
}

/// The application's end of a call's typed messages. Cheap to clone.
#[derive(Debug, Clone)]
pub struct TextTurnSender {
    tx: mpsc::Sender<TextTurn>,
}

impl TextTurnSender {
    /// Queues `turn` without waiting.
    ///
    /// # Errors
    /// Hands `turn` back when the channel is full or the call is over.
    pub fn try_send(&self, turn: TextTurn) -> Result<(), TextTurn> {
        self.tx.try_send(turn).map_err(|error| match error {
            mpsc::error::TrySendError::Full(turn) | mpsc::error::TrySendError::Closed(turn) => turn,
        })
    }

    /// `true` once the call dropped its end.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// The call's end of the typed messages: give it to
/// [`SessionBuilder::with_text_input`](crate::SessionBuilder::with_text_input) (or
/// [`TurnDriver::with_text_input`](crate::TurnDriver::with_text_input)).
#[derive(Debug)]
pub struct TextTurnReceiver {
    pub(crate) rx: mpsc::Receiver<TextTurn>,
}

/// A bounded channel of typed messages holding at most `capacity` (clamped to
/// `1..=`[`MAX_TEXT_CAPACITY`]).
pub fn text_turns(capacity: usize) -> (TextTurnSender, TextTurnReceiver) {
    let (tx, rx) = mpsc::channel(capacity.clamp(1, MAX_TEXT_CAPACITY));
    (TextTurnSender { tx }, TextTurnReceiver { rx })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_or_closed_channel_hands_the_message_back() {
        let (tx, rx) = text_turns(1);
        tx.try_send(TextTurn::now("one")).unwrap();
        assert_eq!(tx.try_send(TextTurn::now("two")), Err(TextTurn::now("two")));
        drop(rx);
        assert!(tx.is_closed());
        assert_eq!(
            tx.try_send(TextTurn::now("three")),
            Err(TextTurn::now("three"))
        );
    }

    #[test]
    fn a_huge_capacity_is_clamped_instead_of_panicking() {
        // `mpsc::channel` panics above its permit limit.
        let (tx, _rx) = text_turns(usize::MAX);
        for n in 0..MAX_TEXT_CAPACITY {
            tx.try_send(TextTurn::now(n.to_string())).unwrap();
        }
        assert!(tx.try_send(TextTurn::now("one too many")).is_err());
        let (tx, _rx) = text_turns(0);
        tx.try_send(TextTurn::now("one")).unwrap();
        assert!(tx.try_send(TextTurn::now("two")).is_err());
    }
}
