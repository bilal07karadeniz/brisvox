//! IP networks in CIDR notation: the SIP source allow-list and the WebRTC trusted-proxy list.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use crate::error::Error;

/// An IP network in CIDR notation (`192.0.2.0/24`, `2001:db8::/32`, or a single address).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// A network of `prefix` leading bits (the host bits of `network` are ignored).
    ///
    /// # Errors
    /// [`Error::Config`] when `prefix` exceeds the address width (32 or 128).
    pub fn new(network: IpAddr, prefix: u8) -> Result<Self, Error> {
        let width = if network.is_ipv4() { 32 } else { 128 };
        if prefix > width {
            return Err(Error::Config(format!(
                "CIDR prefix {prefix} is longer than the {width}-bit address"
            )));
        }
        Ok(Self { network, prefix })
    }

    /// Whether `address` is inside this network. An IPv4-mapped IPv6 address matches the IPv4
    /// network it stands for.
    pub fn contains(&self, address: IpAddr) -> bool {
        let address = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
            v4 @ IpAddr::V4(_) => v4,
        };
        match (self.network, address) {
            (IpAddr::V4(net), IpAddr::V4(addr)) => {
                prefix_matches(&net.octets(), &addr.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(addr)) => {
                prefix_matches(&net.octets(), &addr.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

/// Compares the first `prefix` bits of two equally long byte strings.
fn prefix_matches(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let whole = usize::from(prefix / 8);
    let rest = prefix % 8;
    if a.get(..whole) != b.get(..whole) {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    match (a.get(whole), b.get(whole)) {
        (Some(x), Some(y)) => x & mask == y & mask,
        _ => false,
    }
}

impl FromStr for Cidr {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        let bad = || Error::Config(format!("{text:?} is not an IP address or CIDR block"));
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let network: IpAddr = address.trim().parse().map_err(|_| bad())?;
        let width = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.trim().parse::<u8>().map_err(|_| bad())?,
            None => width,
        };
        Self::new(network, prefix)
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(text: &str) -> Cidr {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn blocks_match_their_range_only() {
        let net = cidr("54.172.60.0/30");
        assert!(net.contains(ip("54.172.60.0")));
        assert!(net.contains(ip("54.172.60.3")));
        assert!(!net.contains(ip("54.172.60.4")));
        assert!(!net.contains(ip("54.172.61.0")));
        let wide = cidr("168.86.128.0/18");
        assert!(wide.contains(ip("168.86.191.255")));
        assert!(!wide.contains(ip("168.86.192.0")));
        assert!(cidr("0.0.0.0/0").contains(ip("203.0.113.5")));
    }

    #[test]
    fn a_bare_address_is_a_single_host() {
        let host = cidr("192.0.2.7");
        assert!(host.contains(ip("192.0.2.7")));
        assert!(!host.contains(ip("192.0.2.8")));
        assert_eq!(host.to_string(), "192.0.2.7/32");
    }

    #[test]
    fn ipv6_and_mapped_addresses() {
        let net = cidr("2001:db8::/32");
        assert!(net.contains(ip("2001:db8:1::1")));
        assert!(!net.contains(ip("2001:db9::1")));
        assert!(!net.contains(ip("192.0.2.1")));
        assert!(cidr("192.0.2.0/24").contains(ip("::ffff:192.0.2.9")));
    }

    #[test]
    fn bad_blocks_are_refused() {
        for bad in ["", "x", "1.2.3.4/33", "1.2.3.4/x", "::1/129", "1.2.3/24"] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad:?} parsed");
        }
    }
}
