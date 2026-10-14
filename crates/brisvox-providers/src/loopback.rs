//! The one rule for "this host is the local machine", shared by the adapters that refuse to
//! send a key over plain `ws://` / `http://` to another machine and by the config checks that
//! report the same mistake while the file is read (they used to disagree about
//! `127.0.0.2`).

/// Whether `host` is a loopback host: `localhost` (any case) or an IP literal in a loopback
/// range (`127.0.0.0/8`, `::1`). `host` is the bare host: IPv6 without brackets, no port, no
/// user info.
pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::is_loopback_host;

    #[test]
    fn loopback_names_and_literals() {
        for yes in ["localhost", "LocalHost", "127.0.0.1", "127.0.0.2", "::1"] {
            assert!(is_loopback_host(yes), "{yes}");
        }
        for no in [
            "",
            "[::1]",
            "10.0.0.5",
            "::",
            "example.com",
            "localhost.evil.com",
            "127.0.0.1.evil.com",
        ] {
            assert!(!is_loopback_host(no), "{no}");
        }
    }
}
