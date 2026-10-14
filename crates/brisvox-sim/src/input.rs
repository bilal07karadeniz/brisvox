//! [`InputSink`]: where a simulated caller's audio goes.
//!
//! The reference pipeline takes the caller's audio straight into its first stage. A production
//! pipeline takes it from a transport (for example a loopback client that sends it through a real
//! transport session). [`play_input`](crate::scenario::play_input) and
//! [`Fixture::play`](crate::fixture::Fixture::play) do not care which: they hand every frame to an
//! `InputSink`.
//!
//! Three kinds exist:
//! - [`InputSink::direct`]: a pipeline input ([`FrameSender`]) known up front.
//! - [`InputSink::pipeline`]: a pipeline input that is not running yet. A
//!   [`SessionFactory`](crate::factory::SessionFactory) hands this out because the input only
//!   exists once the pipeline has started; the runner [binds](InputSink::bind) it right after
//!   the start and [unbinds](InputSink::unbind) it before the stop.
//! - [`InputSink::target`]: any [`InputTarget`], for example a wrapper around a transport client.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::frame::AudioFrame;
use brisvox_core::{Clock, FrameSender};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};

/// How long the simulated caller waits for a destination to take one frame
/// ([`InputSink::send_bounded`], used by [`play_input`](crate::scenario::play_input) and
/// [`Fixture::play`](crate::fixture::Fixture::play)). A destination that blocks longer is
/// stuck, and the run fails instead of hanging. Far above any legitimate backpressure in the
/// suite (the longest provider stall is a few seconds).
pub const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// The future returned by [`InputTarget::send`].
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

/// A destination for caller audio other than a pipeline input.
///
/// `send` is awaited once per 20 ms frame; the box it returns is a per-frame allocation, which
/// is fine for the simulator (the pipeline-input kinds do not allocate).
pub trait InputTarget: Send + Sync + 'static {
    /// Delivers one frame, waiting for room if the destination is full. Returns `false` once the
    /// destination is gone for good (the call ended), which ends the caller's script quietly.
    fn send(&self, frame: AudioFrame) -> SendFuture<'_>;

    /// Called by [`InputSink::bind`] when the runner has started the pipeline, with its input.
    /// A target that wraps another sink forwards the call to it; the default ignores it.
    fn bind(&self, _input: &FrameSender) {}

    /// Called by [`InputSink::bind_upstream`] right after [`Self::bind`], with the started
    /// pipeline's upstream input: the last stage's inbox for frames travelling upstream. A target
    /// that owns a transport pump (which reports mark echoes as upstream frames) starts it here;
    /// the default ignores it.
    fn bind_upstream(&self, _upstream: &FrameSender) {}

    /// Called by [`InputSink::unbind`] when the caller's script is over.
    fn unbind(&self) {}
}

#[derive(Clone)]
enum Kind {
    /// A pipeline input, bound later (or never).
    Pipeline(Arc<Mutex<Option<FrameSender>>>),
    /// Any other destination.
    Target(Arc<dyn InputTarget>),
}

/// Where the simulated caller's audio frames are delivered. Cheap to clone; clones share their
/// binding.
#[derive(Clone)]
pub struct InputSink {
    kind: Kind,
}

impl InputSink {
    /// A sink for a pipeline input that is bound later with [`Self::bind`]. Until then (and
    /// after [`Self::unbind`]) frames are refused, as if the pipeline were gone.
    pub fn pipeline() -> Self {
        Self {
            kind: Kind::Pipeline(Arc::default()),
        }
    }

    /// A sink delivering into `sender`, a running pipeline's input.
    pub fn direct(sender: FrameSender) -> Self {
        Self {
            kind: Kind::Pipeline(Arc::new(Mutex::new(Some(sender)))),
        }
    }

    /// A sink delivering to `target`.
    pub fn target(target: Arc<dyn InputTarget>) -> Self {
        Self {
            kind: Kind::Target(target),
        }
    }

    /// Points a [`Self::pipeline`] sink at the started pipeline's input. A target gets
    /// [`InputTarget::bind`] (a no-op unless it wraps another sink).
    pub fn bind(&self, sender: FrameSender) {
        match &self.kind {
            Kind::Pipeline(slot) => {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(sender);
            }
            Kind::Target(target) => target.bind(&sender),
        }
    }

    /// Hands a target the started pipeline's upstream input (see
    /// [`InputTarget::bind_upstream`]); the runner calls it right after [`Self::bind`]. A
    /// [`Self::pipeline`] sink has no use for it.
    pub fn bind_upstream(&self, upstream: FrameSender) {
        if let Kind::Target(target) = &self.kind {
            target.bind_upstream(&upstream);
        }
    }

    /// Drops the bound pipeline input, so this sink no longer keeps the pipeline's first inbox
    /// open. A target gets [`InputTarget::unbind`].
    pub fn unbind(&self) {
        match &self.kind {
            Kind::Pipeline(slot) => {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
            Kind::Target(target) => target.unbind(),
        }
    }

    /// Whether a frame sent now would reach a destination.
    pub fn is_bound(&self) -> bool {
        match &self.kind {
            Kind::Pipeline(slot) => slot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some(),
            Kind::Target(_) => true,
        }
    }

    /// Delivers one frame, waiting for room without bound: the caller must bound the wait (use
    /// [`Self::send_bounded`] when playing a script). Returns `false` when the destination is
    /// gone (the pipeline stopped, was never bound, or the target refused). Cancel-safe:
    /// dropping the future before it completes drops the frame.
    pub async fn send(&self, frame: AudioFrame) -> bool {
        match &self.kind {
            Kind::Pipeline(slot) => {
                let sender = slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
                match sender {
                    Some(sender) => sender.send(frame.into()).await.is_ok(),
                    None => false,
                }
            }
            Kind::Target(target) => target.send(frame).await,
        }
    }
}

impl InputSink {
    /// Like [`Self::send`], bounded: gives up when `cancel` fires (`Ok(false)`, the script ends
    /// quietly) and fails when the destination takes nothing for [`SEND_TIMEOUT`] on `clock`
    /// (paused time included: the deadline is a timer, so a stuck destination fails the run
    /// instead of hanging it).
    ///
    /// # Errors
    /// [`Error::Assertion`] naming the stuck destination.
    pub async fn send_bounded(
        &self,
        clock: &dyn Clock,
        frame: AudioFrame,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(false),
            delivered = self.send(frame) => Ok(delivered),
            () = clock.sleep(SEND_TIMEOUT) => Err(Error::Assertion(format!(
                "the input {self:?} took no frame for {SEND_TIMEOUT:?}: the destination is stuck"
            ))),
        }
    }
}

impl From<FrameSender> for InputSink {
    fn from(sender: FrameSender) -> Self {
        Self::direct(sender)
    }
}

impl From<&FrameSender> for InputSink {
    fn from(sender: &FrameSender) -> Self {
        Self::direct(sender.clone())
    }
}

impl fmt::Debug for InputSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.kind {
            Kind::Pipeline(_) => "pipeline",
            Kind::Target(_) => "target",
        };
        f.debug_struct("InputSink")
            .field("kind", &kind)
            .field("bound", &self.is_bound())
            .finish()
    }
}
