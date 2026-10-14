//! Short-lived TURN credentials for the browser (review 2026-10-13 section 3b).
//!
//! The server is ICE-lite and normally has a reachable address, so it does not need TURN itself
//! (a host without UDP ingress uses the same credentials for its own allocation: `relay`). The
//! *browser* may need a relay to reach it: a client network that blocks UDP (TURNS on 443), or a
//! symmetric NAT. This module issues the credentials the page hands to `RTCPeerConnection`.
//! Three providers (`docs/providers/webrtc.md`, "TURN credentials", fetched 2026-10-13):
//!
//! - **coturn** (`use-auth-secret`): computed locally, no network. Username
//!   `<unix expiry>:<label>`, password `base64(HMAC-SHA1(secret, username))`. The label is
//!   [`session_label`] of the request's session token `sid` when it carried one (the first 16 hex
//!   digits of SHA-256 of the `sid`: relay traffic in coturn's logs traces back to the visitor
//!   and token the application backend issued, and a `sid` with odd characters cannot break a
//!   header), else a random id.
//! - **Cloudflare Realtime TURN**: `POST .../turn/keys/<key id>/credentials/generate-ice-servers`
//!   with `{"ttl": n}` and the API token as bearer.
//! - **Twilio Network Traversal Service**: `POST /2010-04-01/Accounts/<sid>/Tokens.json` with
//!   `Ttl=n`, basic auth.
//!
//! Fetched credentials (Cloudflare, Twilio) are **shared by every caller** for
//! [`TurnConfig::cache`] (a single bounded entry, shorter than the credential lifetime): the
//! provider's bill and logs cannot tell callers apart, and a visitor who copies the credentials
//! can use them until they expire. Use coturn when relay usage must be traceable per session. A fetch has a [`TurnConfig::fetch_timeout`]
//! deadline on the injected clock; a failure is remembered for [`FAILURE_BACKOFF`], so a provider
//! outage costs one slow request per backoff and never fails a call: the page just gets no relay.
//!
//! Everything that goes into a `Link` header or JSON is validated here (upstream answers are
//! data, not trusted): only `stun`/`stuns`/`turn`/`turns` URLs without spaces, quotes, commas,
//! angle brackets, backslashes or control characters, no port 53 (browsers block it), at most
//! [`MAX_SERVERS`] servers with [`MAX_URLS`] URLs each.
//!
//! # Who waits for a provider
//! `POST <whip_path>` waits at most [`SIGNALING_BUDGET`] (lock wait included) for credentials and
//! then goes on with the last ones that still have a quarter of their lifetime left, or none: a
//! provider outage must not stall the offer. `GET <whip_path>/ice` (what the bundled pages call
//! before `createOffer`; the `Link` headers of the `201` come too late for them, so the pages do
//! not read them) may wait the full [`TurnConfig::fetch_timeout`]. A fetch that times out is
//! recorded like any failure ([`FAILURE_BACKOFF`]).
//!
//! # Abuse
//! Credentials are minted for whoever asks `GET <whip_path>/ice` (and `POST`): without a
//! `bearer_token`, anyone who can reach the signaling port can obtain relay credentials, which
//! for coturn are valid for the whole [`TurnConfig::ttl`] and cost relay bandwidth. A per-client
//! token bucket on `GET <whip_path>/ice` bounds the rate; set a bearer token (or keep the
//! signaling port private) when relay traffic costs money.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use brisvox_core::{Secret, SharedClock};
use ring::hmac;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::{Error, Result};

/// How long a failed fetch is remembered (callers get no relay meanwhile, without waiting).
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(10);
/// Servers kept from one provider answer.
pub const MAX_SERVERS: usize = 8;
/// URLs kept per server.
pub const MAX_URLS: usize = 16;
/// Longest wait of `POST <whip_path>` for TURN credentials, lock wait included.
pub const SIGNALING_BUDGET: Duration = Duration::from_millis(1_500);
/// `GET <whip_path>/ice` requests one client may burst before it is limited.
const ICE_BURST: u32 = 5;
/// One `GET <whip_path>/ice` token comes back per this time.
const ICE_REFILL: Duration = Duration::from_secs(2);
/// Clients the `GET <whip_path>/ice` limiter tracks; beyond it, new clients are refused until an
/// entry is idle again (fail closed: the limiter protects relay credentials).
const ICE_MAX_CLIENTS: usize = 4_096;
/// Longest URL, username or credential kept, in bytes.
const MAX_TEXT: usize = 512;
/// Bytes read from a provider answer (they are a few hundred bytes; this bounds a hostile one).
const MAX_BODY: usize = 64 * 1024;

/// Default base URL of the Cloudflare Realtime TURN API.
pub const CLOUDFLARE_BASE: &str = "https://rtc.live.cloudflare.com";
/// Default base URL of the Twilio REST API.
pub const TWILIO_BASE: &str = "https://api.twilio.com";

/// Where credentials come from.
#[derive(Debug, Clone)]
pub enum TurnProvider {
    /// A coturn server with `use-auth-secret` and `static-auth-secret` (or the same secret in its
    /// database).
    Coturn {
        /// `turn:` / `turns:` URLs of the server, e.g. `turns:turn.example.com:443?transport=tcp`.
        urls: Vec<String>,
        /// coturn's `static-auth-secret`.
        secret: Secret<String>,
    },
    /// Cloudflare Realtime TURN.
    Cloudflare {
        /// The TURN key id.
        key_id: String,
        /// The TURN key API token.
        api_token: Secret<String>,
        /// API base URL ([`CLOUDFLARE_BASE`]; a loopback `http://` URL for tests).
        base_url: String,
    },
    /// Twilio Network Traversal Service.
    Twilio {
        /// The account SID (in the URL).
        account_sid: String,
        /// Basic-auth user: the account SID, or an API key SID.
        auth_user: String,
        /// Basic-auth password: the auth token, or the API key secret.
        auth_secret: Secret<String>,
        /// API base URL ([`TWILIO_BASE`]; a loopback `http://` URL for tests).
        base_url: String,
    },
}

/// TURN credential issuing.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// The credential source.
    pub provider: TurnProvider,
    /// Lifetime of issued credentials. 30 s to 24 h. Default 600 s.
    pub ttl: Duration,
    /// How long fetched (Cloudflare, Twilio) credentials are reused for the next callers. At most
    /// half of [`ttl`](Self::ttl). Default 60 s. Not used for coturn (computed per call).
    pub cache: Duration,
    /// Deadline of a provider request. Default 5 s.
    pub fetch_timeout: Duration,
}

impl TurnConfig {
    /// Default credential lifetime.
    pub const DEFAULT_TTL: Duration = Duration::from_secs(600);

    /// A configuration with the default lifetime, cache and fetch deadline.
    pub fn new(provider: TurnProvider) -> Self {
        Self {
            provider,
            ttl: Self::DEFAULT_TTL,
            cache: Duration::from_secs(60),
            fetch_timeout: Duration::from_secs(5),
        }
    }

    /// Checks the configuration.
    ///
    /// # Errors
    /// [`Error::Config`] naming the first broken rule.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Config(format!("webrtc turn: {what}")));
        if !(30..=86_400).contains(&self.ttl.as_secs()) {
            return bad("ttl must be 30 s to 24 h");
        }
        if self.cache > self.ttl / 2 {
            return bad("cache must be at most half of the ttl");
        }
        if self.fetch_timeout.is_zero() {
            return bad("fetch_timeout must be longer than zero");
        }
        match &self.provider {
            TurnProvider::Coturn { urls, secret } => {
                if urls.is_empty() || urls.len() > MAX_URLS {
                    return bad("coturn needs 1 to 16 urls");
                }
                for url in urls {
                    if !url_ok(url) || !(url.starts_with("turn:") || url.starts_with("turns:")) {
                        return bad(
                            "coturn urls must be turn: or turns: URLs without spaces, quotes or brackets",
                        );
                    }
                }
                if secret.expose().is_empty() {
                    return bad("the coturn secret must not be empty");
                }
            }
            TurnProvider::Cloudflare {
                key_id,
                api_token,
                base_url,
            } => {
                if !token_ok(key_id) || api_token.expose().is_empty() {
                    return bad("cloudflare needs a key id and an api token");
                }
                check_base_url(base_url)?;
            }
            TurnProvider::Twilio {
                account_sid,
                auth_user,
                auth_secret,
                base_url,
            } => {
                if !token_ok(account_sid) || !token_ok(auth_user) || auth_secret.expose().is_empty()
                {
                    return bad("twilio needs an account sid and an auth token or api key");
                }
                check_base_url(base_url)?;
            }
        }
        Ok(())
    }
}

/// `https://`, or `http://` on a loopback host (tests): credentials never travel in cleartext to
/// a remote host.
fn check_base_url(base: &str) -> Result<()> {
    let bad = |what: &str| Err(Error::Config(format!("webrtc turn: base url {what}")));
    let Ok(url) = reqwest::Url::parse(base) else {
        return bad("is not a valid absolute URL");
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = url.host_str().unwrap_or_default();
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let loopback = host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            if loopback {
                Ok(())
            } else {
                bad(
                    "is plain http:// on a non-loopback host (credentials would travel in cleartext)",
                )
            }
        }
        _ => bad("must be an http:// or https:// URL"),
    }
}

/// An identifier that goes into a URL path: ASCII alphanumerics, `-` and `_`.
fn token_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Printable ASCII without the characters that would break a quoted `Link` parameter or a URI
/// reference (and without `,`, which separates `Link` values when a proxy folds the headers into
/// one line), and not too long.
fn text_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_TEXT
        && text
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b'"' | b'\\' | b'<' | b'>' | b','))
}

/// A usable ICE server URL: `stun:`, `stuns:`, `turn:` or `turns:`, no port 53.
fn url_ok(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    if !matches!(scheme, "stun" | "stuns" | "turn" | "turns") || !text_ok(url) {
        return false;
    }
    let host_port = rest.split(['?', '/']).next().unwrap_or_default();
    // Port 53 is blocked by browsers and makes ICE gathering wait for a timeout.
    !host_port.ends_with(":53")
}

/// One ICE server in the shape of a browser's `RTCIceServer`.
#[derive(Clone)]
pub struct IceServer {
    /// `stun:` / `turn:` / `turns:` URLs.
    pub urls: Vec<String>,
    /// TURN username.
    pub username: Option<String>,
    /// TURN credential (a password). Never printed by `Debug`.
    pub credential: Option<Secret<String>>,
}

impl fmt::Debug for IceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls)
            .field("username", &self.username)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// `{"iceServers":[...]}` as a browser's `RTCConfiguration` takes it.
pub fn ice_servers_json(servers: &[IceServer]) -> Vec<u8> {
    let list: Vec<Value> = servers
        .iter()
        .map(|server| {
            let mut entry = json!({ "urls": server.urls });
            if let (Some(username), Some(credential), Some(map)) =
                (&server.username, &server.credential, entry.as_object_mut())
            {
                map.insert("username".to_owned(), json!(username));
                map.insert("credential".to_owned(), json!(credential.expose()));
            }
            entry
        })
        .collect();
    serde_json::to_vec(&json!({ "iceServers": list }))
        .unwrap_or_else(|_| b"{\"iceServers\":[]}".to_vec())
}

/// WHIP `Link` header values, one per URL (RFC 9725 section 4.6):
/// `<turns:host:443?transport=tcp>; rel="ice-server"; username="..."; credential="..."`.
///
/// `credential-type` is omitted: RFC 9725 does not define it (a password is the only kind).
pub fn link_headers(servers: &[IceServer]) -> Vec<String> {
    let mut links = Vec::new();
    for server in servers {
        for url in &server.urls {
            let mut value = format!("<{url}>; rel=\"ice-server\"");
            if let (Some(username), Some(credential)) = (&server.username, &server.credential) {
                value.push_str(&format!(
                    "; username=\"{username}\"; credential=\"{}\"",
                    credential.expose()
                ));
            }
            links.push(value);
        }
    }
    links
}

/// The label in a coturn username for a request that carried a session token: the first 16 hex
/// digits of SHA-256 of the token's `sid` (so an application can compute the same label for the
/// session ids it issued, and a `sid` with quotes or commas cannot reach a header).
pub fn session_label(sid: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, sid.as_bytes());
    let mut label = String::with_capacity(16);
    for byte in digest.as_ref().iter().take(8) {
        for nibble in [byte >> 4, byte & 0x0f] {
            label.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
    label
}

/// coturn's TURN REST API password: `base64(HMAC-SHA1(secret, username))`.
pub fn coturn_password(secret: &str, username: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hmac::sign(&key, username.as_bytes()).as_ref())
}

/// Reads the ICE servers out of a Cloudflare (`iceServers`) or Twilio (`ice_servers`) answer.
/// Entries and URLs that fail validation are dropped; a TURN entry without credentials is
/// dropped whole.
pub fn parse_ice_servers(answer: &Value) -> Vec<IceServer> {
    let root = answer
        .get("iceServers")
        .or_else(|| answer.get("ice_servers"));
    let entries: Vec<&Value> = match root {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(object @ Value::Object(_)) => vec![object],
        _ => return Vec::new(),
    };
    let mut servers = Vec::new();
    for entry in entries {
        if servers.len() >= MAX_SERVERS {
            break;
        }
        let raw_urls = entry.get("urls").or_else(|| entry.get("url"));
        let candidates: Vec<&str> = match raw_urls {
            Some(Value::String(url)) => vec![url.as_str()],
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            _ => continue,
        };
        let urls: Vec<String> = candidates
            .into_iter()
            .filter(|url| url_ok(url))
            .take(MAX_URLS)
            .map(str::to_owned)
            .collect();
        if urls.is_empty() {
            continue;
        }
        let username = entry
            .get("username")
            .and_then(Value::as_str)
            .filter(|u| text_ok(u))
            .map(str::to_owned);
        let credential = entry
            .get("credential")
            .and_then(Value::as_str)
            .filter(|c| text_ok(c))
            .map(|c| Secret::new(c.to_owned()));
        let relays = urls.iter().any(|u| u.starts_with("turn"));
        if relays && (username.is_none() || credential.is_none()) {
            continue;
        }
        servers.push(IceServer {
            urls,
            username,
            credential,
        });
    }
    servers
}

struct Cached {
    servers: Vec<IceServer>,
    /// Until when the next callers get these without a fetch (or, after a failure, without a
    /// retry).
    until: Instant,
    /// When the credentials stop working (`ttl` after the fetch began).
    expires: Instant,
}

impl Cached {
    /// The servers, unless they are about to expire (less than `floor` left): a browser needs
    /// them for the length of one ICE gathering, not for the rest of the call.
    fn usable(&self, now: Instant, floor: Duration) -> Vec<IceServer> {
        if self.expires.saturating_duration_since(now) < floor {
            Vec::new()
        } else {
            self.servers.clone()
        }
    }
}

/// A per-client token bucket for `GET <whip_path>/ice`. Bounded (see [`ICE_MAX_CLIENTS`]); IPv6
/// clients are keyed by their /64.
#[derive(Default)]
struct IceLimiter {
    buckets: std::sync::Mutex<HashMap<IpAddr, Bucket>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: u32,
    refilled: Instant,
}

impl Bucket {
    /// Adds the tokens earned up to `now`.
    fn refill(&mut self, now: Instant) {
        let earned = now.saturating_duration_since(self.refilled).as_millis()
            / ICE_REFILL.as_millis().max(1);
        let earned = u32::try_from(earned).unwrap_or(u32::MAX);
        if earned > 0 {
            self.tokens = self.tokens.saturating_add(earned).min(ICE_BURST);
            self.refilled = if self.tokens >= ICE_BURST {
                now
            } else {
                self.refilled + ICE_REFILL.saturating_mul(earned)
            };
        }
    }
}

impl IceLimiter {
    fn allow(&self, client: IpAddr, now: Instant) -> bool {
        let key = match client.to_canonical() {
            IpAddr::V6(v6) => {
                let [a, b, c, d, ..] = v6.segments();
                IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
            }
            v4 @ IpAddr::V4(_) => v4,
        };
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !buckets.contains_key(&key) && buckets.len() >= ICE_MAX_CLIENTS {
            // Entries that are full again carry no information.
            buckets.retain(|_, bucket| {
                bucket.refill(now);
                bucket.tokens < ICE_BURST
            });
            if buckets.len() >= ICE_MAX_CLIENTS {
                return false;
            }
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: ICE_BURST,
            refilled: now,
        });
        bucket.refill(now);
        if bucket.tokens == 0 {
            return false;
        }
        bucket.tokens = bucket.tokens.saturating_sub(1);
        true
    }
}

/// The credential issuer of one server.
pub(crate) struct TurnService {
    config: TurnConfig,
    clock: SharedClock,
    client: Option<reqwest::Client>,
    /// The latest fetched credentials; the lock is held during a fetch, so concurrent callers
    /// wait for the one fetch instead of starting their own.
    cache: Mutex<Option<Cached>>,
    limiter: IceLimiter,
}

impl TurnService {
    /// Builds the service (and its HTTP client for the fetching providers).
    pub(crate) fn new(config: TurnConfig, clock: SharedClock) -> Result<Self> {
        config.validate()?;
        let client = match config.provider {
            TurnProvider::Coturn { .. } => None,
            _ => Some(build_client(config.fetch_timeout)?),
        };
        Ok(Self {
            config,
            clock,
            client,
            cache: Mutex::new(None),
            limiter: IceLimiter::default(),
        })
    }

    /// Whether `client` may ask for credentials on `GET <whip_path>/ice` now (a token bucket:
    /// [`ICE_BURST`] requests, then one per [`ICE_REFILL`]).
    pub(crate) fn allow_ice_request(&self, client: IpAddr) -> bool {
        self.limiter.allow(client, self.clock.now())
    }

    /// The ICE servers for session `sid` (empty on a provider failure: the call goes on without a
    /// relay). Never fails, never waits longer than the fetch deadline plus one lock hand-over.
    pub(crate) async fn servers(&self, sid: &str) -> Vec<IceServer> {
        self.servers_within(sid, None).await
    }

    /// Like [`servers`](Self::servers), but waits at most `budget` (lock wait and fetch together)
    /// and then answers from the cache while its credentials last, else with none.
    pub(crate) async fn servers_bounded(&self, sid: &str, budget: Duration) -> Vec<IceServer> {
        self.servers_within(sid, Some(budget)).await
    }

    async fn servers_within(&self, sid: &str, budget: Option<Duration>) -> Vec<IceServer> {
        match &self.config.provider {
            TurnProvider::Coturn { urls, secret } => {
                let wall = self.clock.wall();
                let now = wall
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let expiry = now.saturating_add(self.config.ttl.as_secs());
                let username = format!("{expiry}:{sid}");
                let password = coturn_password(secret.expose(), &username);
                vec![IceServer {
                    urls: urls.clone(),
                    username: Some(username),
                    credential: Some(Secret::new(password)),
                }]
            }
            _ => self.fetched(budget).await,
        }
    }

    /// What the cache holds if nobody is fetching right now (the lock is not waited for).
    fn stale(&self) -> Vec<IceServer> {
        let now = self.clock.now();
        let floor = self.config.ttl / 4;
        self.cache
            .try_lock()
            .ok()
            .and_then(|cache| cache.as_ref().map(|cached| cached.usable(now, floor)))
            .unwrap_or_default()
    }

    async fn fetched(&self, budget: Option<Duration>) -> Vec<IceServer> {
        let begun = self.clock.now();
        let floor = self.config.ttl / 4;
        let mut cache = match budget {
            None => self.cache.lock().await,
            Some(budget) => {
                tokio::select! {
                    biased;
                    guard = self.cache.lock() => guard,
                    () = self.clock.sleep(budget) => return self.stale(),
                }
            }
        };
        let now = self.clock.now();
        if let Some(cached) = cache.as_ref()
            && now < cached.until
        {
            return cached.usable(now, floor);
        }
        let limit = budget.map_or(self.config.fetch_timeout, |budget| {
            budget
                .saturating_sub(now.saturating_duration_since(begun))
                .min(self.config.fetch_timeout)
        });
        if limit.is_zero() {
            return cache
                .as_ref()
                .map(|cached| cached.usable(now, floor))
                .unwrap_or_default();
        }
        let outcome = tokio::select! {
            biased;
            fetched = self.fetch() => fetched,
            () = self.clock.sleep(limit) => Err("timed out".to_owned()),
        };
        let after = self.clock.now();
        match outcome {
            Ok(servers) => {
                *cache = Some(Cached {
                    servers: servers.clone(),
                    until: after + self.config.cache,
                    expires: now + self.config.ttl,
                });
                servers
            }
            Err(reason) => {
                tracing::warn!(%reason, "webrtc turn credentials unavailable; calls go without a relay");
                // Keep what is left of the old credentials for the callers of the backoff window.
                let (servers, expires) = cache
                    .take()
                    .map_or((Vec::new(), after), |old| (old.servers, old.expires));
                let kept = Cached {
                    servers,
                    until: after + FAILURE_BACKOFF,
                    expires,
                };
                let served = kept.usable(after, floor);
                *cache = Some(kept);
                served
            }
        }
    }

    /// One provider request. Errors are texts without URLs or credentials.
    async fn fetch(&self) -> std::result::Result<Vec<IceServer>, String> {
        let client = self.client.as_ref().ok_or("no http client")?;
        let ttl = self.config.ttl.as_secs();
        let builder = match &self.config.provider {
            TurnProvider::Coturn { .. } => return Err("coturn needs no fetch".to_owned()),
            TurnProvider::Cloudflare {
                key_id,
                api_token,
                base_url,
            } => {
                let url = format!(
                    "{}/v1/turn/keys/{key_id}/credentials/generate-ice-servers",
                    base_url.trim_end_matches('/')
                );
                client
                    .post(url)
                    .bearer_auth(api_token.expose())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(json!({ "ttl": ttl }).to_string())
            }
            TurnProvider::Twilio {
                account_sid,
                auth_user,
                auth_secret,
                base_url,
            } => {
                let url = format!(
                    "{}/2010-04-01/Accounts/{account_sid}/Tokens.json",
                    base_url.trim_end_matches('/')
                );
                client
                    .post(url)
                    .basic_auth(auth_user, Some(auth_secret.expose()))
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(format!("Ttl={ttl}"))
            }
        };
        let mut response = builder
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the provider answered {status}"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("reading the answer failed: {}", e.without_url()))?
        {
            let room = MAX_BODY.saturating_sub(body.len());
            body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
            if body.len() >= MAX_BODY {
                break;
            }
        }
        let answer: Value =
            serde_json::from_slice(&body).map_err(|_| "the answer is not JSON".to_owned())?;
        let servers = parse_ice_servers(&answer);
        if servers.is_empty() {
            return Err("the answer holds no usable ICE server".to_owned());
        }
        Ok(servers)
    }
}

/// The HTTP client: rustls with the `ring` provider and the compiled-in webpki roots, no
/// proxy, no redirects (credentials never follow one), a connect deadline.
fn build_client(connect_timeout: Duration) -> Result<reqwest::Client> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Config(format!("webrtc turn: TLS setup failed: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("brisvox/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|e| Error::Config(format!("webrtc turn: cannot build the HTTP client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_coturn_password_matches_a_reference_vector() {
        // python3: base64(hmac.new(b"north", b"1700000600:abc", sha1).digest())
        assert_eq!(
            coturn_password("north", "1700000600:abc"),
            "cn17rOcIFYzHsN4D2xqDw8sMqrc="
        );
    }

    fn server() -> IceServer {
        IceServer {
            urls: vec![
                "turns:turn.example.com:443?transport=tcp".to_owned(),
                "turn:turn.example.com:3478?transport=udp".to_owned(),
            ],
            username: Some("1700000600:abc".to_owned()),
            credential: Some(Secret::new("pw+/=".to_owned())),
        }
    }

    #[test]
    fn link_headers_follow_rfc_9725_one_per_url() {
        let links = link_headers(&[server()]);
        assert_eq!(links.len(), 2);
        assert_eq!(
            links[0],
            "<turns:turn.example.com:443?transport=tcp>; rel=\"ice-server\"; \
             username=\"1700000600:abc\"; credential=\"pw+/=\""
        );
        let stun = IceServer {
            urls: vec!["stun:stun.example.com:3478".to_owned()],
            username: None,
            credential: None,
        };
        assert_eq!(
            link_headers(&[stun]),
            vec!["<stun:stun.example.com:3478>; rel=\"ice-server\"".to_owned()]
        );
    }

    #[test]
    fn json_has_the_rtc_ice_server_shape_and_debug_hides_the_credential() {
        let body = ice_servers_json(&[server()]);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["iceServers"][0]["username"], "1700000600:abc");
        assert_eq!(value["iceServers"][0]["credential"], "pw+/=");
        assert_eq!(
            value["iceServers"][0]["urls"][1],
            "turn:turn.example.com:3478?transport=udp"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&ice_servers_json(&[])).unwrap(),
            json!({ "iceServers": [] })
        );
        assert!(!format!("{:?}", server()).contains("pw+/="));
    }

    #[test]
    fn a_cloudflare_answer_is_parsed_and_port_53_is_dropped() {
        let answer = json!({ "iceServers": [
            { "urls": ["stun:stun.cloudflare.com:3478", "stun:stun.cloudflare.com:53"] },
            { "urls": ["turn:turn.cloudflare.com:3478?transport=udp",
                       "turn:turn.cloudflare.com:53?transport=udp",
                       "turns:turn.cloudflare.com:443?transport=tcp"],
              "username": "u1", "credential": "c1" }
        ]});
        let servers = parse_ice_servers(&answer);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls, vec!["stun:stun.cloudflare.com:3478"]);
        assert_eq!(servers[1].urls.len(), 2);
        assert_eq!(servers[1].username.as_deref(), Some("u1"));
    }

    #[test]
    fn a_twilio_answer_with_string_urls_is_parsed() {
        let answer = json!({ "username": "x", "password": "y", "ttl": "600", "ice_servers": [
            { "url": "stun:global.stun.twilio.com:3478", "urls": "stun:global.stun.twilio.com:3478" },
            { "url": "turn:global.turn.twilio.com:443?transport=tcp",
              "urls": "turn:global.turn.twilio.com:443?transport=tcp",
              "username": "tu", "credential": "tc" }
        ]});
        let servers = parse_ice_servers(&answer);
        assert_eq!(servers.len(), 2);
        assert_eq!(
            servers[1].credential.as_ref().map(|c| c.expose().as_str()),
            Some("tc")
        );
    }

    #[test]
    fn hostile_answers_cannot_inject_header_text_or_unsafe_servers() {
        let answer = json!({ "iceServers": [
            { "urls": ["turn:a.example.com:3478"], "username": "u\"; evil=\"", "credential": "c" },
            { "urls": ["turn:a.example.com:3478"], "username": "u" },
            { "urls": ["http://evil.example.com/>; rel=x"], "username": "u", "credential": "c" },
            { "urls": ["turn:b.example.com:3478>"], "username": "u", "credential": "c" },
            { "urls": ["turn:ok.example.com:3478"], "username": "u", "credential": "c" },
        ]});
        let servers = parse_ice_servers(&answer);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, vec!["turn:ok.example.com:3478"]);
        assert!(parse_ice_servers(&json!({})).is_empty());
        assert!(parse_ice_servers(&json!({ "iceServers": 5 })).is_empty());
        let many = json!({ "iceServers": (0..50).map(|n| json!({ "urls": [format!("stun:h{n}.example.com:3478")] })).collect::<Vec<_>>() });
        assert_eq!(parse_ice_servers(&many).len(), MAX_SERVERS);
    }

    #[test]
    fn invalid_configurations_are_refused() {
        let coturn = |urls: Vec<&str>, secret: &str| {
            TurnConfig::new(TurnProvider::Coturn {
                urls: urls.into_iter().map(str::to_owned).collect(),
                secret: Secret::new(secret.to_owned()),
            })
        };
        assert!(
            coturn(vec!["turn:t.example.com:3478"], "s")
                .validate()
                .is_ok()
        );
        assert!(coturn(vec![], "s").validate().is_err());
        assert!(coturn(vec!["stun:t.example.com"], "s").validate().is_err());
        assert!(
            coturn(vec!["turn:t.example.com:3478"], "")
                .validate()
                .is_err()
        );
        assert!(coturn(vec!["turn:a b"], "s").validate().is_err());
        let mut config = coturn(vec!["turn:t.example.com:3478"], "s");
        config.ttl = Duration::from_secs(5);
        assert!(config.validate().is_err());
        config.ttl = Duration::from_secs(600);
        config.cache = Duration::from_secs(400);
        assert!(config.validate().is_err());
        let cloudflare = |base: &str| {
            TurnConfig::new(TurnProvider::Cloudflare {
                key_id: "kid".to_owned(),
                api_token: Secret::new("tok".to_owned()),
                base_url: base.to_owned(),
            })
        };
        assert!(cloudflare(CLOUDFLARE_BASE).validate().is_ok());
        assert!(cloudflare("http://127.0.0.1:1").validate().is_ok());
        assert!(cloudflare("http://turn.example.com").validate().is_err());
        assert!(cloudflare("ftp://x").validate().is_err());
    }

    #[test]
    fn debug_output_hides_provider_secrets() {
        let config = TurnConfig::new(TurnProvider::Twilio {
            account_sid: "AC1".to_owned(),
            auth_user: "AC1".to_owned(),
            auth_secret: Secret::new("very-secret".to_owned()),
            base_url: TWILIO_BASE.to_owned(),
        });
        assert!(!format!("{config:?}").contains("very-secret"));
    }

    // -----------------------------------------------------------------------------------------
    // The credential cache on a clock the test steps (the HTTP mock is real, so time does not
    // run on its own: `advance` is the only thing that ages credentials).

    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use brisvox_core::{Clock, SystemClock};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Real timers, plus an offset the test adds to move "now" (and the wall clock) forward.
    #[derive(Debug)]
    struct StepClock {
        real: SharedClock,
        offset_ms: AtomicU64,
    }

    impl StepClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                real: SystemClock::shared(),
                offset_ms: AtomicU64::new(0),
            })
        }

        fn advance(&self, by: Duration) {
            self.offset_ms
                .fetch_add(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
        }

        fn offset(&self) -> Duration {
            Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
        }
    }

    impl Clock for StepClock {
        fn now(&self) -> Instant {
            self.real.now() + self.offset()
        }

        fn wall(&self) -> std::time::SystemTime {
            self.real.wall() + self.offset()
        }

        fn sleep_until(&self, deadline: Instant) -> tokio::time::Sleep {
            let real = deadline
                .checked_sub(self.offset())
                .unwrap_or_else(|| self.real.now());
            self.real.sleep_until(real)
        }
    }

    #[derive(Clone, Copy)]
    enum Mode {
        Ok,
        Fail,
        Hang,
    }

    /// A Cloudflare look-alike: `cred-<n>` for the n-th request, remembering when (on the stepped
    /// clock) each credential was issued.
    struct Mock {
        addr: std::net::SocketAddr,
        hits: Arc<AtomicUsize>,
        mode: Arc<StdMutex<Mode>>,
        issued: Arc<StdMutex<Vec<Duration>>>,
    }

    impl Mock {
        async fn start(clock: Arc<StepClock>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mock = Self {
                addr: listener.local_addr().unwrap(),
                hits: Arc::default(),
                mode: Arc::new(StdMutex::new(Mode::Ok)),
                issued: Arc::default(),
            };
            let (hits, mode, issued) = (
                Arc::clone(&mock.hits),
                Arc::clone(&mock.mode),
                Arc::clone(&mock.issued),
            );
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (hits, mode, issued, clock) = (
                        Arc::clone(&hits),
                        Arc::clone(&mode),
                        Arc::clone(&issued),
                        Arc::clone(&clock),
                    );
                    tokio::spawn(async move {
                        let mut raw = Vec::new();
                        let mut buf = [0u8; 2048];
                        // Head, then the (short) body the request announces.
                        let needed = loop {
                            let n = stream.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            raw.extend_from_slice(&buf[..n]);
                            let text = String::from_utf8_lossy(&raw).into_owned();
                            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                                let length = head
                                    .to_ascii_lowercase()
                                    .split("content-length: ")
                                    .nth(1)
                                    .and_then(|rest| rest.split("\r\n").next()?.parse().ok())
                                    .unwrap_or(0usize);
                                break length.saturating_sub(body.len());
                            }
                        };
                        let mut left = needed;
                        while left > 0 {
                            let n = stream.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            left = left.saturating_sub(n);
                        }
                        let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
                        issued.lock().unwrap().push(clock.offset());
                        let mode = *mode.lock().unwrap();
                        let (status, body) = match mode {
                            Mode::Ok => (
                                201,
                                format!(
                                    r#"{{"iceServers":[{{"urls":["turn:turn.example.com:3478"],"username":"u{n}","credential":"cred-{n}"}}]}}"#
                                ),
                            ),
                            Mode::Fail => (500, "{}".to_owned()),
                            Mode::Hang => std::future::pending().await,
                        };
                        let response = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
            });
            mock
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        fn set(&self, mode: Mode) {
            *self.mode.lock().unwrap() = mode;
        }
    }

    fn real_now() -> Instant {
        SystemClock::shared().now()
    }

    const TTL: Duration = Duration::from_secs(600);

    async fn service(fetch_timeout: Duration) -> (TurnService, Arc<StepClock>, Mock) {
        let clock = StepClock::new();
        let mock = Mock::start(Arc::clone(&clock)).await;
        let mut config = TurnConfig::new(TurnProvider::Cloudflare {
            key_id: "key1".to_owned(),
            api_token: Secret::new("tok".to_owned()),
            base_url: format!("http://{}", mock.addr),
        });
        config.ttl = TTL;
        config.cache = TTL / 2;
        config.fetch_timeout = fetch_timeout;
        let shared: SharedClock = Arc::clone(&clock) as SharedClock;
        (TurnService::new(config, shared).unwrap(), clock, mock)
    }

    fn credential(servers: &[IceServer]) -> Option<usize> {
        servers
            .iter()
            .find_map(|s| s.credential.as_ref())
            .and_then(|c| c.expose().strip_prefix("cred-")?.parse().ok())
    }

    #[tokio::test]
    async fn credentials_are_refetched_after_the_cache_window_and_always_have_half_their_life_left()
    {
        let (service, clock, mock) = service(Duration::from_secs(5)).await;
        let first = service.servers("s").await;
        assert_eq!(credential(&first), Some(1));
        assert_eq!(mock.hits(), 1);

        // Just inside the cache window: no new fetch.
        clock.advance(Duration::from_secs(299));
        assert_eq!(credential(&service.servers("s").await), Some(1));
        assert_eq!(mock.hits(), 1);
        // At the window's end: one new fetch, shared by the next callers.
        clock.advance(Duration::from_secs(1));
        assert_eq!(credential(&service.servers("s").await), Some(2));
        assert_eq!(credential(&service.servers("s").await), Some(2));
        assert_eq!(mock.hits(), 2);

        // Over many windows, whatever is served has at least ttl/2 of its life left.
        for _ in 0..80 {
            clock.advance(Duration::from_secs(31));
            let served = service.servers("s").await;
            let n = credential(&served).expect("a credential");
            let issued = mock.issued.lock().unwrap()[n - 1];
            let left = TTL.saturating_sub(clock.offset() - issued);
            assert!(left >= TTL / 2, "credential {n} has only {left:?} left");
        }
        // 80 * 31 s = 2480 s of steps, one fetch per 300 s window at most.
        assert!(mock.hits() <= 2 + 2480 / 300 + 1, "{} fetches", mock.hits());
    }

    #[tokio::test]
    async fn a_failed_fetch_is_not_retried_until_the_backoff_has_passed() {
        let (service, clock, mock) = service(Duration::from_secs(5)).await;
        mock.set(Mode::Fail);
        assert!(service.servers("s").await.is_empty());
        assert_eq!(mock.hits(), 1);
        clock.advance(FAILURE_BACKOFF - Duration::from_secs(1));
        assert!(service.servers("s").await.is_empty());
        assert_eq!(mock.hits(), 1, "still backing off");
        mock.set(Mode::Ok);
        clock.advance(Duration::from_secs(1));
        assert_eq!(credential(&service.servers("s").await), Some(2));
        assert_eq!(mock.hits(), 2);
    }

    #[tokio::test]
    async fn old_credentials_survive_a_failed_refresh_only_while_a_quarter_of_their_life_is_left() {
        let (service, clock, mock) = service(Duration::from_secs(5)).await;
        assert_eq!(credential(&service.servers("s").await), Some(1));
        mock.set(Mode::Fail);
        // 300 s old (300 s left, above ttl/4 = 150 s): the failed refresh serves the old ones.
        clock.advance(Duration::from_secs(300));
        assert_eq!(credential(&service.servers("s").await), Some(1));
        assert_eq!(mock.hits(), 2);
        // 460 s old (140 s left): too little to hand to a browser.
        clock.advance(FAILURE_BACKOFF);
        clock.advance(Duration::from_secs(150));
        assert!(service.servers("s").await.is_empty());
        assert_eq!(mock.hits(), 3);
    }

    #[tokio::test]
    async fn a_bounded_request_gives_up_at_its_budget_and_a_hung_provider_costs_one_wait_per_backoff()
     {
        // The provider hangs; the full fetch deadline is 5 s but the offer path waits 300 ms.
        let (service, clock, mock) = service(Duration::from_secs(5)).await;
        mock.set(Mode::Hang);
        let started = real_now();
        let servers = tokio::time::timeout(
            Duration::from_secs(3),
            service.servers_bounded("s", Duration::from_millis(300)),
        )
        .await
        .expect("the bounded request must not wait for the fetch deadline");
        assert!(servers.is_empty());
        assert!(real_now().saturating_duration_since(started) < Duration::from_secs(2));
        assert_eq!(mock.hits(), 1);
        // The timeout is a failure like any other: the next callers do not wait again.
        let started = real_now();
        assert!(
            service
                .servers_bounded("s", Duration::from_millis(300))
                .await
                .is_empty()
        );
        assert!(real_now().saturating_duration_since(started) < Duration::from_millis(100));
        assert_eq!(mock.hits(), 1);
        clock.advance(FAILURE_BACKOFF);
        mock.set(Mode::Ok);
        assert_eq!(
            credential(&service.servers_bounded("s", Duration::from_secs(1)).await),
            Some(2)
        );
    }

    #[tokio::test]
    async fn a_bounded_request_does_not_wait_behind_a_slow_unbounded_fetch() {
        let (service, _clock, mock) = service(Duration::from_secs(5)).await;
        let service = Arc::new(service);
        mock.set(Mode::Hang);
        // The ICE endpoint's request holds the cache lock for its whole fetch.
        let slow = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.servers("a").await })
        };
        while mock.hits() == 0 {
            SystemClock::shared().sleep(Duration::from_millis(5)).await;
        }
        let started = real_now();
        let servers = tokio::time::timeout(
            Duration::from_secs(3),
            service.servers_bounded("b", Duration::from_millis(300)),
        )
        .await
        .expect("the offer path must not queue behind the fetch");
        assert!(servers.is_empty());
        assert!(real_now().saturating_duration_since(started) < Duration::from_secs(2));
        slow.abort();
    }

    #[test]
    fn the_ice_limiter_allows_a_burst_then_one_per_refill_and_shares_an_ipv6_64() {
        let clock = StepClock::new();
        let limiter = IceLimiter::default();
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let other: IpAddr = "192.0.2.2".parse().unwrap();
        let t0 = clock.now();
        for _ in 0..ICE_BURST {
            assert!(limiter.allow(a, t0));
        }
        assert!(!limiter.allow(a, t0));
        // Others are unaffected.
        assert!(limiter.allow(other, t0));
        // One token per refill period.
        assert!(!limiter.allow(a, t0 + ICE_REFILL - Duration::from_millis(1)));
        assert!(limiter.allow(a, t0 + ICE_REFILL));
        assert!(!limiter.allow(a, t0 + ICE_REFILL));
        // An IPv6 client is limited as a /64: another host of the same prefix shares the bucket.
        let v6a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let v6b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        for _ in 0..ICE_BURST {
            assert!(limiter.allow(v6a, t0));
        }
        assert!(!limiter.allow(v6b, t0));
        // An IPv4-mapped address is the IPv4 address.
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        assert!(!limiter.allow(mapped, t0 + ICE_REFILL));
    }

    #[test]
    fn the_ice_limiter_stays_bounded_and_forgets_idle_clients() {
        let clock = StepClock::new();
        let limiter = IceLimiter::default();
        let t0 = clock.now();
        let mut admitted = 0usize;
        for n in 0..(ICE_MAX_CLIENTS + 100) {
            let ip = IpAddr::from([10, (n >> 16) as u8, (n >> 8) as u8, n as u8]);
            // Every client spends a token, so none is "idle" yet.
            if limiter.allow(ip, t0) {
                admitted += 1;
            }
        }
        assert_eq!(admitted, ICE_MAX_CLIENTS, "fail closed at the cap");
        // After the burst has refilled, the idle entries are reclaimed and new clients fit again.
        let later = t0 + ICE_REFILL * (ICE_BURST + 1);
        assert!(limiter.allow(IpAddr::from([172, 16, 0, 1]), later));
        assert!(limiter.buckets.lock().unwrap().len() <= ICE_MAX_CLIENTS);
    }

    #[test]
    fn credential_text_with_a_comma_is_refused_so_link_headers_stay_splittable() {
        assert!(!text_ok("a,b"));
        assert!(text_ok("a-b_c=+/"));
        let answer = json!({ "iceServers": [
            { "urls": ["turn:a.example.com:3478"], "username": "u,x", "credential": "c" },
            { "urls": ["turn:b.example.com:3478"], "username": "u", "credential": "c,d" },
            { "urls": ["turn:c.example.com:3478?a=1,2"], "username": "u", "credential": "c" },
        ]});
        assert!(parse_ice_servers(&answer).is_empty());
    }
}
