//! The transport rule of the agent link: a plaintext `ws://` URL is
//! accepted only when the agent process is on a private network, because the link carries
//! transcripts, the LLM context and tool calls. Everything else needs `wss://` or an explicit
//! `allow_insecure`.
//!
//! "Private" means the host is, by its literal text (no DNS lookup, so the rule is a pure
//! function and cannot be steered by a resolver):
//! - an IPv4 loopback (`127.0.0.0/8`), RFC 1918 (`10/8`, `172.16/12`, `192.168/16`) or
//!   link-local (`169.254/16`) address;
//! - an IPv6 loopback (`::1`), unique-local (`fc00::/7`) or link-local (`fe80::/10`) address,
//!   or an IPv4-mapped one (`::ffff:a.b.c.d`) whose IPv4 part is private. A zone id
//!   (`fe80::1%eth0`, or `fe80::1%25eth0` in a URL, RFC 6874) is accepted **only on a link-local
//!   address**, where it belongs; on any other address the zone is ignored for the verdict, so
//!   a public address with a zone is still public;
//! - `localhost`, or a name under `.localhost`, `.internal` (Railway, AWS, GCP private DNS) or
//!   `.local` (mDNS, Kubernetes `*.cluster.local`);
//! - a single-label name without a dot (a Docker Compose or Kubernetes service name such as
//!   `agent`). It is not a public DNS name by itself, **but a resolver's search list can turn it
//!   into one** (`agent` becomes `agent.example.com` under `search example.com`), so it is
//!   accepted with a one-time warning: use the full `.internal`/`.local` name or an address
//!   where the resolver is not yours.

use std::sync::atomic::{AtomicBool, Ordering};

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use tokio_tungstenite::tungstenite::http::Uri;

/// Whether the host of the `ws://` or `wss://` URL `url` is a private-network host (see the
/// module docs). `false` for a URL that does not parse or has no host.
#[must_use]
pub fn host_is_private(url: &str) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    uri.host().is_some_and(private_host)
}

/// [`host_is_private`] for a bare host as `http::Uri::host` returns it (IPv6 in brackets).
fn private_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    // A zone id (`%eth0`, URL-encoded `%25eth0`) belongs to a link-local IPv6 address only.
    if let Some((address, zone)) = bare.split_once('%') {
        let Ok(v6) = address.parse::<Ipv6Addr>() else {
            return false; // a host name cannot contain `%`, an IPv4 address has no zone
        };
        // In a URL the `%` is written `%25` (RFC 6874): that prefix is not part of the zone.
        let zone = zone.strip_prefix("25").unwrap_or(zone);
        return zone_ok(zone) && is_link_local_v6(v6);
    }
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => private_v4(v4),
            IpAddr::V6(v6) => private_v6(v6),
        };
    }
    // A name. Lowercase, and a trailing root dot does not change it.
    let name = bare.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    // Digits and dots that were not an IPv4 address (`10.0.0`, `1.2.3.4.5`) are not a name
    // either: refuse them rather than let a single-label rule pass `10`.
    if name.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return false;
    }
    if name == "localhost"
        || name.ends_with(".localhost")
        || name.ends_with(".internal")
        || name.ends_with(".local")
    {
        return true;
    }
    if !name.contains('.') {
        if first_time(&SINGLE_LABEL_WARNED) {
            tracing::warn!(
                host = %name,
                "agent link: the single-label host is accepted as a private service name, but \
                 the resolver's search list can turn it into a public name; prefer the full \
                 .internal/.local name or an address"
            );
        }
        return true;
    }
    false
}

/// Set once the single-label warning was logged.
static SINGLE_LABEL_WARNED: AtomicBool = AtomicBool::new(false);

/// `true` the first time it is called with `flag` (and sets it).
fn first_time(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

/// A zone id: 1 to 64 interface-name characters (also `%` for the URL-encoded form).
fn zone_ok(zone: &str) -> bool {
    (1..=64).contains(&zone.len())
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~' | b'%'))
}

fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments().first().copied().unwrap_or(0);
    (first & 0xffc0) == 0xfe80
}

/// `url` reduced to `scheme://host[:port]`: userinfo (a token or password in the URL), path and
/// query are dropped, so the result is safe to log or put in an error. A text that is not a URL
/// becomes `<invalid url>`.
#[must_use]
pub fn redact_url(url: &str) -> String {
    let Ok(uri) = url.trim().parse::<Uri>() else {
        return "<invalid url>".to_owned();
    };
    let (Some(scheme), Some(host)) = (uri.scheme_str(), uri.host()) else {
        return "<invalid url>".to_owned();
    };
    match uri.port_u16() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

fn private_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn private_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return private_v4(v4);
    }
    let first = ip.segments().first().copied().unwrap_or(0);
    ip.is_loopback() || (first & 0xfe00) == 0xfc00 || is_link_local_v6(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_hosts_are_accepted() {
        for url in [
            "ws://127.0.0.1:9000/agent",
            "ws://127.9.9.9",
            "ws://10.0.0.5:8080",
            "ws://172.16.0.1",
            "ws://172.31.255.254",
            "ws://192.168.1.10/x",
            "ws://169.254.169.254",
            "ws://[::1]:9000/agent",
            "ws://[fc00::1]",
            "ws://[fd12:3456::1]:80",
            "ws://[fe80::1]",
            "ws://[febf::1]",
            "ws://[::ffff:10.1.2.3]",
            "ws://[::ffff:127.0.0.1]",
            "ws://localhost:7000",
            "ws://LOCALHOST",
            "ws://localhost.:7000",
            "ws://agent.localhost",
            "ws://agent.railway.internal:8080",
            "ws://agent.internal",
            "ws://agent.default.svc.cluster.local",
            "ws://printer.local",
            "ws://agent:8080",
            "ws://user:pw@10.0.0.1:80/path?x=1",
            "wss://10.0.0.5",
        ] {
            assert!(host_is_private(url), "{url} should be private");
        }
    }

    #[test]
    fn public_and_odd_hosts_are_refused() {
        for url in [
            "ws://8.8.8.8",
            "ws://1.1.1.1:80",
            "ws://172.15.0.1",
            "ws://172.32.0.1",
            "ws://192.169.0.1",
            "ws://169.253.0.1",
            "ws://100.64.0.1",
            "ws://[2001:db8::1]",
            "ws://[2606:4700::1111]",
            "ws://[fec0::1]",
            "ws://[fe00::1]",
            "ws://[fb00::1]",
            "ws://[::ffff:8.8.8.8]",
            "ws://[::]",
            "ws://0.0.0.0",
            "ws://agent.example.com",
            "ws://example.com:80",
            "ws://internal.example.com",
            "ws://evil-localhost.com",
            "ws://localhost.example.com",
            "ws://agent.internal.example.com",
            "ws://10.0.0",
            "ws://10",
            "ws://1.2.3.4.5",
            "ws://",
            "not a url",
            "",
        ] {
            assert!(!host_is_private(url), "{url} should be refused");
        }
    }

    #[test]
    fn a_zone_id_counts_only_on_a_link_local_address() {
        for url in [
            "ws://[fe80::1%25eth0]:9000/agent",
            "ws://[fe80::1%eth0]",
            "ws://[fe80::abcd:1%25en0]",
            "ws://[febf::1%25wlan0]",
        ] {
            assert!(host_is_private(url), "{url} should be private");
        }
        for url in [
            // A public address stays public with a zone, and so do addresses where a zone
            // makes no sense.
            "ws://[2001:db8::1%25eth0]",
            "ws://[2606:4700::1111%eth0]",
            "ws://[::ffff:8.8.8.8%25eth0]",
            "ws://[fc00::1%25eth0]",
            "ws://[::1%25lo]",
            "ws://[fe80::1%25]",
            "ws://[fe80::1%25eth0/../x]",
            "ws://10.0.0.1%25eth0",
            "ws://agent%25eth0",
        ] {
            assert!(!host_is_private(url), "{url} should be refused");
        }
        assert!(private_host("[fe80::1%eth0]"));
        assert!(!private_host("[fe80::1%]"));
        assert!(!private_host("[fe80::1%a b]"));
        assert!(!private_host("[2001:db8::1%eth0]"));
        assert!(!private_host(&format!("[fe80::1%{}]", "z".repeat(65))));
    }

    #[test]
    fn the_single_label_warning_is_given_once() {
        let flag = AtomicBool::new(false);
        assert!(first_time(&flag));
        assert!(!first_time(&flag));
        assert!(!first_time(&flag));
        // The verdict does not change with the warning.
        assert!(host_is_private("ws://agent:8080"));
        assert!(host_is_private("ws://agent:8080"));
    }

    #[test]
    fn redaction_keeps_only_scheme_host_and_port() {
        for (url, want) in [
            ("ws://localhost:7000", "ws://localhost:7000"),
            ("wss://agent.example.com", "wss://agent.example.com"),
            (
                "wss://user:s3cret@agent.example.com:8443/path/x?token=abc#frag",
                "wss://agent.example.com:8443",
            ),
            ("ws://tok@10.0.0.1/a", "ws://10.0.0.1"),
            ("ws://[::1]:9000/agent?k=v", "ws://[::1]:9000"),
            (" ws://x:1/?t=1 ", "ws://x:1"),
            ("https://u:p@example.com/a", "https://example.com"),
        ] {
            let redacted = redact_url(url);
            assert_eq!(redacted, want, "{url}");
            for secret in ["s3cret", "token", "abc", "frag", "path", "u:p"] {
                assert!(!redacted.contains(secret), "{url} leaked {secret}");
            }
        }
        for url in ["", "not a url", "/just/a/path?token=abc", "ws://"] {
            assert_eq!(redact_url(url), "<invalid url>", "{url:?}");
        }
    }

    #[test]
    fn userinfo_does_not_hide_a_public_host() {
        assert!(!host_is_private("ws://10.0.0.1@evil.example.com/x"));
        assert!(!host_is_private("ws://localhost:80@evil.example.com/x"));
    }
}
