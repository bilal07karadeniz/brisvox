# Anthropic — LLM (Claude Messages API: streaming, tool use, prompt caching)

Researched 2026-10-08. Every fact below came from a page fetched that day; URLs are listed per
section and in "Sources". Pages were read through a summarising fetch tool (the streaming page
was read from the saved raw markdown), so JSON/SSE blocks marked "(doc)" were copied from the docs
and blocks marked "(illustrative)" were rebuilt from field lists and are NOT wire captures. Anything
not confirmed is marked UNVERIFIED. A live capture must confirm wire shapes before
they are trusted. The documented model generation (Claude 5.5 family, released Sep/Oct 2026) is
newer than any training data: never hard-code model ids in logic, read them from config.

- Docs: https://platform.claude.com/docs/en (old hosts `docs.claude.com/en/api/...` answer 301 to
  `platform.claude.com/docs/en/...`; the API-reference schema still links `docs.anthropic.com`)
  (fetched 2026-10-08)
- Auth: `x-api-key: <API key>` plus `anthropic-version: 2023-06-01` plus
  `content-type: application/json`. The overview page lists `Authorization: Bearer <token>` as
  the primary header ("Yes, unless `x-api-key` is set"; the token is an API key or a short-lived
  token from Workload Identity Federation) and calls `x-api-key` the "Legacy fallback ... still
  supported". The Messages examples on every page still use `x-api-key`; use that. Optional
  `anthropic-workspace-id` (required only with a multi-workspace key) and `anthropic-beta`
  (comma separated, or the header repeated). Source: https://platform.claude.com/docs/en/api/overview
  and https://platform.claude.com/docs/en/api/beta-headers
- Endpoint(s) / regions: base `https://api.anthropic.com`; Messages: `POST /v1/messages`
  (also `POST /v1/messages/count_tokens`, `GET /v1/models`). Max request size 32 MB (413
  `request_too_large`, returned by Cloudflare before the API servers). Region choice: request
  field `inference_geo` (string or null, defaults to the workspace `default_inference_geo`;
  `"us"` and `"global"` seen in docs; US-only inference is priced 1.1x on Claude 4.6+ per the
  service-tiers page). Supported-regions/data-residency pages were NOT fetched: UNVERIFIED which
  geos exist and any regional hostnames. Rate limits are shared across `inference_geo` values.
- Versioning: `anthropic-version: 2023-06-01` is the only non-initial version listed ("Anthropic
  recommends using the latest API version"; `2023-01-01` is the initial release). Within a version
  Anthropic may "Add additional optional inputs", "Add additional values to the output", "Change
  conditions for specific error types", "Add new variants to enum-like output values (for example,
  streaming event types)". So: serde without `deny_unknown_fields`, unknown event/delta/block
  types ignored. Source: https://platform.claude.com/docs/en/api/versioning

## Decision (surface and model recommendation)

**Use `POST /v1/messages` with `stream: true`, stateless full-history replay (the API is
stateless: "you always send the full conversational history"; synthetic assistant turns are
allowed), raw HTTP + SSE.** No official Rust SDK: the seven official client SDKs are Python,
TypeScript, C#, Go, Java, PHP, Ruby (https://platform.claude.com/docs/en/cli-sdks-libraries/overview).
Community Rust crates were not evaluated: UNVERIFIED. The OpenAI-SDK-compatibility layer exists
but is a "library/integration", not the Messages API, and is "not considered a long-term or
production-ready solution for most use cases"; do not use it. Documented limitations of that layer
(https://platform.claude.com/docs/en/cli-sdks-libraries/libraries/openai-sdk): prompt caching not
supported, the OpenAI SDK does not return Claude's thinking, `strict` is ignored, system/developer
messages are hoisted and concatenated at the start.

Models (ids from https://platform.claude.com/docs/en/models/overview, .../models/haiku-5-5/overview,
.../api/messages/create `model` enum, all fetched 2026-10-08):
| Model ID | Role per docs | Price /MTok in / out | Cache write 5m / 1h / read | Latency label |
|---|---|---|---|---|
| `claude-haiku-5-5` | "For high-volume, latency-sensitive tasks such as classification, extraction, and routing"; API enum text "Fastest model for high-volume, real-time tasks"; reduce-latency page: "For speed-critical applications, Claude Haiku 5.5 offers the fastest response times while maintaining high intelligence" | $0.10 / $0.50 for prompts <= 100,000 tokens; $0.50 / $2.50 above | $0.125 / $0.20 / $0.01 (<= 100K); $0.625 / $1 / $0.05 (> 100K) | "Fastest" |
| `claude-sonnet-5-5` | "The best combination of speed and intelligence" (models overview; the API enum text says "Efficient model for coding and agents") | $2 / $10 | $2.50 / $4 / $0.10 | "Fast" |
| `claude-opus-5-5` | "For long-running agentic coding and knowledge work" | $4 / $20 | $5 / $8 / $0.20 | "Moderate" |
| `claude-fable-5-1` | "demanding reasoning and long-horizon agentic work" | $10 / $50 | n/a here (read 0.025x) | "Slower" |
All four: 1M context, 128K max output, text+image in, text out, reliable knowledge cutoff Jun 2026.
`claude-haiku-5-5` released 2026-10-07 (yesterday relative to this research), retirement "Not
sooner than October 7, 2027"; the id "is a fixed model ID with no date suffix and no separate alias"
(Haiku 5.5 migration guide). Legacy ids still listed: `claude-sonnet-5`, `claude-haiku-4-5`
(`claude-haiku-4-5-20251001`), `claude-opus-5`, `claude-opus-4-8`, ...; `claude-sonnet-4-5*`
deprecated (EOL 2026-11-30).
**Recommended default for brisvox voice: `claude-haiku-5-5`** (docs call it the fastest; no TTFT or
tokens/s numbers are published on any page fetched, so absolute latency is UNVERIFIED: measure in
a live run), with `claude-sonnet-5-5` as the quality tier. Reasons Haiku 5.5 fits a streaming
voice loop: forced `tool_choice` is accepted (Sonnet 5.5 and Opus 5.5 reject `any`/`tool` with 400),
`thinking: {"type":"disabled"}` is accepted at effort <= `high`, 128K output, 512-token cache minimum.
Haiku 5.5 restrictions (all HTTP 400, from the Haiku migration guide and thinking page):
- `temperature`, `top_p`, `top_k`: omit. Haiku migration guide: if present, `temperature` must be
  `1`, `top_p` must be `0.99` (a `top_p` of `1` is rejected), `top_k` any value is rejected, and
  `temperature` + `top_p` together is rejected. (The Messages-create reference words `top_p` as "A
  value >= 0.99 will be accepted", which disagrees on `1`: send neither.) For Sonnet 5.5 and Opus 5.5
  the docs say generically that Claude 4.7+ models return 400 for a non-default `temperature`/`top_p`/
  `top_k` (working-with-messages page); the exact accepted values for those two were only read for
  Haiku: UNVERIFIED. So brisvox has no temperature knob for Claude 5.x.
- Assistant prefill (a final assistant message) is rejected even with thinking off: the `messages`
  array must end with a user turn.
- `thinking: {"type":"enabled","budget_tokens":N}` is rejected; `between_tools` is rejected on Haiku
  (Sonnet 5.5 only).
- Priority Tier is not supported on Haiku 5.5 / Sonnet 5.5 / Opus 5.5 (service-tiers page; the
  tier is also closed to new purchases: "Priority Tier capacity commitments are no longer
  available for purchase"). `service_tier` request values: `auto` (default), `standard_only`.
- Haiku 5.5 uses the Claude 4.7+ tokenizer: "approximately 30% more tokens than on Claude Haiku 4.5".
- Safety classifiers can decline a request: `stop_reason: "refusal"` with `stop_details`; Haiku
  5.5 has no server-side fallback ("Sending the same request ... again usually returns another
  refusal").

## Request (fields from the Messages create reference; names exact)
Source: https://platform.claude.com/docs/en/api/messages/create (page truncated after
`top_p`/response, request parts read in two chunks).
- `model` (string), `max_tokens` (number >= 0; `0` "populates the prompt cache without generating a
  response"), `messages` (array, max 100,000), `system` (string or array of text blocks),
  `stream` (bool), `stop_sequences`, `tools`, `tool_choice`, `thinking`, `output_config`
  (`effort`: `low|medium|high|xhigh|max`; `format`: `{"type":"json_schema","schema":{...}}`),
  `metadata` (`{"user_id": string|null}`, <= 512 chars, opaque id, not PII), `service_tier`,
  `cache_control` (top-level automatic caching), `inference_geo`, `container`, `diagnostics`
  (`{"previous_message_id": string|null}`, makes the response carry `diagnostics.cache_miss_reason`),
  `speed` (not found on the fetched part of the page; "fast mode" research preview is Opus only
  (Opus 5.5, Opus 5, Opus 4.8) per the rate-limits page: UNVERIFIED for Haiku).
- `messages[].role`: `user`, `assistant`, or `system` (system only mid-conversation, see below; the
  schema lists `system` but the field description still says there is "no system role", an
  inconsistency in the reference). `content`: string (one text block) or array of blocks: `text`
  (`text` minLength 1, optional `cache_control`), `image`, `document`, `thinking` (`signature`,
  `thinking`), `redacted_thinking` (`data`), `tool_use` (`id`, `name`, `input`), `tool_result`
  (`tool_use_id`, `content`, `is_error`), ... An empty `text` block is invalid (minLength 1) and
  "empty text blocks" are not cacheable.
- Whether consecutive same-role messages are merged or rejected, and whether the first message must
  be `user`: the pages fetched do not say: UNVERIFIED. Build strictly alternating turns:
  consecutive brisvox `Tool` messages must go into ONE user message (required anyway, see tool
  results), and a user text that follows tool results goes into the same user message AFTER the
  `tool_result` blocks.
- Minimal streaming request (doc, streaming page; model id there is the doc's example):
```bash
curl https://api.anthropic.com/v1/messages \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -d '{
    "model": "claude-opus-5-5",
    "messages": [{"role": "user", "content": "Hello"}],
    "max_tokens": 256,
    "stream": true
  }'
```
- Latency example (doc, reduce-latency page): `"model": "claude-haiku-5-5", "max_tokens": 1024,
  "output_config": {"effort": "low"}` ("`low`, the cheapest and fastest level"). Thinking tokens
  count toward `max_tokens` "even when the thinking content isn't returned"; Haiku 5.5 guide: "A
  `max_tokens` value sized for Claude Haiku 4.5 requests that ran without thinking can cut the
  reply off, so leave room for thinking." With thinking disabled this does not apply.
- Voice request sketch for Haiku 5.5 (illustrative, assembled from the rules above, not a doc
  block): `{"model":"claude-haiku-5-5","max_tokens":<n>,"stream":true,"system":[{"type":"text",
  "text":"<persona>"}],"cache_control":{"type":"ephemeral"},"thinking":{"type":"disabled"},
  "output_config":{"effort":"medium"},"tools":[...],"messages":[...]}`.

### Thinking / effort settings for voice (Haiku 5.5, Sonnet 5.5)
Sources: https://platform.claude.com/docs/en/build-with-claude/thinking ,
.../build-with-claude/effort , .../models/haiku-5-5/migration-guide ,
.../build-with-claude/prompt-engineering/prompting-claude-haiku-5-5 .
- Per-model `thinking` table (doc): Haiku 5.5: no `thinking` field = adaptive thinking;
  `"adaptive"` ok; `"enabled"` + `budget_tokens` 400; `"between_tools"` 400; `"disabled"` = "Thinking
  off at `high` effort or below" (400 at `xhigh`/`max`). Sonnet 5.5: no field = adaptive;
  `"disabled"` 400 (message: `To turn thinking off on this model, send "thinking": {"type":
  "between_tools"} instead of {"type": "disabled"}`); `"between_tools"` = "Up-front thinking off at
  `high` effort or below" (400 at `xhigh`/`max`). Opus 5.5: `"disabled"` always 400 (thinking
  always on). **Thinking is ON by default on all three**, so a voice adapter must send `thinking`
  explicitly or accept a thinking pause before the first text.
- Effort levels `low`, `medium`, `high`, `xhigh`, `max`; defaults: Haiku 5.5 `medium`, Sonnet 5.5
  `high`, Opus 5.5 `medium`. Setting the default explicitly equals omitting it. Effort affects all
  tokens including tool calls ("Lower effort also means fewer and terser tool calls").
  Haiku guide: `low` "Use it for chat, short tool tasks, and simple, high-volume requests. In long
  agent prompts, the model is more likely to skip a search, stop early, or skip a check at this
  level." Sonnet 5.5 guide: "For chat and other latency-sensitive work, start with `medium` or
  `low`."
- `display`: `"omitted"` is the default on Haiku 5.5 and Sonnet 5.5 (empty `thinking` text,
  `signature` still returned) per the thinking page and the Haiku migration guide; NOTE the
  Messages-create reference says `display` "Defaults to `summarized`", which contradicts them:
  trust the thinking page, confirm in a live run. "Set `display: "omitted"` when your application
  doesn't surface thinking content ... faster time-to-first-text-token when streaming: the server
  skips streaming thinking tokens entirely and delivers only the signature". You are billed for the
  full thinking tokens either way.
- Changing the thinking mode or the top-level `effort` between requests invalidates the message
  cache ("Treat any thinking or top-level effort change as starting the cache over"). Per-message
  effort (system message with `output_config.effort`, beta header
  `mid-conversation-output-config-2026-07-01`) keeps the cache but is rejected with 400 on Haiku
  5.5 when thinking is `disabled` and effort differs from the level in effect.
- **Haiku 5.5 gotcha for TTS:** "Claude Haiku 5.5 sometimes writes reasoning-like text in the reply
  that users see. This happens more often with thinking off or at `low` effort. If you see this
  behavior, switch to adaptive thinking and `medium` effort." A voice agent would speak that text.
  The thinking-off/low-effort voice setting and this warning pull in opposite directions: decide
  empirically in a live run and keep both `thinking` and `effort` configuration values.
- Haiku 5.5 with thinking off and `output_config.format` JSON may skip a needed tool call
  (prompting guide); not relevant unless structured output is combined with tools.
- Haiku 5.5 forced tool use works but "the response starts with the tool call and has no `thinking`
  block"; also Anthropic says tool_choice `any`/`tool` prefill means "the models will not emit a
  natural language response or explanation before `tool_use` content blocks".

## SSE framing and event flow
Source: https://platform.claude.com/docs/en/build-with-claude/streaming (page title "Streaming
messages"; `docs/en/api/messages-streaming` redirects here).
- Standard SSE. "Each event uses an SSE event name (for example, `event: message_stop`), and
  includes the matching event `type` in its data." (The `type` field in the JSON `data` is
  authoritative for parsing; the `event:` line repeats it.) Events are separated by blank lines.
  There is no `data: [DONE]` sentinel in this version (versioning page: "Removed unnecessary
  `data: [DONE]` event"); the adapter should still ignore a `[DONE]` payload defensively.
- Flow (doc): 1. `message_start` (a `Message` with empty `content`). 2. A series of content blocks,
  each with `content_block_start`, one or more `content_block_delta`, `content_block_stop`; each
  block has an `index` "that corresponds to its index in the final Message `content` array".
  3. "One or more `message_delta` events, indicating top-level changes to the final `Message`
  object." 4. A final `message_stop`. `ping` events "dispersed throughout the response". Whether
  blocks are ever interleaved (deltas of block N+1 before `content_block_stop` of N): the flow text
  implies sequential blocks, not stated explicitly: UNVERIFIED; key accumulation by `index`.
- A server-side-fallback `fallback` content block (start+stop pair, no deltas) appears only for
  models with server-side fallback (not Haiku 5.5); ignore unknown block types.

| `type` | Fields | Use |
|---|---|---|
| `message_start` | `message` {`id`, `type:"message"`, `role:"assistant"`, `content:[]`, `model`, `stop_reason:null`, `stop_sequence:null`, `usage` {`input_tokens`, `output_tokens`, optionally `cache_creation_input_tokens`, `cache_read_input_tokens`}} | record `id` for logs; **usage baseline: input tokens and cache fields are here** |
| `content_block_start` | `index`, `content_block`: `{"type":"text","text":""}` / `{"type":"tool_use","id","name","input":{}}` / `{"type":"thinking","thinking":"","signature":""}` / `redacted_thinking` / `server_tool_use` / ... | open a block; for `tool_use` take `id` + `name`; `input:{}` is a placeholder |
| `content_block_delta` | `index`, `delta` | see delta table |
| `content_block_stop` | `index` | block complete: parse a `tool_use` input here |
| `message_delta` | `delta` {`stop_reason`, `stop_sequence`, (`stop_details`, `container`)}, `usage` (cumulative: `output_tokens`; may also carry `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `server_tool_use`, `output_tokens_details`) | **stop_reason arrives here, not in `message_start`**; final usage |
| `message_stop` | none | terminal success |
| `ping` | none | ignore (keepalive-like) |
| `error` | `error` {`type`, `message`} | terminal failure after HTTP 200 |

Delta types in `content_block_delta.delta` (doc):
| `delta.type` | Field | Meaning |
|---|---|---|
| `text_delta` | `text` | text for a `text` block -> `LlmEvent::TextDelta` |
| `input_json_delta` | `partial_json` | partial JSON string for a `tool_use` `input`; "the final `tool_use.input` is always an *object*" |
| `thinking_delta` | `thinking` | thinking text (empty string with `display:"omitted"`) |
| `signature_delta` | `signature` | opaque signature, "sent just before the `content_block_stop` event" |
| `citations_delta` | `citation` | citations (only with citation-enabled documents; ignore) |
Usage is cumulative: "The token counts shown in the `usage` field of the `message_delta` event are
*cumulative*." (`message_delta.usage.input_tokens` and the cache fields are "number or null"; a
`null`/absent field means "not updated": keep the `message_start` value.)

Examples (doc, copied from the streaming page):
```sse
event: message_start
data: {"type": "message_start", "message": {"id": "msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY", "type": "message", "role": "assistant", "content": [], "model": "claude-opus-5-5", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 25, "output_tokens": 1}}}

event: content_block_start
data: {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}

event: content_block_delta
data: {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "!"}}

event: content_block_stop
data: {"type": "content_block_stop", "index": 0}

event: message_delta
data: {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence":null}, "usage": {"output_tokens": 15}}

event: message_stop
data: {"type": "message_stop"}
```
Tool-use stream (doc, same page; a text preamble block 0, then the tool block 1):
```sse
event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01T1x1fJ34qAmk2tNTrN7Up6","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" \"San"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" CA\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":89}}

event: message_stop
data: {"type":"message_stop"}
```
(The first `input_json_delta` has an empty `partial_json`. The doc's `message_start` for this
example carries `"usage":{"input_tokens":472,"output_tokens":2}` and `"content":[]`.)
Thinking stream (doc, `display:"summarized"`):
```sse
event: content_block_start
data: {"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}

event: content_block_delta
data: {"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "I need to find the GCD of 1071 and 462 using the Euclidean algorithm.\n\n1071 = 2 × 462 + 147"}}

event: content_block_delta
data: {"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds..."}}

event: content_block_stop
data: {"type": "content_block_stop", "index": 0}

event: message_delta
data: {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}}
```
With `display:"omitted"`: "The thinking block opens, receives a `thinking_delta` with an empty
`thinking` string and then a single `signature_delta`, and closes."
Usage with caching (doc, web-search example `message_start`): `"usage":{"input_tokens":2679,
"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":3}`; its
`message_delta` carried `"usage":{"input_tokens":10682,"cache_creation_input_tokens":0,
"cache_read_input_tokens":0,"output_tokens":510,"server_tool_use":{"web_search_requests":1}}`.
Error event (doc):
```sse
event: error
data: {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}
```
"normally correspond[s] to an HTTP 529 in a non-streaming context". After the HTTP 200 the usual
status-code handling does not apply: the adapter must treat an `error` event as terminal.

Tool input granularity (doc): "Current models only support emitting one complete key and value
property from `input` at a time. As such, when using tools, there may be delays between streaming
events while the model is working." So tool-argument fragments arrive in lumps with gaps; do not
use a short idle timeout during a tool call (see timeouts). Per-tool opt-in
`"eager_input_streaming": true` (tool definition field; no beta header needed, all models) streams
fragments "without server-side buffering or JSON validation" ("typically longer, with fewer
mid-word breaks"); invalid or truncated JSON possible; wrap unparseable input as a tool result
`{"type":"tool_result","tool_use_id":"...","is_error":true,"content":"{\"INVALID_JSON\":
\"<the unparseable input you received>\"}"}`. (The legacy beta header
`fine-grained-tool-streaming-2025-05-14` still turns it on for tools that leave the field unset.)
Source: https://platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming .

### Stop reasons (handling-stop-reasons page, messages create reference)
`end_turn`, `max_tokens`, `stop_sequence`, `tool_use`, `pause_turn` (server-tool loop limit; not
used by client tools), `refusal`, `model_context_window_exceeded`. In streaming, `stop_reason` is
`null` in `message_start`, set in `message_delta`, absent elsewhere. `stop_details` is `null`
except for `refusal`: `{"type":"refusal","category":"cyber"|"bio"|"frontier_llm"|
"reasoning_extraction"|"general_harms"|null,"explanation": string|null}`.
Empty response gotcha (doc): Claude "sometimes returns an empty response (exactly 2–3 tokens, no
content) with `stop_reason: "end_turn"`. This typically occurs when Claude interprets that the
assistant turn is complete, particularly after tool results", caused by "adding text blocks
immediately after tool results" or "sending Claude's completed response back without adding
anything"; fix: send tool results with no extra text in the same user message (if still empty, add a
continuation prompt in a new user message rather than retrying the empty response).
The adapter must handle an `end_turn` with zero text and zero tool calls (voice: dead air).

### Mapping to brisvox `LlmEvent` (crates/brisvox-providers/src/llm.rs) — design, not implemented
- `TextDelta` <- `content_block_delta` with `delta.type == "text_delta"` (`delta.text`).
- `ToolCallDelta { index, id, name, arguments }`: `index` = dense counter over `tool_use` blocks
  (the block `index` also counts text/thinking blocks, so keep a map block index -> dense index);
  `id` = `content_block.id` (`toolu_...`, pattern `^[a-zA-Z0-9_-]+$`), `name` = `content_block.name`,
  both emitted at `content_block_start` with empty `arguments`; every `input_json_delta.partial_json`
  becomes a pure fragment (the empty first fragment may be skipped). The tool result is keyed by
  this `id` as `tool_use_id`.
- `Done { finish, usage, provider_data }`: emit after `message_stop` (not at `message_delta`), using
  the `stop_reason` from `message_delta`. `end_turn`/`stop_sequence` -> `Stop`; `tool_use` ->
  `ToolCalls`; `max_tokens` -> `Length`; `model_context_window_exceeded` -> `Length` (truncated;
  no brisvox variant, gap); `refusal` -> no variant (gap: map to `Stop` + log, or extend the enum);
  `pause_turn` -> not expected. A stream that ends without `message_stop`/`error` is cut off
  (`Disconnected`).
- `Usage`: **Anthropic `input_tokens` excludes cached tokens.** Docs: "`input_tokens`: tokens after the
  last cache breakpoint, not all input tokens. Total input = `cache_read_input_tokens` +
  `cache_creation_input_tokens` + `input_tokens`." So `Usage.input_tokens` = sum of the three,
  `Usage.cached_input_tokens` = `cache_read_input_tokens` (usage = `message_start.usage` overridden
  by non-null `message_delta.usage` fields; `output_tokens` is cumulative). Tokens that arrive
  only if the stream completes; a cut stream loses `message_delta` usage.
- `provider_data` (opaque JSON text, echoed back on the assistant `Message`): the JSON array of
  the response's assistant content blocks that must be replayed verbatim: every `thinking` block
  (with `thinking` text and `signature` assembled from deltas), every `redacted_thinking` block
  (`data`), and the `tool_use` blocks, in the order produced (plus a placeholder where the text
  block sat, as the OpenAI adapter does). `None` when the response had none of these (text only).
  Streaming shape of a `redacted_thinking` block (start event with `data` in `content_block`, any
  delta?): the schema lists `RedactedThinkingBlock` as a possible `content_block` of
  `content_block_start`, but no streaming example was found: UNVERIFIED.

## Tool definitions
Source: https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools ,
.../tool-use/strict-tool-use , .../tool-use/parallel-tool-use .
Tool (doc):
```json
{
  "name": "get_weather",
  "description": "Get the current weather in a given location",
  "input_schema": {
    "type": "object",
    "properties": {
      "location": {
        "type": "string",
        "description": "The city and state, e.g. San Francisco, CA"
      },
      "unit": {
        "type": "string",
        "enum": ["celsius", "fahrenheit"],
        "description": "The unit of temperature, either 'celsius' or 'fahrenheit'"
      }
    },
    "required": ["location"]
  }
}
```
- Fields: `name` (regex `^[a-zA-Z0-9_-]{1,128}$`; max 128, vs 64 for OpenAI), `description`
  ("by far the most important factor in tool performance"; aim for 3-4 sentences), `input_schema`
  (JSON Schema, must be `"type": "object"`), optional `input_examples` (array of example input
  objects, must validate against the schema else 400; ~20-50 tokens each), `strict`,
  `eager_input_streaming`, `cache_control`, `defer_loading`, `allowed_callers`.
  Mapping: `ToolSpec.name` -> `name`, `.description` -> `description`, `.parameters_schema` (JSON text)
  -> `input_schema` (parse; root must be an object schema, else `Protocol` error before any I/O).
- `strict: true` = grammar-constrained sampling ("Tool `input` strictly follows the
  `input_schema`"). Schema rules (structured-outputs page): `additionalProperties` must be `false`
  on objects; `required` supported; `anyOf` and `allOf` supported "with limitations" (`allOf` with
  `$ref` not); `enum`, `const`, local `$ref`/`$def`/`definitions`, `default` and string formats
  (`date-time`, `time`, `date`, `duration`, `email`, `hostname`, `uri`, `ipv4`, `ipv6`, `uuid`)
  supported; NOT supported: recursive schemas, `minimum`/`maximum`/`multipleOf`,
  `minLength`/`maxLength`, `minItems` other than 0/1, external `$ref`, regex
  lookahead/lookbehind/backreferences/`\b`/`\B`; `oneOf` is mentioned in neither list (UNVERIFIED);
  unsupported feature -> 400. Limits per request (combined across strict schemas): 20 strict tools,
  24 optional parameters, 16 union-typed parameters (`anyOf` or type arrays); too complex -> 400
  "Schema is too complex for compilation." (180 s compile timeout). **First use of a schema adds
  latency** ("additional latency while the grammar compiles"), compiled grammars are cached 24 h from
  last use; the cache is invalidated when the schema structure or the tool set changes
  (name/description changes do not). No number for that latency is published: UNVERIFIED. For a
  voice agent, either pre-warm (`warm()`) or keep `strict` off by default and validate arguments
  yourself (as with OpenAI). Strict mode is not needed for correctness if the caller validates
.
- `tool_choice` (doc): `{"type":"auto"}` (default when tools are given; Claude may answer in text
  or call tools), `{"type":"any"}` (must call some tool), `{"type":"tool","name":"..."}` (forced),
  `{"type":"none"}`. Each of auto/any/tool takes `disable_parallel_tool_use` (bool, default false,
  **inside the `tool_choice` object, not top level**): with `auto`, at most one tool per response;
  with `any`/`tool`, exactly one. **`any`/`tool` return 400 on Opus 5.5, Sonnet 5.5, Fable 5.1,
  Mythos 5.1** ("tool_choice: type "tool" and "any" are not supported for this model."); `auto`
  and `none` are fine; Haiku 5.5 supports forced use. Manual extended thinking (`enabled`) also
  forbids forced tool use. Changing `tool_choice` or `disable_parallel_tool_use` invalidates the
  message cache (tools and system stay cached).
- Parallel tool use (doc): by default Claude "may call multiple tools in a single response": several
  `tool_use` blocks in one assistant turn with `stop_reason: "tool_use"`; "The API doesn't prescribe
  an execution order". Brisvox invariants (SideEffect tools never twice, never speculative) mean
  the caller decides; the adapter just reports all calls.
- Model text before tool calls (doc): "Claude often comments on what it's doing ... before calling
  tools" (a `text` block, then `tool_use`): in a voice agent this preamble is spoken before the tool
  runs. Lower effort => "Proceed directly to action without preamble".

## Returning tool results
Source: https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls .
Assistant tool turn (doc) and the user reply (doc):
```json
{
  "id": "msg_01Aq9w938a90dw8q",
  "model": "claude-opus-5-5",
  "stop_reason": "tool_use",
  "role": "assistant",
  "content": [
    {"type": "text", "text": "I'll check the current weather in San Francisco for you."},
    {"type": "tool_use", "id": "toolu_01A09q90qw90lq917835lq9", "name": "get_weather", "input": { "location": "San Francisco, CA", "unit": "celsius" }}
  ]
}
```
```json
{
  "role": "user",
  "content": [
    {"type": "tool_result", "tool_use_id": "toolu_01A09q90qw90lq917835lq9", "content": "15 degrees"}
  ]
}
```
- `tool_result` fields: `tool_use_id` (the `tool_use.id`), `content` (optional: a string, a list of
  `text`/`image`/`document`/`search_result` blocks), `is_error` (optional bool). Error example (doc):
  `{"type":"tool_result","tool_use_id":"toolu_01A09q90qw90lq917835lq9","content":"ConnectionError:
  the weather service API is not available (HTTP 500)","is_error":true}`.
- Rules (doc): "Tool result blocks must immediately follow their corresponding tool use blocks in
  the message history" (no message between the assistant tool_use message and the user tool_result
  message); "the tool_result blocks must come FIRST in the content array. Any text must come AFTER
  all tool results" (text before a tool_result is a 400). Error text to expect when violated: "tool_use
  ids were found without tool_result blocks immediately after". Parallel calls: "return one
  `tool_result` for each `tool_use` block, all together in the next user message"; for a call you
  chose not to run still return `{"type":"tool_result","tool_use_id":"toolu_02","is_error":true,
  "content":"Not executed: the preceding write_file call failed."}`.
  => Brisvox dangling calls (cancelled by barge-in) must be answered with a synthetic `is_error`
  result before the next request (or the whole assistant turn dropped with its results), exactly
  as with OpenAI; the adapter should validate pairing before any I/O and raise `Protocol` naming
  the call id.
- Mapping brisvox history -> Anthropic messages: leading `System` messages -> top-level `system`
  (string or `[{"type":"text","text":...}]`); `User` -> `{"role":"user","content":<text>}`;
  `Assistant` -> `{"role":"assistant","content":[<provider_data blocks in order>, {"type":"text",
  "text":<played text>} if non-empty, tool_use blocks for `tool_calls`]}` with `input` parsed from the
  argument JSON text; a run of `Tool` messages -> ONE `{"role":"user","content":[tool_result...]}`;
  user text after tool results goes after them in the same message. A `System` message later in the
  history -> `{"role":"system","content":...}`: allowed on Haiku 5.5 and Sonnet 5.5, no beta header,
  "cannot be the first entry", "must immediately follow a `user` turn (or an assistant turn ending in
  a server tool result)" and be last or followed by an assistant turn (may follow a user message
  that carries `tool_result` blocks, but NOT between an assistant `tool_use` and its `tool_result`;
  other positions -> 400); consecutive system messages are merged; appending one "does not
  change the prefix hash and the cache still hits" but editing/removing an earlier one invalidates
  cache and (on Haiku 5.5/Sonnet 5.5) later thinking blocks. Source:
  https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages .
- Prompt-injection note from the Haiku 5.5 guide: user text typed mid-task must NOT go inside a
  `tool_result` block ("the model can then treat it as untrusted text and ignore it"); append it
  as a text block after the last `tool_result` of the same user message.

## Thinking blocks and signatures (brisvox `provider_data`)
Sources: https://platform.claude.com/docs/en/build-with-claude/thinking ,
.../build-with-claude/preserved-thinking , https://platform.claude.com/docs/en/api/errors .
- Shapes (doc):
```json
{"type": "thinking", "thinking": "Let me break this down. ...", "signature": "WaUjzkypQ2mUEVM36O2Txu...."}
{"type": "thinking", "thinking": "", "signature": "EosnCkYICxIMMb3LzNrMu..."}
{"type": "redacted_thinking", "data": "..."}
```
  The middle one is `display: "omitted"` (default on Haiku 5.5 / Sonnet 5.5). The `signature` is
  "an encrypted copy of the full reasoning", opaque ("don't interpret or parse it"), "significantly
  longer in Claude 4 and later models", valid across platforms (Claude API, Bedrock, Google Cloud).
  `redacted_thinking.data` is opaque, no readable text.
- Echo requirement: "when you return tool results, you must pass the thinking blocks from the
  assistant message back to the API, complete and unmodified." "Required: within a tool-use turn,
  pass thinking blocks back. Recommended: across turns, pass everything back. Allowed: outside tool
  use, omit prior turns' thinking." "Within the latest assistant message, the sequence of consecutive
  `thinking` blocks must match what the model generated in the original request: you can't
  rearrange, edit, or partially drop them. This includes `redacted_thinking` blocks." Filtering on
  `block.type == "thinking"` alone "silently drops `redacted_thinking` blocks and breaks the
  multi-turn protocol". Violations -> 400 `invalid_request_error`, message starts with the block
  position (e.g. `messages.1.content.0`) and contains: "`thinking` or `redacted_thinking` blocks
  in the latest assistant message cannot be modified. These blocks must remain as they were in the
  original response." Text put in the empty `thinking` field of an omitted block is ignored.
- Preservation by model: Haiku 5.5 (and Opus 4.5+, Sonnet 4.6+) "Keep all prior turns" in context
  (and bill them as input tokens when read from cache); older Haiku models keep the last turn only.
- **Haiku 5.5 / Sonnet 5.5 / Opus 5.5 / Fable 5.1 prefix binding ("preserved thinking"):** a thinking
  block "stays valid only while everything you sent before it is unchanged": top-level `system`, the
  set of `tools`, and every earlier message. Editing/reordering/deleting earlier messages, changing
  `system`, adding/removing/renaming/editing a tool, or clearing an earlier `tool_result` makes later
  thinking blocks invalid; appending messages, changing `effort`/`max_tokens`/`tool_choice`/
  `metadata`, and adding/moving `cache_control` markers do not. Result is 400 `invalid_request_error`
  (`messages.{i}.content.{j}: Invalid `signature` in `thinking` block. The block is bound to a
  different conversation. Remove the block, or set `thinking.block_binding.prefix_mismatch_behavior`
  to "drop_block".`) by default for accounts created on or after 2026-08-31 00:00 UTC; older accounts
  get the error only when they set `thinking.block_binding.prefix_mismatch_behavior`. Beta header
  `thinking-binding-controls-2026-08-01` adds `block_binding.prefix_mismatch_behavior`
  (`"error"|"drop_block"`) and an `input_transformations` array on responses. A tampered or
  undecryptable signature is always 400. Removing thinking blocks from the start, the end or all of
  them is valid (the model just loses that reasoning); removing one from the middle is not.
- Account binding: thinking blocks from Haiku 5.5 / Sonnet 5.5 "work only in the account that
  produced them, or in an account linked to it"; another account's request silently drops them (no
  error). Relevant for multi-tenant stores that replay history through different API keys: always
  replay a conversation through the key (account) that produced it.
- Model switch mid-conversation: the API silently drops blocks the target model cannot read; e.g.
  Sonnet 5.5 does not read Opus 5.5 / Opus 5 / Fable blocks, but reads Haiku 5.5's on the Claude API
  and Google Cloud, and Opus 5.5 reads Sonnet 5.5's and Haiku 5.5's.
- **Implication for the interruption invariant** ("after an interruption the LLM context contains
  ONLY what was actually played"): truncating the played text of an interrupted assistant turn edits
  content that comes AFTER that turn's thinking blocks, so those blocks stay valid by the prefix
  rule, but interleaved thinking that sits after the truncated text, or a `tool_use` that was never
  played, would be invalid or must be dropped with it. Whether a truncated assistant text block in a
  completed turn invalidates thinking produced in LATER turns of the same conversation is covered by
  the same rule (edit of an earlier message), but the requests after an interruption produce new
  thinking blocks bound to the edited prefix, which is valid. These readings of the rules are
  inferences: UNVERIFIED until a live run replays a truncated turn. The adapter's rule (review
  fix): only the text that was not played is removed from the replayed turn; thinking in
  front of it stays pending and goes out directly in front of the next kept block, so a turn whose
  preamble text was not played but whose `tool_use` survives is still echoed as `[thinking...,
  tool_use]`. Thinking in front of a call that is dropped, or with nothing kept after it, is dropped
  with it. A live run must confirm that the API accepts `[thinking, tool_use]` after the text
  between them was removed. **The simplest safe choice for
  voice is `thinking: {"type":"disabled"}` on Haiku 5.5**: no thinking blocks exist, nothing to echo,
  no binding errors, and no change in thinking mode between turns.
- Mid-turn thinking mode changes: "If you toggle thinking mid-turn ... the API doesn't error. Instead,
  it silently disables thinking for that request" and may strip blocks; toggle between turns only.
- Fable 5.1, Mythos 5.1, Opus 5.5, Sonnet 5.5 and Fable 5 may write short "progress update" text as
  `thinking` blocks before tool calls (thinking page, "Progress updates between tool calls"; Sonnet
  5.5 `between_tools` returns their summary text). The page does NOT list Haiku 5.5 among the models
  that write them. Treat any such block as a thinking block: never speak it; echo it unchanged.

## Prompt caching
Sources: https://platform.claude.com/docs/en/build-with-claude/prompt-caching (first 100,000 of
160,149 characters read; the cookbook tail was not read), .../agents-and-tools/tool-use/
tool-use-with-prompt-caching , https://platform.claude.com/docs/en/api/rate-limits .
- Two mechanisms (doc): **automatic caching** = one top-level `cache_control` field; "The system
  places the breakpoint on the last cacheable block and moves it forward as the conversation
  grows." **Explicit breakpoints** = `cache_control` on individual blocks. Up to **4** breakpoints;
  automatic uses one of the 4; with 4 explicit ones already present automatic caching returns 400.
  Only type: `{"type":"ephemeral"}`. Doc request (automatic, copied):
```json
{
  "model": "claude-opus-5-5",
  "max_tokens": 1024,
  "cache_control": {"type": "ephemeral"},
  "system": "You are an AI assistant tasked with analyzing literary works. Your goal is to provide insightful commentary on themes, characters, and writing style.",
  "messages": [
    {"role": "user", "content": "Analyze the major themes in Pride and Prejudice."}
  ]
}
```
  Explicit on a system block (doc): `"system":[{"type":"text","text":"You are a helpful assistant.",
  "cache_control":{"type":"ephemeral"}}]`. On tools: `cache_control` on the LAST tool caches the whole
  tool-definitions prefix.
- TTL: default **5 minutes** ("refreshed for no additional cost each time the cached content is
  used"; "measured from the start of the request that writes or reads the cache entry, not from the
  end of its response"); **1 hour** via `"cache_control": { "type": "ephemeral", "ttl": "1h" }` (2x
  base input price to write). Mixing: 1-hour entries must appear before 5-minute entries. For a phone
  call 5 min is enough per turn; a long-held, repeatedly reused system prompt across calls benefits
  from 1 h (UNVERIFIED cost trade off: depends on call volume).
- **Minimum cacheable length (tokens): 512 for Claude Haiku 5.5, Sonnet 5.5, Opus 5.5, Fable 5.1,
  Fable 5, Opus 5, Mythos 5/5.1; 1,024 for Sonnet 5, Sonnet 4.6/4.5, Opus 4.8/4.1/4; 2,048 for Opus
  4.7, Haiku 3.5, Mythos Preview; 4,096 for Opus 4.6/4.5 and Haiku 4.5.** Shorter prompts are processed
  uncached without an error; check that `cache_creation_input_tokens` and `cache_read_input_tokens`
  are not both 0.
- What is cached: "the entire prompt: `tools`, `system`, and `messages` (in that order), up to and
  including the block designated with `cache_control`". Cacheable: tool definitions, system blocks,
  user/assistant text, images/documents (user turns), `tool_use` and `tool_result` blocks. Not
  cacheable directly: thinking blocks (cached as part of prior assistant turns; they count as input
  tokens when read), empty text blocks, citations sub-content.
- Lookback: writes only at a breakpoint; on read the system checks up to **20 block positions**
  backward from the breakpoint (consecutive `tool_use` blocks count as one position, likewise
  consecutive `tool_result`). If a growing conversation moves the breakpoint 20+ blocks past the last
  write, add a second breakpoint (automatic caching moves forward by itself; the 20-block gap
  applies to explicit placement).
- Invalidation (doc table; hierarchy tools -> system -> messages): tool definitions change: all
  invalid; `tool_choice` change: messages invalid; images added/removed: messages invalid; thinking
  parameters and `output_config.effort` change: messages invalid (tools/system model-specific);
  web search / citations toggles: system + messages invalid. Setting a parameter to its default is
  equivalent to omitting it. "Hits require 100% identical prefix content".
- Concurrency: "A cache entry only becomes available after the first response begins. If you need
  cache hits for parallel requests, wait for the first response before sending subsequent requests."
- Scope: isolated per organisation and, on the Claude API, per workspace. Output: "Prompt caching
  has no effect on output token generation. The response you receive is identical to what you would
  get if prompt caching were not used."
- Usage fields (in `message_start.usage` when streaming; `message_delta.usage` repeats the
  cumulative values): `input_tokens` (after the last breakpoint), `cache_creation_input_tokens`,
  `cache_read_input_tokens`, `cache_creation` {`ephemeral_5m_input_tokens`,
  `ephemeral_1h_input_tokens`} ("`cache_creation_input_tokens` equals the sum of the values in the
  `cache_creation` object"), `output_tokens`. Example (doc):
```json
{
  "usage": {
    "input_tokens": 2048,
    "cache_read_input_tokens": 1800,
    "cache_creation_input_tokens": 248,
    "output_tokens": 503,
    "cache_creation": {"ephemeral_5m_input_tokens": 148, "ephemeral_1h_input_tokens": 100}
  }
}
```
- Pricing multipliers (doc): 5 m write 1.25x base input, 1 h write 2x, read 0.1x (0.05x on Opus 5.5 /
  Sonnet 5.5, 0.025x on Fable/Mythos 5.1); Haiku 5.5 read $0.01/MTok (<= 100K prompt). Cache hits do
  not count toward rate limits for models other than Haiku 3.5 (see rate limits).
- Cache diagnostics (request `diagnostics.previous_message_id` -> response
  `diagnostics.cache_miss_reason` with types `model_changed`, `system_changed`, `tools_changed`,
  `messages_changed`, `previous_message_not_found`, `unavailable`, each of the first four with
  `cache_missed_input_tokens`): "null means diagnosis is still pending — the response was serialized
  before the background comparison completed" (and `diagnostics` itself is `null` when none was
  requested or no divergence was detected). Whether this field arrives in a streamed
  `message_start`/`message_delta`: UNVERIFIED (the stream schema's `message_start.message` is a full
  `Message`, whose schema has the `diagnostics` field, but the `message_delta` schema lists no
  `diagnostics`; the field is likely `null` at `message_start` time). A cheap debugging aid for the
  live test.
- **Warm-up candidate:** `max_tokens: 0` "populates the prompt cache without generating a response"
  (Messages create reference); prompt-caching page: "The API reads your prompt into the model and
  writes the cache at any `cache_control` breakpoint, then returns immediately without generating any
  output." Billed as a cache write; whether it is allowed with `stream: true`, its latency and
  response shape (assume a normal non-streaming `Message` with empty `content`): UNVERIFIED. Combine
  with a HEAD/TLS warm-up for `warm()`.
- Prompt-cache friendly ordering for brisvox (callers + adapter): (1) stable persona in the leading
  system message(s) (top-level `system`); (2) stable tool list and order, `cache_control` on the last
  tool or let automatic caching cover it; (3) history append-only (interruption truncation changes
  only the tail); (4) volatile data (time, caller name, retrieved snippets) at the end of the newest
  user message or in a late `role:"system"` message, never in the top-level system prompt; (5) never
  change model, thinking mode, `effort`, `tool_choice` or tool set mid-call; (6) keep serialising
  `tools`/`system`/`messages` byte-stably (the OpenAI adapter's rendered-bytes reuse applies too).
  Rate-limit upside: cache reads do not count toward ITPM.

## Errors, status codes, rate limits
Sources: https://platform.claude.com/docs/en/api/errors ,
https://platform.claude.com/docs/en/api/rate-limits .
- Error body (doc):
```json
{
  "type": "error",
  "error": {
    "type": "not_found_error",
    "message": "The requested resource could not be found."
  },
  "request_id": "req_011CSHoEeqs5C35K2UUqR7Fy"
}
```
  `error.type` may "grow over time". The same id is in the `request-id` response header (log it).
- HTTP statuses (doc): 400 `invalid_request_error` (also other 4XX; also the spend limit you set
  yourself: "begins `You have reached your specified API usage limits`"), 401
  `authentication_error`, 402 `billing_error`, 403 `permission_error`, 404 `not_found_error`, 409
  `conflict_error`, 413 `request_too_large`, 429 `rate_limit_error`, 500 `api_error`, 504
  `timeout_error`, **529 `overloaded_error`** ("temporarily overloaded ... when the API experiences high
  traffic across all users"; "ramp up your traffic gradually" to avoid 429 acceleration limits).
  Mapping to brisvox: pre-stream non-2xx -> `Err(Rejected { status })`; mid-stream `error` event ->
  `LlmEvent::Error`: `overloaded_error` -> `Rejected 529` (retryable), `rate_limit_error` -> 429,
  `api_error` -> 500, `invalid_request_error` -> 400, other -> `Protocol` naming the sanitised type
  (never copy `message`).
- 429 variants: rate limit hit (has `retry-after`); **tier spend cap** (monthly cap Start $500 / Build
  $1,000 / Scale $200,000): `rate_limit_error` with `error.details.error_code:
  "enforced_spend_limit_reached"` and NO `retry-after` header, fails until 00:00 UTC on the 1st of the
  next month, retries are pointless; acceleration limits on sharp usage increases.
- Retry (doc): the official SDKs retry transient failures (connection errors, rate limits, 5xx)
  "twice by default, honoring the `retry-after` header". The brisvox adapter does not retry (turn
  logic owns it) but should read and log `retry-after` and `request-id`.
- Rate limits: RPM, ITPM (input tokens/min), OTPM (output tokens/min) per model class, org level,
  token-bucket (continuously replenished, can trigger on shorter bursts than the minute); limits
  are separate per model (some older models share a combined bucket). Cache-aware ITPM:
  `input_tokens` and `cache_creation_input_tokens` count, `cache_read_input_tokens` does NOT (except
  Haiku 3.5). ITPM is estimated at request start and adjusted; OTPM is counted as tokens are
  produced, `max_tokens` does not count. Standard limits (RPM / ITPM / OTPM), Haiku 5.5 (same numbers
  for Sonnet 5.5 and Opus 5.5): Start 1,000 / 2,000,000 / 400,000; Build 5,000 / 5,000,000 /
  1,000,000; Scale 10,000 / 10,000,000 / 2,000,000; Custom tier by contact. New organisations "may
  start in the Evaluation tier, with limits below the standard limits". Concurrency limits separate
  from RPM: none stated (UNVERIFIED). Hundreds of concurrent calls: each turn is one request, so
  Build tier 5,000 RPM is not the bottleneck; ITPM is, unless caching works.
- Rate-limit response headers (doc): `retry-after` (seconds), `anthropic-ratelimit-requests-limit`,
  `-requests-remaining`, `-requests-reset` (RFC 3339), `anthropic-ratelimit-tokens-limit|remaining|reset`
  (most restrictive limit), `anthropic-ratelimit-input-tokens-limit|remaining|reset`,
  `anthropic-ratelimit-output-tokens-limit|remaining|reset` (remaining values "rounded to the nearest
  thousand"), `anthropic-priority-*` (Priority Tier only). Other response headers: `request-id`,
  `anthropic-organization-id`, `anthropic-workspace-id`.
- Not fetched / not found: sample header values, a streamed-response header example: UNVERIFIED.

## Keepalive, timeouts, cancel
- `ping` events are the only documented keepalive ("any number of `ping` events"); no interval is
  stated: UNVERIFIED. No SSE comment lines are documented.
- No server-side idle/stream timeout is documented for streaming; the errors page advises streaming
  for long requests ("Some networks may drop idle connections after a variable period of time") and
  "setting a TCP socket keep-alive can reduce the impact of idle connection timeouts". SDKs validate
  that non-streaming requests are not expected to exceed 10 minutes and set TCP keep-alive. (A
  specific SDK threshold of `max_tokens` > 21,333 for requiring streaming is reported only by
  third-party pages, not by the official docs fetched: UNVERIFIED.) Brisvox must set its own timeouts
  (connect <= ~2 s, headers, first output, inter-event). Because tool-input fragments can pause (one
  key/value at a time), pre-text thinking is on by default, and `ping`s should not count as output,
  use the OpenAI adapter's pattern: first-output deadline separate from the idle deadline, and a
  generous idle limit while a tool block is open.
- Cancel: no cancel endpoint for a streaming Messages request is documented; dropping the HTTP
  connection (brisvox: drop `LlmStream`) is the plan. Billing of tokens generated before the drop,
  and whether partial generation stops promptly: UNVERIFIED.
- Docs' own interrupted-stream recovery (not used by brisvox): for Claude 4.6+ models, send a new
  user message "Your previous response was interrupted and ended with [previous_response]. Continue
  from where you left off." because prefill is unsupported; "Tool use and extended thinking blocks
  cannot be partially recovered."

## Turkish support
Not stated for Turkish. The multilingual page
(https://platform.claude.com/docs/en/build-with-claude/multilingual-support) says Claude is
"capable in many languages beyond those benchmarked" and "Test with any languages relevant to your
specific use cases"; its benchmark table (zero-shot CoT, MMLU translated into 14 languages, % of
English) covers only Claude Sonnet 4.5 and Haiku 4.5 and does NOT include Turkish (rows: Spanish,
Portuguese (Brazil), Italian, French, Indonesian, German, Arabic, Chinese (Simplified), Korean,
Japanese, Hindi, Bengali, Swahili, Yoruba). Nothing on the Claude 5.5 generation. So Turkish quality
and Haiku 5.5 vs Sonnet 5.5 differences: UNVERIFIED, measure with the Turkish conversation
fixtures. Doc advice: state the response language explicitly in the system prompt ("Always respond
in French, regardless of the language the user writes in." pattern), submit text in native script
(Turkish diacritics, not transliteration), and "prompt Claude to use idiomatic speech as if it were a
native speaker" for fluency. Token inflation: the 4.7+ tokenizer yields ~30% more tokens than
Haiku 4.5's for the same text (1M tokens = "roughly 555k words or 2.5M Unicode characters");
Turkish-specific token counts are UNVERIFIED (count with `POST /v1/messages/count_tokens`).

## Gotchas / things the implementer must handle
- Anthropic `usage.input_tokens` EXCLUDES cache reads and writes: sum the three fields for brisvox
  `Usage.input_tokens`; `cached_input_tokens` = `cache_read_input_tokens`.
- `stop_reason` is only in `message_delta`; usage there is cumulative; usage in `message_start` holds
  the input side. Emit `Done` after `message_stop`.
- `tool_use` blocks: `input: {}` at start is a placeholder; fragments start with an empty
  `partial_json`; the object is complete at `content_block_stop`; arguments may be partial/invalid
  with `eager_input_streaming` or at `max_tokens`; validate before dispatch.
- Tool results: `tool_result` blocks first in ONE user message, no text before them; pair every
  `tool_use` id; synthesise `is_error` results for cancelled calls.
- Thinking is on by default on Claude 5.5 models: send `thinking` explicitly; Haiku 5.5 `disabled`
  works only at effort <= `high`; Sonnet 5.5 uses `between_tools`; reasoning-like text may leak into
  speech with thinking off or `low` effort on Haiku 5.5.
- Thinking blocks (and `redacted_thinking`) must be echoed unmodified within a tool turn, and the
  conversation prefix must stay append-only (400 on accounts created on/after 2026-08-31).
- No `temperature`/`top_p`/`top_k` on 5.x; no prefill (final message must be a user turn); forced
  `tool_choice` is rejected on Sonnet 5.5 and Opus 5.5.
- `tool_choice` or `disable_parallel_tool_use` changes invalidate the message cache; changing
  thinking or `effort` invalidates it too.
- Empty `end_turn` response after tool results is possible (dead air); handle it.
- 529 `overloaded_error` can arrive as an in-stream `error` event after HTTP 200.
- `anthropic-version` must be sent; unknown event/delta/block types must be ignored.
- Tool names allow 128 chars (OpenAI 64); the schema root must be `"type":"object"`.
- Model ids and documented behaviours are from a generation newer than the model's training data.

## UNVERIFIED list
- Absolute latency numbers (TTFT, tokens/s) for Haiku 5.5 / Sonnet 5.5; effect of
  `thinking:{"type":"disabled"}`, `effort:"low"` and `display:"omitted"` on TTFT in voice turns.
- Inter-`ping` interval; whether SSE comments occur; server-side idle/stream timeouts; cost of
  dropped streams; cancel semantics; the SDK's `max_tokens` > 21,333 streaming threshold (third-party
  sources only).
- Whether content blocks can interleave; streaming shape of `redacted_thinking`; whether
  `diagnostics.cache_miss_reason` appears in streams.
- Rules for consecutive same-role messages and first-message role (pages fetched do not say).
- Strict-tool grammar-compile latency; `oneOf` support in strict schemas.
- `max_tokens: 0` with `stream: true`; its latency; its billing.
- Whether truncating the played text of an interrupted assistant turn invalidates thinking blocks
  under the prefix-binding rule (inference only); behaviour of thinking blocks when a `tool_use`
  of the turn is dropped.
- Regional hostnames, `inference_geo` values, data residency; `speed`/fast mode for Haiku.
- Whether concurrency is limited separately from RPM/ITPM/OTPM; Evaluation-tier numbers.
- Turkish quality and tokenisation for the 5.5 models.
- Default `display` for thinking (reference says `summarized`, thinking page says `omitted` on 5.5
  models); exact accepted `temperature`/`top_p` values on Sonnet 5.5 and Opus 5.5.
- Community Rust crates (none evaluated); prompt-caching page tail (characters 100,000-160,149) and
  the structured-outputs page tail (3,037 chars) were not read.
- Full sample values for rate-limit headers.

## Sources (all fetched 2026-10-08)
- https://platform.claude.com/docs/en/build-with-claude/streaming (via redirect from
  https://docs.claude.com/en/api/messages-streaming)
- https://platform.claude.com/docs/en/api/overview
- https://platform.claude.com/docs/en/api/versioning
- https://platform.claude.com/docs/en/api/errors
- https://platform.claude.com/docs/en/api/rate-limits
- https://platform.claude.com/docs/en/api/service-tiers
- https://platform.claude.com/docs/en/api/beta-headers
- https://platform.claude.com/docs/en/api/messages/create
- https://platform.claude.com/docs/en/models/overview (the `/about-claude/models/overview` URL was
  fetched and returned this page)
- https://platform.claude.com/docs/en/models/haiku-5-5/overview
- https://platform.claude.com/docs/en/models/haiku-5-5/migration-guide
- https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-haiku-5-5
- https://platform.claude.com/docs/en/build-with-claude/thinking
- https://platform.claude.com/docs/en/build-with-claude/preserved-thinking
- https://platform.claude.com/docs/en/build-with-claude/effort
- https://platform.claude.com/docs/en/build-with-claude/prompt-caching
- https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons
- https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback
- https://platform.claude.com/docs/en/build-with-claude/working-with-messages
- https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages
- https://platform.claude.com/docs/en/build-with-claude/multilingual-support
- https://platform.claude.com/docs/en/build-with-claude/structured-outputs
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/parallel-tool-use
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/strict-tool-use
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-use-with-prompt-caching
- https://platform.claude.com/docs/en/test-and-evaluate/strengthen-guardrails/reduce-latency
- https://platform.claude.com/docs/en/cli-sdks-libraries/overview
- https://platform.claude.com/docs/en/cli-sdks-libraries/libraries/openai-sdk

Fact-checked 2026-10-08: ~130 claims checked, 10 corrected. Corrections: (1) progress-update
`thinking` blocks are written by Fable 5.1, Mythos 5.1, Opus 5.5, Sonnet 5.5 and Fable 5, NOT Haiku 5.5
(was "Sonnet 5.5 and Haiku 5.5"); (2) empty-response gotcha is "exactly 2–3 tokens", quote and fix
re-worded to the page text; (3) the prompt-caching "Caching does not change output tokens" quote did
not exist, replaced by the real sentence ("Prompt caching has no effect on output token generation ...");
(4) OpenAI-compat rationale replaced by the documented limitations (no prompt caching, thinking not
returned, `strict` ignored, system messages hoisted), the "tool_result semantics" reason was not in the
docs; (5) the "SDK requires streaming when `max_tokens` > 21,333" claim is not in the official docs,
moved to UNVERIFIED; (6) strict-tool schema rules: `anyOf`/`allOf` ARE supported (with limits), plus
`const`, `$ref`, string formats (only `oneOf` remains unmentioned); (7) `top_p`: reference says ">= 0.99
accepted" vs Haiku guide "exactly 0.99" and exact Sonnet/Opus 5.5 values unverified; (8) `display`
default conflict (create reference says `summarized`, thinking page says `omitted`) flagged; (9)
`diagnostics.cache_miss_reason` streaming placement and the `null` semantics clarified; (10)
`max_tokens: 0` doc sentence added ("returns immediately without generating any output"), plus the
API-enum description of Sonnet 5.5, TTL measurement quote, and the legacy fine-grained-streaming header
note. Remaining UNVERIFIED items are listed in "UNVERIFIED list" above.

## Live verification (2026-10-13)

Real key, default model `claude-haiku-5-5`, `tests/anthropic/live.rs`: streaming, the message
cache read on turn 2 (2100 of 2152 input tokens cached), a tool call and its replay, and an
adaptive-thinking turn whose thinking blocks are echoed through the tool loop all pass. First
token 0.7-1.3 s. Adaptive thinking skips trivial turns (no blocks for "weather in Rome"), so the
live test asks a question that needs reasoning and asserts that blocks come back.
