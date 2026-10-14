//! TCP dialing with address racing ("happy eyeballs", RFC 8305), shared by every WebSocket
//! adapter (found by the Cartesia live run).
//!
//! `api.cartesia.ai` is served from CloudFront and resolves to eight IPv6 and four IPv4
//! addresses. `tokio_tungstenite::connect_async` hands the host to `TcpStream::connect`, which
//! tries the addresses one after the other and waits for the OS to give up on each. On a host
//! with an IPv6 route that silently drops packets (common on residential and VPN setups) every
//! IPv6 attempt takes the OS connect timeout, so the handshake never finishes inside the
//! adapter's deadline although the IPv4 addresses answer in milliseconds. This module races the
//! addresses instead: families alternate, a new attempt starts 250 ms after the previous one
//! (or at once when the previous one failed), and the first socket that connects wins.
//!
//! Everything here is cancel-safe: dropping the future drops every in-flight connect attempt.
//! The caller bounds the whole dial (name lookup included) with its own deadline.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use brisvox_core::Clock;
use brisvox_core::clock::wait_for;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::net::TcpStream;

/// Delay before the next address is tried while earlier attempts are still pending (RFC 8305
/// "Connection Attempt Delay").
pub const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// At most this many addresses are tried per dial (bounds the sockets in flight).
pub const MAX_ADDRS: usize = 8;

/// Orders `addrs` for racing: the families alternate, starting with the family of the first
/// resolver answer, and the list is capped at [`MAX_ADDRS`]. The order inside a family is kept.
pub fn interleave(addrs: impl IntoIterator<Item = SocketAddr>) -> VecDeque<SocketAddr> {
    let mut first_family = None;
    let mut same = VecDeque::new();
    let mut other = VecDeque::new();
    for addr in addrs {
        let v6 = addr.is_ipv6();
        if *first_family.get_or_insert(v6) == v6 {
            same.push_back(addr);
        } else {
            other.push_back(addr);
        }
    }
    let mut out = VecDeque::new();
    while out.len() < MAX_ADDRS && (!same.is_empty() || !other.is_empty()) {
        if let Some(addr) = same.pop_front() {
            out.push_back(addr);
        }
        if out.len() < MAX_ADDRS
            && let Some(addr) = other.pop_front()
        {
            out.push_back(addr);
        }
    }
    out
}

/// Resolves `host` (a name or an IP literal without brackets) and connects to the first
/// address that answers (Nagle disabled).
///
/// # Errors
/// The last connect error, or `InvalidInput` when the name resolves to nothing. The lookup and
/// the connects are unbounded here: the caller applies the deadline.
pub async fn dial(host: &str, port: u16, clock: &dyn Clock) -> io::Result<TcpStream> {
    let addrs = interleave(tokio::net::lookup_host((host, port)).await?);
    let stream = race(addrs, clock).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Races connects to `remaining` (see the module docs).
async fn race(mut remaining: VecDeque<SocketAddr>, clock: &dyn Clock) -> io::Result<TcpStream> {
    let mut pending = FuturesUnordered::new();
    let mut last_error = None;
    loop {
        if let Some(addr) = remaining.pop_front() {
            pending.push(TcpStream::connect(addr));
        }
        if remaining.is_empty() {
            // Nothing left to start: wait for the attempts in flight.
            while let Some(result) = pending.next().await {
                match result {
                    Ok(stream) => return Ok(stream),
                    Err(e) => last_error = Some(e),
                }
            }
            return Err(last_error.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the host resolved to no address",
                )
            }));
        }
        // More addresses wait: start the next one after the delay, or as soon as every attempt
        // in flight has failed.
        let delay = wait_for(clock, ATTEMPT_DELAY);
        tokio::pin!(delay);
        loop {
            tokio::select! {
                result = pending.next(), if !pending.is_empty() => match result {
                    Some(Ok(stream)) => return Ok(stream),
                    Some(Err(e)) => {
                        last_error = Some(e);
                        if pending.is_empty() {
                            break;
                        }
                    }
                    None => break,
                },
                () = &mut delay => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(n: u8) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::new(10, 0, 0, n), 443))
    }

    fn v6(n: u16) -> SocketAddr {
        SocketAddr::from((Ipv6Addr::new(0x2600, 0, 0, 0, 0, 0, 0, n), 443))
    }

    #[test]
    fn families_alternate_starting_with_the_first_answer() {
        let order = interleave([v6(1), v6(2), v6(3), v4(1), v4(2)]);
        assert_eq!(
            order.into_iter().collect::<Vec<_>>(),
            vec![v6(1), v4(1), v6(2), v4(2), v6(3)]
        );
        let order = interleave([v4(1), v6(1), v6(2)]);
        assert_eq!(
            order.into_iter().collect::<Vec<_>>(),
            vec![v4(1), v6(1), v6(2)]
        );
    }

    #[test]
    fn the_list_is_capped_and_an_empty_answer_stays_empty() {
        let many = (1..=20).map(v6);
        assert_eq!(interleave(many).len(), MAX_ADDRS);
        assert!(interleave(std::iter::empty()).is_empty());
    }

    #[tokio::test]
    async fn a_reachable_address_wins_and_no_address_is_an_error() {
        let clock = brisvox_core::clock::SystemClock;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = race(VecDeque::from([addr]), &clock).await.unwrap();
        assert!(stream.peer_addr().is_ok());
        let err = race(VecDeque::new(), &clock).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn a_dead_address_does_not_block_the_next_one() {
        let clock = brisvox_core::clock::SystemClock;
        // A bound-then-dropped port refuses at once; the live listener must still win.
        let dead = {
            let l = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            l.local_addr().unwrap()
        };
        let live = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let live_addr = live.local_addr().unwrap();
        let stream = race(VecDeque::from([dead, live_addr]), &clock)
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), live_addr);
    }
}
