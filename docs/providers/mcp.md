# MCP (Model Context Protocol) — client side tool use for voice agents

Researched: 2026-10-23. Every fact below was read from the pages listed per section on that date. Anything not
confirmed on a fetched page is marked UNVERIFIED; statements labelled "Brisvox design" or "Inferred" are engineering
conclusions, not documentation claims. Scope: brisvox acts as an MCP CLIENT (host side) that lists tools of external MCP
servers and exposes them to the LLM; it does not implement an MCP server.

- Docs root: https://modelcontextprotocol.io/specification/latest (fetched 2026-10-23)
- Auth: none at protocol level for stdio (credentials come from the environment); OAuth 2.1 profile for HTTP servers (see 5)
- Endpoint(s): server-defined; Streamable HTTP uses ONE endpoint URL (e.g. `https://example.com/mcp`)
- Protocol: JSON-RPC 2.0 (see 2-4)
- Keepalive / timeouts: see 4.6
- Finalize / flush / cancel: see 4.5
- Timestamps / interim results / tool-call deltas: tool results are one-shot (`tools/call` response); progress via `notifications/progress` (4.7)
- Concurrency / rate limits: not specified by the protocol; servers "MUST ... Rate limit tool invocations" (tools page)
- Turkish support: not applicable (protocol is language-neutral JSON; tool text/description language is server-defined)
- Gotchas found during implementation (rmcp 3.7.0, crates.io 2026-10-22, Apache-2.0, MSRV 1.88; checked against the
  crate source and live runs on 2026-10-23, section 7.1 lists what was still UNVERIFIED before):
  - Pick and features: `rmcp = { version = "3.7.0", default-features = false, features = ["client",
    "transport-child-process", "which-command", "transport-streamable-http-client-reqwest"] }`; it compiles without
    `schemars`/`server`. The server side (`server`, `transport-io`, `transport-streamable-http-server`) is a
    dev-dependency of the tests' MCP servers only. The crate's `reqwest` feature is `reqwest/rustls` (aws-lc, needs cmake
    and nasm); not used: the HTTP transport is built with `StreamableHttpClientTransport::with_client(client, config)` over
    the provider adapters' shared `reqwest::Client` (ring, webpki roots, no redirects), so TLS is one stack.
  - Bearer: `StreamableHttpClientTransportConfig::with_uri(url).auth_header(token)`; the value is the bare token (the
    transport adds `Bearer`). Sent on every POST (checked by a test server that rejects anything else).
  - Dual era works as written: `ClientLifecycleMode::Auto { preferred_versions: vec![ProtocolVersion::LATEST],
    legacy_version: Some(ProtocolVersion::V_2025_11_25) }` against rmcp's own server (newest revision, stdio and HTTP), a
    hand-written session-era server over stdio (its `server/discover` answer is "method not found") and over HTTP, and
    against DeepWiki's public server (`https://mcp.deepwiki.com/mcp`, protocol 2025-06-18,
    answers SSE, tools `ask_wiki_question`, `read_wiki_contents`, `read_wiki_structure`; results carry `structuredContent`
    `{"result": "..."}` through an `outputSchema`). A legacy server that never answers `server/discover` costs the 10 s probe
    timeout before the `initialize` fallback, so the per-server startup timeout (default 30 s) bounds it.
  - rmcp's own server cannot play a session-era server: it always answers `server/discover`, and a server limited to
    legacy versions makes the client fail with "no compatible protocol version" instead of falling back. The tests use
    hand-written session-era servers (stdio and HTTP with `Mcp-Session-Id`); they showed that a restarted session
    server breaks every later call unless `StreamableHttpClientTransportConfig::reinit_on_expired_session(true)` is set
    (a new session after the 404, the request retried once: the 404 means it was not processed). It is set.
  - Cancellation: `Peer::call_tool_once` is cancel-on-drop (an armed guard sends `notifications/cancelled` when the future
    is dropped); the server side sees it as `on_cancelled` and as the request context's token. `RunningService::call_tool`
    (not on `Peer`) is the one that drives `input_required` rounds; use `call_tool_once` and refuse them.
  - Shutdown: `RunningService::waiting(self)` consumes the service, so take `cancellation_token()` first; cancelling closes
    the transport, and the child transport closes stdin, waits 3 s and kills (`MAX_WAIT_ON_DROP_SECS`). A child that
    exits on stdin EOF is not killed (tested). A dropped transport kills the child from a spawned task (needs a runtime).
  - `TokioChildProcess::builder(cmd)` inherits the child's stderr by default; `.stderr(Stdio::piped())` and read it
    (here: logged at debug). `which_command("npx")` resolves `npx.cmd` on Windows (the plain `Command::new` does not).
  - The client's extra `_meta` keys on `CallToolRequestParams.meta` reach the server (the idempotency key arrives as
    `io.brisvox/idempotency_key`; rmcp servers hand it over in `RequestContext::meta` or the params' `meta`).
  - `list_all_tools()` follows `nextCursor` without a page cap: a hostile server can page forever, bounded here only by the
    startup timeout. The SDK's types are `#[non_exhaustive]` (`ContentBlock`, `ServiceError`, `CallToolResponse`): match
    with a wildcard arm.
  - A restarted stdio server is a new process with no state: tools keep working because MCP 2026-07-28 is stateless; a
    legacy-era server that kept state in the old session loses it.

## 1. Spec versions (date-stamped) and what changed

Sources: https://modelcontextprotocol.io/specification/versioning , https://modelcontextprotocol.io/specification/2026-07-28/changelog (both fetched 2026-10-23).

| Fact | Value |
|---|---|
| Version format | `YYYY-MM-DD`, "the last date backwards incompatible changes were made" |
| CURRENT revision | `2026-07-28` (versioning page: "The **current** protocol version is 2026-07-28") |
| Previous revision | `2025-11-25` (Final). Older still documented: `2025-06-18`, `2025-03-26`, `2024-11-05` |
| Where | Prose: `https://modelcontextprotocol.io/specification/<version>/...`; source of truth schema: `https://github.com/modelcontextprotocol/specification/blob/main/schema/2026-07-28/schema.ts` (+ `schema.json`) |
| Index of all pages | https://modelcontextprotocol.io/llms.txt |

### 1.1 2026-07-28 vs 2025-11-25 — changes that matter for a client (changelog page)

| # | Change | Client impact |
|---|---|---|
| 1 | Protocol-level sessions and `Mcp-Session-Id` header REMOVED from Streamable HTTP. `tools/list` no longer varies per connection | No session id to store or resend |
| 2 | MCP is STATELESS: `initialize` / `notifications/initialized` handshake REMOVED. Every request carries `_meta` keys `io.modelcontextprotocol/protocolVersion` (required), `.../clientCapabilities` (required), `.../clientInfo` (SHOULD). Mismatch gives `UnsupportedProtocolVersionError` (code -32022) | Build `_meta` on every request |
| 3 | New mandatory server RPC `server/discover` (supportedVersions, capabilities, instructions, `_meta` serverInfo). Clients MAY call it first; it is also the stdio backward-compat probe | Optional call |
| 4 | HTTP GET stream and `resources/subscribe` replaced by `subscriptions/listen` (long-lived POST-response stream; opt-in flags `toolsListChanged`, `promptsListChanged`, `resourcesListChanged`, `resourceSubscriptions`) | list_changed only arrives if you open this stream |
| 5 | `ping`, `logging/setLevel`, `notifications/roots/list_changed` REMOVED. Log level per request via `_meta` `io.modelcontextprotocol/logLevel` | No ping in the new revision |
| 6 | Tasks moved to an official extension `io.modelcontextprotocol/tasks` | Ignore unless negotiated |
| 7 | Multi Round-Trip Requests (MRTR): server-initiated sampling/elicitation/roots become `InputRequiredResult` (`resultType: "input_required"`, `inputRequests`, `requestState`); client retries the SAME call with `inputResponses` and a NEW JSON-RPC id | A `tools/call` may return `input_required`; a voice client without elicitation must handle/refuse it |
| 8 | Every result carries required `resultType` (`"complete"` or `"input_required"`); clients MUST treat a missing `resultType` from older servers as `"complete"`; unknown value is invalid (basic page) | Parse `resultType` |
| 9 | SSE resumability (`Last-Event-ID`, event ids) REMOVED; a broken stream loses the in-flight request and the client MUST re-issue it as a NEW request with a new id | Retry only if the tool is idempotent (Brisvox design: tie to SideEffect/idempotency rules) |
| 10 | Streamable HTTP POST MUST carry `MCP-Protocol-Version`, `Mcp-Method`, and `Mcp-Name` (for `tools/call`, `resources/read`, `prompts/get`); tool params with `x-mcp-header` are mirrored to `Mcp-Param-{Name}` headers (client MUST) | Header mirroring required for HTTP |
| 11 | `tools/list` etc. results carry `ttlMs` and `cacheScope` (CacheableResult); servers SHOULD return tools in deterministic order | Cache tool list per `ttlMs` |
| 12 | `inputSchema`/`outputSchema` may use any JSON Schema 2020-12 keywords; `structuredContent` may be ANY JSON value (was object only) | Do not assume object |
| 13 | New error codes -32020 HeaderMismatch, -32021 MissingRequiredClientCapability, -32022 UnsupportedProtocolVersion; -32020..-32099 reserved for MCP spec | |
| 14 | Auth: client MUST validate `iss` (RFC 9207) when present; Client ID Metadata Documents preferred; Dynamic Client Registration (RFC 7591) DEPRECATED but kept | See 5 |
| 15 | Deprecated (still in spec): Roots, Sampling, Logging features; HTTP+SSE transport (2024-11-05); `includeContext` thisServer/allServers | Do not implement sampling/roots/logging |

Note (changelog, verbatim idea): "Clients MUST treat results from earlier-protocol servers that omit the field as complete."

### 1.2 Two protocol "eras" (versioning page, fetched 2026-10-23)

- Modern = `2026-07-28` and later (per-request metadata). Legacy = `2025-11-25` and earlier (`initialize` handshake). Dual-era = supports both.
- Compatibility matrix: Modern client with Legacy server FAILS; Dual-era client works with both. A brisvox client that wants to talk to
  the many existing servers (UNVERIFIED how many deployed servers already speak 2026-07-28) must be dual-era.
- stdio probe: send `server/discover` first with preferred modern version in `_meta`. DiscoverResult => modern. A recognized modern error
  (e.g. UnsupportedProtocolVersion, code -32022, `data: {supported:[...], requested:"..."}`) => modern, retry with a supported version, do NOT fall back.
  Any other error, or no answer within a reasonable timeout => legacy, fall back to `initialize`. The fallback MUST NOT key on one specific error code (legacy servers answer with -32601, -32602, or nothing).
- HTTP probe: send a modern request; on `400 Bad Request` inspect the body: recognized modern JSON-RPC error => modern; empty/other body => fall back to `initialize` (and possibly to deprecated HTTP+SSE).
- Cache the era result per server process (stdio) or origin (HTTP); re-probe if the cached assumption fails.

## 2. Handshake and negotiation

### 2.1 Modern (2026-07-28) — no handshake
Sources: https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning , .../basic/index , .../server/discover (fetched 2026-10-23).

Every request: `params._meta` = `{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name","version"},"io.modelcontextprotocol/clientCapabilities":{}}`.
Missing required field => server returns -32602 (HTTP 400). Missing capability the server needs => -32021 `MissingRequiredClientCapabilityError` with `data.requiredCapabilities`.
`server/discover` request: only `_meta`. Result: `supportedVersions[]`, `capabilities`, `instructions?`, `_meta["io.modelcontextprotocol/serverInfo"]`, `ttlMs`, `cacheScope`. `serverInfo` is self-reported: "SHOULD NOT" be used for behavior or security decisions.
Brisvox client capabilities to declare: `{}` (no elicitation/sampling/roots). Inferred: servers needing those answer -32021, so surface that as a tool-unavailable error.

### 2.2 Legacy (2025-11-25) `initialize`
Source: https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle (fetched 2026-10-23).
- Client sends `initialize` {`protocolVersion` (SHOULD be the client's latest), `capabilities`, `clientInfo{name,title?,version,...}`}. Server replies {`protocolVersion`, `capabilities` (e.g. `tools:{listChanged:true}`), `serverInfo`, `instructions?`}.
- Negotiation: server supports requested version => echo it; else respond with another version it supports (SHOULD be its latest). If the client does not support the server's version it SHOULD disconnect.
- Then client MUST send `notifications/initialized`. Client SHOULD NOT send requests other than pings before the initialize response.
- Over HTTP the client MUST send `MCP-Protocol-Version: <negotiated>` on all later requests.
- Only use capabilities that were negotiated.

## 3. Tools (client view)

Sources: https://modelcontextprotocol.io/specification/2026-07-28/server/tools and https://modelcontextprotocol.io/specification/2025-11-25/server/tools (fetched 2026-10-23); schema.ts 2026-07-28 for annotation defaults (https://raw.githubusercontent.com/modelcontextprotocol/modelcontextprotocol/main/schema/2026-07-28/schema.ts, fetched 2026-10-23).

### 3.1 `tools/list`
- Request `params.cursor?` (opaque). Response `tools[]`, `nextCursor?`; modern also `resultType`, `ttlMs`, `cacheScope` (pagination page: https://modelcontextprotocol.io/specification/2026-07-28/server/utilities/pagination).
- Pagination rules: page size is server-chosen, clients MUST NOT assume a fixed size; cursors are opaque (do not parse/modify); an EMPTY STRING cursor is valid and MUST NOT be treated as end; absent `nextCursor` = end; invalid cursor => -32602.
- Tool fields: `name`, `title?`, `description`, `icons?`, `inputSchema` (JSON Schema, default dialect 2020-12, MUST be an object schema, never null), `outputSchema?`, `annotations?`, and (2025-11-25 only) `execution.taskSupport` (`forbidden` default, `optional`, `required`).
- No-param tools: `{ "type": "object", "additionalProperties": false }` (recommended) or `{ "type": "object" }`.
- Tool names: SHOULD be 1-128 chars, case-sensitive, `[A-Za-z0-9_.-]` only. 2026-07-28 note: clients aggregating several servers "MAY" see collisions and SHOULD prefix names with a server identifier; serverInfo.name is not unique, do not rely on it.
- Dynamic list: `tools` capability `listChanged`. Modern: client must open `subscriptions/listen` with `notifications:{toolsListChanged:true}`; first message is `notifications/subscriptions/acknowledged`; then `notifications/tools/list_changed` (each tagged `_meta["io.modelcontextprotocol/subscriptionId"]` = JSON-RPC id of the listen request) => re-run `tools/list`. Legacy: plain `notifications/tools/list_changed` on the session.

### 3.2 Annotations (ToolAnnotations) and trust
Defaults from schema.ts 2026-07-28 (fetched 2026-10-23); interface doc comment: "NOTE: all properties in ToolAnnotations are **hints**. They are not guaranteed to provide a faithful description of tool behavior ... Clients should never make tool use decisions based on ToolAnnotations received from untrusted servers."

| Field | Meaning (schema comment) | Default |
|---|---|---|
| `title` | human-readable title (display precedence: `title`, `annotations.title`, `name`) | none |
| `readOnlyHint` | tool does not modify its environment | false |
| `destructiveHint` | may perform destructive updates (meaningful only when readOnlyHint == false); false = additive only | true |
| `idempotentHint` | repeated call with same args has no additional effect (meaningful only when readOnlyHint == false) | false |
| `openWorldHint` | may interact with an open world of external entities (web search = open, memory tool = closed) | true |

Tools page, both revisions: "clients **MUST** consider tool annotations to be untrusted unless they come from trusted servers." Overview page: "descriptions of tool behavior such as annotations should be considered untrusted, unless obtained from a trusted server."
Brisvox design (Inferred, not spec): map `readOnlyHint`/`idempotentHint` only from operator-configured TRUSTED servers into the SideEffect/idempotency classification; for any other server treat the tool as SideEffect with no speculation and no automatic retry (this follows the "never run speculatively, never twice" invariant). Operator config should be able to override per tool.

### 3.3 `tools/call`
- Request `params`: `name`, `arguments` (object), `_meta?` (`progressToken` opt-in for progress). Modern retry after input_required adds `inputResponses`, `requestState` (new JSON-RPC id).
- Result: `content[]` (unstructured) and/or `structuredContent`, `isError?`, modern `resultType`.
- Content item types: `text` {text}; `image` {data base64, mimeType}; `audio` {data base64, mimeType}; `resource_link` {uri, name, description?, mimeType?}; `resource` embedded {resource{uri, mimeType, text|blob}}. All support optional `annotations` {audience, priority, lastModified}.
- `structuredContent`: modern = any JSON value; legacy = JSON object. "a tool that returns structured content SHOULD also return the serialized JSON in a TextContent block". If `outputSchema` exists: servers MUST conform, clients SHOULD validate.
- Two error channels: (a) JSON-RPC protocol errors (unknown tool => -32602, malformed request, server errors): clients "MAY" give to the LLM, recovery unlikely; (b) tool execution errors = result with `isError: true` (API failure, input validation, business logic): clients "SHOULD" give these to the LLM for self-correction.
- Stateful tools (2026-07-28 non-normative): no session; servers hand out explicit handles (e.g. `basket_id`) that the model carries as ordinary arguments.

### 3.4 Client-side SHOULDs from the Tools "Security Considerations" (both revisions)
Prompt for confirmation on sensitive operations; show tool inputs to the user before calling (anti-exfiltration); validate tool results before passing to the LLM; implement timeouts; log tool usage for audit. 2026-07-28 adds: follow `$ref` rules when validating inputs/outputs. Human-in-the-loop: "there SHOULD always be a human in the loop with the ability to deny tool invocations" (tools page; Brisvox is a real-time voice agent, so a per-operator allowlist/policy replaces interactive confirmation: Inferred design choice, record in DECISIONS).

### 3.5 JSON Schema handling (basic page, 2026-07-28)
Clients/servers MUST support 2020-12 (default when `$schema` absent); other dialects handled gracefully (return an error if unsupported). Implementations MUST NOT auto-dereference `$ref` to network URIs (opt-in only, off by default; if on, allowlist and block loopback/link-local/private ranges, apply timeouts/size limits). Unresolved external `$ref` SHOULD reject the schema. SHOULD bound composition-keyword cost (max depth, subschema cap, time budget). For LLM function declarations Brisvox must still translate `inputSchema` to each LLM's accepted subset (see provider docs; Inferred).

## 4. Transports, cancellation, progress, timeouts

### 4.1 stdio (2026-07-28)
Source: https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio ; legacy: https://modelcontextprotocol.io/specification/2025-11-25/basic/transports (fetched 2026-10-23).
- Client spawns the server as a subprocess. Messages are UTF-8 JSON-RPC, one per line, delimited by newlines, "MUST NOT contain embedded newlines". Server writes ONLY valid MCP messages to stdout; client writes ONLY valid MCP messages to stdin. Server MAY log to stderr; client MAY capture/forward/ignore it and SHOULD NOT treat stderr as an error indicator.
- Modern server MUST NOT write JSON-RPC requests to stdout (server-to-client interaction is MRTR). Legacy servers may send requests (sampling etc.).
- Shutdown (client SHOULD): close the server's stdin; wait for exit; if it does not exit in reasonable time, force-terminate (POSIX: SIGTERM then SIGKILL; Windows: TerminateProcess or Job Objects). Servers SHOULD exit promptly on stdin EOF.
- Unexpected exit (modern): client SHOULD restart; in-flight requests are lost and can be retried (protocol is stateless); `subscriptions/listen` must be re-sent.
- Clients "SHOULD support stdio whenever possible" (2025-11-25 transports page).
- Notifications on stdio are demultiplexed by `io.modelcontextprotocol/subscriptionId`.

### 4.2 Streamable HTTP (2026-07-28)
Source: https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http (fetched 2026-10-23).
- One endpoint path accepting POST. Every JSON-RPC request or notification is its own POST. Client MUST send `Accept: application/json, text/event-stream` and MUST support both response types: `Content-Type: application/json` (single object) or `text/event-stream` (SSE scoped to that request; may carry `notifications/progress` first, final response SHOULD terminate the stream). Notification POST => `202 Accepted`, no body. Client MUST NOT send JSON-RPC responses.
- Required headers every POST: `MCP-Protocol-Version: 2026-07-28` (must equal `_meta` protocolVersion, else 400 + HeaderMismatch -32020), `Mcp-Method: <method>`, `Mcp-Name: <params.name|uri>` for tools/call, resources/read, prompts/get. Non-ASCII/unsafe values are encoded `=?base64?<b64 of UTF-8>?=` (also for plain-ASCII values that look like the sentinel).
- `x-mcp-header` (client MUST support): a property schema may carry `"x-mcp-header": "Region"` => client adds `Mcp-Param-Region: <value>` (string as-is, integer decimal, boolean `true`/`false`; omit when absent/null). Client MUST reject (drop from tools/list result, log a warning) tools whose `x-mcp-header` violates constraints (non-empty tchar token, case-insensitively unique, only primitive types integer/string/boolean, not `number`, only on statically reachable `properties` chains, never through items/oneOf/anyOf/allOf/not/if/$ref). On HeaderMismatch the client SHOULD re-run `tools/list` and retry.
- Server errors: unsupported version => 400 + UnsupportedProtocolVersionError (-32022); unknown method => 404 + JSON-RPC -32601; missing headers => 400.
- No `Mcp-Session-Id`, no GET stream, no `Last-Event-ID`. A modern server answers GET/DELETE with 405 and ignores Mcp-Session-Id / Last-Event-ID.
- Servers SHOULD send `X-Accel-Buffering: no` on SSE; long-lived streams may emit SSE comment lines (`:`) as keep-alive, which clients must ignore.
- Security (server-side, relevant if brisvox ever binds locally): validate `Origin` (403 if invalid), bind localhost only when local.
- Cancellation: closing the SSE response stream IS the cancel signal (server MUST treat it as cancellation, SHOULD stop work, MUST NOT send more messages for it).

### 4.3 Streamable HTTP legacy (2025-11-25) — needed for dual-era
Source: https://modelcontextprotocol.io/specification/2025-11-25/basic/transports (fetched 2026-10-23).
- Endpoint supports POST and GET. Server MAY assign a session at initialize via header `MCP-Session-Id` (visible ASCII 0x21-0x7E). If assigned, client MUST send it on all later requests; server MAY terminate it, after which it answers 404 to that id and the client MUST start a new session with a new `InitializeRequest` without a session id. Client SHOULD send HTTP DELETE with the id to end a session (server may answer 405). HTTP header names are case-insensitive (the spec writes `MCP-Session-Id`).
- POST response may be JSON or SSE; server MAY send requests/notifications before the response. Disconnect "SHOULD NOT be interpreted as the client cancelling its request"; to cancel send `notifications/cancelled`.
- Optional resumability: SSE `id` fields; client resumes with HTTP GET + `Last-Event-ID`; server SHOULD send `retry` before closing a connection and client MUST respect it. GET on the endpoint MAY open a server-initiated SSE stream (server answers 405 if not offered).
- `MCP-Protocol-Version` header required on all later requests; if absent the server SHOULD assume `2025-03-26`; unsupported => 400.
- Backward compat with the 2024-11-05 HTTP+SSE: POST first; if 400/404/405 then GET expecting an `endpoint` SSE event.

### 4.4 Is the old HTTP+SSE transport deprecated?
YES. 2026-07-28 streamable-http page: "Deprecated: The HTTP+SSE transport from protocol version 2024-11-05 has been deprecated since protocol version 2025-03-26 and is classified as Deprecated under the feature lifecycle policy (SEP-2596). New implementations SHOULD NOT adopt it ... eligible for removal in a future revision." (fetched 2026-10-23). Deprecation window: at least 12 months (versioning page).

### 4.5 Cancellation
Source: https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/cancellation (fetched 2026-10-23).
- `notifications/cancelled` {`requestId`, `reason?`}. Modern: on Streamable HTTP, closing the response stream cancels (no notification expected); on stdio the client MUST send `notifications/cancelled`. Receiver SHOULD stop, free resources, not respond. Cancel only requests that are still in flight; races are expected (both sides MUST handle them gracefully); client SHOULD ignore a late response to a cancelled request; invalid/unknown cancellations are ignored. Server sends `notifications/cancelled` ONLY to end a `subscriptions/listen`.
- Brisvox design (Inferred): barge-in on a pending tool call = cancel (stream close / notification) AND drop the late result by turn epoch; a cancelled SideEffect tool may still have taken effect, so the LLM context must not claim it did not run (tie to "after an interruption the LLM context contains only what was played").

### 4.6 Timeouts and ping
- Timeouts (lifecycle page 2025-11-25, repeated in the 2026-07-28 cancellation page): "Implementations SHOULD establish timeouts for all sent requests"; on timeout the sender SHOULD cancel and stop waiting; SDKs SHOULD make timeouts per-request configurable; MAY reset the clock on a matching progress notification, but SHOULD always enforce a maximum timeout. The spec gives NO numeric default.
- Ping: legacy (2025-11-25 ping page): either side can send `{"method":"ping"}`; receiver MUST respond promptly with `{}`; no response in a reasonable time => MAY treat connection as stale. REMOVED in 2026-07-28 (changelog item 5), so a modern client cannot ping; a legacy client must answer server pings even during initialize.
- Source: https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle , https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/ping (fetched 2026-10-23).

### 4.7 Progress
Source: https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/progress (fetched 2026-10-23). Client opts in with `params._meta.progressToken` (string or integer, unique across active requests). Server MAY send `notifications/progress` {`progressToken`, `progress` (MUST increase), `total?`, `message?`}; MAY send none; MUST stop after completion; both sides SHOULD rate-limit. Over HTTP these ride the request's own SSE stream.

## 5. Authorization (HTTP servers only)

Sources: https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/index , .../client-registration (fetched 2026-10-23).
- Authorization is OPTIONAL. HTTP implementations SHOULD conform; stdio implementations SHOULD NOT and "retrieve credentials from the environment". Clients and servers MAY negotiate their own custom auth (basic page).
- Full OAuth 2.1 client duties when you do support it: handle 401 + `WWW-Authenticate: Bearer resource_metadata="..."[, scope="..."]`; fetch RFC 9728 Protected Resource Metadata (clients MUST use it for AS discovery); support BOTH RFC 8414 and OpenID Connect discovery; obtain a client_id (priority: pre-registered, then Client ID Metadata Document if AS advertises `client_id_metadata_document_supported`, then DCR (deprecated) if `registration_endpoint`, else ask the user); PKCE; `resource` parameter (RFC 8707) on BOTH authorization and token requests, always sent; validate RFC 9207 `iss` against the recorded issuer; step-up on 403 `insufficient_scope` (union scopes, retry only a few times); `Authorization: Bearer <token>` on EVERY HTTP request, never in the query string; MUST NOT send tokens other than ones issued by the server's authorization server; keep refresh tokens confidential; key stored client credentials by issuer.
- Minimal client (Brisvox design, Inferred): a voice agent runs headless, so the browser-based interactive flow is not usable. Minimum viable = no auth (stdio, or open HTTP) + static `Authorization: Bearer <token>` header from config/secret store (what LiveKit and Pipecat do, see 7). The spec's OAuth flow (discovery, CIMD/DCR, PKCE, browser redirect) MAY be skipped initially; a `client_credentials` machine flow exists as an official extension (https://modelcontextprotocol.io/extensions/auth/oauth-client-credentials, listed in llms.txt; contents NOT fetched, UNVERIFIED).
- Whatever is skipped, still apply: HTTPS only (loopback exempt), SSRF guards if discovery is ever added (see 6).

## 6. Security notes for clients

Sources: https://modelcontextprotocol.io/specification/latest (Security and Trust & Safety), https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices (fetched 2026-10-23).
- Principles (spec index): explicit user consent and control; data privacy (hosts MUST obtain explicit consent before exposing user data to servers); tool safety: "Tools represent arbitrary code execution", "descriptions of tool behavior such as annotations should be considered untrusted, unless obtained from a trusted server", hosts MUST obtain explicit consent before invoking any tool.
- The fetched pages do NOT use the term "tool poisoning"; the nearest normative content is the untrusted-annotations rule, "Validate tool results before passing to LLM", and "Show tool inputs to the user before calling the server". Treating tool names, descriptions, schemas, and results as untrusted prompt-injection input to the LLM is Inferred good practice, not a spec statement.
- Local servers (stdio): a client with one-click server config MUST show the exact command without truncation, flag it as dangerous, and get explicit approval; SHOULD sandbox with minimal privileges, warn about `sudo`/`rm -rf`/network ops. Servers run with the client's privileges. Brisvox design: stdio commands come only from operator config, never from LLM or caller input.
- SSRF during OAuth discovery (if implemented): require HTTPS (loopback exempt), block private/link-local/loopback ranges (10/8, 172.16/12, 192.168/16, 127/8, ::1, 169.254/16, fc00::/7, fe80::/10), validate redirect targets, consider egress proxy, beware DNS rebinding TOCTOU; do not hand-roll IP parsing.
- OAuth URL handling: only open `http`/`https` authorization URLs; reject `javascript:`, `data:`, `file:`, `vbscript:`; never open via a shell.
- Token passthrough is forbidden (server side); confused deputy applies to MCP proxies (server side); state-handle hijacking applies to servers that mint handles.
- Icons: treat as untrusted, HTTPS/`data:` only, no credentials on fetch (Brisvox does not need icons; ignore).
- `serverInfo`/`clientInfo` are unverified self-reports: SHOULD NOT change behavior or security decisions.

## 7. Rust implementations (crates.io checked 2026-10-23)

### 7.1 Official SDK: `rmcp`
Sources: https://crates.io/api/v1/crates/rmcp , https://crates.io/api/v1/crates/rmcp/3.7.0 , https://github.com/modelcontextprotocol/rust-sdk (README, Cargo.toml, crates/rmcp/Cargo.toml, CHANGELOG), https://docs.rs/crate/rmcp/3.7.0 (all fetched 2026-10-23).

| Item | Value |
|---|---|
| Crate | `rmcp` (+ `rmcp-macros`, optional; both at 3.7.0) |
| Current version | 3.7.0, published 2026-10-22; 3.6.0 on 2026-10-20, 3.5.1 on 2026-10-05, 3.5.0 on 2026-09-28 (fast cadence) |
| Licence | Apache-2.0 (crates.io + workspace Cargo.toml) |
| MSRV | Rust 1.88 (`rust-version` in workspace and crates.io); edition 2024 |
| Repo / status | https://github.com/modelcontextprotocol/rust-sdk "The official Rust SDK for the Model Context Protocol"; ~4.0k stars, 681 forks, 27 open issues at fetch; README: issues/PRs auto-triaged then assigned to a maintainer on weekly rotation; has a `conformance` workspace member. No SDK tier declared in README. Downloads: 38,987,427 total, 21,800,162 recent |
| Spec support | README: implements `2026-07-28`, compatible with `2025-11-25` and earlier; standalone HTTP+SSE (2024-11-05) intentionally NOT provided. `ProtocolVersion::LATEST`, `V_2026_07_28`, `KNOWN_VERSIONS` |
| Breaking 3.0 series | MCP 2026 methods, `server/discover`, stateless negotiation, MRTR, SEP-2243 headers, cache hints, tasks extension; `stateful_mode` renamed `legacy_session_mode`; a missing `issuer` in AS metadata is rejected by default (CHANGELOG) |

Client API (crates/rmcp/src/service/client.rs on main, fetched 2026-10-23; UNVERIFIED against the exact 3.7.0 tag):
- `Peer<RoleClient>::list_tools(Option<PaginatedRequestParams>) -> Result<ListToolsResult, ServiceError>`, `list_all_tools() -> Result<Vec<Tool>, ServiceError>`, `call_tool(CallToolRequestParams) -> Result<CallToolResult, ServiceError>`, `notify_cancelled(CancelledNotificationParam)`.
- `ClientLifecycleMode`: `Initialize` | `Discover { preferred_versions }` | `Auto { preferred_versions, legacy_version }`. `Auto` probes `server/discover` and falls back to legacy `initialize`; probe timeout `DEFAULT_AUTO_DISCOVER_TIMEOUT` = 10 s (README, client.rs). This is exactly the dual-era behaviour of section 1.2.
- Timeouts (service.rs): `PeerRequestOptions { timeout: Option<Duration>, meta, reset_timeout_on_progress: bool, max_total_timeout: Option<Duration> }` with `with_timeout(..)`; on expiry it sends a cancel notification (reason "request timeout") and returns `ServiceError::Timeout`. `RequestHandle::cancel(reason)` and `await_response()` exist.
- Streamable HTTP client: `StreamableHttpClientTransport::from_uri("http://localhost:8000/mcp")` (README); `control_request_timeout` default 5 s (README); auth example uses a configured reqwest client (examples/clients/README). How to set a static `Authorization` header: NOT confirmed on a fetched page, UNVERIFIED (check the transport config struct before implementing).
- Stdio client: `TokioChildProcess::new(Command::new(..).configure(|c| ..))?` then `().serve(transport).await?` (README). Shutdown: `waiting().await` / `cancel().await` (README); precise close-stdin/kill behaviour UNVERIFIED.
- MRTR: default max rounds 10 (`DEFAULT_MRTR_MAX_ROUNDS`), configurable per call (README). `ToolAnnotations` accessible via `Tool` model (field names UNVERIFIED in rmcp).
- CHANGELOG 3.6.0: abandoned `tools/call` requests can be cancelled; pending request tokens cancelled after transport EOF; server pings answered during initialize.

Feature flags (crates.io 3.7.0 features table, fetched 2026-10-23). NOTE the default features are `base64, macros, server` (crates.io + Cargo.toml on main), so a client-only build MUST set `default-features = false`:

| Feature | Enables |
|---|---|
| `client` | `dep:tokio-stream` |
| `transport-child-process` | `transport-async-rw`, `tokio/process`, `process-wrap` (stdio client; `which-command` adds `which`) |
| `transport-streamable-http-client` | `client-side-sse` (sse-stream, http, bytes, base64), `transport-worker` |
| `transport-streamable-http-client-reqwest` | the above + `reqwest` dep (no TLS by itself) |
| `reqwest` | `reqwest/rustls` (recommended TLS); alternatives `reqwest-native-tls`, `reqwest-tls-no-provider` |
| `auth` | `async-trait`, `oauth2` 5.0, `reqwest`, `url` (OAuth 2.0 client; skip initially) |
| `request-state`, `elicitation`, `schemars`, `macros`, `server*` | not needed for a client |

Minimum client feature set (Inferred from the table above, NOT compile-tested): `rmcp = { version = "3.7", default-features = false, features = ["client", "transport-child-process", "transport-streamable-http-client-reqwest", "reqwest"] }`. Whether `client` compiles without `schemars`/`server` is UNVERIFIED; verify with a `cargo check` when the client is built.

Dependency footprint (crates/rmcp/Cargo.toml main, fetched 2026-10-23): always-on: serde 1 (derive, rc), serde_json, thiserror 2, tokio 1 (sync, macros, rt, time), futures 0.3, indexmap 2 (serde), tracing 0.1, tokio-util 0.7, pin-project-lite 0.2, chrono 0.4.38 (non-wasm: default-features=false, serde+now; Linux also libc 0.2). Optional per feature: reqwest ^0.13.2 (default-features=false; json, stream), sse-stream ^0.2.4, http 1, bytes 1, process-wrap 10, which 8, oauth2 5, jsonwebtoken 11, uuid, rand 0.10, hyper/hyper-util, hmac/sha2/zeroize, schemars 1, base64 0.23. Brisvox note: `docs/providers/call-control.md` records `reqwest 0.13.5` already in the workspace, which satisfies `^0.13.2`, so the reqwest build should be shared (verify feature unification: rmcp sets `default-features=false` and TLS is chosen by the `reqwest` feature). docs.rs documentation coverage 49.45% (1117/2259 items) for 3.7.0.

### 7.2 Alternatives
Source: https://crates.io/api/v1/crates?q=mcp%20client (fetched 2026-10-23), https://crates.io/api/v1/crates/rust-mcp-sdk , https://raw.githubusercontent.com/rust-mcp-stack/rust-mcp-sdk/main/README.md.

| Crate | Version / date | Licence | MSRV | Notes |
|---|---|---|---|---|
| `rust-mcp-sdk` (+ `rust-mcp-transport`, `rust-mcp-schema` 2.0.0) | 2.0.0, 2026-08-27 | MIT | 1.80.0 (crates.io 2.0.0) | Community SDK for servers AND clients. Features `client, server, auth, macros, stdio, streamable-http, sse`. README: 2.0 (`main`) implements 2026-07-28; 1.x LTS = 2025-11-25; reports 440/440 official client conformance tests. Client usage shown: `request_tool_list(None)`, `call_tool(CallToolRequestParams{..})`; stdio client example only; HTTP client usage not shown in the README. 319,475 downloads (115,535 recent) |
| `smg-mcp` | 2.3.5, 2026-10-15 | UNVERIFIED | UNVERIFIED | "Model Context Protocol (MCP) client implementation", part of the smg-project (a gateway); 1,813,017 downloads; not evaluated beyond crates.io listing, likely tied to that project |

Hand-rolling a client (Inferred): feasible for tools-only, but must implement the dual-era probe, header mirroring (`Mcp-Method`, `Mcp-Name`, `x-mcp-header`, base64 sentinel), SSE parsing, MRTR handling, pagination, JSON Schema 2020-12 validation and cancellation; the spec surface changed substantially in 2026-07-28, so prefer `rmcp`.

## 8. How LiveKit Agents and Pipecat expose MCP tools

### 8.1 LiveKit Agents (Python)
Source: https://docs.livekit.io/agents/logic/tools/mcp.md (fetched 2026-10-23).
- Python only (install `livekit-agents[mcp]~=1.8`); Node.js not supported.
- Config: wrap each server as `MCPToolset(id=..., mcp_server=...)` and put it in the agent's/session's `tools` list. The `mcp_servers` param on `AgentSession`/`Agent` is DEPRECATED (auto-generated ids not stable).
- `MCPServerHTTP(url, headers=, transport_type=, allowed_tools=, client_session_timeout_seconds=, tool_result_resolver=)`. Transport auto-detect by URL suffix: `/mcp` => streamable HTTP, `/sse` => SSE (override with `transport_type="streamable_http"`). Auth via `headers={"Authorization": f"Bearer {...}"}`.
- `MCPServerStdio(command, args, env=, cwd=)` for local servers.
- Filtering: `allowed_tools=[...]` per server; `toolset.filter_tools(predicate)` after setup. `tool_options` per tool: `flags` (`ToolFlag.CANCELLABLE` lets the LLM cancel a running tool, exposing `lk_agents_get_running_tasks`/`lk_agents_cancel_task`), `on_duplicate` (`allow|reject|replace|confirm`), `report_progress` (forward progress to the user). Errors raise `ToolError` before the result resolver runs. Servers initialize in parallel; a server that fails to connect is logged and the agent still starts. Long tools may exceed the default per-request timeout; raise `client_session_timeout_seconds` (example 120). No latency figures or approval flow other than `on_duplicate="confirm"` are documented.

### 8.2 Pipecat (Python)
Source: https://docs.pipecat.ai/api-reference/server/utilities/mcp/mcp.md (fetched 2026-10-23).
- `MCPClient(server_params=..., tools_filter=None, tools_output_filters=None, tools_arguments=None)`; requires MCP Python SDK 2.1.1+ (`pipecat-ai[mcp]`).
- `server_params` is one of `StdioServerParameters` (command, args, env), `SseServerParameters` (url, headers, timeout, sse_read_timeout), `StreamableHttpParameters` (url, headers, timeout, sse_read_timeout, terminate_on_close). Doc examples: `timeout=5`, `sse_read_timeout=300`.
- `tools_arguments` merges fixed arguments into every call of a tool, overriding model-supplied values and hiding them from the LLM schema (useful for forcing e.g. tenant ids; adopt idea, Inferred).
- Usage: `context = LLMContext(messages=[...], tools=await mcp.tools())`. `register_tools`/`get_tools_schema`/`register_tools_schema` DEPRECATED since 1.8.0 (removed in 2.0.0). Connection opens on first `tools()` or `start()` and is reused; `close()` at pipeline teardown. No per-tool-call timeout documented; how results reach the LLM is not described.

## 9. Brisvox implications (summary; Inferred, record in DECISIONS when decided)
1. Use `rmcp` 3.x with `ClientLifecycleMode::Auto` (dual-era), stdio + Streamable HTTP, no OAuth initially; static bearer header from secrets.
2. Declare empty client capabilities; reject/answer `input_required` and legacy server requests (sampling/elicitation/roots) with a clear error.
3. Cache `tools/list` per `ttlMs`; refresh via `subscriptions/listen` `toolsListChanged` only if worth the open stream; otherwise re-list at session start (a mid-call list change is a latency/consistency risk).
4. Per-call timeout and cancellation wired to `CancellationToken` and the turn epoch; HTTP cancel = drop the stream; stdio cancel = `notifications/cancelled`.
5. Treat all server-provided text/annotations as untrusted unless the server is operator-marked trusted; SideEffect by default.
6. Filter tools by an operator allowlist (`allowed_tools` pattern); namespace tool names by server id to avoid collisions.
