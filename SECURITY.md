# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue, pull request or discussion.

Use GitHub's private vulnerability reporting: open the
[Security tab](https://github.com/bilal07karadeniz/brisvox/security) of this repository, choose
**Report a vulnerability** and fill in the advisory form. Only the maintainers can see it. If you
cannot use that form, open a public issue that says only that you need a private channel for a
security report, and no details; a maintainer will arrange one.

A useful report has:

- what is affected (crate, package or image, and the version or commit),
- how to reproduce it, ideally a minimal configuration or request,
- what an attacker gains (for example: another caller's audio, a tool running without
  authorization, a denial of service of other calls, a leaked credential).

What to expect from us:

- an acknowledgement within a week,
- a fix or a mitigation plan as soon as the issue is understood, and a coordinated release: the
  fix ships first, then the advisory is published and credits you if you want to be named,
- no legal action against good-faith research that stays within the limits below.

Please do not test against servers you do not own, do not access other people's data, and give us a
reasonable time to fix the problem before you disclose it.

## Supported versions

Security fixes go into the latest release. While the project is below 1.0, that means the latest
`0.x` minor version; older versions are not patched.

| Version | Supported |
|---|---|
| latest `0.x` release | yes |
| anything older | no |

The Rust crates, the Python package `brisvox-agent`, the npm packages `@brisvox/agent` and
`@brisvox/web`, and the container image `ghcr.io/bilal07karadeniz/brisvox-server` share one version
number.

## Scope notes

- The generic websocket listener (`BRISVOX_WS_ADDR`) binds to loopback by default and, on any other
  address, refuses to start without `BRISVOX_WS_TOKEN` or `BRISVOX_SESSION_SECRET` (signed session
  tokens). See the book's operations and deployment chapters for what each listener checks.
- API keys of speech and language providers, carrier credentials and token secrets belong in
  environment variables or your secret store, never in an agent definition file that you commit.
- Dependencies are checked against the RustSec advisory database with `cargo deny` on every push
  and weekly (`deny.toml`).
