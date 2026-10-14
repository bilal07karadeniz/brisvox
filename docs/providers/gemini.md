# Google Gemini API — LLM (streaming generateContent with function calling)

Researched 2026-10-08. Every fact below came from a page fetched that day; URLs are listed per
section and in "Sources". Pages were read through a summarising fetch tool (it paraphrases and
refuses long verbatim quotes), so JSON blocks marked "(doc)" were copied from the docs and blocks
marked "(illustrative)" were rebuilt from field lists and are NOT verbatim wire captures. Anything
not confirmed is marked UNVERIFIED. A live capture must confirm wire shapes before
they are trusted. Fact-checked a second time on 2026-10-08 (see the last line of this file).

- Docs: https://ai.google.dev/gemini-api/docs (the markdown versions `*.md.txt` of each page are
  listed in https://ai.google.dev/gemini-api/docs/llms.txt), API reference
  https://ai.google.dev/api/generate-content (fetched 2026-10-08)
- Auth: API key in the header `x-goog-api-key: $GEMINI_API_KEY` (the current REST examples in the
  function-calling, text-generation and get-started guides all use this header; the API reference
  page's examples still show the older query form `?alt=sse&key=$GEMINI_API_KEY`; the caching guide's
  create example also uses `?key=...`. Prefer the header: a key in a URL leaks into logs).
  `Content-Type: application/json`. Keys are server-side secrets.
  Sources: .../docs/generate-content/get-started.md.txt, .../docs/generate-content/function-calling.md.txt,
  https://ai.google.dev/api/generate-content.
- Endpoint(s) / regions:
  - Base `https://generativelanguage.googleapis.com/v1beta/` (the guides use `v1beta`; `v1` is not
    mentioned in the pages fetched).
  - Non-streaming: `POST /v1beta/models/{model}:generateContent`.
  - Streaming: `POST /v1beta/models/{model}:streamGenerateContent?alt=sse`
    (reference: `POST /v1beta/{model=models/*}:streamGenerateContent`; `model` = `models/{model}`).
    Without `alt=sse` the response framing is not described in the pages fetched: UNVERIFIED (we use
    `alt=sse`, which the docs show for streaming).
  - Regional / data-residency endpoints for the Gemini Developer API: UNVERIFIED (the
    "Gemini API vs Vertex" page does not specify hostnames). Availability by country is on
    https://ai.google.dev/available_regions ("The Gemini API and Google AI Studio are available in the
    following countries and territories"; Türkiye is in the list).

## Important: API surface decision (generateContent vs Interactions)

Google now documents TWO surfaces on the same host:
1. `generateContent` / `streamGenerateContent` (stateless, resend full `contents` each turn).
2. The **Interactions API** (`POST /v1beta/interactions` per the streaming page; stateful by default
   via `previous_interaction_id`, `store=false` for stateless; typed SSE events `interaction.created`,
   `interaction.status_update`, `step.start`, `step.delta`, `step.stop`, `interaction.completed`,
   `error`, and a terminal `event: done` + `data: [DONE]` shown in the REST examples of the streaming
   page; the migrate page lists a slightly different event set, e.g. `interaction.in_progress`,
   `interaction.requires_action`, and its REST examples appeared to use a `v1beta2/interactions` path
   in the fetch: UNVERIFIED which is current, use the streaming page).
   The top-level pages `function-calling`, `thinking`, `thought-signatures`, `caching`, `streaming`
   (no `/generate-content/` in the path) now describe the Interactions API (the top-level
   `thought-signatures` page is just a "moved" stub); the generateContent versions live under
   `/gemini-api/docs/generate-content/...` (e.g. `.../generate-content/function-calling.md.txt`).

The migration page states: "While `generateContent` remains fully supported, we recommend the
Interactions API for all new development." No sunset date is given (migrate-to-interactions page).
The same page says (paraphrase, the fetcher did not return a verbatim quote) that on generateContent
streaming function calls arrive whole, with no partial arguments, whereas on Interactions function
call arguments stream incrementally as `arguments` deltas (`step.delta` with `type: "arguments_delta"`
carrying partial JSON strings, per the streaming page).

**Recommendation for the brisvox adapter: `streamGenerateContent?alt=sse`**, stateless,
full history replayed from the engine's played-text history. Reasons backed by fetched docs:
1. The stateless model matches the brisvox invariant "after an interruption the LLM context contains
   ONLY what was actually played": we build `contents` ourselves. With Interactions the default is
   server-side state (`store=true`); `store=false` exists but the stateless replay rules there (resend
   all `thought` steps exactly as received) are a different, newer shape.
2. The task text asks for `streamGenerateContent` with `alt=sse`, and it is fully supported.
3. Cost of the choice: tool-call arguments arrive whole (one chunk), not as deltas. For brisvox this
   only affects when a tool call can start (after the call's chunk arrives), not text streaming. The
   adapter emits one `LlmEvent::ToolCallDelta` per call with the full `arguments` JSON.
4. Re-evaluate Interactions (incremental arguments, typed events) as a follow-up if tool-call latency
   matters. Interactions was not researched in depth here: UNVERIFIED beyond the facts quoted above.

Do NOT use the `streamFunctionCallArguments` option (incremental `partialArgs`): the JS SDK reference
says the field is "not supported in the Gemini API" (partial results would appear in
`FunctionCall.partial_args`; it is a Vertex-oriented field; see below).

## Models (fast ones) — names from https://ai.google.dev/gemini-api/docs/models

| Model ID | Role per docs | Notes |
|---|---|---|
| `gemini-3.8-flash` | Most capable Flash model (long-running coding, agents, enterprise workflows); "latest models: 3.5 Flash-Lite or 3.8 Flash" recommended for new projects | stable; in 1,048,576 / out 65,536 tokens (model page); text out only; function calling, caching, structured output, batch/Flex/Priority supported; **not** Live API; thinking levels `low`/`medium`(default per thinking page; the model page does not state the default)/`high`; `minimal` "is not supported and returns an error" |
| `gemini-3.5-flash-lite` | "Fastest, most cost-effective 3.5 model"; recommended for new projects | stable; default thinking level `minimal`; levels minimal/low/medium/high |
| `gemini-3.7-flash`, `gemini-3.6-flash`, `gemini-3.5-flash` | previous generations (3.5 labelled legacy) | stable; 3.6/3.5: `minimal` allowed; 3.7: `minimal` errors |
| `gemini-3.1-flash-lite` | lite tier | stable; `minimal` default |
| `gemini-3-flash-preview` | Flash (preview) | the thinking page lists it as "3 Flash" (the ID `gemini-3-flash-preview` is from the models page); default thinking `high` |
| `gemini-2.5-flash`, `gemini-2.5-flash-lite` | not deprecated; access "limited to users who have used them in the past" | 2.5 Flash-Lite: thinking off by default, `thinkingBudget: 0` allowed (range 512-24576 for Flash-Lite; 2.5 Flash 0-24576, default dynamic `-1`) |
| `gemini-flash-latest` | alias, not a fixed model: points to the latest release of the Flash variation and changes when new releases ship (models page) | do not use for production voice (behaviour changes silently) |
| Shut down per the models page | `gemini-2.0-flash`, `gemini-3.1-flash-lite-preview`, `gemini-3-pro-preview` | do not use (`gemini-2.0-flash-lite` was not listed in the models page re-fetch: shutdown status UNVERIFIED, avoid anyway) |

Context window / output limits were confirmed for `gemini-3.8-flash` only (model page; the models
list page gives no token limits except 1M context for the shut-down `gemini-2.0-flash`); other
models' limits: UNVERIFIED. Prices (standard tier, paid, USD per 1M tokens; pricing page):
`gemini-3.8-flash` input $0.75 / cached $0.075 / output (incl. thinking) $3.75 through 2026-12-31,
then $1.50 / $0.15 / $7.50 from 2027-01-01, cache storage $0.50 per 1M tokens-hour (then $1.00);
`gemini-3.5-flash-lite` $0.30 / $0.03 / $2.50, storage $1.00; `gemini-3.1-flash-lite` $0.25 (text,
image, video; audio $0.50) / $0.025 (audio $0.05) / $1.50, storage $1.00. Free tier: 3.8 Flash has
free input/output/caching; the two Lite models are free for input/output but context caching is "not
available" on the free tier.

**Recommended default for brisvox voice: `gemini-3.5-flash-lite`** (documented as the fastest 3.5
model and defaulting to `minimal` thinking), with `gemini-3.8-flash` at `thinkingLevel: "low"` as the
quality option. No TTFT / tokens-per-second figures are published on the pages fetched, so "lowest
latency" is an inference: UNVERIFIED. Measure in the live test. Model IDs must be configuration, not
hard-coded. Gemini 3 guide (note: that page is marked deprecated and describes the Interactions API):
`minimal` "Minimizes latency for chat or high throughput applications", `low` "Minimizes latency and
cost" / best for "simple instruction following, chat, or high-throughput applications"; `high` means
"the model may take significantly longer to reach a first (non thinking) output token".

### Thinking (latency-critical for voice)
Source: https://ai.google.dev/gemini-api/docs/generate-content/thinking.md.txt, API reference
GenerationConfig/ThinkingConfig (https://ai.google.dev/api/generate-content.md.txt).
- Request fields: `generationConfig.thinkingConfig.thinkingLevel` (enum `THINKING_LEVEL_UNSPECIFIED`
  (default), `MINIMAL`, `LOW`, `MEDIUM`, `HIGH`; reference: "recommended for Gemini 3 or later, and
  earlier models return an error if it is set"), `thinkingBudget` (int, Gemini 2.5; accepted on
  Gemini 3 for backward compatibility), `includeThoughts` (bool; thought summaries come back as parts
  with `thought: true`).
  "An error will be returned if [thinkingConfig] is set for models that don't support thinking."
  Do not send `thinkingLevel` together with the legacy `thinkingBudget`: the Gemini 3 guide says you
  "cannot use both" in the same request and that it returns 400 (that guide is the Interactions-era
  page; the generateContent thinking page does not state it: treat as true, UNVERIFIED for
  generateContent specifically).
- Examples (doc):
```json
{"contents":[{"parts":[{"text":"Provide a list of 3 famous physicists..."}]}],"generationConfig":{"thinkingConfig":{"thinkingLevel":"low"}}}
{"generationConfig":{"thinkingConfig":{"thinkingBudget":1024}}}
```
- Per-model levels (thinking page table): 3.8/3.7 Flash: low, medium (default), high, `minimal` errors;
  3.6/3.5 Flash: minimal, low, medium (default), high; 3.5/3.1 Flash-Lite: minimal (default), low,
  medium, high; 3 Flash: all four, default high. Thinking cannot be fully turned off on the
  Gemini 3 Flash/Flash-Lite models ("`minimal` does not guarantee that thinking is off, the model may
  reason very minimally for complex tasks"). Whether `thinkingBudget: 0` works on `gemini-3.8-flash`:
  UNVERIFIED (the thinking page does not list 3.x models in the thinkingBudget table).
- `maxOutputTokens` includes thinking tokens; hitting it during reasoning gives
  `finishReason: MAX_TOKENS` with truncated/empty output while thinking tokens are still billed.
- Temperature: for Gemini 3 "strongly recommend keeping the temperature parameter at its default of
  1.0"; values below 1.0 "may lead to unexpected behavior, such as looping or degraded performance"
  (Gemini 3 guide). The function-calling guide best-practice says to use a low temperature (e.g., 0)
  for more deterministic and reliable function calls: the two recommendations conflict for Gemini 3;
  do not set temperature by default, measure in the live test. (Troubleshooting page gives the model
  parameter range for temperature as "0.0-1.0" while the API reference says `temperature` "Values
  can range from [0.0, 2.0]": conflicting, use the API reference but UNVERIFIED which is right.)
- Defaults on Gemini 3.x: thinking ON by default; raises latency and token use (troubleshooting page).

## Request shape (illustrative, from the field lists)

Reference fields of `GenerateContentRequest`: `contents[]` (required), `tools[]` (`Function` and
`codeExecution` listed), `toolConfig`, `safetySettings[]`, `systemInstruction` ("Currently text
only"), `generationConfig`, `cachedContent` (`cachedContents/{id}`), `labels`, `serviceTier`
(`unspecified`/`standard`/`flex`/`priority`), `store` (bool, overrides project-level logging).
`Content`: `{ "role": "user" | "model", "parts": [...] }` (role is optional and "must be either
'user' or 'model'"; the API has no `system`/`tool` role). `Part` holds at most one data field of
`text`, `inlineData`, `functionCall`, `functionResponse`, `fileData`, `executableCode`,
`codeExecutionResult`, `toolCall`, `toolResponse` plus metadata `thought`, `thoughtSignature`,
`partMetadata`.

```json
{
  "systemInstruction": {"parts": [{"text": "You are a phone agent. Answer in Turkish."}]},
  "contents": [
    {"role": "user", "parts": [{"text": "Hava nasil Londra'da?"}]}
  ],
  "tools": [{"functionDeclarations": [{
    "name": "get_current_temperature",
    "description": "Gets the current temperature for a given location.",
    "parameters": {"type": "object",
                   "properties": {"location": {"type": "string", "description": "City name"}},
                   "required": ["location"]}
  }]}],
  "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
  "generationConfig": {"maxOutputTokens": 512, "thinkingConfig": {"thinkingLevel": "low"}}
}
```
- System instruction key spelling: the text-generation guide's REST example uses snake_case
  `"system_instruction": {"parts":[{"text":"You are a cat. Your name is Neko."}]}`, the Apps Script
  example camelCase `systemInstruction`; the reference JSON uses camelCase. The proto3 JSON mapping
  normally accepts both: whether it accepts both here is UNVERIFIED. Use camelCase (reference).
  Same for the `thoughtSignature` / `thought_signature` spelling, see below.
- `FunctionDeclaration` fields (reference): `name`, `description` (required), `behavior`
  (`UNSPECIFIED`, `BLOCKING` default, `NON_BLOCKING`; the reference says only BidiGenerateContent
  (Live) supports it), `parameters` (OpenAPI-subset `Schema`),
  `parametersJsonSchema` ("Describes the parameters to the function in JSON Schema format.
  This field is mutually exclusive with `parameters`"), `response`, `responseJsonSchema` (mutually
  exclusive with `response`).
  Brisvox `ToolSpec.parameters_schema` is arbitrary JSON Schema, so `parametersJsonSchema` is the
  natural target; which JSON-Schema keywords it supports: UNVERIFIED (not on the pages fetched; the
  function-calling guide only says the `parameters` form uses a subset of the OpenAPI schema).
- Function name: the reference for `FunctionDeclaration.name` allows letters, digits, underscores,
  colons, dots and dashes, max length 128; `FunctionCall.name` / `FunctionResponse.name` allow letters,
  digits, underscores and dashes, max length 128 (the two rules differ). The function-calling guide
  advises "descriptive names without spaces or special characters (use underscores or camelCase)".
  A "no spaces, periods, or dashes" best-practices bullet was NOT found on the re-fetch: UNVERIFIED.
  Set accepted by the adapter: `[A-Za-z0-9_-]{1,64}`: the intersection of the three documented
  rules (a name must survive in `FunctionDeclaration.name`, `FunctionCall.name` and
  `FunctionResponse.name`, so no dots or colons) and exactly the alphabet and length of the
  agent's `ToolRegistry`, so no name the engine accepts fails the request. (An earlier
  `[A-Za-z0-9_]` "safe set" was a guess, not a documented limit; it rejected `get-weather`
  and with it the whole LLM turn.)
- Tool limits: guide says keep the active tool set to roughly 10-20 at most; function descriptions and
  parameters count toward input tokens; in `ANY` mode "very large or deeply nested schemas may be
  rejected" (fixes: shorten property names, reduce nesting, declare fewer functions).
- Built-in tools can be combined with function declarations on Gemini 3 (the function-calling guide
  marks this feature as Preview); the guide also notes function call parts may not be the last part of
  the parts array, so iterate all parts. Not used by brisvox.

### `toolConfig.functionCallingConfig.mode` (API reference, `FunctionCallingConfig`)
`AUTO` (model chooses; default when only function declarations are given), `ANY` (must call a
function), `NONE` (no function calls; same as sending no declarations), `VALIDATED` ("Like AUTO, but
function calls are validated with constrained decoding"; guide: default when built-in tools or
structured output are enabled). `allowedFunctionNames[]` limits the callable set, "Set this only when
the mode is ANY or VALIDATED" (reference; the JS SDK page says only `ANY`: minor conflict).
Brisvox `LlmRequest` has no tool_choice field, so send nothing (AUTO).

## Streaming response (SSE)

Sources: https://ai.google.dev/gemini-api/docs/generate-content/text-generation.md.txt,
https://ai.google.dev/api/generate-content, migrate-to-interactions page.
- Request: `POST ...:streamGenerateContent?alt=sse` with `x-goog-api-key`; the guide's curl uses
  `--no-buffer`.
- The text-generation guide does not describe SSE framing (`data:` prefix) or stream end; it says the
  streaming body "contains a stream of `GenerateContentResponse` instances" and that each streamed
  response contains a `responseId` that ties the full response together.
- Each SSE event's `data:` line is one complete `GenerateContentResponse` JSON (illustrative shape
  from the docs' description, not a verbatim capture): `candidates[]` (with `content.parts[]`,
  `content.role: "model"`, `index`), `usageMetadata`, `modelVersion`, `responseId`. That `responseId`
  is "identical across all chunks": UNVERIFIED (not found on the re-fetch; the guide only says it ties
  the response together). Full response fields: `candidates[]`, `promptFeedback`, `usageMetadata`,
  `modelVersion`, `responseId`, `modelStatus` (`modelStage`, `retirementTime`, `message`).
```json
data: {"candidates":[{"content":{"role":"model","parts":[{"text":"Merhaba, "}]},"index":0}],"usageMetadata":{"promptTokenCount":12},"modelVersion":"gemini-3.8-flash","responseId":"<id>"}
```
- Termination: the generateContent pages fetched do NOT describe an end-of-stream sentinel. The
  `[DONE]` sentinel and `event:` names are documented only for the Interactions API. So the
  adapter must treat the end as: a candidate chunk with `finishReason` set, then the HTTP body
  closing; it must tolerate a `[DONE]` data line and SSE comments. Whether chunks carry an `event:`
  line: UNVERIFIED (assume no). The thought-signatures guide says to read the stream until the
  `finish_reason` is returned.
- `finishReason` is "optional and output-only. It stays empty while the model is still generating";
  `finishMessage` is populated only when `finishReason` is set. Whether the last chunk can arrive
  with empty `parts` (e.g. only `finishReason` and `usageMetadata`): likely, UNVERIFIED.
- Text deltas: `candidates[0].content.parts[]` entries with `text` (and no `thought: true`) are
  answer text; parts with `thought: true` are thought summaries (only when `includeThoughts`), which
  brisvox must NOT speak. A text part may be an empty string that only carries a `thoughtSignature`
  (thought-signatures page: when streaming without a function call the signature may arrive in a text
  part with empty content): skip empty text.
- Several parts per chunk are possible (a chunk may contain a text part and a `functionCall` part):
  iterate all parts of every chunk, do not assume one.
- `candidateCount` defaults to 1 (reference): use only `candidates[0]`.
- Mid-stream error shape: UNVERIFIED (pre-stream errors are HTTP errors with the standard `error`
  body, below; whether a failure after the first chunk is an in-band `{"error":{...}}` event or a
  connection abort is not documented in the pages fetched).
- Grounding note (not used): when streaming, `groundingMetadata.groundingChunks[]` holds only chunks
  not in previous responses (UNVERIFIED on the re-check, irrelevant to brisvox).

### `finishReason` values (API reference `Candidate`, doc)
`FINISH_REASON_UNSPECIFIED` (unused), `STOP` ("natural stop point or a provided stop sequence"),
`MAX_TOKENS`, `SAFETY`, `RECITATION`, `LANGUAGE` (unsupported language), `OTHER`, `BLOCKLIST`,
`PROHIBITED_CONTENT`, `SPII`, `MALFORMED_FUNCTION_CALL` ("the model's function call was invalid"),
`IMAGE_SAFETY`, `IMAGE_PROHIBITED_CONTENT`, `IMAGE_OTHER`, `NO_IMAGE`, `IMAGE_RECITATION`,
`UNEXPECTED_TOOL_CALL` (a tool call was generated but no tools were enabled), `TOO_MANY_TOOL_CALLS`
("too many consecutive tool calls"), `MISSING_THOUGHT_SIGNATURE` ("at least one thought signature was
missing from the request"), `MALFORMED_RESPONSE`, `ESCALATION`, `PUP_LIMITED_DISABLED`.
- There is NO `FUNCTION_CALL`/`TOOL_CALLS` value: a response that calls a function ends with `STOP`
  as far as the list shows (UNVERIFIED in practice). The adapter must derive
  `FinishReason::ToolCalls` from "any `functionCall` part was seen", `Length` from `MAX_TOKENS`, else
  `Stop`. The other values (safety, recitation, language, malformed call, missing signature) have no
  brisvox variant: map to `Stop` + log, or `Error(Protocol)` for `MALFORMED_FUNCTION_CALL` /
  `MISSING_THOUGHT_SIGNATURE` / `UNEXPECTED_TOOL_CALL` (gap: extend the enum or decide in the adapter).
- `PromptFeedback.blockReason` (prompt blocked, "no candidates are returned"): `BLOCK_REASON_UNSPECIFIED`,
  `SAFETY`, `OTHER`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `IMAGE_SAFETY`. A chunk can therefore have
  `promptFeedback` and no `candidates`.
- Troubleshooting page: `RECITATION` can be reduced by a more unique prompt and a higher temperature;
  "prompts in unsupported languages can produce unexpected or blocked responses".

### `usageMetadata` (reference, partly truncated by the fetcher)
Fields: `promptTokenCount` (docs: when `cachedContent` is set, this is still the "total effective
prompt size", i.e. it includes the cached tokens), `cachedContentTokenCount` ("Number of tokens in
the cached part of the prompt (the cached content)"), `candidatesTokenCount` ("Total number of tokens
across all the generated response candidates"), `toolUsePromptTokenCount`, `thoughtsTokenCount`
("Number of tokens of thoughts for thinking models"), `totalTokenCount` ("prompt + thoughts +
response candidates"), `promptTokensDetails[]`, `cacheTokensDetails[]`, `candidatesTokensDetails[]`,
`toolUsePromptTokensDetails[]`, `serviceTier`.
- Mapping to brisvox `Usage`: `input_tokens = promptTokenCount`, `cached_input_tokens =
  cachedContentTokenCount`, `output_tokens = candidatesTokenCount + thoughtsTokenCount` (thinking is
  billed as output: pricing page "Output (incl. thinking)"; `candidatesTokenCount` excludes thoughts
  per `totalTokenCount` = prompt + thoughts + candidates). Adapter must default each missing field to 0.
- Whether `usageMetadata` is cumulative per chunk (last chunk holds the totals) or only on the last
  chunk: UNVERIFIED (the docs' streaming example shows a truncated `promptTokenCount` on a text
  chunk). Take the usage from the LAST chunk that has it, never sum across chunks.

## Function calling

Sources: https://ai.google.dev/gemini-api/docs/generate-content/function-calling.md.txt,
https://ai.google.dev/api/generate-content.md.txt (Part, FunctionCall, FunctionResponse).

### Call in the response
A `functionCall` part: fields `id` ("Optional. Unique identifier of the function call. If populated,
the client to execute the `functionCall` and return the response with the matching `id`"), `name`,
`args` (a JSON object, i.e. already parsed, NOT a JSON string; contrast OpenAI). Gemini 3 "now always
returns a unique `id` with every `functionCall`"; earlier models may omit it (then key the result by
`name` and position). Illustrative shape:
```json
{"role":"model","parts":[{"functionCall":{"id":"<id>","name":"get_current_temperature","args":{"location":"London"}},"thoughtSignature":"<sig>"}]}
```
- **Arguments do NOT stream incrementally on generateContent.** migrate-to-interactions (paraphrase):
  function calls arrive whole, no partial arguments. The Vertex-style `partialArgs`/`willContinue`
  mechanism is gated by `FunctionCallingConfig.streamFunctionCallArguments`, which the JS SDK
  reference says is not supported in the Gemini API. The adapter emits one `ToolCallDelta {index, id,
  name, arguments: <args serialised to JSON text>}` per `functionCall` part. `index` = dense counter
  in arrival order; `id` = `functionCall.id`, or a synthesised id (e.g. `call_<n>`) when absent so the
  brisvox `Message::tool_result(call_id, ..)` pairing still works (replay must then omit `id` on the
  part).
- Parallel calls: the model "supports calling multiple functions in a single turn"; results can be
  returned in any order because the API maps each one by `id` (guide), BUT a later example in the same
  guide says to "include the responses in the same order as they were requested": the guide is
  inconsistent, so return results in the order the calls were emitted. Whether parallel calls can span
  two chunks: UNVERIFIED (assume yes).
- `MALFORMED_FUNCTION_CALL` finish reason: the guide says required structured text (e.g. XML) before a
  tool call can trigger "Malformed_Function_Call" errors (workarounds: have notes sent via a dedicated
  `update()` function, use Markdown headers, or require no text before tool calls). Treat as a failed
  turn (no call executed).

### Result back (stateless replay)
Append the model content exactly as received (all parts, with signatures), then a new turn with
`role: "user"` holding one `functionResponse` part per call (guide: functionResponse has `name`,
`response`, `id`; role `user`; "Always include the exact `id`" is from the thought-signature/editing
guidance). `response` is "Required. The function response in JSON object format" (a JSON OBJECT:
wrap plain strings, e.g. `{"result":"..."}`; the guide's examples nest output under `result`, and the
reference says an `error` key can report a failed execution). Optional `willContinue` (applies only to
NON_BLOCKING calls, otherwise ignored) and `scheduling` (default `WHEN_IDLE`; other values `SILENT`,
`INTERRUPT`) belong to NON_BLOCKING functions, not used. Multimodal results nest `parts[].inlineData`
(not used). Fragment from the guide (doc, REST multimodal example, trimmed):
```json
"functionResponse": {"name": "get_image", "id": "UNIQUE_CALL_ID_HERE", "response": {"image_ref": {"$ref": "instrument.jpg"}}, "parts": [ {"inlineData": {"displayName": "...", "mimeType": "...", "data": "..."}} ]}
```
Illustrative full turn:
```json
{"role":"user","parts":[{"functionResponse":{"id":"<id>","name":"get_current_temperature","response":{"temperature_c":18}}}]}
```
- Ordering rule (thought-signatures page, parallel calls): the expected order is `FC1 (+signature),
  FC2, FR1, FR2`; interleaving `FC1, FR1, FC2, FR2` returns a 400. So all functionResponse parts of
  one model turn go into ONE user content (or consecutive after the full model content).
  Whether to put all `functionResponse` parts in one `user` Content or several: UNVERIFIED, use one.
- Brisvox `Message` mapping: `Role::Tool` messages (consecutive) -> ONE `user` content with one
  `functionResponse` part each, keyed by `tool_call_id`; name must be looked up from the matching
  assistant `tool_calls` entry (brisvox `Message` for tool results carries no name).
  Content with `role: "user"` must also not be adjacent-merged with a following user text turn
  without checking: whether two consecutive same-role contents are accepted: UNVERIFIED.
- Dangling call without result (barge-in cancelled the tool): UNVERIFIED how the API reacts
  (probably 400); the LLM stage must repair history (synthetic result such as "cancelled") as for
  OpenAI.

## Thought signatures (brisvox `provider_data`)

Source: https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures.md.txt (the older
`.../docs/thought-signatures` page just redirects to the Thinking guide ("This page has moved");
the top-level `.../docs/thinking` signatures section describes the Interactions API, not
generateContent).
- Thinking models (Gemini 3 and 2.5) "may return a `thoughtSignature` in content parts, such as
  `text` or `functionCall`". Pass it back "exactly as received when sending the conversation
  history". Official SDKs do this automatically; REST callers (brisvox) must do it by hand.
- Placement: Gemini 3 function calls: on the FIRST `functionCall` part (in parallel calls only the
  first part has it; later parallel parts have none); returning it is mandatory. Gemini 2.5 function
  calls: on the first part regardless of type, returning it is optional. Gemini 3 without a function
  call: on the LAST part (if the model generated a thought), not strictly enforced. Gemini 2.5
  without a function call: none.
- **Validation (Gemini 3):** omitting the signature of the first `functionCall` in a step of the
  current turn = HTTP 400 validation error. It applies to the CURRENT turn only ("Only current turn is
  required; we don't validate on previous turns"): the current turn starts at the most recent user
  message with standard content such as text (not a `functionResponse`), and every model
  `functionCall` step after it is checked. Signatures on non-function-call parts are recommended but
  not validated (omitting them does not error, quality may drop). Also applies at `minimal` thinking
  on Gemini 3 Flash. A request missing one can also surface as `finishReason:
  MISSING_THOUGHT_SIGNATURE` (reference).
- Editing history manually: keep each signature in its original `Part`; always include the exact
  `id`; do not merge a signed part with an unsigned one; do not combine two parts that both have
  signatures. (The merge/combine rules were not re-confirmed on the 2026-10-08 re-check: UNVERIFIED
  wording, keep as a safe rule.)
- Injected/synthetic function-call blocks (no real signature): strongly discouraged, but the
  documented escape hatch is the dummy signature value `"context_engineering_is_the_way_to_go"` or
  `"skip_thought_signature_validator"` (either skips validation; the page names transferring history
  from another model / client-executed calls / imported traces as the use cases). Use this for the
  brisvox fallback of replaying a call without provider data (e.g. the history was reconstructed from
  played text).
- Key spelling in REST JSON: model responses use `thoughtSignature`; the page's request-history
  examples show `thought_signature` in places (the OpenAI-compatible format nests it under
  `extra_content.google.thought_signature`). The reference Part JSON field is `thoughtSignature`
  (opaque base64 string). Send `thoughtSignature` (proto3 JSON parsers normally accept both:
  UNVERIFIED).
- Streaming: the signature can arrive in a part with empty text; read the stream until the
  `finish_reason` is returned. Accumulate across chunks.
- Signatures sent back add to input tokens charged (generate-content thinking page).
- Brisvox design (same pattern as the OpenAI adapter): `LlmEvent::Done.provider_data` = JSON array of
  the model turn's parts exactly as received (text parts concatenated is NOT enough; keep
  `functionCall` parts with `id`, `args`, `thoughtSignature`, and any trailing signed empty-text
  part), stored on the assistant `Message.provider_data` and replayed as the `model` content in the
  next request. After an interruption the played text must replace the model's `text` while the
  `functionCall` parts and their signatures stay; whether a signature attached to a text part that is
  truncated/removed must be dropped (the docs say text-part signatures are optional) is
  UNVERIFIED but safe.
- Switching between model versions mid-call: for the Interactions API the top-level thinking guide
  says "When switching models within a session, you should still resend the previous model's thought
  blocks" and "The backend manages compatibility". For generateContent nothing is stated: UNVERIFIED.

## Implicit and explicit context caching

Sources: https://ai.google.dev/gemini-api/docs/generate-content/caching.md.txt,
https://ai.google.dev/api/caching (CachedContent), pricing page.
- **Implicit:** "enabled by default for all Gemini 2.5 and newer models", no code change (savings are
  not guaranteed). Minimum input tokens: 4,096 for Gemini 3.8/3.7/3.6/3.5 Flash and 3.1 Pro Preview;
  2,048 for 2.5 Flash and 2.5 Pro (the minimum for `gemini-3.5-flash-lite`, `gemini-3.1-flash-lite`
  was not listed: UNVERIFIED; the pricing page says context caching is "not available" on their free
  tier). Hits are visible in `usageMetadata.cachedContentTokenCount` (the guide says "usage_metadata
  field"; the exact field name is from the API reference). Tips: put large, common content at the
  beginning of the prompt and send requests with a similar prefix within a short time. Discount for
  implicit hits: pricing page lists "context caching" prices (3.8 Flash cached input is 10% of input
  price); whether implicit hits are billed at that rate: UNVERIFIED. TTL / eviction of implicit
  caches: UNVERIFIED.
- **Explicit (Beta, `v1beta`):** `POST https://generativelanguage.googleapis.com/v1beta/cachedContents`
  (the guide's example appends `?key=...`; use the header) with a `CachedContent` body: `model`
  (required, immutable, `models/{model}`), `contents`, `systemInstruction` (text only), `tools`,
  `toolConfig` (all input-only, immutable), `displayName` (<= 128 Unicode chars), and `ttl` (duration
  in seconds ending in `s`, e.g. `"300s"`) OR `expireTime` (RFC 3339 UTC; one of the two). Default TTL
  "1 hour"; "no minimum or maximum bounds on the TTL". Output: `name` (`cachedContents/{id}`),
  `createTime`, `updateTime`, `expireTime`, `usageMetadata.totalTokenCount`. Only `ttl`/`expireTime`
  can be updated (`PATCH .../v1beta/{cachedContent.name=cachedContents/*}` with an optional
  `updateMask` query parameter); delete with `DELETE .../v1beta/<CACHE_NAME>`.
  Use: set `"cachedContent": "<CACHE_NAME>"` in the generate request; usable only with the model it
  was created for; "The model doesn't make any distinction between cached tokens and regular input
  tokens. Cached content is a prefix to the prompt." (this exact quote was not re-confirmed:
  UNVERIFIED wording). No special rate limits (standard GenerateContent limits apply); token limits
  include cached tokens. Minimum tokens for explicit caching "varies by model" (no table). Storage is
  billed per token-hour (prices above). Whether a request may ALSO send `systemInstruction`/`tools`
  next to `cachedContent`: UNVERIFIED (the page says nothing; the cache can hold them).
  Create example (doc):
```json
{
  "model": "models/gemini-3.8-flash",
  "contents": [{"parts": [{"inline_data": {"mime_type": "text/plain", "data": "<base64-encoded file>"}}], "role": "user"}],
  "systemInstruction": {"parts": [{"text": "You are an expert at analyzing transcripts."}]},
  "ttl": "300s"
}
```
  Use example (doc): `{"contents":[{"parts":[{"text":"Please summarize this transcript"}],"role":"user"}],"cachedContent":"<CACHE_NAME>"}`.
- Brisvox ordering for implicit-cache hits (same logic as OpenAI): stable system instruction and tool
  list first, append-only history, volatile content (time, caller name) at the END of the newest user
  turn, never change model or thinking level mid-call. Gemini has no `prompt_cache_key`; routing
  is by prefix only (UNVERIFIED that nothing else exists). Explicit caching needs the whole prefix
  created beforehand via a separate API call: only worthwhile for a large static persona/knowledge
  prefix; a candidate for `warm()`-style setup, not for v1.

## Errors, status codes, rate limits

Sources: https://ai.google.dev/gemini-api/docs/generate-content/api-errors.md.txt,
https://ai.google.dev/gemini-api/docs/rate-limits.md.txt,
https://ai.google.dev/gemini-api/docs/troubleshooting.md.txt.
- HTTP statuses (status string): 400 `INVALID_ARGUMENT` (malformed body, or newer-version features on
  an older endpoint), 400 `FAILED_PRECONDITION` (free tier not available in your country and billing
  not enabled), 403 `PERMISSION_DENIED` (key lacks permission), 404 `NOT_FOUND` (a referenced
  image/audio/video file not found), 429 `RESOURCE_EXHAUSTED` (RPM/TPM/RPD or spend-based limit), 500
  `INTERNAL` (e.g. context too long), 503 `UNAVAILABLE` (overloaded; switching to another model is
  advised), 504 `DEADLINE_EXCEEDED` (prompt too large; raise client timeout), 499 `CANCELLED` (client
  closed early). A 402 `RESOURCE_EXHAUSTED` (depleted Prepay credits) must not be retried blindly.
  401 is not in the table; the page's own sample for an invalid key is HTTP 400 `INVALID_ARGUMENT`
  with `ErrorInfo.reason: "API_KEY_INVALID"`.
- Error body: JSON with an `error` object: `code` (integer, HTTP status), `message`, `status` (gRPC
  name in SCREAMING_CASE, e.g. `INVALID_ARGUMENT`), `details[]` (e.g. `ErrorInfo` with `reason`,
  `domain`, `metadata`, or `LocalizedMessage`). The page has this sample (doc, trimmed):
```json
{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com","metadata":{"service":"generativelanguage.googleapis.com"}}]}}
```
  Map: 429 -> `Rejected{429}`, 5xx -> `Rejected{5xx}`, 400/403/404 -> `Rejected` with the status;
  never copy `message` (may echo prompt text).
- Retries: troubleshooting page: exponential backoff with jitter and a max retry count; retry only
  transient errors "429, 408, or 5xx"; do not retry client errors like 400, 402, 403. The Python SDK
  "automatically retries transient errors up to four times" (initial delay about 1 s, max 60 s).
  `Retry-After` / rate-limit headers: NOT mentioned in the pages fetched: UNVERIFIED. The adapter
  never retries itself (turn logic owns retries).
- Rate limits: dimensions RPM, TPM (input tokens), RPD (resets at midnight Pacific; some models also
  have IPM or TPD); limits are per project, not per API key; tiers: Free (active project), Tier 1
  (billing linked, cap $250), Tier 2 ($100 cumulative + 3 days, cap $2,000), Tier 3 ($1,000
  cumulative + 30 days, cap $20,000 to $100,000+). Spend-based limits per rolling 10 minutes: Tier 1
  $10, Tier 2 $50, Tier 3 $200 (exceeding gives 429). Priority inference: 0.3x the standard rate
  limit. **The page lists NO concrete RPM/TPM/RPD numbers**: read them in AI Studio
  (https://aistudio.google.com/rate-limit). Concurrency limits: the only one stated is 100
  concurrent batch requests; simultaneous streaming request limits: UNVERIFIED (not mentioned). For
  hundreds of concurrent calls one project needs a high tier plus Priority/Flex decisions: open
  question for the maintainers.
- Timeouts / keep-alive: no server-side stream timeout or SSE keep-alive spec on the pages fetched:
  UNVERIFIED. Brisvox must set its own connect, first-event and inter-event timeouts. A 504 can occur
  for large prompts (raise client timeout). Cancel: no cancel call documented for a streaming
  generateContent request; dropping the HTTP body (drop `LlmStream`) is the plan; billing of tokens
  generated before the drop: UNVERIFIED.

## Turkish support
- The text-model pages fetched (models, generate-content guides, Gemini 3) give no language list:
  Turkish for `gemini-3.8-flash` / `gemini-3.5-flash-lite` text generation is UNVERIFIED from official
  docs. Turkish (`tr`) IS listed in the Live API capabilities table of 99 languages
  (https://ai.google.dev/gemini-api/docs/live-api/capabilities.md.txt) and the
  `gemini-3.8-flash-tts` model page (https://ai.google.dev/gemini-api/docs/models/gemini-3.8-flash-tts,
  "over 130 languages") lists Turkish; the troubleshooting page says "The API supports only a number of
  select languages" and warns that prompts in unsupported languages can produce unexpected or blocked
  responses; `finishReason: LANGUAGE` exists ("unsupported language"). Test Turkish prompts in the
  live test and treat `LANGUAGE` as an error. Availability by country includes Türkiye
  (available_regions page).

## Vertex AI differences (note only)
- Google's "Gemini Developer API vs Gemini Enterprise Agent Platform (Vertex AI)" page
  (https://ai.google.dev/gemini-api/docs/migrate-to-cloud.md.txt): the same `google-genai` SDK and
  the same `generateContent`/Interactions APIs; Developer API auth = API key; Vertex = Google Cloud
  service accounts, needs a project ID and a location; the page uses the same model IDs
  (`gemini-3.8-flash`) on both. The Interactions API on Vertex is Preview and supports only
  `location="global"`. Hostnames, URL paths, regional endpoints and Vertex-only features
  (streamed `partialArgs`) were NOT stated: UNVERIFIED. A Vertex adapter would differ in URL
  (`.../projects/.../locations/.../publishers/google/models/...:streamGenerateContent`: from memory,
  UNVERIFIED) and bearer-token auth: a separate piece of work, not in the adapter.
  Vertex docs: https://docs.cloud.google.com/gemini-enterprise-agent-platform/ and
  https://cloud.google.com/gemini-enterprise-agent-platform/overview (both linked from the migrate
  page; the product was renamed; not read further).

## Live API (note only)
- Stateful WebSocket (WSS) real-time voice/vision API: input raw 16-bit PCM 16 kHz little-endian,
  output raw 16-bit PCM 24 kHz; also image frames (JPEG or PNG, up to 1 FPS) and text; function
  calling supported (live-api overview lists it as an integrated tool); server VAD on by default
  (`realtimeInputConfig.automaticActivityDetection`), interruption =
  `BidiGenerateContentServerContent` with `interrupted: true`. That pending function calls are
  discarded on interruption: UNVERIFIED (not found on the re-check).
  Models listed (capabilities page): `gemini-3.8-live` (default; `thinkingLevel` must be omitted),
  `gemini-3.8-live-extended-thinking` (supports low/medium/high), legacy Gemini 3.1 Flash Live
  Preview (`gemini-3.1-flash-live-preview`; exact ID string UNVERIFIED, the page names it by title).
  `gemini-3.8-flash` does NOT support the Live API (model page). The WebSocket URL was not on the
  pages fetched: UNVERIFIED.
  Sources: https://ai.google.dev/gemini-api/docs/live-api.md.txt,
  .../live-api/capabilities.md.txt. Out of scope for the cascade pipeline (STT -> LLM -> TTS); a
  possible future speech-to-speech adapter.

## Mapping to brisvox `LlmEvent` (crates/brisvox-providers/src/llm.rs)
- `TextDelta` <- every non-empty `parts[].text` with `thought != true` in `candidates[0]`.
- `ToolCallDelta {index, id, name, arguments}` <- one per `functionCall` part, whole, in arrival order
  (`arguments` = serialised `args` object).
- `Done {finish, usage, provider_data}` <- chunk with `finishReason` set and the stream end:
  `ToolCalls` if any `functionCall` seen, `Length` for `MAX_TOKENS`, else `Stop`; other reasons per the
  table above. `provider_data` = the model turn's parts as received (signatures included).
- `Error` <- HTTP error before the stream -> `Err(Rejected{status})` from `stream()`; body ending
  before a `finishReason` -> `Error(Disconnected)`; in-band error JSON (shape UNVERIFIED) ->
  `Error`. `promptFeedback.blockReason` with no candidates -> error (prompt blocked).
- The stream ends without `[DONE]` as far as the generateContent docs say: do not wait for one.

## Guesses the implementation must make (all UNVERIFIED until a live run)
- `x-goog-api-key` header + `alt=sse` accepted together on `v1beta` for the chosen model.
- Chunk framing (`data: {json}\n\n`, no `event:` line, no `[DONE]`), `usageMetadata` placement.
- `thoughtSignature` spelling accepted in requests (camelCase), dummy signature accepted for replays
  without provider data on Gemini 3.
- `finishReason` is `STOP` on function-call turns; function calls always whole in one chunk.
- A functionCall `id` is present on Gemini 3 models only.
- Turkish quality/behaviour of the chosen model.

## Gotchas found during research
- The top-level docs pages (`function-calling`, `thinking`, `caching`, `thought-signatures`,
  `streaming`) now describe the Interactions API, not generateContent; use the `/generate-content/`
  variants or the API reference for generateContent facts. Examples for the same feature differ
  (`tool_choice`, `function_result` steps, `thinking_level`, `signature` deltas belong to
  Interactions). The `gemini-3.md.txt` guide is likewise Interactions-oriented and marked deprecated.
- Gemini 3.8/3.7 Flash reject `thinkingLevel: "minimal"` with an error; the cheapest valid level is
  `low`. `gemini-3.5-flash-lite` defaults to `minimal`.
- `maxOutputTokens` counts thinking tokens: with thinking on, a small cap can yield an empty answer
  with `MAX_TOKENS`.
- `promptTokenCount` includes cached tokens; `thoughtsTokenCount` is separate from
  `candidatesTokenCount` (add both for output).
- Do not speak `thought: true` parts; skip empty-text parts that carry only a signature.
- Do not merge a signed part with an unsigned one when rebuilding history.
- Free tier: the Lite models have no context caching.
- The function-call arguments are a JSON object (`args`), results must also be a JSON object.

## Implementation: `brisvox_providers::gemini`

Feature `provider-gemini`; `GeminiLlm::new(GeminiConfig, SharedClock)` implements `LlmProvider`.
Code: `crates/brisvox-providers/src/gemini/{mod,config,request,schema,events,sse}.rs`, tests in
`crates/brisvox-providers/tests/gemini/` (contract suite, protocol tests, live test).

- **Surface:** `streamGenerateContent?alt=sse` (stateless), as recommended above. Auth header
  `x-goog-api-key` (marked sensitive; never in the URL, logs, errors or `Debug`). Default model
  `gemini-3.5-flash-lite` (config). No redirects, no proxy, h2 + http/1.1 via the shared rustls
  config; plain `http://` only for loopback unless `allow_insecure_http`.
- **Request:** leading system messages -> `systemInstruction` (joined with a blank line); a late
  system message -> a user text part `System note: ...` (no system role exists); `user`/`model`
  roles; tool results -> `functionResponse` parts of ONE `user` content in call order (results
  may come back in any order, the adapter sorts them: the guide says "same order as requested");
  the `response` must be an object: a JSON-object result is sent as is, anything else becomes
  `{"result": "<text>"}`. Adjacent contents of one role are merged (UNVERIFIED whether the API
  accepts two in a row; an assistant turn truncated to nothing leaves two user turns). A history
  that starts with a `model` turn (the agent's greeting) gets a placeholder `user` content
  `(The call has started.)` in front (`leading_user_placeholder`, `None` disables): whether the API
  accepts a leading model content is UNVERIFIED (some Gemini endpoints answered "First content
  should be with role user"), so the request is made valid either way; a live run decides.
  Empty user text is dropped. `generationConfig` only when something is set (`maxOutputTokens`,
  `temperature`, `thinkingConfig.thinkingLevel` XOR `thinkingBudget`; `new()` refuses both).
  `toolConfig` only with a configured `function_calling_mode` and tools in the request.
  Tool names must match `[A-Za-z0-9_-]{1,64}` (see above), duplicates are refused.
  A request with a call lacking its result, a result lacking its call, or no content at all fails
  with `Protocol` before any I/O.
- **Tool schemas** (`schema.rs`, `SchemaMode::OpenApiSubset`, default): the JSON Schema is
  converted to the OpenAPI subset of `parameters`. The fields of that subset are NOT listed on
  the fetched pages (UNVERIFIED), so only the classic core is kept: `type` (upper-cased; null
  unions become `nullable`; unions of several types become `anyOf`; a schema with no type
  information becomes `STRING`, logged as a warning), `description`, `properties`, `required` (filtered to existing
  properties), `items` (default `STRING`), `enum` (strings only), `nullable`, `anyOf`
  (`oneOf` too), `minimum`/`maximum`, `minItems`/`maxItems`, `format` for `date-time`,
  `int32`/`int64`, `float`/`double`. Rewritten: local `$ref` inlined, `allOf` merged, `const` as
  one-value enum, non-string enums moved into the description ("Allowed values: ..."). Dropped:
  `$schema`, `$id`, `$defs`, `definitions`, `title`, `default`, `examples`,
  `additionalProperties`, `pattern`, `minLength`, `maxLength`, `exclusive*`, `multipleOf`,
  `uniqueItems`, `if/then/else`, `not`, `patternProperties`, `propertyNames`, unknown keywords.
  A root object without properties is declared without `parameters`; a NESTED object without
  properties (`{"type":"object","additionalProperties":true}`) is never sent as a bare `OBJECT`
  (Gemini is believed to reject `OBJECT` with empty `properties`: UNVERIFIED on the fetched
  pages, and one such node would fail every tool of the request with a 400): it becomes `STRING`
  with "Pass a JSON object encoded as a string." appended to its description. The model then
  sends a JSON string, and the adapter turns it back into an object in the forwarded
  `ToolCallDelta.arguments` (`RenderedTools::restore_args`, `schema::restore_objects`: walks the
  converted schema along `properties`, `items` and `anyOf`, replaces only a string at a degraded
  node that parses as a JSON OBJECT, leaves everything else alone; arguments over 32 KiB are
  forwarded as received, with a warning, to keep the re-parse small on the worker), so the tool's
  own schema validation (`type: object`) passes. The replay data keeps the string as the model
  sent it. Each degradation (this one, and the type-less -> `STRING` one,
  which turns e.g. a number-or-object parameter into a string) is logged once per rendering as a
  warning naming the tool; `SchemaMode::JsonSchema` avoids both. Refused with `Protocol`
  naming the tool: external `$ref`, unknown `type`, non-object root, recursion deeper than 24
  levels or more than 10 000 nodes (counted separately for the `$ref`/`allOf` inlining pass and
  for the conversion pass, so a small DAG whose `$ref`/`allOf` fan out exponentially fails
  fast instead of pinning a worker). `SchemaMode::JsonSchema` sends the schema verbatim as
  `parametersJsonSchema` (the field the reference documents; which keywords it accepts is
  UNVERIFIED). The rendered tool array is cached (the 8 most recently used tool sets, so sessions with
  different tool sets sharing one `GeminiLlm` do not evict each other) and byte-stable for
  identical `ToolSpec`s; a cache miss with more than 4 KiB of schema text, or with any `$ref`/`allOf`
  in a schema (their cost is not a function of the text size), renders on the blocking pool, not
  on a tokio worker.
- **Stream mapping** (`events.rs`): each SSE `data` is one `GenerateContentResponse`; only
  `candidates[0]` is read, all `parts` are walked. Text -> `TextDelta`; `thought: true` parts and
  empty text are never spoken; a `functionCall` -> ONE `ToolCallDelta {index, id, name,
  arguments}` with the whole `args` text as received (`{}` when absent or `null`; a non-object is
  `Protocol`). `index` = dense arrival counter; `id` = `functionCall.id`, else the made-up
  `brisvox_call_<index>`, which is recorded without an `id` and never sent back (the doc says to
  omit it). Caps: 128 calls, 1 MiB args per call, 4 MiB args total, 16 MiB replay data (text, call
  names, ids and arguments, AND `thoughtSignature` strings, plus 32 bytes per part), SSE
  event cap (config). `[DONE]` lines, SSE comments, empty data and unknown fields are tolerated.
- **Ending:** `Done` is emitted when `finishReason` AND `usageMetadata` have been seen (normally
  the same chunk), otherwise at the clean end of the body, a `[DONE]` line, a transport error
  after the `finishReason` (all content was delivered; the signature must not be lost), or
  `usage_grace` (250 ms) after the `finishReason` when the body stays open (the last chunk may
  not carry usage: UNVERIFIED; `Done.usage` is then `None` or the usage seen earlier). A body
  that ends or drops before any `finishReason` is `Disconnected`. `STOP` -> `ToolCalls` if a call was seen else `Stop`; `MAX_TOKENS` -> `Length`.
  `MALFORMED_FUNCTION_CALL`, `MISSING_THOUGHT_SIGNATURE`, `UNEXPECTED_TOOL_CALL`,
  `TOO_MANY_TOOL_CALLS`, `MALFORMED_RESPONSE`, `LANGUAGE` -> `Error(Protocol)` naming the reason
  (no call may run). `SAFETY`, `RECITATION`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII`, `OTHER`,
  image reasons: if the turn only spoke it ends as `Stop` (logged); if it produced nothing, or
  already emitted a function call, it is `Error(Rejected {400})` (a response cut for policy never
  gets its side-effect tool executed). Reasons without a mapping (`ESCALATION`,
  `PUP_LIMITED_DISABLED`, future values): `Stop` (logged) after spoken text only; with no output
  or after a call `Error(Protocol)` naming the sanitised reason (never a silent empty success).
  `promptFeedback.blockReason` without candidates -> `Rejected {400}`.
  Usage: `input = promptTokenCount` (includes cached), `cached = cachedContentTokenCount`,
  `output = candidatesTokenCount + thoughtsTokenCount`, from the last chunk that has it.
- **Errors:** every non-2xx status is `Rejected {status}` (an invalid key is 400 per the doc, not
  401); the adapter logs `Retry-After`, `error.status` and the first `ErrorInfo.reason`
  (sanitised), never the message, and never retries. An in-band `{"error": {...}}` chunk (shape
  UNVERIFIED) maps by its HTTP-like `code`, else by the gRPC `status` name
  (`RESOURCE_EXHAUSTED` 429, `UNAVAILABLE` 503, `INTERNAL` 500, `DEADLINE_EXCEEDED` 504, ...);
  anything else is `Protocol` with the sanitised status.
- **Deadlines (injected clock):** `first_byte_timeout` (connect -> headers, `Core(Timeout)` from
  `stream()`), `first_output_timeout` (headers -> first text/call/end; comments and usage-only
  chunks do not extend it), `idle_timeout` (between events that carry OUTPUT after the first
  output; comments, empty `data:` heartbeats, `{}`, usage-only and part-less chunks do not count;
  consumer back-pressure is not charged), `usage_grace` (after a `finishReason` without usage),
  `ERROR_BODY_TIMEOUT` (1 s, reading a refusal body), `DRAIN_TIMEOUT` (500 ms) with
  `DRAIN_LIMIT` (16 KiB) for the rest of the body after the finish, the `warm()` HEAD (bounded
  by `first_byte_timeout`), `consumer_stall_timeout` (30 s: a consumer that is alive but leaves the full event channel
  unread this long gets its response abandoned, so it cannot pin the socket and replay buffer;
  the stream then ends without `Done`), `max_stream_duration`. Dropping the stream hangs up at
  once (HTTP/1.1: TCP close; HTTP/2: `RST_STREAM`, connection kept). `warm()` = unauthenticated `HEAD` on the streaming URL (the pages say nothing
  about warm-up; any HTTP answer leaves a pooled connection), HTTP/2 keep-alive pings, 10 min pool.
- **Thought signatures:** `Done.provider_data` = JSON array of the response's parts (adjacent
  unsigned text coalesced, a signed text part kept apart, `functionCall` parts with `id`/`args`/
  `thoughtSignature`), `None` when there is no call and no signature. Replay: the recorded
  parts go back as the `model` content; if the played text differs from the recorded text, the
  played text replaces ALL recorded text parts (their signatures are dropped; the doc calls them
  optional) and calls and their signatures stay, so an interruption never sends the unplayed
  text back; calls the history no longer holds are dropped. Without provider data the calls are
  rebuilt from `ToolCall`, and when no part carries a signature the first call gets
  `skip_thought_signature_validator` (`GeminiConfig::fallback_signature`; `None` disables;
  acceptance by the API: UNVERIFIED until a live run). `replay_thought_signatures = false`
  ignores provider data. The request key is `thoughtSignature` (camelCase); `thought_signature`
  and `function_call` are accepted when reading.
- **Not implemented:** explicit context caching (`cachedContent`; whether `systemInstruction`/
  `tools` may accompany it is UNVERIFIED), the Interactions API, Vertex AI, the Live API,
  `includeThoughts`, safety settings, `candidateCount > 1`, multimodal parts.
- **HTTP/2:** `GeminiConfig::http2_prior_knowledge` (off by default) makes the client speak
  cleartext HTTP/2 without ALPN, for the `h2`-crate mock in `tests/gemini/h2mock.rs`. Over h2 the
  tests prove: `HEAD` warm-up and streams share one connection, 40 concurrent streams multiplex on
  one connection, dropping a stream sends `RST_STREAM` and the connection is reused. Still only
  provable against the real endpoint: ALPN negotiation with Google's servers, h2
  keep-alive pings against Google's idle timeouts, TLS-level behaviour.
- **Tests:** unit tests next to the code (schema conversion, request building, stream mapping, SSE
  parser); `tests/gemini/contract.rs` runs the shared LLM contract suite against a loopback mock;
  `tests/gemini/protocol.rs` pins the request on the wire, every message shape above, error
  mapping, timeouts, pooling, cancel, replay and secrecy; `tests/gemini/review.rs` covers
  completion without usage, idle semantics, deadlines on the injected clock (a clock running 2 s
  ahead of real time delays every timeout by 2 s; all nine of the adapter's deadlines are in that
  join: first byte, first output, idle, stream cap, usage grace, error body, consumer stall,
  drain, warm), the stalled consumer, the greeting turn, the tool cache and HTTP/2;
  `protocol.rs` also pins the post-finish drain bound (a server that keeps the body open, and
  one that keeps sending) and Turkish text (raw 2/3-byte UTF-8 delivered one byte per HTTP
  chunk; the SSE unit tests split multi-byte characters at every position; the request tests
  send Turkish history and an interrupted turn). Where the code decides to move work off the
  worker (`build_request_body`, `tools_for`), the unit tests log the thread that really ran the
  closure and assert it differs from (large) or equals (small) the caller's thread; `tests/gemini/live.rs` is the
  `BRISVOX_LIVE_TESTS=1` + `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) check, optional `GEMINI_MODEL`.
- **Follow-up:** a live run (needs a key; blocking before the Gemini adapter is relied
  on): confirm the UNVERIFIED wire shapes (chunk framing, usage placement, whole calls, signature
  spelling and fallback signature acceptance, `OBJECT` with empty properties, a leading `model`
  content, Turkish, time to first token of `gemini-3.5-flash-lite` vs `gemini-3.8-flash` at
  `low`) and DECIDE THE DEFAULT SCHEMA MODE from a real capture: the doc lists no keyword subset
  for `parameters` and names `parametersJsonSchema` as the natural target for arbitrary JSON
  Schema, while the default (`OpenApiSubset`) keeps a list taken from the implementer's memory
  and drops (the type-less and free-form-object cases with a logged warning) `additionalProperties`,
  `pattern`, `minLength`, `maxLength`, `title`. Also confirm over real TLS: h2 negotiation,
  keep-alive pings, `RST_STREAM` on drop, warm-up reuse.

## Sources (all fetched 2026-10-08)
- https://ai.google.dev/gemini-api/docs/llms.txt (page index)
- https://ai.google.dev/gemini-api/docs/models and .../models/gemini-3.8-flash
- https://ai.google.dev/gemini-api/docs/models/gemini-3.8-flash-tts (Turkish listed)
- https://ai.google.dev/api/generate-content and https://ai.google.dev/api/generate-content.md.txt
- https://ai.google.dev/api/caching (CachedContent, ToolConfig, FunctionCallingConfig)
- https://ai.google.dev/gemini-api/docs/generate-content/function-calling.md.txt
- https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures.md.txt
- https://ai.google.dev/gemini-api/docs/generate-content/thinking.md.txt
- https://ai.google.dev/gemini-api/docs/thinking.md.txt (Interactions-oriented; model switching note)
- https://ai.google.dev/gemini-api/docs/generate-content/caching.md.txt
- https://ai.google.dev/gemini-api/docs/generate-content/text-generation.md.txt
- https://ai.google.dev/gemini-api/docs/generate-content/get-started.md.txt
- https://ai.google.dev/gemini-api/docs/generate-content/api-errors.md.txt
- https://ai.google.dev/gemini-api/docs/rate-limits.md.txt
- https://ai.google.dev/gemini-api/docs/troubleshooting.md.txt
- https://ai.google.dev/gemini-api/docs/gemini-3.md.txt
- https://ai.google.dev/gemini-api/docs/pricing
- https://ai.google.dev/gemini-api/docs/migrate-to-interactions.md.txt
- https://ai.google.dev/gemini-api/docs/migrate-to-cloud.md.txt
- https://ai.google.dev/gemini-api/docs/streaming.md.txt (Interactions streaming)
- https://ai.google.dev/gemini-api/docs/live-api.md.txt and .../live-api/capabilities.md.txt
- https://ai.google.dev/available_regions
- https://googleapis.github.io/js-genai/release_docs/interfaces/types.FunctionCallingConfig.html
  (`streamFunctionCallArguments`: partial results in `partial_args`, "not supported in the Gemini API")

Fact-checked 2026-10-08: 78 claims checked, 17 corrected. Corrections: (1) `gemini-flash-latest` is
an alias for the latest Flash (models page), not UNVERIFIED; (2) 3.8 Flash description is "most
capable Flash", not "most intelligent"; (3) `gemini-2.0-flash-lite` shutdown not confirmed (only
`gemini-2.0-flash`, `gemini-3.1-flash-lite-preview`, `gemini-3-pro-preview` listed as shut down);
(4) thinking page lists "3 Flash", ID `gemini-3-flash-preview` comes from the models page; (5)
thinkingLevel + thinkingBudget 400 and temperature/latency quotes come from the Interactions-era
Gemini 3 guide and their wording was corrected; (6) `THINKING_LEVEL_UNSPECIFIED` added to the enum;
(7) function-name rules differ for FunctionDeclaration (also colons, dots) vs FunctionCall, and the
"no periods or dashes" bullet was not found; (8) parallel results order: the guide says both "any
order" and "same order as requested"; (9) `result` key convention exists in examples and the
reference mentions an `error` key (was "not confirmed"); (10) Interactions SSE event names/endpoint:
the `done`/`[DONE]` terminator comes from the streaming page, not the migrate page, and the
function-call-whole quote was a paraphrase; (11) API error page DOES contain a sample body, a
`FAILED_PRECONDITION` 400 row, and an invalid key returns 400 not 401; (12) retry guidance includes
408 and 402 is non-retryable, SDK delay 1 s to 60 s; (13) rate limits: only concurrency limit is
100 batch requests; IPM/TPD exist; (14) Live API frames are JPEG or PNG, "pending calls discarded"
unconfirmed, `live-api.md.txt` names no models (capabilities page does); (15) Turkish is listed on the
`gemini-3.8-flash-tts` model page (fetched, not just a web search); (16) `responseId` "identical across
all chunks" not confirmed; (17) model-switching thought-block note is from the Interactions thinking
page (confirmed there), generateContent unstated. Remaining UNVERIFIED items are marked inline.

## Live verification (2026-10-13)

Real key (free tier), `tests/gemini/live.rs`: plain Turkish streaming, a function call with its
thought signature kept (`provider_data`) and replayed statelessly, and the answer after the tool
result all pass (`gemini-3.1-flash-lite`; also `gemini-3.5-flash`). Error mapping confirmed live:
503 UNAVAILABLE ("high demand") and 429 RESOURCE_EXHAUSTED map to `Rejected { status }` in under
1 s. Free tier limits seen: 5 requests/minute and 20 requests/day per model
(`GenerateRequestsPerDayPerProjectPerModel-FreeTier`). Under overload Google answered a 503 only
after 15 s (curl) and sometimes sent no response headers for 30 s+: the adapter's
`first_byte_timeout` is what bounds that. First tokens on
the free tier were 2.5-7 s, not representative of paid latency.
