# OpenAI — LLM (streaming chat with tool calling)

Researched 2026-10-08, fact-checked 2026-10-08 (second pass, see last line). Every fact below came
from a page fetched that day; URLs are listed per section and in "Sources". Pages were read through
a summarising fetch tool, so JSON blocks marked "(doc)" were copied from the docs and blocks marked
"(illustrative)" were rebuilt from field lists and are NOT verbatim wire captures. Anything not
confirmed is marked UNVERIFIED. A live capture must confirm wire shapes before they
are trusted.

- Docs: https://developers.openai.com/api/docs (old `platform.openai.com/docs/...` URLs 301 to
  `developers.openai.com/api/docs/...`, re-confirmed for `guides/streaming-responses`;
  `platform.openai.com/docs/api-reference/*` returned 403 to the fetcher in the first pass, not
  re-checked) (fetched 2026-10-08)
- Auth: `Authorization: Bearer <API key>` (the overview page also allows a short-lived access token
  from workload identity federation). Optional `OpenAI-Organization: <id>`,
  `OpenAI-Project: <id>` (needed for multiple orgs or legacy user API keys). Keys are server-side
  only. Source: https://developers.openai.com/api/reference/overview
- Endpoint(s) / regions: base `https://api.openai.com/v1/` (the overview page does not state the
  base URL as text; its examples use `https://api.openai.com/v1/...`; response header
  `openai-version: 2020-10-01`).
  - Responses: `POST /v1/responses` (reference page: `POST /responses`; the websocket-mode guide
    names "one persistent connection to `/v1/responses`").
  - Chat Completions: `POST /v1/chat/completions` (confirmed: model page endpoint list shows
    `v1/chat/completions`; openai-python `api.md` lists `post /chat/completions` relative to the base
    URL).
  - Regional/data-residency hostnames: UNVERIFIED (not listed on the overview page). The claims
    "EU data residency exists for Standard/Flex/Batch" and "regional processing costs a 10%
    premium" were NOT re-verified in the fact-check pass: UNVERIFIED. Confirmed: Fast mode is not
    available with EU data residency for GPT-6 Astra, GPT-6.1 Sol, GPT-6 Sol, or GPT-6 Luna
    (fast-mode guide).

## Decision (surface recommendation)

**Use the Responses API (`POST /v1/responses`, `stream: true`, `store: false`, stateless full
history replay) for the brisvox OpenAI adapter.**

Reasons, each backed by fetched docs:
1. OpenAI: "While Chat Completions remains supported, Responses is recommended for all new
   projects." (migrate-to-responses guide).
2. Tool calling on the current models needs Responses. latest-model guide: "GPT-6 Astra and
   GPT-6.1 Sol support Chat Completions, but tool calling requires Responses." Model pages: Sol
   = Chat Completions "without tool calling"; Luna = Chat Completions function calling "only
   when `reasoning_effort` is `none`". The migrate guide says the same from GPT-5.4 on: "Starting
   with GPT-5.4, Chat Completions does not support tool calling with `reasoning_effort` values
   other than `none`." (Note: the Astra model page lists `function_calling` as a supported feature
   on `v1/chat/completions` without qualification, which conflicts with the latest-model guide;
   follow the guide and use Responses.)
3. Reasoning items (needed between tool turns) are first-class items in Responses; Chat
   Completions has no field to carry them (UNVERIFIED as absence, but no such field appears in
   the chat message param types fetched).
4. Docs claim better cache utilisation on Responses ("40% to 80% improvement" compared to Chat
   Completions in internal tests; OpenAI's own internal number, not independently verified).
5. `store: false` + replaying our own history matches the brisvox invariant "after an
   interruption the LLM context contains ONLY what was actually played": we build `input`
   ourselves from the engine's played-text history. Do NOT use `previous_response_id` or
   `conversation` (server-side history would contain the unplayed text of an interrupted answer).
   Note `instructions` is not carried over when using `previous_response_id` anyway (SDK
   docstring for `instructions`; the migrate guide says "`previous_response_id` does not carry
   over the previous response's top-level `instructions`" and advises resending stable
   `instructions` on each request).
6. Chat Completions stays a possible fallback for OpenAI-compatible vendors (Groq, vLLM, Ollama
   etc.), which is a separate adapter ("openai-compatible", Priority 2 in PROVIDERS.md) and is
   documented in the secondary section below.

Model choice for voice (names from https://developers.openai.com/api/docs/models,
.../models/gpt-6-luna, .../models/gpt-6.1-sol, .../models/gpt-6-astra and
.../guides/latest-model):
| Model ID | Role per docs | Price /1M in / cached in / out | Reasoning effort values |
|---|---|---|---|
| `gpt-6-luna` | "most efficient model for focused, high-volume tasks"; latest-model guide: "Fastest and most cost-effective" | $0.10 / $0.01 / $0.50 (cache write $0.125) | `none`, low, medium (default), high, xhigh, max |
| `gpt-6.1-sol` | "Near-Astra performance for complex work at a lower cost" | $2 / $0.10 / $10 (cache write $2.50) | low, medium (default), high, xhigh, max (no `none`/`minimal`) |
| `gpt-6-astra` | "Our most capable model for the most demanding work" | $10 / $1 / $50 (cache write $12.50) | low, medium, high, xhigh, max (default not stated); `none` returns HTTP 400 (reasoning guide: "returns HTTP 400" on Responses and Chat Completions) |
All three: 1,050,000 context (max input 922,000), 128K max output. Modalities text+image in, text
out were confirmed on the Astra page only (Luna and Sol pages not checked for modalities:
UNVERIFIED). Prompts over 272K input tokens cost 2x input/1.5x output (Luna page; assumed same for
others: UNVERIFIED). The models page lists only these three text models with full details and no
"mini"/"nano" tiers. The latest-model guide also mentions an older `gpt-6-sol` (supports `none`)
and the rate-limits page mentions "Terra" (not described anywhere fetched).
**Recommended default for brisvox voice: `gpt-6-luna`** with `reasoning.effort = "none"` (or
`"low"` if answer quality needs it), because it is the only model on the models page with `none`
(the latest-model guide says `gpt-6-sol` also supports `none`) and the cheapest. No latency numbers
(TTFT, tokens/s) are published on the pages fetched, so "lowest latency" is an inference:
UNVERIFIED. The latest-model guide calls Luna "Fastest" but does not tie that to `none`. Measure in
the live test. Model IDs must be configuration, not hard-coded.
Latency levers from docs: `service_tier: "fast"` (fast-mode guide: "up to 2.5x faster speeds" than
Standard, stated for `gpt-5.6-sol`; "Fast mode costs twice the corresponding Standard rate" for
GPT-5.6 Sol; Luna and Sol pages say Fast mode is priced at 2x; `priority` provides the same
behaviour for supported models, and the response reports `priority` for GPT-5.6 and earlier; the
SDK also lists `ultrafast`, `gpt-5.6-sol` only). Fast mode is not available with EU residency (see
above). Ramp traffic no faster than +50% per 15 min once at 1M input TPM or some requests are
downgraded to standard speed and the response shows `service_tier:"default"`
(https://developers.openai.com/api/docs/guides/fast-mode). Whether Luna supports Fast mode: Luna's
model page says "Fast mode: 2x applicable rates" and the fast-mode guide's EU sentence names Luna,
so it seems yes, but the guide has no explicit supported-model list: UNVERIFIED; streaming TTFT
effect also UNVERIFIED.
Voice/realtime models exist separately (`gpt-realtime-2.1`, `gpt-realtime-2.1-mini`,
`gpt-live-1`, `gpt-realtime-2`, `gpt-realtime-translate`, `gpt-realtime-1.5`, deprecated
`gpt-realtime-mini`; IDs confirmed on the models page); see "Realtime" below.

## Responses API streaming (primary surface)

Sources: https://developers.openai.com/api/docs/guides/streaming-responses ,
https://developers.openai.com/api/docs/guides/function-calling ,
openai-python types at https://github.com/openai/openai-python/tree/main/src/openai/types/responses
(`response_*_event.py`, `response_stream_event.py`), all fetched 2026-10-08.

### Request (fields from `response_create_params.py`)
`model`, `input` (string or item list), `instructions`, `store` (default `true`: set `false`),
`stream: true`, `stream_options.include_obfuscation` (bool, on by default; adds random padding in
an `obfuscation` field on deltas to hide payload sizes; can be disabled on trusted links to save
bandwidth), `tools`, `tool_choice`,
`parallel_tool_calls`, `reasoning`, `max_output_tokens` (includes reasoning tokens),
`prompt_cache_key`, `prompt_cache_retention` (deprecated in favour of `prompt_cache_options.ttl`;
values `in_memory`, `24h`; `24h` is the only option for gpt-5.5 and later), `prompt_cache_options`
(`mode` = `implicit` (default) | `explicit`, `ttl` = `30m` only, `prewarm` (default false; when
true sets `generate` to false so no output is produced), `comparison_response_id`; gpt-5.6 and
later), `safety_identifier` (<= 64 chars), `service_tier` (`auto` (default), `default`, `flex`,
`priority`, `fast` (request opt-in), `ultrafast` (gpt-5.6-sol only)), `truncation` (`auto` |
`disabled`, default `disabled`; `disabled` overflow -> HTTP 400), `text`, `temperature` (0..2;
remove `temperature`, `top_p`, `top_logprobs` when reasoning effort is not `none` per latest-model
guide), `include`, `context_management` (only `compaction` supported, with `compact_threshold`),
`background`, `previous_response_id` (cannot be combined with `conversation`).
Input message items (`EasyInputMessageParam`): `{"type":"message","role":"user|assistant|system|developer","content":<string|content list>}`
(`type` is optional in the SDK TypedDict); optional `phase` (`commentary` | `final_answer`,
assistant only). The text guide lists the roles `developer`, `user`, `assistant` and says
`developer` messages are prioritised ahead of `user` messages.
Content-part type strings (SDK param files): `input_text` (`{"type":"input_text","text":...}`) and
`output_text` (`{"type":"output_text","text":...,"annotations":[...]}`; `annotations` is required
in the SDK param type, `logprobs` optional).

### SSE framing
SDK decoder behaviour (openai-python `_streaming.py`): standard SSE with fields `event`, `data`,
`id`, `retry`; other fields ignored; lines starting with `:` are comments (this is where keep-alive
comments would appear; whether OpenAI sends any: UNVERIFIED); event dispatched on blank line;
multiple `data` lines joined with `\n`. A data payload that is JSON with a truthy `error` key is
treated as an error even mid-stream (SDK raises `APIError`, message from `error.message`, default
"An error occurred during streaming"). The SDK ends iteration on any event whose data starts with
`[DONE]` (both Chat and Responses use the same Stream class). For Chat Completions the stream ends
with a `data: [DONE]` sentinel. For Responses the streaming guide does not mention `[DONE]`;
termination is the terminal lifecycle event (`response.completed` / `response.incomplete` /
`response.failed`) then connection close: `[DONE]` use on Responses is UNVERIFIED, so the adapter
must tolerate both. (The streaming guide only shows `response.completed` in an example and does not
formally define terminal events; the terminal-event set is inferred from the SDK event types.)
The `type` JSON field discriminates events (SDK union `ResponseStreamEvent` has 59 members, counted
2026-10-08; `sequence_number` on every event gives total order).

### Events brisvox needs (fields from SDK type files)
| `type` | Fields | Use |
|---|---|---|
| `response.created`, `response.in_progress`, `response.queued` | `response` (Response), `sequence_number` | ignore (record `response.id` for logs) |
| `response.output_item.added` | `item`, `output_index`, `sequence_number` | start of an item (message / function_call / reasoning). For `function_call`, `item` carries `id`, `call_id`, `name`, `arguments:""` |
| `response.content_part.added` / `.done` | content part info | ignore |
| `response.output_text.delta` | `delta` (text), `item_id`, `output_index`, `content_index`, `logprobs`, `sequence_number` | -> `LlmEvent::TextDelta` |
| `response.output_text.done` | `text` (final text), `item_id`, `output_index`, `content_index`, `logprobs`, `sequence_number` (class `ResponseTextDoneEvent`) | ignore, we accumulate |
| `response.refusal.delta` | `delta`, `item_id`, `output_index`, `content_index`, `sequence_number` (`response.refusal.done` fields not checked: UNVERIFIED) | treat as text or error: policy decision |
| `response.function_call_arguments.delta` | `delta` (arguments fragment), `item_id`, `output_index`, `sequence_number` | -> `LlmEvent::ToolCallDelta` |
| `response.function_call_arguments.done` | `arguments` (full string), `item_id`, `output_index`, `sequence_number` (no `name` field) | validate JSON; the doc says it carries the full `arguments` |
| `response.output_item.done` | completed `item`, `output_index` | **for reasoning items, take `encrypted_content` from here, not from `.added`** (SDK docstring: value in `.added` may be incomplete) |
| `response.completed` | `response` (full Response incl. `usage`, `output`), `sequence_number` | terminal success |
| `response.incomplete` | `response`, `sequence_number`; `response.incomplete_details.reason` in `max_output_tokens`, `max_messages`, `content_filter`, `steered` (stopped at a safe boundary after a WebSocket `response.steer` event) | terminal; `max_output_tokens` -> `FinishReason::Length` |
| `response.failed` | `response` (`response.error` has `code` (enum, e.g. `server_error`, `rate_limit_exceeded`, `invalid_prompt`, `data_residency_mismatch`, image errors), `message`, optional `misalignment`), `sequence_number` | terminal error |
| `error` | `code` (opt), `message`, `param` (opt), `sequence_number` | -> `LlmEvent::Error` |
| `response.reasoning_summary_text.delta` etc., `response.reasoning_text.delta` | reasoning text | ignore (summaries only if opted in) |

Response `status` values: `completed`, `failed`, `in_progress`, `cancelled`, `queued`,
`incomplete`.

Examples from the function-calling guide (doc; the guide shows seven delta lines for one call with
`delta` values `{"`, `location`, `":"`, `Paris`, `,`, ` France`, `"}`; one of them is shown here):
```json
{"type":"response.output_item.added","response_id":"resp_1234xyz","output_index":0,"item":{"type":"function_call","id":"fc_1234xyz","call_id":"call_1234xyz","name":"get_weather","arguments":""}}
{"type":"response.function_call_arguments.delta","response_id":"resp_1234xyz","item_id":"fc_1234xyz","output_index":0,"delta":"Paris"}
{"type":"response.function_call_arguments.done","response_id":"resp_1234xyz","item_id":"fc_1234xyz","output_index":0,"arguments":"{\"location\":\"Paris, France\"}"}
```
Note: the guide's examples include `response_id` while the SDK event classes list `item_id`,
`output_index`, `sequence_number` only; treat unknown fields as ignorable (serde: no
`deny_unknown_fields`). Whether events carry the `event:` SSE line equal to `type`: UNVERIFIED.
Accumulation rule from the guide: its sample code appends each
`response.function_call_arguments.delta` event's `delta` to the matching call's `arguments`
string, keyed by `output_index` (the doc text says "you're aggregating chunks into an encoded
`arguments` JSON object").

### Mapping to brisvox `LlmEvent` (crates/brisvox-providers/src/llm.rs)
- `TextDelta` <- `response.output_text.delta.delta`.
- `ToolCallDelta { index, id, name, arguments }`: `index` = `output_index` (or a dense counter
  over function_call items only; output_index also counts message/reasoning items, so a dense
  counter is cleaner), `id` = `item.call_id` (NOT `item.id`; tool results are keyed by
  `call_id`), `name` = `item.name`, both emitted from `response.output_item.added`
  (with empty `arguments`) and followed by pure fragments. Parallel calls are sequential items
  in OpenAI's examples but interleaving must still be assumed possible (key by `item_id`).
- `Done { finish, usage }` <- terminal event. `FinishReason`: Responses has no `finish_reason`;
  derive `ToolCalls` if the output contains any `function_call` item, `Length` if
  `response.incomplete` with `max_output_tokens`, else `Stop`. `content_filter` has no brisvox
  variant (gap; map to `Stop` + log or extend the enum).
- `Usage`: `input_tokens`, `input_tokens_details.cached_tokens`, `output_tokens`
  (`output_tokens_details.reasoning_tokens` is included in `output_tokens`, UNVERIFIED but docs
  say reasoning tokens "are billed as output tokens"). `input_tokens_details.cache_write_tokens`
  also exists (required int in the SDK type; gpt-5.6+), plus `total_tokens`.
- Stream ending without a terminal event -> report as cut off (brisvox `LlmStream` contract).

## Tool definitions and results

### Responses (doc, function-calling guide)
```json
{
  "type": "function",
  "name": "get_weather",
  "description": "Retrieves current weather for the given location.",
  "parameters": {
    "type": "object",
    "properties": {
      "location": {"type": "string", "description": "City and country e.g. Bogotá, Colombia"},
      "units": {"type": "string", "enum": ["celsius", "fahrenheit"], "description": "Units the temperature will be returned in."}
    },
    "required": ["location", "units"],
    "additionalProperties": false
  },
  "strict": true
}
```
- Strict mode: `additionalProperties: false` on every object, every property in `required`;
  optional = add `"null"` to the type (`["string","null"]`). Non-compliant schema with
  `strict: true` is rejected (that last point was not re-confirmed in the fact-check: UNVERIFIED).
  If `strict` is omitted in Responses the API "will attempt to normalize your schema into strict
  mode when possible" and "will fall back to non-strict, best-effort function calling" otherwise;
  Chat Completions "remain non-strict by default". Brisvox `ToolSpec.parameters_schema` is
  arbitrary JSON Schema, so the adapter must either set `strict:false` or normalise: decision for
  the adapter (default `strict:false` unless the schema already complies; arguments must be validated
  regardless, docs warn they may be invalid).
- `parallel_tool_calls: false` -> "exactly zero or one tool is called". Default allows several.
  "Built-in tools cannot be included in a parallel function-call batch."
- `tool_choice`: `"auto"` (default; zero, one or multiple functions), `"required"` (one or more),
  `{"type":"function","name":"get_weather"}` (exactly one), `"none"`, or `allowed_tools`
  (restricts the callable subset WITHOUT changing the tools list; "recommended for prompt caching"
  is from the first pass and was not re-confirmed on the function-calling page: UNVERIFIED):
```json
"tool_choice": {"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"get_weather"},{"type":"function","name":"search_docs"}]}
```
- Function name constraint (SDK chat `FunctionDefinition` docstring, not stated in the
  function-calling guide): a-z A-Z 0-9 `_` `-`, max 64 chars.
- `async: true` on a function or custom tool lets the model keep reasoning/call other tools while
  the application runs the tool; the result is returned "using the original `call_id`" (latest-model
  guide, listed under GPT-6 in general, not specifically Luna; not mentioned in the
  function-calling guide). The SDK `ResponseFunctionToolCall` has an `async` field. Remaining
  semantics: UNVERIFIED; do not use in v1.

### Returning tool results (Responses)
Append the model's `function_call` item(s) (and any reasoning items returned with them) to
`input`, then a result item keyed by `call_id` (field names from the guide's JS sample and the SDK
`FunctionCallOutput` type; the guide has no standalone JSON example):
```json
{"type": "function_call_output", "call_id": "call_12345xyz", "output": "{\"temperature_c\":18}"}
```
`output` is a string ("should typically be a string ... format is up to you") or a content list
(SDK: `Union[str, ResponseFunctionCallOutputItemListParam]`, "Text, image, or file output").
Replay rule (reasoning guide): "you should pass back all reasoning items, function call items,
and function call output items, since the last `user` message." Simplest: pass all reasoning items
of the previous response back; "Our systems will smartly ignore any reasoning items that aren't
relevant to your functions."
`function_call` item replay fields (SDK `ResponseFunctionToolCall`): `type:"function_call"`,
`call_id`, `name`, `arguments` (JSON string), optional `id`, `status`, `namespace`, `caller`,
`async`.
Whether `id` must be echoed on replay with `store:false`: UNVERIFIED (OpenAI examples append
`response.output` unchanged, which includes it; the safe approach is to echo the items exactly as
received from `response.output_item.done`).

## Implementation: `brisvox_providers::openai`

Feature `provider-openai`; code in `crates/brisvox-providers/src/openai/` (`config.rs`,
`request.rs`, `sse.rs`, `events.rs`, `mod.rs`); tests in `crates/brisvox-providers/tests/openai/`.

**API surface decision (recorded):** Responses API, `POST {base}/v1/responses`, `stream: true`,
`store: false`, full history replayed every turn, no `previous_response_id` / `conversation`.
Reasons are the six listed under "Decision" above; the adapter does not implement Chat
Completions (that is the separate "openai-compatible" adapter).

### Types
`OpenAiLlm::new(OpenAiConfig, SharedClock) -> Result<OpenAiLlm>` implements `LlmProvider`.
`OpenAiConfig::new(Secret<String>)` defaults: model `gpt-6-luna` (the doc recommendation; a
config value, never hard-coded in logic), `reasoning.effort = "none"` (`ReasoningEffort`,
`None` = field omitted), `parallel_tool_calls = true` (sent only when tools are present),
`StrictTools::WhenCompliant`, `replay_reasoning = true`, `include_obfuscation = false`,
connect 2 s, response headers 10 s, first output 30 s, idle (between events) 10 s, total 5 min,
SSE event cap 4 MiB, pool idle timeout 10 min. Optional `max_output_tokens`,
`prompt_cache_key`, `service_tier`, `organization`, `project`.
`base_url` is configurable (any server that implements the Responses API, such as a local
gateway, works; Chat-Completions-only servers do not). Plain `http://` is accepted for loopback
hosts only (`localhost`, `127.0.0.0/8`, `::1`; the host is taken from the parsed URL, so
`http://127.0.0.1@evil.example` is remote); for another host `OpenAiLlm::new` returns
`Protocol` unless `allow_insecure_http` is set, because the bearer key would otherwise travel
in cleartext. Other schemes are refused.
`Debug` of the config and adapter prints the key as `[REDACTED]`; the key lives only in a
sensitive `Authorization` header value.

### Interface extension (documented because `llm.rs` changed)
The provider-neutral `Message` could not carry what a stateless Responses replay needs, so it
grew two fields, plus one on `Done`; nothing else changed:
- `Message::tool_calls: Vec<ToolCall { id, name, arguments }>` (assistant turns). Without them a
  `function_call_output` answers no `function_call` and the API rejects the request. (The
  agent's own `Message::Assistant` already has `tool_calls`; the sim `provider_messages` mapping
  in `brisvox-sim/src/reference.rs` does not copy them yet because that reference stage runs
  without tools: follow-up for the real LLM stage.)
- `Message::provider_data: Option<Arc<str>>` and `LlmEvent::Done { provider_data }`: opaque JSON
  text the caller stores on the assistant message of the turn and passes back. For OpenAI it is
  the JSON array of the response's `reasoning` and `function_call` output items exactly as
  received in `response.output_item.done` (the `encrypted_content` of a reasoning item is only
  complete there). The agent's `Delta::ProviderData` / `Message::Assistant.provider_data` are the
  intended carriers (they hold `serde_json::Value`; the providers crate keeps text so it needs
  no JSON dependency outside this feature).
- Constructors `Message::tool_result(call_id, content)` and `Message::assistant(content,
  tool_calls, provider_data)`.

### Request mapping
- Leading `System` messages -> `instructions` (joined by a blank line); a later `System`
  message (a late note) -> an input item with role `developer`.
- `User` -> `{"type":"message","role":"user","content":text}`.
- `Assistant` -> the items of `provider_data` in the order the server produced them (only if
  `replay_reasoning`): each `reasoning` item verbatim and directly in front of the item it led
  into, the played text (`{"type":"message","role":"assistant","content":<played text>}`, if not
  empty) at the position the response's message had, and the exact `function_call` item (keeps
  `id`/`status`) for every `tool_calls` entry whose `call_id` is in the data. So a response
  `rs1, fc1, rs2, fc2` is replayed `rs1, fc1, rs2, fc2`, never `rs1, rs2, fc1, fc2`. `Done`'s
  `provider_data` carries a placeholder `{"type":"message"}` where the message sat (only when
  there is also a reasoning or call item to replay; a text-only answer has no data); it is never
  sent. Data without the placeholder (older or foreign) puts the text before the reasoning run
  that leads into the first call, or after everything when there is no call. Reasoning whose
  follower is gone (the message was not played, or its call is not in `tool_calls`) is dropped
  with it; a `tool_calls` entry missing from the data is synthesised as `{"type":"function_call",
  "call_id","name","arguments"}` after the rest. With `replay_reasoning = false`, or without
  data, the turn is rebuilt from the engine's own record only: the text, then one synthesised
  `function_call` per call, no `id`, no `status` (this is the fallback if the API rejects a
  replay, so it must not depend on `fc_` ids that are no longer preceded by their reasoning).
  An assistant turn with no text and no calls (everything cut by an interruption) is dropped
  together with its reasoning: a reasoning item without a following message or call is rejected
  by the API (known behaviour, not in the fetched docs: UNVERIFIED but defensive).
- `Tool` -> `{"type":"function_call_output","call_id","output"}`. The history is checked before
  any I/O and a `Protocol` error names the call id when: a tool message has no `tool_call_id`;
  a tool result has no earlier unanswered call (the assistant turn was dropped because it was
  empty, or the result is answered twice); or a call has no result by the end of the history (a
  tool cancelled by a barge-in). OpenAI answers these with HTTP 400 ("No tool output found for
  function call ...", behaviour known but not in the fetched docs: UNVERIFIED) after a full
  round trip, which in a voice turn is dead air with an opaque `Rejected{400}`; so the LLM stage
  must repair the history first (answer the cancelled call with a synthetic result such as
  "cancelled", or drop the whole assistant turn together with its result).
- `tools`: `{"type":"function","name","description","parameters","strict"}`. `strict` is `true`
  only if the schema already complies (object root, `additionalProperties:false` and every
  property in `required` on every object, none of `oneOf/allOf/not/patternProperties/if/then/
  else/dependentSchemas/dependentRequired/propertyNames/unevaluatedProperties/unevaluatedItems/
  contains/minContains/maxContains`), and every sub-schema (property, `items`, `anyOf` branch,
  `$defs` entry) declares its shape with `type` (a string or an array of strings), `$ref` or
  `anyOf`: a typeless leaf such as `{"enum":["a","b"]}`, `{"description":"x"}` or `{}` makes the
  tool `strict:false` (the strict validator is believed to reject "schema must have a 'type'
  key", UNVERIFIED, so the conservative choice applies), else `false` (a conservative list: whether the API rejects
  `strict:true` for each of them is UNVERIFIED, and a wrong `true` would fail every request of
  the agent with HTTP 400, while a wrong `false` only loses constrained decoding); arguments must be validated by the caller either way. Names must match
  `[A-Za-z0-9_-]{1,64}`; a bad name or non-JSON schema is a `Protocol` error before any I/O.
- Always sent: `model`, `stream:true`, `store:false`, `stream_options.include_obfuscation`;
  `include:["reasoning.encrypted_content"]` while `replay_reasoning`. `tool_choice` is not sent
  (`auto`): `LlmRequest` has no field for it.

### Response mapping
| Server event | Output |
|---|---|
| `response.output_text.delta`, `response.refusal.delta` | `TextDelta` (a refusal is spoken like an answer) |
| `response.output_item.added` with a `function_call` item | `ToolCallDelta { index, id: call_id, name, arguments }` (index = dense position among the response's calls; the call is keyed by `item_id`, falling back to `output_index`) |
| `response.function_call_arguments.delta` | `ToolCallDelta { index, None, None, fragment }` |
| `response.function_call_arguments.done`, `response.output_item.done` (function_call) | only the part of `arguments` the deltas missed is forwarded; an item never announced is opened with its full arguments |
| `response.output_item.done` (reasoning / function_call / the first message) | collected into `provider_data` (the message as a placeholder, see above); at most 256 items and 16 MiB, more is `Protocol` |
| (caps on tool-call arguments) | the arguments streamed for one call are capped at 1 MiB and for all calls of a response together at 4 MiB (the adapter keeps a copy of each call's arguments to reconcile `...arguments.done`); one byte more is `Protocol` and ends the stream. The consumer's own budget (the assembler, 256 KiB) is tighter; this cap bounds the adapter on its own, whatever `base_url` it talks to |
| an SSE event with empty data (`data:` alone) | ignored (a heartbeat) |
| `response.completed` | `Done`: `ToolCalls` if any function call was seen, else `Stop`; usage with `cached_input_tokens`. The response object is read leniently: an unreadable `usage`, `output` or `incomplete_details` is skipped (logged at debug), never turned into an error after the answer was delivered |
| `response.incomplete` | `Done`: `Length` for `max_output_tokens`, else as above (`content_filter` maps to `Stop`, logged at debug: no brisvox variant) |
| `response.failed`, `error` (also the untyped `{"error":{...}}`) | `Error`: the code may be a string or a number (a number 400-599 is that status); `error` may be an object or a string (then there is no code and the result is `Protocol`); code `rate_limit_exceeded`/`slow_down`/`insufficient_quota` -> `Rejected 429`, `server_error` -> 500, `server_is_overloaded` -> 503, `invalid_prompt`/`context_length_exceeded`/... -> 400, anything else `Protocol` naming the sanitised code. The free-text message is never copied (it can echo prompt text or a key fragment) |
| body ends / `[DONE]` before a terminal event, transport error | `Error(Disconnected)` |
| invalid JSON, unknown call, event over the cap | `Error(Protocol)` |
| everything else (59-member union, reasoning summary events, content parts, ...) | ignored; an event is parsed in two steps (`type` first, then only the fields of that kind), so odd field types on an ignored or future event cannot fail the stream |

HTTP status other than 2xx -> `Err(Rejected { status })` from `stream()` (400, 401, 403, 429,
5xx, even 3xx: redirects are never followed, so the key cannot leave). `Retry-After` and the
error `type`/`code` are read (error body capped at 8 KiB and 1 s: a body that is huge, stalls or
trickles never delays the `Rejected`; tests with a stalled and with an oversized body) and logged at `warn`; the
adapter never retries, because a retry of a streaming request belongs to the turn logic.

### Timeouts, cancellation, warm-up
- All deadlines run on the injected clock: `first_byte_timeout` covers connect, TLS, the request
  and the response headers (`Core(Timeout)` from `stream()`); `first_output_timeout` runs from
  the headers to the first text or tool-call fragment (or the terminal event) and is not
  extended by SSE comments or lifecycle events (`response.created`, ...): a proxy that
  keep-alives while the model is stuck is cut off there, not after five minutes of dead air
  (`Core(Timeout "openai first output")` as an event); after that `idle_timeout` is the longest
  gap between SSE *events* (`"openai idle"`; comments do not count, any dispatched event does,
  and the time the consumer spends not reading is not charged to the server);
  `max_stream_duration` caps the whole response (`"openai stream"`). The HTTP client also gets
  `connect_timeout` (its own timer, belt and braces). `first_output_timeout` is longer than the
  idle gap on purpose, since a reasoning model may think before it speaks; raise `idle_timeout`
  as well for efforts above `none`, where gaps between events can be long (UNVERIFIED how long).
- Dropping the `LlmStream` drops the receiver; the reader task notices at once (it selects on
  `Sender::closed()`), drops the HTTP response (HTTP/1.1: connection closed; HTTP/2: stream
  reset) and ends, also when the server is silent (the reader selects on `Sender::closed()`
  next to the socket read); dropping the `stream()` future before the headers arrive aborts the
  request the same way. Billing of tokens generated before the drop is UNVERIFIED (see above).
- After a terminal event the reader drops its sender (so the stream ends) and drains the
  remaining body for at most 16 KiB and 500 ms *in total*, so an HTTP/1.1 connection returns to
  the pool and a server that trickles bytes cannot keep the task alive.
- Request building: `stream()` parses every assistant message's `provider_data` and serialises
  the whole history on each request, a cost linear in the history (replayed encrypted reasoning
  can be tens of KB per turn). Up to 32 KiB of history (messages, call arguments and
  `provider_data`; about one pass of tens of microseconds) it runs inline; above that it runs on
  tokio's blocking pool (`spawn_blocking`), never on a worker thread, so audio and VAD tasks
  are not stalled by a long conversation. A panic in it is `Core(ProcessorPanicked)`. Not done
  (follow-up): caching the parsed replay per message.
- Channel: `LLM_EVENT_CAPACITY` (64). Overflow policy: backpressure; the reader stops reading
  the socket until the consumer catches up (TCP flow control), events are never dropped, and the
  idle deadline is re-armed only after the consumer has taken an event's outputs, so a slow
  consumer is not charged to the server.
- `warm()`: unauthenticated `HEAD {base}/v1/responses` on the shared client; any HTTP status
  counts, only a transport failure is an error. The connection stays in the pool (HTTP/2:
  pinged every 20 s while idle), the next `stream()` skips DNS/TCP/TLS. The pool keeps an idle
  connection for `pool_idle_timeout` (default 10 minutes; `None` = until the server closes it),
  so a warm-up survives a long ringing period up to that bound; after a longer quiet period
  call `warm()` again (it is idempotent and cheap). No key is sent and nothing is billed. Idempotent. The pool is per
  `OpenAiLlm`: share one instance. Not done: `prompt_cache_options.prewarm` (a billed,
  model-side warm-up, GPT-5.6+; follow-up).
- TLS: the shared `net::tls_config()` clone with ALPN `h2`, `http/1.1`; no system proxy.
  The client settings are now tested over a TLS + h2 loopback server in `brisvox-providers`
  (`net/http/tls_tests.rs`): ALPN `h2`, one TLS handshake for `warm()` plus the
  first streams, HTTP/2 PING keep-alive while idle, no system proxy. **Still untested until
  a live run:** the adapter's own SSE parsing over `h2` (the mocks speak cleartext
  HTTP/1.1 or prior-knowledge h2), certificate/webpki-roots wiring against the real provider,
  the post-terminal drain and the stream reset on drop over TLS.

### Prompt-cache friendly ordering (what the adapter does and what callers must do)
Adapter: renders the `tools` array once and reuses the bytes while the `ToolSpec`s are equal
(also saves re-parsing schemas per turn); serialises `instructions` / `input` in a fixed order;
replays reasoning and calls exactly as received; never reorders or rewrites earlier items;
sends a configured `prompt_cache_key` unchanged. Test: the second request's body starts with the
first request's body up to the end of `input`, and the `tools` substring is identical.
Callers (the LLM stage): (1) stable persona in the leading system message(s), (2) stable tool
list and order, (3) history append-only (the played-text truncation of an interrupted answer
changes only the tail), (4) volatile content (time, caller name, retrieved snippets) at the end
of the newest user message, never in the system prompt, (5) do not change model or
`reasoning.effort` mid-call, (6) a stable `prompt_cache_key` per agent configuration, sharded
for more than ~15 requests/min on models before GPT-5.6. `usage.cached_input_tokens` in `Done`
shows whether it worked; the live test prints it for its last request.

### Guesses the implementation had to make (all UNVERIFIED until a live run)
- Assistant text is replayed as an easy message with string content (the docs show output
  messages as content lists with `annotations`); a reasoning item is replayed without the
  `msg_...` item it originally preceded.
- `include_obfuscation:false` and `include:["reasoning.encrypted_content"]` are accepted for the
  chosen model (both are in the SDK's create params).
- `parallel_tool_calls` is accepted whenever tools are sent; omitted when there are none.
- Whether response headers arrive before the first event; `first_byte_timeout` is therefore
  generous (10 s) while `idle_timeout` is the voice-relevant bound.
- Dense call indexes (instead of `output_index`) and `call_id` as the id: as recommended above.
- An `error` event carries `code` at the top level; a nested `error.code` and the SDK's untyped
  `{"error":...}` are accepted too.
- `[DONE]` may or may not follow the terminal event; both are tolerated.
- `HEAD /v1/responses` without credentials answers with some HTTP status (even 401/404/405).
- The replayed history is accepted when a `function_call` item keeps its server `id`/`status`
  and is separated from its reasoning item only by the played text (the order the server
  produced); the `replay_reasoning = false` fallback (no `id`, no reasoning) is accepted too.
- Whether OpenAI sends SSE comments or empty-data events at all (the adapter tolerates both and
  no longer lets comments hold the idle deadline).

### Tests
Unit (in the module): SSE parser (every split position, CRLF/CR/LF, comments, cap, UTF-8),
request builder (exact JSON, replay rules, strict detection, prefix stability), event mapper
(every event type of the table, failures, defensive paths). Integration
(`tests/openai/`, loopback mock server speaking HTTP/1.1 chunked SSE): the shared provider contract suite
(`llm_contract`: streaming, cancel, reconnect, error mapping, timeout, warm, task leaks; about
8 s because the suite waits out its own 2 s quiet windows), the exact request on the wire, a
full server stream cut into 37-byte chunks, statuses/redirects, in-stream failures,
first-output/idle/total timeouts (comments and lifecycle events do not extend them),
pooled-connection reuse, cancellation (also while the server is silent and before the headers),
backpressure (600 events to a consumer that starts late, none lost, no idle timeout; and 16 MiB
scripted to a consumer that does not read at all: the server's writes block after a fraction of
it, then the consumer resumes and gets everything in order), the error-body bounds (stalled
body returns after about 1 s, oversized body returns at once), a 400 KiB history sent intact
through the blocking-pool path, the
bounded post-terminal drain, heartbeats and odd field types, numeric error codes, a malformed
terminal response, the `http://` rule, dangling tool calls/results, byte-stable prefixes, the
provider_data round trip and secrecy. Unit tests cover the replay order (interleaved
reasoning/calls, message position, fallback without the marker, `replay_reasoning=false`), the
pairing check, the strict-schema keywords and typeless sub-schemas, the argument caps, and the
inline/blocking-pool split of the request builder. The live test (`tests/openai/live.rs`) needs
`BRISVOX_LIVE_TESTS=1` and `OPENAI_API_KEY` (optionally `OPENAI_MODEL`) and is skipped
otherwise.

## Chat Completions streaming (secondary / fallback surface)

Sources: SDK type files `chat/chat_completion_stream_options_param.py`,
`chat/chat_completion_tool_message_param.py`, `chat/chat_completion_assistant_message_param.py`,
`chat/chat_completion_function_tool_param.py`,
`chat/chat_completion_message_function_tool_call_param.py`,
`shared_params/function_definition.py`; chunk reference
https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events
(all fetched 2026-10-08).

- Request: `stream: true`, `stream_options: {"include_usage": true, "include_obfuscation": <bool>}`.
  `include_usage` (SDK docstring): "If set, an additional chunk will be streamed before the `data:
  [DONE]` message. The `usage` field on this chunk shows the token usage statistics for the entire
  request, and the `choices` field will always be an empty array. All other chunks will also
  include a `usage` field, but with a null value." If the stream is interrupted "you may not
  receive the final usage chunk".
- Chunk fields: `id` (same in all chunks), `object` = `"chat.completion.chunk"`, `created` (Unix
  seconds), `model`, `choices[]`, `system_fingerprint`, `service_tier`, `obfuscation`, `usage`,
  `moderation` (optional `{input, output}`).
- `choices[]`: `index`, `delta`, `finish_reason` (`null` until the last content chunk), `logprobs`.
- `delta`: `role`, `content` (string|null), `refusal`, `tool_calls[]`, deprecated `function_call`,
  `audio`.
- `delta.tool_calls[]` fragment: `index` (required, identifies the call), `id` (first fragment),
  `type: "function"`, `function: {name, arguments}` (`arguments` is a JSON string fragment; may be
  invalid/hallucinated: validate).
- `finish_reason` values: `stop`, `length`, `tool_calls`, `content_filter`, deprecated
  `function_call`. In the audio flow the last update omits it (not re-checked: UNVERIFIED).
- `usage`: `prompt_tokens`, `completion_tokens`, `total_tokens`,
  `completion_tokens_details` (`reasoning_tokens`, `accepted_prediction_tokens`,
  `rejected_prediction_tokens`, `audio_tokens`, `text_tokens`), `prompt_tokens_details`
  (`cached_tokens`, `cache_write_tokens`, `audio_tokens`, `image_tokens`, `text_tokens`).
- Illustrative chunks (rebuilt from the field list, NOT verbatim; model id in them is a
  placeholder):
```json
{"id":"chatcmpl-456","object":"chat.completion.chunk","created":1700000000,"model":"<model>","choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null,"logprobs":null}]}
{"id":"chatcmpl-456","object":"chat.completion.chunk","created":1700000000,"model":"<model>","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":"{\"city\":"}}]},"finish_reason":null,"logprobs":null}]}
{"id":"chatcmpl-456","object":"chat.completion.chunk","created":1700000000,"model":"<model>","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls","logprobs":null}]}
{"id":"chatcmpl-456","object":"chat.completion.chunk","created":1700000000,"model":"<model>","choices":[],"usage":{"prompt_tokens":25,"completion_tokens":12,"total_tokens":37}}
```
- Tool definition (Chat): `{"type":"function","function":{"name","description","parameters","strict"}}`
  (`strict` optional bool; `parameters` omitted = empty parameter list).
- Tool result (Chat): message `{"role":"tool","tool_call_id":"<id>","content":"<string>"}`
  (`content` may also be an iterable of text parts); the preceding assistant message carries
  `tool_calls: [{"id","type":"function","function":{"name","arguments"}}]` (and `content` may be
  null/omitted when `tool_calls` is set: "Required unless `tool_calls` or `function_call` is
  specified").
- Chat `delta.tool_calls` maps 1:1 onto `LlmEvent::ToolCallDelta` (`index`, `id`, `function.name`,
  `function.arguments`), the existing brisvox design assumption; `usage` arrives in a separate
  last chunk with empty `choices` after `finish_reason`, before `data: [DONE]`.
- Limitation: on gpt-6 models tool calling with reasoning is not available (see Decision).

## Errors, status codes, rate limits, timeouts
Sources: https://developers.openai.com/api/docs/guides/error-codes ,
https://developers.openai.com/api/docs/guides/rate-limits ,
https://developers.openai.com/api/reference/overview .
- Pre-stream HTTP errors (-> `Error::Rejected { status }` from `stream()`): 400 (error-codes page:
  invalid `service_tier`, type `invalid_request_error`, `error.param:"service_tier"`; context
  overflow with `truncation:"disabled"` is from the SDK docstring), 401 (invalid authentication /
  incorrect key, not in an org, IP not allowlisted), 403 (country/region/territory not supported),
  429, 500, 503.
- 429 variants: rate limit reached (temporary), `slow_down` (`rate_limit_error`; rate rose too
  quickly, can happen within RPM/TPM), and non-retryable `credit_balance_exhausted`,
  `organization_spend_limit_exceeded`, `project_spend_limit_exceeded`,
  `organization_usage_limit_exceeded` (`error.type` may be `insufficient_quota`; check
  `error.code`): "Retrying billing, spend, or quota errors won't restore API access". 503
  `server_is_overloaded` (type `service_unavailable_error`): retry honouring `Retry-After`. 500:
  retry after a short wait.
- Error JSON fields referenced: `error.type` (e.g. `rate_limit_error`, `service_unavailable_error`),
  `error.code`, `error.param`, `error.message` (full example body not on the page; the standard
  `{"error":{...}}` envelope is confirmed only by the SDK stream decoder, which reads `error`
  and `error.message`).
- Retry: honour `Retry-After` (seconds, only sometimes present) as a minimum plus a small random
  delay, else exponential backoff with jitter and capped retries (attempts and total time).
  Failed requests still count against the per-minute limit. Official SDKs auto-retry (SDK default
  `max_retries` 2; the exact set of retried statuses is not stated in the docs fetched: UNVERIFIED).
- Rate-limit headers (names + sample values from the guide): `x-ratelimit-limit-requests` (60),
  `x-ratelimit-limit-tokens` (150000), `x-ratelimit-remaining-requests` (59),
  `x-ratelimit-remaining-tokens` (149984), `x-ratelimit-reset-requests` (1s),
  `x-ratelimit-reset-tokens` (6m0s), project variants `x-ratelimit-limit-project-tokens` (60000),
  `x-ratelimit-remaining-project-tokens` (57000), `x-ratelimit-reset-project-tokens` (3s). Other
  headers: `x-request-id`, `openai-processing-ms`, `openai-organization`, `openai-version`
  (`2020-10-01`). Client-set `X-Client-Request-Id` (ASCII only, <= 512 chars, unique per request;
  not added automatically) recommended for tracing (400 if invalid). Total request headers must stay
  < 64 KiB (custom header values <= 60 KiB total).
- Limits (RPM / TPM, per org+project, per model group; Standard): Build (>= $5 credit)
  Astra/Sol/Terra 5,000 / 1M, Luna 5,000 / 2M; Launch (>= $100) 10,000 / 4M (Luna 10,000 / 10M);
  Grow (>= $500) 15,000 / 40M (Luna 30,000 / 180M). Whether cached input tokens count toward TPM,
  and whether concurrency is limited separately: UNVERIFIED (the rate-limits page does not say;
  it lists RPM/TPM only). Luna's RPM bounds new calls/min.
- Timeouts: no server-side stream timeout/keepalive spec found: UNVERIFIED. SDK defaults:
  total timeout 600 s, connect timeout 5 s, 1000 max connections, 100 keep-alive
  (openai-python `_constants.py`). Brisvox must set its own connect (<= ~2 s), first-event and
  inter-event timeouts.
- Cancel semantics: no cancel call documented for a streaming Responses request other than
  closing the connection (WebSocket guide examples only `ws.close()`); dropping the HTTP body
  (brisvox: drop `LlmStream`) is the plan. Billing of tokens generated before the drop:
  UNVERIFIED. `response.status:"cancelled"` exists (relevant to `background` mode).

## Prompt caching (cache-friendly ordering)
Source: https://developers.openai.com/api/docs/guides/prompt-caching (fetched 2026-10-08).
- Automatic, on by default for supported models; no code change required (not re-confirmed
  verbatim: UNVERIFIED). Minimum cacheable prompt: 1,024 tokens for GPT-5.6 and later (older
  models: "varies by request settings", UNVERIFIED which; older models report `cached_tokens`
  rounded down to a multiple of 128).
- "Cache reuse requires the entire rendered prefix to match." Any change before a breakpoint
  invalidates everything after it. Changing the model, tool definitions or ordering, structured
  output format, reasoning effort, verbosity, or compaction can change the prefix. Cached content =
  rendered instructions, developer messages, tool definitions and conversation history (the exact
  list of content kinds is from the first pass).
- Usage fields: Responses `usage.input_tokens_details.cached_tokens` (reads) and
  `.cache_write_tokens` (writes; gpt-5.6+). Chat `usage.prompt_tokens_details.cached_tokens`
  (SDK chunk field list). Map to `Usage.cached_input_tokens`.
- `prompt_cache_key`: "Keys influence routing; they do not pin requests to a machine or guarantee
  a cache hit"; on models before GPT-5.6 keep about 15 requests per minute per key ("traffic above
  15 requests per minute can lead to overflow routing"), so partition across keys. On GPT-5.6 and
  later: "The key is optional and is not needed to optimize caching on these models"; separate keys
  keep "separate cache accounting for customers or users". Caches are not shared across
  organisations (nor across regional processing boundaries).
- Retention: GPT-5.6+: `prompt_cache_options.ttl`, only value `30m` (default): prefix stays
  eligible 30 min after last write/reuse. Older: `prompt_cache_retention` `in_memory` ("around 5 to
  10 minutes of inactivity, up to one hour") or `24h` (around 30 minutes, up to 24 hours; the only
  option for gpt-5.5 and later per the SDK docstring; without ZDR the default is `24h`, with ZDR
  `in_memory`). No manual clearing (first pass, not re-confirmed).
- Breakpoints (GPT-5.6+): implicit mode (default) "places a breakpoint at the end of the latest
  eligible message" (SDK: plus up to three explicit ones); explicit mode disables the automatic
  one and allows up to four. Explicit `prompt_cache_breakpoint` (`{"mode":"explicit"}`) on content
  blocks, "up to four cache writes" per request.
- Pricing (GPT-5.6+): cache writes cost 1.25x the standard uncached input rate, reads 0.1x (0.05x
  for GPT-6.1 Sol); Luna cached input $0.01 vs $0.10 uncached. "Cache-write pricing is not an
  additive fee".
- Ordering rules for brisvox message construction (from the guide's advice + our needs):
  1. Fixed order: `tools` (stable order and schemas, never reordered per turn) -> `instructions`/
     developer message (stable persona) -> shared reference data -> conversation history
     (append-only) -> newest user turn last. Put volatile content (timestamps, caller name,
     retrieved snippets) at the END of the latest user/developer item, not in the system prompt.
     (latency-optimization guide: "put dynamic content later in the prompt to maximize prompt
     caching".)
  2. Append new messages, never rewrite earlier turns. After an interruption the played-text
     truncation of the last assistant message changes only the tail, so earlier cache stays valid;
     cache hit for the truncated turn itself is lost, accepted.
  3. Use `tool_choice: "none"` or `allowed_tools` rather than removing tools for a turn.
  4. Do not vary `reasoning.effort`/`model` mid-call if cache matters.
  5. Send a stable `prompt_cache_key` per agent configuration (not per call) and keep each key
     under ~15 req/min (pre-5.6 models); for many concurrent calls of one agent, shard keys (e.g.
     key + shard id).
  6. `prompt_cache_options.prewarm: true` prepares the cache without generating output (sets
     `generate` to false); the warm-up tokens are billed at the cache-write rate. Candidate for
     `warm()`.

## Reasoning items (brisvox `provider_data`)
Sources: https://developers.openai.com/api/docs/guides/reasoning ,
`response_reasoning_item.py` (SDK), `response_output_item_added_event.py`.
- Reasoning tokens "still occupy space in the model's context window and are billed as output
  tokens" and are not visible; items are opaque ("the API does not return their reasoning text").
- Item shape (doc, summary only shown): `{"id":"rs_6876cf02e0bc8192b74af0fb64b715ff06fa2fcced15a5ac","type":"reasoning","summary":[{"type":"summary_text","text":"..."}]}`
  (the guide's sample output has this id and `type:"reasoning"`; the `summary_text` inner shape is
  from the first pass).
  SDK fields: `id`, `type:"reasoning"`, `summary[]`, optional `content[]` (`reasoning_text`),
  optional `encrypted_content`, optional `status` (`in_progress|completed|incomplete`).
- Stateless mode (`store:false`): "reasoning items in the response's `output` array include an
  `encrypted_content` property by default"; the legacy `reasoning.encrypted_content` value in
  `include` is "still accepted ... for compatibility, but doesn't require it". Always send
  `include: ["reasoning.encrypted_content"]` anyway for compatibility with older models (its
  necessity on older models: UNVERIFIED).
- **Echo requirement**: the docs "highly recommend you pass back any reasoning items returned
  with the last function call" (along with the function output), and all reasoning/function-call/
  function-call-output items since the last `user` message. With `store:false` the guide says
  "preserve every output item, append the next user message, and replay the complete history".
  For brisvox: the adapter needs an opaque `provider_data` blob per assistant turn holding the
  raw JSON of the `reasoning` and `function_call` output items (taken from
  `response.output_item.done`) to replay in the next request. `Message` today has only
  `role/content/tool_call_id`, so the adapter needs either a `provider_data: Option<Arc<str>>` field on
  `Message` or an adapter-side side table keyed by tool call id (flag as interface change).
- With reasoning effort `none` (Luna) no reasoning items are expected: UNVERIFIED.
- `reasoning.context` (`auto|current_turn|all_turns`): `auto` uses the selected model's default;
  `all_turns` "renders available, compatible reasoning items from earlier turns into the next
  sample" and "has an effect only when the request has access to earlier response items". Whether
  GPT-5.6+ renders earlier-turn reasoning by default: UNVERIFIED. "Persisted reasoning can be
  reused only within the same model family"; "reasoning does not carry between the GPT-5.6 and
  GPT-5.5 families". Switching models mid-call may require dropping reasoning items (inferred:
  UNVERIFIED).
- After an interrupted answer: reasoning items of a turn whose text was cut should still be
  replayed only if paired with the items they belong to; whether orphaned reasoning is accepted:
  UNVERIFIED (test live).
- `reasoning.summary`: opt-in (`auto` for the most detailed summary); may need organisation
  verification (UNVERIFIED). Not needed.

## Realtime / WebSocket (note only, not part of the Responses adapter)
- Realtime API: "the best starting point for voice agents that need barge-in" (low first-audio
  latency wording from first pass); transports WebRTC in the browser or WebSocket on the server;
  `POST /v1/realtime/client_secrets` for ephemeral keys; example model `gpt-realtime-2.1`; the
  guide has no WebSocket URL and says nothing on text-only output: UNVERIFIED.
  It is a speech-to-speech product; brisvox's goal (own STT/LLM/TTS pipeline, provider choice)
  does not use it. Luna does NOT support Realtime (Luna model page). Source:
  https://developers.openai.com/api/docs/guides/realtime
- Responses WebSocket mode (`/v1/responses`, client event `response.create`; the payload "mirrors
  the normal Responses create body, except that transport-specific fields like `stream` and
  `background` are not used"): connections last up to 60 min ("Reconnect at the limit"); up to 16
  active in-flight responses per connection (across named and default lanes); up to 32 distinct
  named `stream_id` values (FIFO per lane is from the first pass, not re-checked); continuation via
  `previous_response_id` with only the new input items, compatible with `store=false`/ZDR;
  `generate:false` warms state; uncached id with `store=false` -> `previous_response_not_found`;
  claim "up to roughly 40% faster end-to-end execution" for rollouts with 20+ tool calls. No
  documented cancel; the guide links a separate "Mid-turn steering" guide (and the SDK mentions a
  `response.steer` event) for adding user input while a response runs. WebSocket URL and auth
  handshake not in the guide: UNVERIFIED.
  Candidate later optimisation (pre-warmed persistent connection avoids TLS+connect per turn),
  but it conflicts with stateless history replay; park as a follow-up.
  Source: https://developers.openai.com/api/docs/guides/websocket-mode
- OpenAI's voice-agents guide lists a chained pipeline as an option (best for "Control over each
  speech and text stage"; "Use this path when each stage needs to be visible or replaceable")
  without recommending models for it or giving latency numbers.
  Source: https://developers.openai.com/api/docs/guides/voice-agents

## Turkish support
UNVERIFIED. The text guide, latest-model guide, latency guide and models pages fetched say nothing
about languages; no Turkish statement found. Verify empirically with the Turkish conversation
fixtures (live test) and measure token inflation (Turkish agglutination usually costs more tokens:
unverified assumption).

## Gotchas / things the implementer must handle
- Documentation shows a new model generation (GPT-6 family, GPT-5.6) not in older training data:
  never hard-code model names; read from config.
- `function_call` items have both `id` (`fc_...`) and `call_id` (`call_...`); results use `call_id`.
- Reasoning `encrypted_content` is incomplete in `output_item.added`; use `output_item.done`.
- Argument fragments may be invalid JSON; validate before dispatch (also the brisvox
  `MalformedToolArguments` fault).
- Terminal events carry the whole `Response` (can be large with long history echo; do not log).
- Usage arrives only in the terminal event; if the stream is cut it is lost.
- `truncation` defaults to `disabled`: oversize context returns 400; brisvox must trim itself.
- 429 `slow_down`: ramp load gradually (1M TPM -> +50%/15 min); relevant for load tests.
- Fast mode downgrade returns `service_tier:"default"` in the response.
- `stream_options.include_obfuscation` adds padding bytes per delta (on by default): set `false`
  for lower bandwidth (the SDK docstring says it is safe on trusted links).
- Sol/Luna with Chat Completions and tools: see Decision, do not use.
- `gpt-6-astra` rejects `reasoning.effort:"none"` with HTTP 400.

## UNVERIFIED list
- Responses SSE: `event:` line values, presence of `[DONE]`, any keep-alive comments, `response_id`
  field on delta events (guide shows it, SDK types do not).
- Fields of `response.refusal.done` and content-part events.
- Whether `function_call.id` must be echoed on stateless replay; orphaned reasoning items;
  reasoning after model switch; reasoning items at `effort:"none"`.
- Whether `include:["reasoning.encrypted_content"]` is still needed on older models.
- Latency numbers (TTFT, tok/s) for Luna/Sol; Fast mode effect on Luna streaming TTFT; whether Luna
  officially supports Fast mode.
- Regional hostnames; EU data-residency tiers and the 10% regional premium; WebSocket URL/auth;
  text-only Realtime output.
- Server-side idle/stream timeouts; cost of dropped streams; cancel semantics.
- Minimum cacheable length for pre-5.6 models; whether the >272K surcharge, text+image modalities
  apply to Sol (checked on Luna / Astra pages only).
- Cached input counting toward TPM; separate concurrency limits.
- Turkish quality/tokenisation.
- Full error body example; remaining semantics of `async: true` tools and `configuration_update`
  input item (latest-model guide: changes reasoning effort mid-conversation without rewriting the
  prompt prefix).
- Exact JSON of Chat Completions chunks (above are illustrative).
- "Terra" model (named in rate limits only); whether `reasoning.context` default renders earlier
  reasoning on GPT-5.6+.

## Sources (all fetched 2026-10-08)
- https://developers.openai.com/api/docs/guides/migrate-to-responses
- https://developers.openai.com/api/docs/guides/function-calling
- https://developers.openai.com/api/docs/guides/streaming-responses
- https://developers.openai.com/api/docs/guides/prompt-caching
- https://developers.openai.com/api/docs/guides/reasoning
- https://developers.openai.com/api/docs/guides/latest-model
- https://developers.openai.com/api/docs/guides/fast-mode
- https://developers.openai.com/api/docs/guides/latency-optimization
- https://developers.openai.com/api/docs/guides/error-codes
- https://developers.openai.com/api/docs/guides/rate-limits
- https://developers.openai.com/api/docs/guides/websocket-mode
- https://developers.openai.com/api/docs/guides/realtime
- https://developers.openai.com/api/docs/guides/voice-agents
- https://developers.openai.com/api/docs/guides/text
- https://developers.openai.com/api/docs/models , .../models/gpt-6-luna , .../models/gpt-6.1-sol ,
  .../models/gpt-6-astra
- https://developers.openai.com/api/reference/overview
- https://developers.openai.com/api/reference/resources/responses/methods/create (`POST /responses`)
- https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events
- https://developers.openai.com/api/reference/resources/responses/streaming-events (only `response.created` visible; page truncated)
- openai-python (raw.githubusercontent.com/openai/openai-python/main/src/openai/): `api.md`,
  `_streaming.py`, `_constants.py`, `types/responses/{response_create_params, response, response_stream_event,
  response_text_delta_event, response_text_done_event, response_refusal_delta_event,
  response_function_call_arguments_delta_event,
  response_function_call_arguments_done_event, response_output_item_added_event,
  response_completed_event, response_incomplete_event, response_failed_event, response_error,
  response_error_event, response_usage, response_function_tool_call,
  response_function_tool_call_output_item, response_input_item_param, response_input_text_param,
  response_output_text_param, response_reasoning_item, easy_input_message_param}.py`,
  `types/chat/{chat_completion_stream_options_param,
  chat_completion_tool_message_param, chat_completion_assistant_message_param,
  chat_completion_function_tool_param, chat_completion_message_function_tool_call_param}.py`,
  `types/shared_params/function_definition.py`.

Fact-checked 2026-10-08: 112 claims checked, 19 corrected. Corrections: (1) `gpt-6-astra` cached
input was "?", now $1 (cache write $12.50), description is "Our most capable model for the most
demanding work" (dropped bogus "latestModelInfo"); (2) Luna is not the only model with `none`
(`gpt-6-sol` too per latest-model guide); (3) Fast mode "up to 2.5x faster" and "2x" price are
stated for gpt-5.6-sol / GPT-5.6 Sol, not generically; added `fast`/`ultrafast` service_tier
values; (4) Chat Completions path `/v1/chat/completions` is now confirmed (was UNVERIFIED);
(5) realtime model IDs are confirmed on the models page (were "inferred"), plus 4 more IDs;
(6) reasoning replay quote fixed to "you should pass back all reasoning items, function call items,
and function call output items, since the last `user` message" and "highly recommend you pass back
any reasoning items returned with the last function call"; (7) "The API ignores irrelevant items"
quote replaced by "Our systems will smartly ignore any reasoning items that aren't relevant to your
functions"; (8) `instructions` quote ("Not carried over...") is an SDK docstring paraphrase, the
migrate guide wording differs; (9) `response.output_text.done` fields now verified (`text`,
`content_index`, ...); (10) `response.refusal.delta` fields verified; (11) content-part type
strings `input_text`/`output_text` verified (were UNVERIFIED); (12) `prewarm` semantics now known
(no output generated, billed at cache-write rate); (13) "cached input tokens count toward TPM" and
"concurrency not limited separately" moved to UNVERIFIED (rate-limits page does not say);
(14) `async: true` is documented in the latest-model guide only, not the function-calling guide;
SDK function_call has an `async` field; (15) function name constraints come from the SDK
docstring, not the function-calling guide; (16) WebSocket payload wording ("transport-specific
fields like `stream` and `background`", not "minus"), steering mention added; (17)
`prompt_cache_retention` 24h-only for gpt-5.5+ and ZDR defaults added; the 15 req/min key advice
applies to pre-5.6 models; (18) "No mini/nano tiers" softened to "models page lists only three
text models with full details", the Astra page's unqualified Chat `function_calling` listing
conflicts with the guide (noted); (19) EU residency tiers/10% premium and "non-compliant strict
schema is rejected" and "allowed_tools recommended for caching" were not re-confirmed and are now
marked UNVERIFIED. Confirmed unchanged: 59-member event union, SDK timeouts/retries, rate-limit
tables and headers, error-code table, reasoning item fields, usage fields, prompt-caching numbers.

## Live check (2026-10-13: blocked)

The key provided reaches the API but the account has no credit: the Responses stream fails with
`credit_balance_exhausted`, which the adapter surfaces as a protocol error naming that code. The
wire checks wait for a funded key.
