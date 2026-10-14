//! Flood limits of the signaling endpoint: who the client is behind a reverse proxy,
//! and a per-address token bucket for offers.
//!
//! # Client address
//! The signaling port usually sits behind an HTTPS reverse proxy, so the TCP peer is the proxy and
//! a per-address limit would count every visitor as one. [`client_ip`] reads **one** header, the
//! configured [`ClientIpHeader`] (`X-Forwarded-For` by default, `Forwarded`, RFC 7239, or
//! `X-Real-IP`, one address; the others are never looked at, so a client cannot plant a value in the header the proxy does not
//! overwrite), **only when the TCP peer is in the configured trusted-proxy list**, and takes the
//! **last hop that is not itself a trusted proxy**: the entry the nearest trusted proxy wrote. Entries further left are whatever the client claimed and are never used.
//! From any other peer the headers are ignored (a stranger cannot pick the address it is limited
//! under). An entry that does not parse (`unknown`, an obfuscated `_token`) ends the walk at the
//! proxy's own address.
//!
//! List the proxies' real addresses. A list such as `0.0.0.0/0` makes every client a "trusted
//! proxy": the header of a client that connects directly is read, and every entry in its own
//! address family is skipped as "a proxy", so the limit falls back to the peer, **but** an
//! entry of the other family (an IPv6 address when only `0.0.0.0/0` is listed, and the reverse)
//! is believed, and so every client picks its own address and the per-address limits stop
//! limiting.
//!
//! # Offers per address
//! [`OfferRateLimiter`] is a token bucket per address (an IPv6 address counts as its `/64`): a
//! burst of `per_minute` offers, refilled at `per_minute` a minute, on the injected clock. The
//! table holds at most [`MAX_TRACKED_ADDRESSES`] addresses. When it is full, buckets that have
//! refilled completely (they carry no information) are dropped first; if it is still full the
//! offer is **allowed untracked** (the limiter fails open, because failing closed would let an
//! attacker with many addresses lock everyone out; the call limit, the per-address call cap and the
//! handshake cap still bound the damage) and `table_full` counts it.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use tokio::time::Instant;

use crate::cidr::Cidr;

/// Which forwarding header carries the client address behind a trusted reverse proxy
/// (`BRISVOX_CLIENT_IP_HEADER`). Exactly one is believed; set the one your proxy writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ClientIpHeader {
    /// `X-Forwarded-For` (what nginx, Caddy, HAProxy, Cloudflare and AWS ALB write).
    #[default]
    XForwardedFor,
    /// `Forwarded` (RFC 7239), `for=` elements.
    Forwarded,
    /// `X-Real-IP`: one address, the client's. Railway's edge writes it (and overwrites a copy
    /// the client sent; its docs do not list `X-Forwarded-For`); nginx can too. A value that is
    /// not exactly one address (a repeated header, a list, text) is ignored.
    XRealIp,
}

impl ClientIpHeader {
    /// The HTTP header name, lower case.
    pub const fn header_name(self) -> &'static str {
        match self {
            Self::XForwardedFor => "x-forwarded-for",
            Self::Forwarded => "forwarded",
            Self::XRealIp => "x-real-ip",
        }
    }

    /// Parses `x-forwarded-for`, `forwarded` or `x-real-ip` (case-insensitive).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "x-forwarded-for" => Some(Self::XForwardedFor),
            "forwarded" => Some(Self::Forwarded),
            "x-real-ip" => Some(Self::XRealIp),
            _ => None,
        }
    }
}

/// Addresses the offer limiter tracks at once.
pub(crate) const MAX_TRACKED_ADDRESSES: usize = 8192;

/// The client's address for limiting: `peer`, or behind a trusted proxy the last hop that is not
/// a trusted proxy. `value` is the (joined) value of the header `header` names. See the module
/// docs.
pub(crate) fn client_ip(
    peer: IpAddr,
    trusted: &[Cidr],
    header: ClientIpHeader,
    value: Option<&str>,
) -> IpAddr {
    let is_trusted = |ip: IpAddr| is_trusted_proxy(trusted, ip);
    if !is_trusted(peer) {
        return peer;
    }
    let Some(value) = value else {
        return peer;
    };
    let chain: Vec<&str> = match header {
        ClientIpHeader::XForwardedFor => value.split(',').map(str::trim).collect(),
        ClientIpHeader::Forwarded => forwarded_hops(value),
        // One address: a list ("a, b", from a repeated header) fails to parse below and ends
        // the walk at the peer.
        ClientIpHeader::XRealIp => vec![value.trim()],
    };
    for hop in chain.into_iter().rev() {
        let Some(ip) = parse_hop(hop) else {
            return peer;
        };
        if !is_trusted(ip) {
            return ip;
        }
    }
    peer
}

/// Whether `ip` is in the trusted-proxy list.
pub(crate) fn is_trusted_proxy(trusted: &[Cidr], ip: IpAddr) -> bool {
    trusted.iter().any(|cidr| cidr.contains(ip))
}

/// The `for=` values of a `Forwarded` header, in order.
fn forwarded_hops(value: &str) -> Vec<&str> {
    value
        .split(',')
        .filter_map(|element| {
            element.split(';').map(str::trim).find_map(|pair| {
                let (name, node) = pair.split_once('=')?;
                name.trim()
                    .eq_ignore_ascii_case("for")
                    .then(|| node.trim().trim_matches('"'))
            })
        })
        .collect()
}

/// An address from a forwarding header: `1.2.3.4`, `1.2.3.4:5678`, `2001:db8::1`,
/// `[2001:db8::1]`, `[2001:db8::1]:5678`.
fn parse_hop(hop: &str) -> Option<IpAddr> {
    if let Some(rest) = hop.strip_prefix('[') {
        let (address, _port) = rest.split_once(']')?;
        return address.parse().ok();
    }
    if let Ok(ip) = hop.parse::<IpAddr>() {
        return Some(ip);
    }
    let (address, port) = hop.rsplit_once(':')?;
    port.parse::<u16>().ok()?;
    address.parse::<std::net::Ipv4Addr>().ok().map(IpAddr::V4)
}

/// What the limiter keys on: IPv4-mapped IPv6 as IPv4, other IPv6 by its `/64`.
pub(crate) fn rate_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || {
                let masked = u128::from(v6) & !u128::from(u64::MAX);
                IpAddr::V6(Ipv6Addr::from(masked))
            },
            IpAddr::V4,
        ),
    }
}

struct Bucket {
    /// Thousandths of an offer.
    milli_tokens: u64,
    last: Instant,
}

/// A token bucket per client address (see the module docs).
pub(crate) struct OfferRateLimiter {
    per_minute: u32,
    capacity: u64,
    buckets: HashMap<IpAddr, Bucket>,
    /// Offers let through untracked because the table was full.
    pub(crate) table_full: u64,
}

impl OfferRateLimiter {
    /// A limiter of `per_minute` offers a minute (and a burst of as many), at least 1.
    pub(crate) fn new(per_minute: u32) -> Self {
        let per_minute = per_minute.max(1);
        Self {
            per_minute,
            capacity: u64::from(per_minute) * 1000,
            buckets: HashMap::new(),
            table_full: 0,
        }
    }

    /// Seconds a refused client should wait for its next offer.
    pub(crate) fn retry_after_secs(&self) -> u64 {
        60u64.div_ceil(u64::from(self.per_minute)).max(1)
    }

    fn refilled(&self, bucket: &Bucket, now: Instant) -> u64 {
        let elapsed_ms = u64::try_from(now.saturating_duration_since(bucket.last).as_millis())
            .unwrap_or(u64::MAX);
        let added = elapsed_ms.saturating_mul(u64::from(self.per_minute)) / 60;
        bucket.milli_tokens.saturating_add(added).min(self.capacity)
    }

    /// Takes one offer from `ip`'s bucket; `false` when it is empty.
    pub(crate) fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        let key = rate_key(ip);
        if let Some(bucket) = self.buckets.get(&key) {
            let tokens = self.refilled(bucket, now);
            let allowed = tokens >= 1000;
            if let Some(bucket) = self.buckets.get_mut(&key) {
                bucket.milli_tokens = if allowed { tokens - 1000 } else { tokens };
                bucket.last = now;
            }
            return allowed;
        }
        if self.buckets.len() >= MAX_TRACKED_ADDRESSES {
            let capacity = self.capacity;
            let per_minute = u64::from(self.per_minute);
            self.buckets.retain(|_, bucket| {
                let elapsed_ms =
                    u64::try_from(now.saturating_duration_since(bucket.last).as_millis())
                        .unwrap_or(u64::MAX);
                bucket
                    .milli_tokens
                    .saturating_add(elapsed_ms.saturating_mul(per_minute) / 60)
                    < capacity
            });
            if self.buckets.len() >= MAX_TRACKED_ADDRESSES {
                self.table_full = self.table_full.saturating_add(1);
                return true;
            }
        }
        self.buckets.insert(
            key,
            Bucket {
                milli_tokens: self.capacity - 1000,
                last: now,
            },
        );
        true
    }

    /// Addresses tracked now.
    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // the paused tokio clock is the test's time source
mod tests {
    use std::time::Duration;

    use super::*;

    const XFF: ClientIpHeader = ClientIpHeader::XForwardedFor;
    const FWD: ClientIpHeader = ClientIpHeader::Forwarded;
    const REAL: ClientIpHeader = ClientIpHeader::XRealIp;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn cidrs(list: &[&str]) -> Vec<Cidr> {
        list.iter().map(|c| c.parse().unwrap()).collect()
    }

    #[test]
    fn x_real_ip_is_one_address_believed_only_from_a_trusted_proxy() {
        let trusted = cidrs(&["100.64.0.0/10"]);
        let proxy = ip("100.64.0.7");
        // IPv4 and IPv6, with surrounding blanks.
        assert_eq!(
            client_ip(proxy, &trusted, REAL, Some(" 203.0.113.9 ")),
            ip("203.0.113.9")
        );
        assert_eq!(
            client_ip(proxy, &trusted, REAL, Some("2001:db8::9")),
            ip("2001:db8::9")
        );
        assert_eq!(
            client_ip(proxy, &trusted, REAL, Some("[2001:db8::9]")),
            ip("2001:db8::9")
        );
        // Garbage, a list (a repeated header) and an empty value are ignored: the peer stands.
        for value in [
            "unknown",
            "1.2.3.4, 5.6.7.8",
            "",
            "1.2.3.4 5.6.7.8",
            "999.1.1.1",
        ] {
            assert_eq!(
                client_ip(proxy, &trusted, REAL, Some(value)),
                proxy,
                "{value:?}"
            );
        }
        assert_eq!(client_ip(proxy, &trusted, REAL, None), proxy);
        // A trusted proxy's own address is not a client.
        assert_eq!(client_ip(proxy, &trusted, REAL, Some("100.64.0.8")), proxy);
        // From any other peer the header is ignored: a stranger cannot pick its address.
        assert_eq!(
            client_ip(ip("203.0.113.9"), &trusted, REAL, Some("1.2.3.4")),
            ip("203.0.113.9")
        );
        assert_eq!(
            client_ip(ip("203.0.113.9"), &[], REAL, Some("1.2.3.4")),
            ip("203.0.113.9")
        );
        // Only the configured header is read: `x-forwarded-for` content is not x-real-ip.
        assert_eq!(
            client_ip(proxy, &trusted, XFF, Some("1.2.3.4")),
            ip("1.2.3.4")
        );
    }

    #[test]
    fn forwarding_headers_from_an_untrusted_peer_are_ignored() {
        let trusted = cidrs(&["10.0.0.0/8"]);
        assert_eq!(
            client_ip(ip("203.0.113.9"), &trusted, XFF, Some("1.2.3.4")),
            ip("203.0.113.9")
        );
        assert_eq!(
            client_ip(ip("203.0.113.9"), &[], XFF, Some("1.2.3.4")),
            ip("203.0.113.9")
        );
        assert_eq!(
            client_ip(ip("203.0.113.9"), &[], FWD, Some("for=5.6.7.8")),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn behind_a_trusted_proxy_the_last_untrusted_hop_is_the_client() {
        let trusted = cidrs(&["10.0.0.0/8", "192.0.2.1"]);
        let proxy = ip("10.1.1.1");
        // The client lied at the left; the proxy appended the address it saw.
        assert_eq!(
            client_ip(proxy, &trusted, XFF, Some("6.6.6.6, 198.51.100.7")),
            ip("198.51.100.7")
        );
        // Two trusted hops at the right are skipped.
        assert_eq!(
            client_ip(
                proxy,
                &trusted,
                XFF,
                Some("198.51.100.7, 192.0.2.1, 10.2.2.2")
            ),
            ip("198.51.100.7")
        );
        assert_eq!(
            client_ip(proxy, &trusted, XFF, Some("[2001:db8::7]:4444")),
            ip("2001:db8::7")
        );
        assert_eq!(
            client_ip(proxy, &trusted, XFF, Some("198.51.100.7:5555")),
            ip("198.51.100.7")
        );
        // Forwarded (RFC 7239)
        assert_eq!(
            client_ip(
                proxy,
                &trusted,
                FWD,
                Some("for=6.6.6.6;proto=https, For=\"[2001:db8::9]:1\";by=10.1.1.1")
            ),
            ip("2001:db8::9")
        );
        // Nothing usable: the proxy's own address.
        assert_eq!(client_ip(proxy, &trusted, XFF, None), proxy);
        assert_eq!(client_ip(proxy, &trusted, XFF, Some("unknown")), proxy);
        assert_eq!(client_ip(proxy, &trusted, XFF, Some("10.5.5.5")), proxy);
        assert_eq!(client_ip(proxy, &trusted, XFF, Some("")), proxy);
    }

    #[test]
    fn only_the_configured_header_is_believed() {
        let trusted = cidrs(&["10.0.0.0/8"]);
        let proxy = ip("10.1.1.1");
        // The proxy writes only `Forwarded`; the client planted an X-Forwarded-For. The caller
        // reads the configured header only, so the planted one never reaches `client_ip`.
        assert_eq!(
            client_ip(proxy, &trusted, FWD, Some("for=198.51.100.7")),
            ip("198.51.100.7")
        );
        // An XFF-shaped value read as `Forwarded` (no `for=`) yields nothing.
        assert_eq!(client_ip(proxy, &trusted, FWD, Some("198.51.100.7")), proxy);
        // 0.0.0.0/0 as the trusted list: every IPv4 peer is a proxy and every IPv4 hop is
        // skipped (back to the peer), but a client writing an IPv6 hop picks its own address.
        let everyone = cidrs(&["0.0.0.0/0"]);
        assert_eq!(
            client_ip(ip("203.0.113.9"), &everyone, XFF, Some("1.2.3.4")),
            ip("203.0.113.9")
        );
        assert_eq!(
            client_ip(ip("203.0.113.9"), &everyone, XFF, Some("2001:db8::bad")),
            ip("2001:db8::bad"),
            "the documented danger of a catch-all trusted-proxy list"
        );
    }

    #[test]
    fn the_header_choice_parses() {
        assert_eq!(ClientIpHeader::default(), ClientIpHeader::XForwardedFor);
        assert_eq!(
            ClientIpHeader::parse(" X-Forwarded-For "),
            Some(ClientIpHeader::XForwardedFor)
        );
        assert_eq!(
            ClientIpHeader::parse("Forwarded"),
            Some(ClientIpHeader::Forwarded)
        );
        assert_eq!(
            ClientIpHeader::parse(" X-Real-IP "),
            Some(ClientIpHeader::XRealIp)
        );
        assert_eq!(ClientIpHeader::parse("x-client-ip"), None);
        assert_eq!(ClientIpHeader::XRealIp.header_name(), "x-real-ip");
        assert_eq!(ClientIpHeader::Forwarded.header_name(), "forwarded");
        assert_eq!(
            ClientIpHeader::XForwardedFor.header_name(),
            "x-forwarded-for"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_bucket_allows_a_burst_then_refills_over_time() {
        let mut limiter = OfferRateLimiter::new(5);
        let start = Instant::now();
        let who = ip("198.51.100.1");
        for n in 0..5 {
            assert!(limiter.allow(who, start), "offer {n}");
        }
        assert!(!limiter.allow(who, start));
        assert!(limiter.allow(ip("198.51.100.2"), start), "another address");
        // 12 s refill one offer at 5 a minute.
        assert!(!limiter.allow(who, start + Duration::from_secs(6)));
        assert!(limiter.allow(who, start + Duration::from_secs(12)));
        assert!(!limiter.allow(who, start + Duration::from_secs(12)));
        assert_eq!(limiter.retry_after_secs(), 12);
    }

    #[tokio::test(start_paused = true)]
    async fn an_ipv6_address_is_limited_by_its_64() {
        let mut limiter = OfferRateLimiter::new(1);
        let start = Instant::now();
        assert!(limiter.allow(ip("2001:db8:1:2::1"), start));
        assert!(!limiter.allow(ip("2001:db8:1:2:ffff::9"), start));
        assert!(limiter.allow(ip("2001:db8:1:3::1"), start));
        assert!(limiter.allow(ip("::ffff:198.51.100.1"), start));
        assert!(!limiter.allow(ip("198.51.100.1"), start), "mapped = v4");
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_table_drops_idle_buckets_and_then_fails_open() {
        let mut limiter = OfferRateLimiter::new(5);
        let start = Instant::now();
        for n in 0..MAX_TRACKED_ADDRESSES {
            let n = u32::try_from(n).unwrap();
            let address = IpAddr::from(std::net::Ipv4Addr::from(0x0a00_0000 + n));
            assert!(limiter.allow(address, start));
        }
        assert_eq!(limiter.tracked(), MAX_TRACKED_ADDRESSES);
        // All full of recent information: a new address is let through untracked.
        assert!(limiter.allow(ip("203.0.113.1"), start));
        assert_eq!(limiter.table_full, 1);
        assert_eq!(limiter.tracked(), MAX_TRACKED_ADDRESSES);
        // A minute later every bucket is full again and is dropped to make room.
        assert!(limiter.allow(ip("203.0.113.2"), start + Duration::from_secs(61)));
        assert_eq!(limiter.tracked(), 1);
    }
}
