//! Idle detection for SIP-over-TLS connections.
//!
//! `rsipstack` takes a finished `tokio_rustls` stream over a plain `TcpStream` and reports no
//! activity of its own, so an allowed peer could keep up to `max_connections_per_ip` established
//! connections open without ever sending anything. The stack's types cannot wrap the stream, so
//! the activity is measured one level down: the TLS endpoint (rustls, here) talks to a loopback
//! TCP socket instead of the peer's, and a small relay task copies the bytes between that socket
//! and the real one through [`Tracked`], an `AsyncRead`/`AsyncWrite` adapter that notes the time
//! of every transfer. Any byte counts, in either direction: SIP messages, TLS records and the
//! CRLF keepalives of RFC 5626 (which are TLS records too).
//!
//! The check ([`watch_idle`]) runs on the injected [`Clock`], so a test moves time with a
//! `SimClock`. The cost is two more file descriptors and one task per TLS connection and one
//! copy through the loopback, which signaling traffic does not notice.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use brisvox_core::Clock;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The time of the last byte moved on a connection, in milliseconds since the connection began.
#[derive(Debug)]
pub(crate) struct Activity {
    began: Instant,
    last_ms: AtomicU64,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Activity {
    /// A connection that begins (and has just been active) at `now`.
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            began: now,
            last_ms: AtomicU64::new(0),
        }
    }

    /// Notes activity at `now`.
    pub(crate) fn touch(&self, now: Instant) {
        let at = millis(now.saturating_duration_since(self.began));
        self.last_ms.fetch_max(at, Ordering::Relaxed);
    }

    /// How long nothing has moved, at `now`.
    pub(crate) fn idle_for(&self, now: Instant) -> Duration {
        let since_begin = now.saturating_duration_since(self.began);
        since_begin.saturating_sub(Duration::from_millis(self.last_ms.load(Ordering::Relaxed)))
    }
}

/// An `AsyncRead` + `AsyncWrite` that notes every transfer of at least one byte in an
/// [`Activity`].
pub(crate) struct Tracked<S> {
    inner: S,
    activity: Arc<Activity>,
    clock: Arc<dyn Clock>,
}

impl<S> Tracked<S> {
    pub(crate) fn new(inner: S, activity: Arc<Activity>, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner,
            activity,
            clock,
        }
    }

    fn note(&self) {
        self.activity.touch(self.clock.now());
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Tracked<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.note();
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tracked<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, data);
        if matches!(poll, Poll::Ready(Ok(n)) if n > 0) {
            this.note();
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Copies bytes both ways between `real` (the peer's socket, tracked) and `near` (the loopback
/// socket the TLS endpoint uses) until either side ends or `cancel` fires. Dropping both streams
/// at the end is what tells the TLS endpoint, and through it the SIP stack, that the connection
/// is gone.
pub(crate) async fn relay<R, N>(
    real: R,
    mut near: N,
    activity: Arc<Activity>,
    clock: Arc<dyn Clock>,
    cancel: CancellationToken,
) where
    R: AsyncRead + AsyncWrite + Unpin,
    N: AsyncRead + AsyncWrite + Unpin,
{
    let mut real = Tracked::new(real, activity, clock);
    tokio::select! {
        biased;
        () = cancel.cancelled() => {}
        _ = tokio::io::copy_bidirectional(&mut real, &mut near) => {}
    }
}

/// Waits until `activity` has been idle for `limit` (true) or `ended` fires (false). A `limit`
/// of zero never expires.
pub(crate) async fn watch_idle(
    clock: &dyn Clock,
    activity: &Activity,
    limit: Duration,
    ended: &CancellationToken,
) -> bool {
    if limit.is_zero() {
        ended.cancelled().await;
        return false;
    }
    loop {
        let idle = activity.idle_for(clock.now());
        if idle >= limit {
            return true;
        }
        // Not before the earliest moment the connection could be idle for `limit`; woken early
        // by the end of the connection. At least a millisecond, so a rounding never spins.
        let wait = limit.saturating_sub(idle).max(Duration::from_millis(1));
        tokio::select! {
            biased;
            () = ended.cancelled() => return false,
            () = clock.sleep(wait) => {}
        }
    }
}

/// Makes the loopback socket pairs the relay needs: one listener on `127.0.0.1`, one pair at a
/// time.
pub(crate) struct LoopbackPairs {
    listener: TcpListener,
    one_at_a_time: Mutex<()>,
}

impl LoopbackPairs {
    /// Binds the loopback listener (an ephemeral port).
    pub(crate) async fn bind() -> io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(("127.0.0.1", 0)).await?,
            one_at_a_time: Mutex::new(()),
        })
    }

    /// A connected pair `(near, far)`: the TLS endpoint takes `near`, the relay `far`. The accepted
    /// socket must be the one just connected (another local process may also connect to the
    /// port; its socket is dropped and the pair refused).
    pub(crate) async fn pair(&self) -> io::Result<(TcpStream, TcpStream)> {
        let _turn = self.one_at_a_time.lock().await;
        let address = self.listener.local_addr()?;
        let (near, (far, peer)) =
            tokio::try_join!(TcpStream::connect(address), self.listener.accept())?;
        if near.local_addr()? != peer {
            return Err(io::Error::other(
                "a foreign connection on the loopback pair port",
            ));
        }
        let _ = near.set_nodelay(true);
        let _ = far.set_nodelay(true);
        Ok((near, far))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brisvox_core::SimClock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(start_paused = true)]
    async fn an_idle_connection_expires_on_the_injected_clock() {
        let clock: Arc<dyn Clock> = Arc::new(SimClock::new());
        let activity = Activity::new(clock.now());
        let ended = CancellationToken::new();
        let started = clock.now();
        let expired = watch_idle(&*clock, &activity, Duration::from_secs(300), &ended).await;
        assert!(expired);
        assert_eq!(clock.now() - started, Duration::from_secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn traffic_in_either_direction_restarts_the_idle_time_and_crlf_counts() {
        let clock: Arc<dyn Clock> = Arc::new(SimClock::new());
        let activity = Arc::new(Activity::new(clock.now()));
        let ended = CancellationToken::new();
        let (real_a, mut real_b) = tokio::io::duplex(64);
        let (near_a, mut near_b) = tokio::io::duplex(64);
        let relayed = tokio::spawn(relay(
            real_a,
            near_a,
            Arc::clone(&activity),
            Arc::clone(&clock),
            ended.clone(),
        ));
        let began = clock.now();
        let watching = {
            let (clock, activity, ended) =
                (Arc::clone(&clock), Arc::clone(&activity), ended.clone());
            tokio::spawn(async move {
                let expired = watch_idle(&*clock, &activity, Duration::from_secs(10), &ended).await;
                (expired, clock.now())
            })
        };
        // A keepalive from the peer at 6 s, another at 12 s, a message from our side at 18 s.
        clock.sleep(Duration::from_secs(6)).await;
        real_b.write_all(b"\r\n\r\n").await.unwrap();
        let mut got = [0u8; 4];
        near_b.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"\r\n\r\n");
        clock.sleep(Duration::from_secs(6)).await;
        real_b.write_all(b"\r\n\r\n").await.unwrap();
        near_b.read_exact(&mut got).await.unwrap();
        clock.sleep(Duration::from_secs(6)).await;
        near_b.write_all(b"SIP").await.unwrap();
        let mut reply = [0u8; 3];
        real_b.read_exact(&mut reply).await.unwrap();
        assert!(!watching.is_finished(), "still active at 18 s");
        // Then silence: expires 10 s after the last transfer (at 18 s) = 28 s.
        let (expired, at) = watching.await.unwrap();
        assert!(expired);
        assert_eq!(at - began, Duration::from_secs(28));
        ended.cancel();
        relayed.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_connection_stops_the_watch_and_zero_never_expires() {
        let clock: Arc<dyn Clock> = Arc::new(SimClock::new());
        let activity = Activity::new(clock.now());
        let ended = CancellationToken::new();
        let ender = ended.clone();
        let timer = Arc::clone(&clock);
        tokio::spawn(async move {
            timer.sleep(Duration::from_secs(5)).await;
            ender.cancel();
        });
        assert!(!watch_idle(&*clock, &activity, Duration::from_secs(300), &ended).await);
        let ended = CancellationToken::new();
        let ender = ended.clone();
        let timer = Arc::clone(&clock);
        tokio::spawn(async move {
            timer.sleep(Duration::from_secs(86_400)).await;
            ender.cancel();
        });
        assert!(!watch_idle(&*clock, &activity, Duration::ZERO, &ended).await);
    }

    #[tokio::test]
    async fn loopback_pairs_are_connected_to_each_other() {
        let pairs = LoopbackPairs::bind().await.unwrap();
        let (mut near, mut far) = pairs.pair().await.unwrap();
        near.write_all(b"hello").await.unwrap();
        let mut got = [0u8; 5];
        far.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello");
    }
}
