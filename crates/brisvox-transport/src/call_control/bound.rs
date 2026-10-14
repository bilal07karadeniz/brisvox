//! [`BoundCall`]: a [`CallControl`] bound to one live call, implementing the agent's
//! [`TelephonyOps`] so the telephony tools (`transfer_call`, `send_dtmf`, `end_call`) can act on
//! it. The application creates one per call, registers the tools over it
//! (`brisvox_agent::telephony::register_telephony_tools`) and keeps the `Arc` to drive the parts
//! the tools only start: the farewell hang-up and the second half of a warm transfer.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use brisvox_agent::{
    DtmfString, TelephonyError, TelephonyErrorKind, TelephonyOps, TransferMode, TransferReport,
    TransferRequest, telephony::OpsFuture,
};

use super::{CallControl, CallRef, CallerState, Capabilities, Error, Result, WarmTransfer};
use brisvox_agent::Briefing;

type WarmSlot = Arc<Mutex<Option<WarmTransfer>>>;

fn store(slot: &WarmSlot, transfer: WarmTransfer) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(transfer);
}

/// Resolves when what the assistant said so far was played to the caller. The application
/// supplies it from the engine's play-out marks and bounds its own wait.
pub type SettleFn = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A [`CallControl`] bound to one call.
pub struct BoundCall {
    control: Arc<dyn CallControl>,
    call: CallRef,
    settle: Option<SettleFn>,
    hangup_requested: AtomicBool,
    warm: WarmSlot,
}

impl std::fmt::Debug for BoundCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundCall")
            .field("provider", &self.control.provider())
            .field("call", &self.call)
            .field(
                "hangup_requested",
                &self.hangup_requested.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl BoundCall {
    /// Binds `control` to `call`.
    pub fn new(control: Arc<dyn CallControl>, call: CallRef) -> Self {
        Self {
            control,
            call,
            settle: None,
            hangup_requested: AtomicBool::new(false),
            warm: Arc::new(Mutex::new(None)),
        }
    }

    /// Sets how to wait for the assistant's speech to be played (see
    /// [`TelephonyOps::settle_speech`]). Without it the wait is skipped.
    #[must_use]
    pub fn with_settle(mut self, settle: SettleFn) -> Self {
        self.settle = Some(settle);
        self
    }

    /// What the bound provider can do as configured. Register the `send_dtmf` tool only when
    /// `send_dtmf` is set (`TelephonyPolicy::with_dtmf`), and offer warm transfers only when
    /// `warm_transfer` is.
    pub fn capabilities(&self) -> Capabilities {
        self.control.capabilities()
    }

    /// The call.
    pub fn call(&self) -> &CallRef {
        &self.call
    }

    /// Whether the `end_call` tool asked for the call to end. The application checks it after
    /// the next response was played and then ends the call (the transport's stop, or
    /// [`Self::hangup`]).
    pub fn hangup_requested(&self) -> bool {
        self.hangup_requested.load(Ordering::Acquire)
    }

    /// Ends the call through the provider's REST API (for a stream that cannot end it itself).
    ///
    /// # Errors
    /// As [`CallControl::hangup`].
    pub async fn hangup(&self, idempotency: u64) -> Result<()> {
        self.control.hangup(&self.call, idempotency).await
    }

    /// The warm transfer in progress, if the `transfer_call` tool started one. After a warm
    /// transfer that failed halfway this is the recovery handle (no agent leg): hand it to
    /// [`Self::abort_warm`].
    pub fn warm_transfer(&self) -> Option<WarmTransfer> {
        self.warm
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Finishes the warm transfer after the agent leg answered and the application accepted the
    /// answer (see [`CallControl::complete_warm`]). Does nothing without a transfer in progress.
    /// The transfer is forgotten once the provider accepted the join; after an error it is kept,
    /// so the call can be repeated or the transfer aborted.
    ///
    /// # Errors
    /// As [`CallControl::complete_warm`].
    pub async fn complete_warm(&self) -> Result<()> {
        match self.warm_transfer() {
            Some(transfer) => {
                self.control.complete_warm(&transfer).await?;
                self.warm
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Gives up the warm transfer (see [`CallControl::abort_warm`]) and returns where the caller
    /// was left. The transfer is forgotten only after the provider confirmed the abort: after an
    /// error (timeout, 5xx) it is kept and the call can be repeated. Without a transfer in
    /// progress this does nothing and reports [`CallerState::Unchanged`].
    ///
    /// # Errors
    /// As [`CallControl::abort_warm`].
    pub async fn abort_warm(&self) -> Result<CallerState> {
        let Some(transfer) = self.warm_transfer() else {
            return Ok(CallerState::Unchanged);
        };
        let state = self.control.abort_warm(&transfer).await?;
        self.warm
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        Ok(state)
    }
}

impl TelephonyOps for BoundCall {
    fn settle_speech(&self) -> OpsFuture<'_, ()> {
        Box::pin(async move {
            if let Some(settle) = &self.settle {
                settle().await;
            }
            Ok(())
        })
    }

    fn send_dtmf<'a>(&'a self, digits: &'a DtmfString, idempotency: u64) -> OpsFuture<'a, ()> {
        Box::pin(async move {
            self.control
                .send_dtmf(&self.call, digits, idempotency)
                .await
                .map_err(TelephonyError::from)
        })
    }

    fn transfer<'a>(&'a self, request: &'a TransferRequest) -> OpsFuture<'a, TransferReport> {
        Box::pin(async move {
            match request.mode {
                TransferMode::Cold => {
                    self.control
                        .transfer_cold(&self.call, request)
                        .await
                        .map_err(TelephonyError::from)?;
                    Ok(TransferReport {
                        mode: TransferMode::Cold,
                        briefing: Briefing::None,
                    })
                }
                TransferMode::Warm => {
                    if !self.control.capabilities().warm_transfer {
                        return Err(TelephonyError::unsupported(
                            "warm transfer is not configured for this provider",
                        ));
                    }
                    // The REST steps and the recording of their result run on one task that owns
                    // everything it needs: a tool task that is aborted (the engine's backstop, a
                    // dropped engine) after the caller was moved cannot lose the handle that
                    // `complete_warm` and `abort_warm` need.
                    let control = Arc::clone(&self.control);
                    let call = self.call.clone();
                    let request = request.clone();
                    let slot = Arc::clone(&self.warm);
                    let task = tokio::spawn(async move {
                        match control.transfer_warm(&call, &request).await {
                            Ok(transfer) => {
                                let briefing = transfer.briefing;
                                store(&slot, transfer);
                                Ok(briefing)
                            }
                            Err(error) => {
                                if let Error::WarmFailed {
                                    transfer: Some(handle),
                                    ..
                                } = &error
                                {
                                    store(&slot, (**handle).clone());
                                }
                                Err(error)
                            }
                        }
                    });
                    let briefing = match task.await {
                        Ok(result) => result.map_err(TelephonyError::from)?,
                        Err(_) => {
                            return Err(TelephonyError::new(
                                TelephonyErrorKind::OutcomeUnknown,
                                "the warm transfer task ended unexpectedly",
                            ));
                        }
                    };
                    Ok(TransferReport {
                        mode: TransferMode::Warm,
                        briefing,
                    })
                }
            }
        })
    }

    fn end_call(&self) -> OpsFuture<'_, ()> {
        Box::pin(async move {
            self.hangup_requested.store(true, Ordering::Release);
            Ok(())
        })
    }
}
