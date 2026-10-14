# OpenAI-compatible Chat Completions — LLM (Groq, Together AI, Fireworks AI, vLLM, Ollama, OpenRouter)

Researched 2026-10-08. Every fact below came from a page fetched that day; URLs are given per
section and in "Sources". Pages were read through a summarising fetch tool, so JSON blocks marked
"(doc)" were copied from the docs, and blocks marked "(illustrative)" were rebuilt from field lists
and are NOT verbatim wire captures. Source-code reads (Ollama Go, vLLM Python, openai-python,
groq-python) are labelled "(source)". Anything not confirmed is marked UNVERIFIED. No live capture
was done: a live run per vendor must confirm wire shapes before they are trusted.

**Recommended (read this, skip the survey).**

| You want | Start with | Verified |
|---|---|---|
| A hosted LLM, any language, tools | OpenRouter: `preset = "openrouter"`, `api_key_env = "OPENROUTER_API_KEY"`, model `openai/gpt-6-luna` with `reasoning_effort = "none"` (recipe: `docs/PROVIDERS.md`, "OpenRouter") | Live: streaming, parallel and chained tool calls, voice-to-voice runs in English and Turkish (`docs/TOOLS.md`, `docs/LATENCY.md`). Turkish quality is the one thing to judge by listening. |
| A fully local LLM | Ollama: `preset = "ollama"`, `model = "gemma4:12b-it-qat"` (`ollama pull gemma4:12b-it-qat`), `reasoning_effort = "none"`, no key | Live: streaming, and the six tool scenarios (parallel, chain, repeated booking, invalid-argument retry, slow tool, barge-in) 18/18 on an RTX 5070 with first sentence in ~270 ms. Five local models were tried, only this one called tools reliably (table below, `docs/TOOLS.md`, "Local models through Ollama"). |
| Groq, Together, Fireworks, vLLM | The matching `preset` | Mock servers and contract tests only; no live run against them. Treat the vendor sections below as documentation, and try one call before relying on it. |

The same server speaks the OpenAI-compatible protocol for the local speech side too (a
text-to-speech server); that is a different provider, `openai_speech`
(`docs/providers/openai-speech.md`), and the recommended local speech stack is in the first lines
of `docs/providers/local-speech.md`.

- Docs: see "Sources" at the end (fetched 2026-10-08).
- Auth: `Authorization: Bearer <key>` for all hosted vendors. Local servers: Ollama local needs no
  header (client libraries want a dummy key); vLLM checks a key only if started with `--api-key`.
  Details per vendor below.
- Endpoint(s) / regions: `POST {base_url}/chat/completions`, `base_url` includes the version
  prefix and differs per vendor (table below). Regions: not researched (UNVERIFIED for every
  vendor).
- Protocol: SSE of `chat.completion.chunk` JSON objects, terminated by `data: [DONE]`.
- Keepalive / timeouts: no vendor documents SSE keepalive or server-side stream timeouts
  (UNVERIFIED for all five); brisvox must apply its own connect / first-byte / idle timeouts.
- Finalize / flush / cancel semantics: cancel = close the HTTP connection. Only Fireworks says so
  in docs ("Close the connection to stop generation and avoid billing for ungenerated tokens").
- Timestamps / interim results / tool-call deltas: see "Tool-call streaming matrix".
- Concurrency / rate limits: see "Rate limits and error bodies".
- Turkish support: not researched per model (model-dependent; UNVERIFIED for every vendor/model
  below). Choose the model by a Turkish live test, not by this document.
- Gotchas found during implementation: see "Implementation notes" below; "Known
  deviations" lists the ones the docs reveal.

## Summary table

| | Base URL | Auth | Source |
|---|---|---|---|
| OpenAI (baseline) | `https://api.openai.com/v1` (see `openai.md`) | `Authorization: Bearer` | openai.md |
| Groq | `https://api.groq.com/openai/v1` | `Authorization: Bearer $GROQ_API_KEY` (the page says "passing your Groq API key"; the header itself is the OpenAI-client default: UNVERIFIED as text on the page) | console.groq.com/docs/openai |
| Together AI | `https://api.together.ai/v1` | `-H "Authorization: Bearer $TOGETHER_API_KEY"` (doc) | docs.together.ai/docs/openai-api-compatibility |
| Fireworks AI | `https://api.fireworks.ai/inference/v1` | Bearer token in the `authorization` header (cURL example); also `OPENAI_API_BASE` / `OPENAI_API_KEY` env | docs.fireworks.ai/tools-sdks/openai-compatibility |
| vLLM | `http://localhost:8000/v1` in the page's client example (host/port defaults are NOT stated on the page: UNVERIFIED) | `--api-key <k>` or env `VLLM_API_KEY`; only enforced for the `/v1`, `/v2`, `/inference` prefixes; the page does not name the header (UNVERIFIED; OpenAI clients send Bearer) | docs.vllm.ai openai_compatible_server |
| Ollama local | `http://localhost:11434/v1/` | none; client needs a placeholder such as `'ollama'`, ignored by the server | docs.ollama.com/api/openai-compatibility |
| Ollama cloud | `https://ollama.com/v1` | `Authorization: Bearer $OLLAMA_API_KEY`; "`x-api-key` alone is not supported" (stated for the Anthropic-compatible endpoint) | docs.ollama.com/api/authentication |

OpenRouter (`preset = "openrouter"`; fetched 2026-10-14 from https://openrouter.ai/docs/api-reference/overview ,
read through a summarising fetch tool): `POST https://openrouter.ai/api/v1/chat/completions`,
`Authorization: Bearer <key>`; optional attribution headers `HTTP-Referer`, `X-OpenRouter-Title`
(`X-Title` also accepted), `X-OpenRouter-Categories` (the preset sends none; an application that
wants them sets `extra_headers` through the library). Streaming is SSE and "will occasionally
contain a comment payload, which you should ignore" (the adapter ignores comment lines). Usage:
"exactly once in the final chunk before the [DONE] message"; that chunk has a non-empty
`choices` with a content-free delta that repeats the `finish_reason`, so the response ends at the
usage chunk or `[DONE]` (placement `TrailingChunk`). The token limit is `max_tokens`. The overview
page lists no `reasoning_effort` request field, but the live runs of 2026-10-13 sent it
(`docs/LATENCY.md`: `openai/gpt-6-luna` thinks by default and adds about 1 s to the first token;
`reasoning_effort = "none"` turned it off), so set it for voice. Tools: `tools`, `tool_choice`,
`role: "tool"` messages with `tool_call_id`, `finish_reason` `tool_calls`; only some models
support them (`openrouter.ai/models?supported_parameters=tools`). The quirks of the preset are
OpenAI's own (`max_tokens`, `stream_options.include_usage` sent, usage in a trailing chunk), the
settings of those live runs. The model ids are `vendor/model`; the suggested one is the model of
the live comparison, pick yours by measurement.

vLLM warning (doc): "Do not rely on `--api-key` alone to secure vLLM." Ollama binds
`127.0.0.1:11434` by default; change with `OLLAMA_HOST` (docs.ollama.com/faq).

Brisvox adapter implications: `base_url` (string, includes `/v1` or `/openai/v1` or
`/inference/v1`), optional bearer key (absent for local Ollama/vLLM), extra static headers map,
model string verbatim (vendor-namespaced: Together `openai/gpt-oss-120b`, Fireworks
`accounts/fireworks/models/...`; "OpenAI model strings like `gpt-4o` ... return a 404" on
Together). Plain `http://` only for loopback (same rule as `openai.md`).

## OpenRouter

Not part of the research above (its documentation was not fetched for this file). What the
repository itself uses and has run: base URL `https://openrouter.ai/api/v1`, bearer auth with the
user's own key, a vendor-namespaced model id (`openai/gpt-6-luna`), the top-level request field
`reasoning_effort` (`none`, because that model thinks by default and adds about a second to the
first token). The `openrouter` preset fills in the base URL and that default model:
`provider = "openai_compatible"`, `[llm.openai_compatible]` with `preset = "openrouter"` (or
`base_url` and `model` spelled out), and `api_key_env`. A ready agent file is
`bench/pipecat_baseline/agent_live_tr.toml`; the recipe with notes is in `docs/PROVIDERS.md`
("OpenRouter"), and the live results (tool scenarios, latency) are in `docs/TOOLS.md` and
`docs/LATENCY.md`.

## Baseline: canonical OpenAI Chat Completions streaming

Sources: https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events ,
openai-python `src/openai/types/chat/chat_completion_chunk.py`, `completion_create_params.py`,
`lib/streaming/chat/_completions.py` (all fetched 2026-10-08). See also the Chat section of
`docs/providers/openai.md` (same sources). Note: OpenAI recommends Responses for new work and on
GPT-6 models tool calling needs Responses (openai.md; that claim was not re-checked in the
2026-10-08 fact-check of this file: UNVERIFIED here); this document concerns Chat Completions as
implemented by third parties.

Request: `stream: true`; `stream_options: {"include_usage": true}` (optionally
`include_obfuscation`); `tools: [{"type":"function","function":{"name","description","parameters"}}]`;
`tool_choice` (`none` default without tools, `auto` default with tools, or a named function);
`parallel_tool_calls` ("Whether to enable parallel function calling during tool use");
`max_completion_tokens` (`max_tokens` is deprecated in favour of it); `n` ("Keep `n` as `1`").

Chunk (page field list):
- `id` (same in every chunk), `object` = `"chat.completion.chunk"`, `created` (Unix seconds, same in
  every chunk), `model`, `choices[]`, optional `system_fingerprint`, `service_tier`, `obfuscation`,
  `moderation`, `usage`.
- `choices[]`: `index`, `delta`, `finish_reason` (`null` until the end; `stop`, `length`,
  `tool_calls`, `content_filter`, deprecated `function_call`), `logprobs`.
- `delta`: `role`, `content`, `refusal`, `tool_calls[]`, deprecated `function_call`, `audio`.
- `delta.tool_calls[]`: `index` (required int, identifies the call), `id` (optional, "The ID of the
  tool call"), `type` ("Currently, only `function` is supported"), `function` {`name`,
  `arguments`}; `arguments` is a JSON string that "may be invalid" and must be validated.
- `usage`: only present with `include_usage`; "null in every chunk except the last"; the last
  chunk has `choices: []` ("The array can be empty on the last chunk when
  `stream_options.include_usage` is set"; the `choices: []` quote was not re-confirmed on
  2026-10-08: UNVERIFIED); fields `prompt_tokens`, `completion_tokens`,
  `total_tokens`, `prompt_tokens_details.cached_tokens`, `completion_tokens_details.reasoning_tokens`
  and more. If the stream is interrupted "you may not receive the final usage chunk".
- Stream ends with `data: [DONE]`.

Verbatim first chunk (doc; the page example is cut by the fetch tool at the quote limit, the model
id is whatever the page shows):
```json
{"id":"chatcmpl-123","object":"chat.completion.chunk","created":1694268190,"model":"gpt-6-astra",
"system_fingerprint": "fp_44709d6fcb",
"choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}],
"obfuscation":"r4N7vQ2m"}
```
The reference page has NO JSON example of a `tool_calls` delta or of the usage chunk (reported by
the fetch). Canonical tool-call stream shape (illustrative, from the field list; the standard
OpenAI behaviour is: first fragment carries `index`, `id`, `type`, `function.name` and
`arguments:""`; later fragments carry only `index` and an `arguments` fragment):
```json
{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}
{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}
{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}
{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":25,"completion_tokens":12,"total_tokens":37}}
data: [DONE]
```
Accumulation (source, openai-python `lib/streaming/chat/_completions.py`): `_accumulate_chunk`
merges `choice.delta.to_dict()` into the message snapshot via `accumulate_delta`; for each
`delta.tool_calls` element the SDK locates the snapshot call by `.index`
(`choice_snapshot.message.tool_calls[tool_call_chunk.index]`). Partial-JSON parsing
(`from_json(..., partial_mode=True)`) is done ONLY for `strict` function tools with non-empty
`arguments`; for other tools `parsed_arguments` is set only when the `arguments.done` event fires
(re-checked 2026-10-08). The implementation file `_deltas.py` returned 404 in the fetch: merge
rules for `id`/`name`/strings UNVERIFIED here; the usual rule is string concatenation for
`arguments`.

Tool result message (illustrative, standard): assistant message with
`tool_calls:[{"id","type":"function","function":{"name","arguments"}}]` followed by
`{"role":"tool","tool_call_id":"<id>","content":"<string>"}` (see openai.md Chat section; Groq's
local-tool-calling page confirms in prose that each tool result message includes a `tool_call_id`
matching the call `id` ("Unique identifier for this tool call (used when sending results back)");
the earlier quoted sentence "must match the `id` from the assistant's `tool_calls`" was NOT found
verbatim and is dropped).

## Tool-call streaming matrix (the core question)

| | Streaming format | `id` | `index` | Evidence |
|---|---|---|---|---|
| Groq | The local-tool-calling page's Python streaming example appends each chunk's `delta.tool_calls` to a list (`collected_tool_calls.extend(chunk.choices[0].delta.tool_calls)`); no prose sentence and no JSON example of a chunk. SDK type `ChoiceDeltaToolCall` has `index: int`, `id: Optional[str]`, `type`, `function`. Whether arguments come as fragments or whole in one chunk: UNVERIFIED | optional per SDK type | required int per SDK type | console.groq.com/docs/tool-use/local-tool-calling (code example); groq-python `chat_completion_chunk.py` (source) |
| Together AI | Incremental: "the model returns tool calls incrementally, accessible from the `delta.tool_calls` object in each chunk" (single-call page; the longer wording "The first delta includes the call `id`, `type`, and function `name` with empty `arguments`. Later deltas add the argument text." is from the function-calling overview and was not re-confirmed: UNVERIFIED verbatim). That is the OpenAI format. Caveat: the doc's example of a "later delta" shows the whole arguments object in one fragment (`"arguments": "{\"location\":\"New York City, USA\"}"`), so a model/vendor may emit the full string in one fragment. The non-streaming parallel page shows `"index": 0,1,2` in the same-function example but `"index": null` on every call in the multiple-different-functions example (see deviations) | first delta | per doc example `index: 0` in both deltas; `null` seen in non-streaming multi-function example | docs.together.ai/docs/inference/function-calling/single-call.md, .../parallel.md |
| Fireworks | Incremental: "Arguments are sent incrementally as the model generates them." The page's example code accumulates in a dict keyed by `tool_call.index`, appends `function.arguments` fragments and parses after the stream finishes (the quoted "store `id` and `function.name` when they appear" / `finish_reason == "tool_calls"` wording was not re-confirmed: UNVERIFIED verbatim) | when it appears (first) | yes, accumulate by `index` | docs.fireworks.ai/guides/function-calling |
| vLLM | Depends on the `--tool-call-parser`; the stream generator delegates to `parser.parse_delta(...)` (not re-read), so the shape is parser-specific. For the `hf` parser: "When streaming, each tool call is emitted whole once its region parses." Other parsers (hermes etc.): UNVERIFIED whether fragments or whole | UNVERIFIED for streaming (non-stream default id format `chatcmpl-tool-<uuid>`, `kimi_k2` parser uses `functions.<name>:<idx>`) | UNVERIFIED | docs.vllm.ai features/tool_calling; vllm `chat_completion/serving.py`, `chat_utils.py` (source) |
| Ollama | Not documented either way (streaming page: "Watch for streamed `tool_calls` in each chunk"). Source: the OpenAI layer copies fully parsed `api.ToolCall`s into a chunk delta (`ToToolCalls` JSON-marshals the whole `Arguments` map into one string), i.e. a call is most likely complete in one chunk; confirmation of "never fragmented" UNVERIFIED (parser internals not read) | generated server-side as `"call_" + 8 random [a-z0-9]` in `ChatHandler` (`toolCallId()`; the native chat path `handleNativeChat` was cut off in the fetch: UNVERIFIED there); `omitempty` on `ToolCall.ID` | `Function.Index` (int, `json:"index"`, no omitempty) | docs.ollama.com/capabilities/tool-calling, /streaming; ollama `openai/openai.go`, `server/routes.go`, `api/types.go` (source) |

Design consequence (adapter): the accumulator must accept BOTH shapes with the same code path:
fragments (first with id+name+`""` arguments, then pure arguments) and whole calls (id+name+full
arguments in one delta). Brisvox's `LlmEvent::ToolCallDelta { index, id?, name?, arguments }`
already allows both. Do not assume `index` is non-null (Together non-streaming parallel example)
or that `id` is present (Groq/Together/Fireworks types make it optional; Ollama `omitempty`).

## Per-vendor details

### Groq
Sources: https://console.groq.com/docs/openai , /docs/tool-use , /docs/tool-use/local-tool-calling ,
/docs/api-reference , /docs/rate-limits , /docs/errors , /docs/models , groq-python
`types/chat/chat_completion_chunk.py` and `completion_create_params.py` (source), all fetched
2026-10-08.
- Base URL `https://api.groq.com/openai/v1`. Compat page: change `base_url`, pass the Groq key.
- Unsupported fields, "400 error if supplied": `logprobs`, `logit_bias`, `top_logprobs`,
  `messages[].name`; `n` must equal 1. `temperature: 0` is converted to `1e-8`. (The compat page
  does not mention `stream_options` or `parallel_tool_calls`; the API reference does list both.
  The groq-python `completion_create_params.py` defines `parallel_tool_calls` but NOT
  `stream_options`; it only has `stream` as `Literal[True/False]`.)
- API reference: `stream` (SSE, ends with "`data: [DONE]`"); `stream_options` ("Only set this when
  you set `stream: true`"; the property list is collapsed in the fetched text, so whether
  `include_usage` is accepted is UNVERIFIED); `parallel_tool_calls` (boolean, default `true`);
  `tool_choice` (`none`/`auto`/`required`/named function, default `none` without tools, `auto`
  with); `max_completion_tokens` (replaces deprecated `max_tokens`); `service_tier` (`auto`,
  `on_demand` (default), `flex`, `performance`; `flex` "will succeed or fail quickly").
  `reasoning_effort`: values `none|default|minimal|low|medium|high|xhigh|max`, model
  dependent (qwen3 models: `none` disables reasoning, `default`/null = model default;
  `qwen/qwen3.8-27b` additionally `low|medium|high`, its default is `none`;
  `openai/gpt-oss-20b` and `-120b`: `low|medium|high`, default `medium`); "Values outside a
  model's supported set are rejected with a 400".
- Streaming chunk: SDK type `ChatCompletionChunk` has top-level `usage` and an `x_groq` object
  with `usage`, `id`, `debug`, `error`, `seed`, `usage_breakdown`. The API reference calls `x_groq`
  "Groq-specific metadata for non-streaming chat completion responses"; where Groq puts usage in a
  stream (top-level on the last chunk, inside `x_groq.usage`, or on a trailing empty-`choices`
  chunk): UNVERIFIED. Accept all three. `x_groq.error` exists in the type: a mid-stream error may
  be delivered there (UNVERIFIED).
- `finish_reason` (SDK Literal): `stop`, `length`, `tool_calls`, `function_call` (deprecated). No
  `content_filter` in the Groq type.
- `delta` extras (SDK): `reasoning` (string; per the SDK docstring it is "only populated when the
  request's `reasoning_format` is `parsed`", so by default thinking may arrive inline in
  `content` or be hidden depending on `reasoning_format`: the other `reasoning_format` values are
  UNVERIFIED), `refusal`, `annotations`, `executed_tools` (compound systems), `function_call`
  (deprecated). Reasoning text must NOT be spoken (see gotchas).
- Parallel tool calls by model (tool-use page): parallel yes: `qwen/qwen3.8-27b`,
  `minimaxai/minimax-m2.7`, `llama-3.3-70b-versatile`, `llama-3.1-8b-instant`; parallel NO:
  `openai/gpt-oss-20b`, `openai/gpt-oss-120b`, `openai/gpt-oss-safeguard-20b`. All listed support
  local/remote tool use (the models page lists `qwen/qwen3.8-27b` and `minimaxai/minimax-m2.7`
  under PREVIEW models, "not for production"). Per the local-tool-calling page's Error Handling
  section: `tool_choice: "required"` with no call -> 400; `"none"` can still produce a 400 on some
  models; a named function the model fails to call (or another function) -> 400.
- Invalid tool call -> HTTP 400 `failed_generation` (local-tool-calling page; message "Invalid tool
  call generated"; retry advice: adjust temperature). The JSON below is as recorded earlier; on
  2026-10-08 the fetch tool only confirmed the field names `message`, `type`, `failed_generation`
  {`reason`, `tool_call_id`, `attempted_arguments`} and the example reason "Tool call arguments are
  not valid JSON", so the exact values (`call_abc123`, etc.) are UNVERIFIED:
```json
{
  "error": {
    "message": "Invalid tool call generated",
    "type": "invalid_request_error",
    "failed_generation": {
      "reason": "Tool call arguments are not valid JSON",
      "tool_call_id": "call_abc123",
      "attempted_arguments": "{'location': 'New York'}"
    }
  }
}
```
  The errors page does not mention `tool_use_failed`. Whether this 400 can also occur mid-stream
  (after 200): UNVERIFIED.
- Models (https://console.groq.com/docs/models, production): `llama-3.1-8b-instant` 560 tokens/s,
  131,072 context, 131,072 max completion; `openai/gpt-oss-20b` 1,000 tokens/s, 131,072 context,
  65,536 max completion; `openai/gpt-oss-120b` 500 tokens/s, 131,072 context, 65,536 max
  completion; `llama-3.3-70b-versatile` 280 tokens/s, 131,072 context, 32,768 max completion.
  Speed figures are the page's, not measured TTFT. Low-latency candidates for voice:
  `llama-3.1-8b-instant` (parallel tools yes) and `openai/gpt-oss-20b` (reasoning model, no
  parallel tools). Turkish quality: UNVERIFIED.
- Status codes (errors page): 400, 401, 403, 404, 413, 422, 424, 429, 498 (custom: flex tier at
  capacity), 499 (custom: caller cancelled), 500, 502, 503 (plus 200/206 informational); "You will
  not be charged for requests that return server error codes" (500/502/503; 498/499 not stated).
  Body (doc): `{"error": {"message": "String - description of the specific error", "type":
  "invalid_request_error"}}`.

### Together AI
Sources: https://docs.together.ai/docs/openai-api-compatibility ,
/docs/inference/function-calling/{overview,single-call,parallel,best-practices}.md ,
/reference/chat-completions.md , /docs/error-codes.md , /docs/serverless/rate-limits.md ,
/docs/serverless/models.md , /docs/inference/chat/parameters.md (fetched 2026-10-08).
- Base URL `https://api.together.ai/v1`. Chat completions incl. streaming, function calling and
  `response_format` are supported.
- Differences from OpenAI (compat page): `service_tier`, `store`, `metadata`, `prediction` accepted
  but ignored; `seed` best-effort; `logit_bias` unsupported on most models; `n` not on every
  model; "Extra token counts such as cached and reasoning tokens can appear in different locations
  depending on the model, so clients may need to check both shapes"; chain-of-thought in a
  `reasoning` or `reasoning_content` field on the assistant message, "the field name depends on the
  model"; `id` and `system_fingerprint` use Together formats; errors "follow OpenAI's shape, but
  `type` and `code` values are Together's. Match on HTTP status codes".
- Reference (`ChatCompletionChunk`): required `id`, `object` (`chat.completion.chunk`), `created`,
  `choices`, `model`; optional `system_fingerprint`, `usage`, `warnings`. `choices[]`: `index`,
  `delta`, `finish_reason` (nullable), `logprobs`, `top_logprobs`, `seed`. `delta`: `role`
  (required: UNVERIFIED as "required" on 2026-10-08), optional `content`, `reasoning`,
  `reasoning_content`, `tool_calls`, `function_call`
  (deprecated), `token_id`. `usage` (`prompt_tokens`, `completion_tokens`, `total_tokens`) is
  nullable, "typically null except where populated". `finish_reason` values: `stop`, `eos`,
  `length`, `tool_calls`, `function_call`. **`eos` is a Together-only value**; treat as `Stop`.
- `stream_options` and `parallel_tool_calls`: NOT defined on the reference page (confirmed
  2026-10-08: `ChatCompletionRequest` has only `stream` as a boolean) nor the parameters page:
  support UNVERIFIED. Do not rely on `include_usage`; accept usage if present. The function-calling
  parallel page documents parallel calls as a model behaviour (three calls with `index` 0,1,2 and
  distinct `id`s such as `call_aisak3q1px3m2lzb41ay6rwf`), without a toggle.
- `tool_choice` (best-practices): `"auto"` default, `"none"`, `"required"`, or
  `{"type": "function", "function": {"name": "get_current_stock_price"}}`. `strict: true`
  constrains arguments (streaming interaction not described).
- `max_tokens`; `context_length_exceeded_behavior` defaults to `error` (`truncate` overrides
  `max_tokens` with the model maximum; per the reference, `error` "returns 400 with an appropriate
  error message", which CONFLICTS with the error-codes page's 403, see below).
- Models (serverless catalog page; column "function calling" as extracted): `Qwen/Qwen3.5-9B`
  (262,144 context, yes; $0.17 in / $0.25 out per 1M), `zai-org/GLM-5.3-Flash` (1,048,575, yes;
  $0.15 / $0.50, cached input $0.03), `openai/gpt-oss-120b` (131,072, yes; $0.15 / $0.60),
  `meta-llama/Llama-3.3-70B-Instruct-Turbo` (131,072, yes; $1.04 / $1.04), `Qwen/Qwen3.8-Flash`
  (1,000,000 context, no function calling listed), `together/Tev1-4B-experimental`
  (32,768, no). The page does not mention Llama 3.1 8B (earlier text "isn't in the serverless chat
  catalog" was an inference, not a page statement). The page has no latency
  figures; pick by measurement. Turkish: UNVERIFIED.
- Errors (error-codes page, no JSON body shown): 400, 401, 402 (monthly spend limit), **403 is
  documented as "Bad Request": "Input token count + `max_tokens` parameter must be less than the
  context length of the model being queried"** (a context overflow is documented as a 403 here, not
  400, while the chat-completions reference says 400 for `context_length_exceeded_behavior=error`:
  pages disagree, handle both), 404 (bad URL or model name), 429 (serverless limited under high
  demand, or GPU quota for dedicated), 500, 503, 504, 524 (Cloudflare timeout), 529 (unspecified
  server error). "Serverless performance is best-effort."

### Fireworks AI
Sources: https://docs.fireworks.ai/tools-sdks/openai-compatibility ,
/api-reference/post-chatcompletions , /guides/function-calling , /guides/querying-text-models ,
/guides/quotas_usage/rate-limits , /guides/inference-error-codes , /getting-started/quickstart
(fetched 2026-10-08).
- Base URL `https://api.fireworks.ai/inference/v1`. Model IDs are `accounts/fireworks/models/<id>`
  (examples on the quickstart: `glm-5p3-flash`, `kimi-k3`, `deepseek-v3p1`, `glm-5p3`); dedicated
  deployments `accounts/<ACCOUNT_ID>/deployments/<DEPLOYMENT_ID>` (UNVERIFIED on 2026-10-08, not
  re-seen in fetched text).
- Compat page, "minor differences": `max_tokens` behaves differently when the context is exceeded
  (`context_length_exceeded_behavior`: `truncate` is the DEFAULT and lowers `max_tokens`; `error`
  matches OpenAI). No supported/unsupported parameter list exists on the page.
- Streaming usage: Fireworks returns usage stats for streaming too. The compat page says "the
  `usage` field is returned in the very last chunk on the response"; the querying-text-models page
  says usage is in the final chunk, "the chunk with `finish_reason` set", and calls this a
  "Fireworks extension" (that label appears only on the querying page, not on the compat page).
  Earlier chunks have `"usage":null` (UNVERIFIED on 2026-10-08). The API reference says instead:
  `stream_options` (streaming only) "Unlike the OpenAI spec, Fireworks includes usage by default
  for streaming responses; set this to `false` to opt out" (parameter `include_usage`), and usage arrives as a final
  chunk, "not the chunk carrying `finish_reason`", before `data: [DONE]` (the "empty `choices`
  array" wording and "Per-choice `usage` may appear on the finish chunk, depending on the
  deployment" were not re-confirmed: UNVERIFIED verbatim). **The pages disagree; the adapter must
  take usage from whichever chunk carries it (finish chunk, trailing empty-choices chunk, or a
  per-choice `usage`).** Other `stream_options` fields: `include_internal_content` ("Omitted entirely
  from response chunks when false"), `buffer_tokens` ("Honored only on the vLLM backend"),
  `buffer_mode` (default `'any'`), `buffer_ms` (not re-confirmed: UNVERIFIED); coalescing, adds
  latency, leave unset.
- Request: `parallel_tool_calls` (boolean, "Enable parallel function calling" per the reference;
  the guide says only "Some models support parallel tool calling ... Check the model's
  capabilities"),
  `tool_choice` (default `auto`; `none`; `required` (alias `any`); `allowed_tools`; named
  function: these enumerations were not re-confirmed on 2026-10-08, UNVERIFIED). The API reference
  writes the named form as `{ "type": "function", "name":
  "my_function" }` while the guide writes `{"type": "function", "function": {"name":
  "get_weather"}}` (doc): conflicting; use the OpenAI nested form (guide) and test the other.
  `max_tokens` (`max_completion_tokens` is an "Alias for max_tokens. Cannot be specified together
  with max_tokens."), `prompt_cache_key`,
  `prompt_cache_isolation_key` (not re-confirmed), `perf_metrics_in_response` (streaming:
  `perf_metrics` object on the final chunk; the reference says setting it to `true` "additionally
  includes the same metrics in the response body under the `perf_metrics` field"),
  `response_format`. Tool support per model is exposed as a `supportsTools` field
  (guide troubleshooting; where it appears is not described).
- Chunk `delta` (reference): `role`, `content`, `reasoning_content`, `tool_calls`,
  `internal_content`. `finish_reason`: `stop`, `length`, `function_call`, `tool_calls`; null until
  the end.
- Cancel: "Close the connection to stop generation and avoid billing for ungenerated tokens."
- Errors (codes page): 400, 401, 402, 403, 404, 405, 408, 412, 413, 429, 500, 502, 503, 504, 520.
  429 on serverless = account request/TPM limit exceeded; on dedicated/on-demand = deployment
  saturated ("there are no account-level rate limits"). JSON error body example: not on the page
  (UNVERIFIED shape; assume `{"error":{"message":...}}`).
- Model examples: `accounts/fireworks/models/glm-5p3-flash` (a "Flash" variant; latency
  UNVERIFIED), `deepseek-v3p1`, `kimi-k3`. The model overview page does not list small fast
  models; see https://fireworks.ai/models (not fetched: UNVERIFIED). Turkish: UNVERIFIED.

### vLLM (OpenAI-compatible server)
Sources: https://docs.vllm.ai/en/latest/serving/online_serving/openai_compatible_server/ ,
https://docs.vllm.ai/en/latest/features/tool_calling/ , vllm source
`entrypoints/openai/chat_completion/serving.py`, `entrypoints/chat_utils.py` (raw GitHub, main)
(fetched 2026-10-08). The old URL `.../serving/openai_compatible_server.html` redirects to the one
above (UNVERIFIED on 2026-10-08).
- Start: `vllm serve NousResearch/Meta-Llama-3-8B-Instruct --dtype auto --api-key token-abc123`
  (doc; the fetch confirmed the beginning `vllm serve NousResearch/Meta-Llama-3-8B-Instruct`, the
  remaining flags UNVERIFIED); client example `base_url="http://localhost:8000/v1"`. Default served
  model name = the model argument (UNVERIFIED: not read).
- Chat API: `parallel_tool_calls` supported: "Setting the `parallel_tool_calls` parameter to
  `false` ensures vLLM only returns zero or one tool call per request"; multiple calls with
  `true` are model dependent. Only unsupported item noted: `image_url.detail`; `user` is ignored.
  Extra parameters (selected): `top_k`, `min_p`, `repetition_penalty`, `min_tokens`,
  `chat_template_kwargs`, `structured_outputs`, `priority`, `request_id`, `cache_salt` (the list
  other than `stream_interval` was not re-confirmed: UNVERIFIED),
  `stream_interval` ("Number of tokens to batch into each streamed chunk"; leave at default for
  latency). The page does not mention `stream_options` / `continuous_usage_stats`.
- Source (`serving.py`): with `include_usage` a trailing chunk with `choices=[]` and `usage` is
  emitted after the token loop (serialized with `exclude_none=True`; the earlier note that it also
  uses `exclude_unset=True` was not confirmed); `include_continuous_usage`
  puts `usage` on EVERY chunk (also the initial role chunk and echo chunks, running counts; the
  exact `completion_tokens=0` on the first chunk is UNVERIFIED); every path ends with
  `data: [DONE]` (yielded unconditionally at the end of the generator, even after an error chunk).
  Errors mid-stream are yielded as `data: {...}` error chunks (via `create_streaming_error_response`
  or `_convert_generation_error_to_streaming_response` for `GenerationError`), followed by `[DONE]`
  (body shape not read: UNVERIFIED). A `GenerationError` frame is sent without an explicit
  extra `[DONE]` other than the generator's final one.
- `finish_reason` (source): when tool calls were streamed and the reason is `stop`, it is
  rewritten to `"tool_calls"` - EXCEPT when a named `tool_choice` function is used, where it stays
  `"stop"` even though a tool call was produced. (Non-streaming: also `tool_calls` when
  `tool_choice == "required"` and the engine said `stop`.) Adapter rule: derive `ToolCalls` when
  any tool call was accumulated, whatever the finish_reason says.
- Tool calling requires `--enable-auto-tool-choice` ("mandatory") plus `--tool-call-parser <name>`
  (e.g. `hermes` for Hermes/Qwen2.5, `mistral`, `llama3_json`, `llama4_pythonic`, `pythonic`,
  `granite`, `deepseek_v3`, `kimi_k2`, `glm45`, `qwen3_xml`, ... plus `--tool-parser-plugin`;
  the page lists many more, e.g. `internlm`, `jamba`, `xlam`, `openai`, `hf`).
  Without the flags a request with `tool_choice: "auto"` fails (exact behaviour/error text
  UNVERIFIED: the page only says the flag is mandatory).
  `tool_choice`: named function (default on), `auto`, `required` (vllm >= 0.8.3), `none`. Named
  function and `required` use structured outputs: "guaranteed a validly-parsable function call -
  not a high-quality one". Parallel support is per model ("Parallel tool calls are not supported
  for Llama 3, but it is supported in Llama 4 models"; Mistral 7B struggling: not re-confirmed).
  The `hf` parser rejects "strict tools, required or named tool choice, or
  `parallel_tool_calls=false`". Mistral parsing "requires tool call IDs that are exactly 9
  digits" - per the page this applies to the Transformers tokenization backend only, where
  vLLM's longer ids cause an error; "provided `tool_call_id` fields are truncated to the last 9
  digits". (Not in `chat_utils.py`.)
- Tool call id format (source `make_tool_call_id`): `chatcmpl-tool-<uuid>`; `kimi_k2` parser:
  `functions.<name>:<idx>` (used for model types `kimi_k2`, `kimi_k25`, `kimi_k3`; so ids can be
  long; do not impose a short max).
- Performance / concurrency: set by the deployment (GPU, batch size); not an API limit. 429/503
  behaviour: not researched (UNVERIFIED). Model examples: any HF model; latency = deployment.

### Ollama (OpenAI compatibility)
Sources: https://docs.ollama.com/api/openai-compatibility , /api/authentication , /api/errors ,
/api/usage , /capabilities/tool-calling , /capabilities/streaming , /faq ; ollama source
`openai/openai.go`, `middleware/openai.go`, `server/routes.go`, `api/types.go` (raw GitHub, main)
(fetched 2026-10-08). The old `github.com/ollama/ollama/blob/main/docs/openai.md` returned 404
(not re-checked 2026-10-08).
- Base URL local `http://localhost:11434/v1/`, cloud `https://ollama.com/v1`. Responses API
  (`/v1/responses`, non-stateful only) exists since v0.13.3.
- `/v1/chat/completions` supported request fields: `model`, `messages` (text, base64 images, parts
  arrays; image URLs not supported), `frequency_penalty`, `presence_penalty`, `response_format`,
  `seed`, `stop`, `stream`, `stream_options` (only `include_usage`), `temperature`, `top_p`,
  `max_tokens`, `tools`, `reasoning_effort`, `reasoning` (`effort`: the `effort` sub-field was not
  re-confirmed). **Not supported:
  `tool_choice`, `logit_bias`, `user`, `n`**, logprobs. `max_completion_tokens` and
  `parallel_tool_calls` are not in the list: whether they are ignored or rejected is UNVERIFIED
  (assume unknown JSON fields are ignored; make both opt-in per provider config).
- Streaming (source `middleware/openai.go`): each chunk is written as `data: <json>` followed by a
  blank line; order is content chunks, a dedicated finish chunk (empty `delta`, `finish_reason`),
  then (only with `include_usage`) a second finish-style chunk with `usage` and a non-standard
  `timings` object (durations in milliseconds) and `choices: []`, then `data: [DONE]`. Successful
  streams use `Content-Type: text/event-stream`. Chunk id is `fmt.Sprintf("chatcmpl-%d",
  rand.Intn(999))` (i.e. 0..998; collisions possible; do not treat `id` as unique).
  `system_fingerprint` is `"fp_ollama"`. `created` is taken from the first response and reused.
  The role is sent only on the first chunk. A response with both thinking and content/tool calls
  may be split into two chunks (`ToStreamChunks`); thinking goes in `delta.reasoning`
  (field name `reasoning` not re-confirmed: UNVERIFIED).
- `finish_reason` (source): `DoneReason` or `"stop"`; overridden to `"tool_calls"` only when the
  reason is `stop` and a tool call was sent; other reasons (e.g. `length`) pass through.
- Tool calls (source): each carries `id` (generated as `call_` + 8 lowercase alphanumerics in the
  chat handler via `math/rand`; `omitempty` in the struct, so absent if a path does not set it),
  `index`, `type: "function"`, `function.name`, `function.arguments` (a JSON string made by
  marshalling the parsed arguments map; empty if marshalling fails). The native API example shows
  `index` inside `function`
  (`{"type":"function","function":{"index":0,"name":"get_temperature","arguments":{"city":"New
  York"}}}`, native API, arguments as an OBJECT, not a string); tool results in the native API use
  `{"role":"tool","tool_name":...,"content":...}`; on the OpenAI path use `tool_call_id`
  (UNVERIFIED how the server maps `tool_call_id` back; the docs never mention tool call ids).
  The native non-streaming path returns several parallel calls in one assistant message.
- Usage: `prompt_tokens`, `completion_tokens`, `total_tokens = prompt + eval`, optional
  `prompt_tokens_details.cached_tokens` (only if the cached count is present). Native metrics:
  `prompt_eval_cached_count`, durations in nanoseconds (`load_duration`, `total_duration`, ...);
  for streaming they are in the final chunk where `done` is `true`.
- Errors: native body is `{"error": "the model failed to generate a response"}` (a STRING) with
  codes 200/400/404/429/500/502; "If an error occurs mid-stream, the error will be returned as an
  object in the `application/x-ndjson` format with an `error` property" and the status stays 200.
  On the OpenAI path (source `openai/openai.go`): `{"error": {"message","type","param","code"}}`
  with `type` `invalid_request_error` (400), `not_found_error` (404), else `api_error`; `param` and
  `code` serialize as `null` (not re-confirmed). The chat writer has no SSE error event: a non-200 status is sent as
  plain `application/json`; how a mid-stream failure appears on the OpenAI path: UNVERIFIED
  (`routes.go` `streamResponse` writes a final NDJSON error line for the native path once output
  has started).
- Concurrency (faq): `OLLAMA_NUM_PARALLEL` default 1 per model (the page says "default 1"),
  `OLLAMA_MAX_LOADED_MODELS` default 3 * GPUs (3 on CPU), `OLLAMA_MAX_QUEUE`
  default 512; a full queue answers "503 ... server is overloaded". Models stay loaded 5 minutes
  (`OLLAMA_KEEP_ALIVE`, per-request `keep_alive` overrides; negative = forever, 0 = unload: the
  per-request/negative/0 details were not re-confirmed).
  The documented OpenAI fields have no `keep_alive`, so set `OLLAMA_KEEP_ALIVE` on the server or
  warm via the native API; the first request after unload pays `load_duration`. A cold model is
  the main latency risk for a voice call: send a warm-up request before the call.
- Context size: "The OpenAI API does not have a way of setting the context size for a model";
  use a Modelfile with `PARAMETER num_ctx` and `ollama create`.
- Model examples on the compat page: `gpt-oss:20b`, `qwen3:8b`, `qwen3-vl:8b` (chat examples),
  `llama3.2` (`ollama pull llama3.2`), `gemma4:31b` (cloud direct access; `gemma4:cloud` through
  a signed-in local server). Turkish quality: UNVERIFIED.
- **Live results (2026-10-14, Ollama 0.34.1, RTX 5070 12 GB, `reasoning_effort = "none"`, default
  context 4096).** Tool calling is per model and was measured: six scenarios x three runs through
  the real engine, and a warm voice-style latency (5 runs, p50). Details and failure notes:
  `docs/TOOLS.md`, "Local models through Ollama".

  | Model | GPU memory | Tool scenarios (of 18) | TTFT p50 | First sentence p50 |
  |---|---|---|---|---|
  | `gemma4:12b-it-qat` | 7.7 GB | **18** | 69 ms | 267 ms |
  | `gemma4:e4b-it-qat` | 3.1 GB | 12 (fails the 3-round chain and the invalid-argument retry) | 42 ms | 157 ms |
  | `qwen3.5:4b` | 3.1 GB | 12 (same two) | 65 ms | 208 ms |
  | `qwen3.5:9b` | 5.5 GB | 12 (flaky; once answered with made-up weather, no tool call) | 79 ms | 291 ms |
  | `qwen2.5-coder:7b` | 4.7 GB | 0 (writes the call as JSON text, which would be spoken) | 34 ms | 220 ms |

- **Thinking models.** Set `reasoning_effort = "none"` (the config key of the same name): the
  `ollama` preset does not send it by itself. `/v1/chat/completions` honours it (and the
  nested `reasoning: {effort: "none"}`) but **ignores** the native `think: false`. Without it
  `qwen3.5` spent a whole 200-token budget on hidden reasoning and never spoke; with `medium`
  effort the 9b model passed all tool runs but took 14 to 35 s to the first word.
- **GPU memory.** `gemma4:12b-it-qat` is 7.7 GB at the default context. With only ~5 GB free,
  30 of 48 layers on the GPU gave first sentence p50 1.66 s (TTFT 376 ms; native API with
  `num_gpu = 30`).

## Known deviations (checklist for the adapter and its tests)

1. **Tool-call shape**: fragments (OpenAI, Together, Fireworks) vs whole calls (Ollama most
   likely; vLLM `hf` parser). Same code path must accept both. Never require `arguments:""` on
   the first delta.
2. **Missing or null fields**: `id` optional on delta types for Groq, Fireworks, Together,
   Ollama (`omitempty`); `index` is `null` in Together's non-streaming parallel-multiple example.
   Fallbacks: if `index` is missing, use the position in `tool_calls[]` of that delta, and if a new
   `id` appears for an index already holding a different `id`, start a new call. If no `id` ever
   arrives, synthesise one (`call_<n>`) so the `tool` message can reference it. Different servers
   emit ids of very different formats (`call_xxx`, `chatcmpl-tool-<uuid>`, `functions.name:0`):
   treat ids as opaque strings, no length limit assumed (OpenAI function names are limited to
   64 chars, see openai.md).
3. **`finish_reason` values**: `stop`, `length`, `tool_calls`, `content_filter` (OpenAI),
   `function_call` (deprecated, OpenAI/Groq/Fireworks/Together), `eos` (Together). Mapping to
   brisvox `FinishReason`: `length` -> `Length`; `tool_calls`/`function_call` -> `ToolCalls`;
   `stop`/`eos`/`content_filter` -> `Stop` (content_filter has no variant, same gap as openai.md);
   additionally force `ToolCalls` when any call was accumulated (vLLM named `tool_choice` keeps
   `stop`).
4. **Usage placement**: OpenAI: trailing chunk, `choices: []`, before `[DONE]`. vLLM: same, plus
   optional per-chunk usage. Ollama: same (extra `timings`). Fireworks: documented as BOTH
   "on the finish chunk" and "separate trailing chunk" (pages disagree; on by default, opt-out).
   Groq: top-level `usage` and `x_groq.usage` exist in the SDK type, placement UNVERIFIED.
   Together: `usage` nullable, `stream_options` support UNVERIFIED, cached/reasoning counts "in
   different locations depending on the model". Rule: remember the last non-null `usage` seen
   anywhere (top-level, `x_groq.usage`, per-choice `usage` UNVERIFIED), emit it in `Done`; do not
   wait for it past the terminal event more than a short grace (a missing usage must not delay
   `Done`; the stream can end at `[DONE]` or at EOF).
   Cached tokens: `usage.prompt_tokens_details.cached_tokens` (OpenAI, Ollama); other vendors
   UNVERIFIED -> `cached_input_tokens = 0`.
5. **Reasoning deltas**: Groq `delta.reasoning` (only with `reasoning_format=parsed`), Ollama
   `delta.reasoning`, Fireworks `delta.reasoning_content`, Together `reasoning` /
   `reasoning_content` (model dependent). These
   must never reach TTS: ignore them for `TextDelta` (optionally count latency to first
   non-reasoning token). With reasoning models the first visible token is later than the first
   chunk: first-output timeout must exceed that (as in openai.md). Prefer non-reasoning models
   or `reasoning_effort: none` where supported (Groq: Qwen3; OpenAI: see openai.md).
6. **Unsupported / rejected request fields**: Groq 400 on `logprobs`, `logit_bias`,
   `top_logprobs`, `messages[].name`, `n != 1`; Ollama lacks `tool_choice`, `n`, `logit_bias`;
   Together ignores `service_tier`, `store`, `metadata`, `prediction`. Send only what is needed:
   `model`, `messages`, `stream`, `tools` (when non-empty), `max_tokens` or `max_completion_tokens`
   (configurable name), `temperature`; everything else behind explicit per-provider config
   (`stream_options.include_usage`, `parallel_tool_calls`, `tool_choice`).
7. **Max-token field name**: OpenAI/Groq prefer `max_completion_tokens` (`max_tokens`
   deprecated); Ollama, Together, Fireworks (accepts both, not together), vLLM (docs not read for
   it) document `max_tokens`. Make it a config enum, default `max_tokens`.
8. **Error bodies**: OpenAI-style `{"error":{"message","type","param","code"}}` (OpenAI,
   Groq `message`+`type`, Together "OpenAI's shape ... Together's values", Ollama OpenAI path);
   Ollama native `{"error":"string"}`; vLLM mid-stream `data: {...error...}` then `[DONE]`;
   Ollama native mid-stream NDJSON `{"error":...}` with status 200. Parser: accept `error` as an
   object or a string; also accept `x_groq.error`. Never copy free text into logs/errors verbatim
   (can echo prompt), same policy as openai.md. HTTP status is the portable signal (Together's own
   advice).
9. **Context-overflow status**: Together documents 403 (not 400) for input + `max_tokens` over the
   context on its error-codes page (but its chat-completions reference says 400 for
   `context_length_exceeded_behavior=error`); Fireworks silently truncates `max_tokens` by default
   (`truncate`); OpenAI/Groq 400 (Groq 400 not re-verified for this case).
   Map any of 400/403/413/422 on the pre-stream response to `Rejected { status }` unchanged; the
   LLM stage decides.
10. **Custom status codes**: Groq 498 (flex tier at capacity) and 499; Together 524, 529;
    Fireworks 520; Ollama 502 (unreachable cloud model) and 503 (queue full). Treat 429, 498, 502,
    503, 504, 520, 524, 529 as retryable-at-turn-level (the adapter itself never retries, like
    openai.md).
11. **SSE terminator**: all five document or implement `data: [DONE]` (Groq stated in the API
    reference; Together, Fireworks stated in references (not re-confirmed for Together); vLLM and
    Ollama in source). A stream that ends without
    `[DONE]` and without a `finish_reason` was cut off -> `Error(Disconnected)`. A stream that has
    a `finish_reason` but no `[DONE]` before EOF: accept as complete only after a short bounded
    wait for the usage chunk (UNVERIFIED how each vendor behaves).
12. **Non-standard chunk fields**: Together `warnings`, `token_id`, `seed`, `top_logprobs`;
    Ollama `timings`, `system_fingerprint: "fp_ollama"`; Fireworks `perf_metrics`,
    `internal_content`; Groq `x_groq`. Parse leniently, no `deny_unknown_fields`; parse `type`
    -like discrimination in two steps as openai.md does for events.
13. **Chunk `id` not unique** (Ollama random number < 999): never key state by chunk `id`.
14. **Empty `choices`** can appear for usage chunks (all) and possibly for keep-alives
    (UNVERIFIED): treat `choices: []` as "no content", not an error. `choices[0]` only (n = 1).
15. **Role-only first chunk** `delta: {"role":"assistant","content":""}` (OpenAI) or
    `content: null`: empty string must not produce a `TextDelta`.

## Rate limits and error bodies

| | Headers (documented) | Notes |
|---|---|---|
| OpenAI | `x-ratelimit-limit-requests`, `-limit-tokens`, `-remaining-requests`, `-remaining-tokens`, `-reset-requests`, `-reset-tokens`, `x-request-id`, `Retry-After` sometimes (openai.md) | |
| Groq | `retry-after` (seconds, set only on 429; sample value 2), `x-ratelimit-limit-requests` (14400, always refers to daily RPD), `x-ratelimit-limit-tokens` (18000, always per-minute TPM), `x-ratelimit-remaining-requests` (14370), `x-ratelimit-remaining-tokens` (17997), `x-ratelimit-reset-requests` (2m59.56s), `x-ratelimit-reset-tokens` (7.66s) (page says the sample values are "illustrative"; the six `x-ratelimit-*` headers are on every response) | Example limits for `openai/gpt-oss-120b`: 30 RPM, 1K RPD, 8K TPM, 200K TPD; the page says the listed values are base Developer plan limits (the page does not say which plan that specific row is: UNVERIFIED). Limits per model and plan; check the account Limits page. Voice agents on low plans will hit TPM quickly |
| Together | none documented (rate-limits page names no headers, tiers or values) | 429 and 503 under high demand; "avoid sending large bursts"; "Use exponential backoff when retrying"; "Serverless performance is best-effort" |
| Fireworks | none documented on the rate-limits page (response-header names UNVERIFIED; the error-codes page mentions "rate-limit response headers" for serverless 429s without naming them) | Account-wide: no payment method/credits 10 RPM; with payment method and credits 6,000 RPM (fixed ceiling, "applies to all API usage on the account", "regardless of spending tier"); tiers by spend: Tier 1 valid payment method, Tier 2 $50, Tier 3 $500, Tier 4 $5,000 (spend or add credits), Unlimited (contact sales) set serverless TPM upper bounds (not RPM). Dedicated/on-demand: no account limits, 429 = saturation (the claim that such requests still count towards the account request limit is UNVERIFIED) |
| vLLM | none (deployment-defined) | UNVERIFIED |
| Ollama | none; queue-full -> 503 | `OLLAMA_MAX_QUEUE` 512, `OLLAMA_NUM_PARALLEL` 1 default |

Adapter rule: log `retry-after` and `x-ratelimit-*` when present (bounded body read, same limits as
openai.md: 8 KiB, 1 s), never retry inside the adapter.

## Mapping onto brisvox (`crates/brisvox-providers/src/llm.rs`)

- `LlmRequest { messages, tools }` -> `{"model", "messages":[...], "stream":true, "tools":[{"type":
  "function","function":{"name","description","parameters"}}]}`. `Role::Tool` ->
  `{"role":"tool","tool_call_id","content"}`; `Role::Assistant` with `tool_calls` ->
  `tool_calls:[{"id","type":"function","function":{"name","arguments"}}]` with `content` null/""
  when empty. `Message::provider_data` is ignored (not this adapter's). Tools whose
  `parameters_schema` is not valid JSON -> `Protocol` before any I/O. Omit `tools` when empty
  (some servers reject `tools: []`: UNVERIFIED, but omitting is safe).
- Events: `delta.content` (non-empty) -> `TextDelta`; `delta.tool_calls[]` -> `ToolCallDelta
  { index, id, name, arguments }` (fragment or whole); terminal chunk / `[DONE]` -> `Done`.
  Dense index renumbering is safe (key by upstream `index`, fallback by id).
- Pre-stream HTTP errors -> `Err(Rejected { status })`; in-stream error object -> `LlmEvent::Error`.
- Timeouts: connect, first-byte, first-output (longer for reasoning models), idle between events,
  total; none of the five publishes a keepalive spec (SSE comment lines `:` should be tolerated,
  they do not count as progress, same as openai.md).
- Cancel: drop `LlmStream` -> drop the HTTP body (closes the connection; only Fireworks documents
  billing stops).
- Warm-up: `HEAD`/connection pre-warm as in openai.md is optional; for Ollama a real tiny
  generation (native API) may be needed to load the model (see above). UNVERIFIED that
  `HEAD {base}/chat/completions` answers any status on all five.

## Latency-oriented model examples (docs list names/speeds only; no TTFT numbers anywhere)
- Groq: `llama-3.1-8b-instant` (560 tokens/s), `openai/gpt-oss-20b` (1,000 tokens/s),
  `openai/gpt-oss-120b` (500 tokens/s).
- Together: `Qwen/Qwen3.5-9B`, `zai-org/GLM-5.3-Flash`, `openai/gpt-oss-120b` (function calling
  listed yes for these).
- Fireworks: `accounts/fireworks/models/glm-5p3-flash`.
- Ollama: measured, see "Live results" in the Ollama section above (`gemma4:12b-it-qat` recommended); `gpt-oss:20b`, `qwen3:8b`, `llama3.2` are names from the vendor's examples (not measured).
- vLLM: whichever model the deployment serves.
Pricing pages were not fetched (UNVERIFIED for all vendors except the Together per-1M prices
above, which were re-checked 2026-10-08 on the serverless models page).

## Implementation notes
Code: `brisvox_providers::openai_compatible` (feature `provider-openai-compatible`), tests in
`crates/brisvox-providers/tests/openai_compatible/` (one loopback mock, one wire dialect per
preset, the shared contract suite once per preset, protocol tests, an opt-in live test).

- **Reading is one tolerant path, not a flag per vendor.** The docs disagree with themselves
  (Fireworks usage placement, Together `index: null`, Groq usage location), so the chunk mapper
  accepts every documented shape at once: tool calls as fragments or whole, missing / `null`
  `index`, empty or missing `id` (synthetic `call_<n>`), usage at the top level, in
  `x_groq.usage` or in the choice, finish words `stop` / `length` / `tool_calls` /
  `function_call` / `eos` / `content_filter` / unknown, `arguments` as a string (or, defensively,
  a JSON object), error bodies as objects or strings, `x_groq.error`. `Quirks` only decides
  what is sent and how long the end of a response waits (see below).
- **Call identity.** A fragment with an `index` belongs to the newest call with that index,
  unless it carries a different non-empty `id` (then it opens a new call: a vendor numbering
  every call 0), and so does a fragment whose `name` differs from the name the call already
  has (whole calls of an Ollama-style vendor may all say `index: 0` and carry no id; a repeated
  identical name is taken as a continuation). Without an `index`: a known `id` continues that
  call; no `id` but a `name` opens a call; neither continues the newest call. Names are emitted
  once.
- **Finish.** A response that accumulated tool calls ends in `ToolCalls` when the vendor said
  `stop` / `eos` / unknown (vLLM keeps `stop` for a named `tool_choice`), but stays `Length`
  when it was cut by `length` (the arguments may be truncated; the caller must not run them).
- **When `Done` is emitted.** At the chunk with `finish_reason` if that chunk carries usage or
  `Quirks::usage` is `FinishChunk` / `Absent`; otherwise (`TrailingChunk`, the default and the
  presets for Groq, Fireworks, vLLM, Ollama) at the usage chunk, `[DONE]`, EOF or after
  `usage_grace` (250 ms), whichever is first. Only usage on the finish chunk itself counts as
  final: running totals on earlier chunks (vLLM `continuous_usage_stats`) are kept, but the
  response still waits for the trailing chunk and reports the last usage seen. With
  `FinishChunk` / `Absent` a separate usage chunk after the finish chunk is deliberately not
  waited for (the preset says there is none; a vendor that sends one belongs on
  `TrailingChunk`). A stream that ends or breaks (EOF, reset, cut chunked body) after its
  `finish_reason` is complete; one that ends before it is `Disconnected`; `[DONE]` without any
  `finish_reason` completes as `Stop` when no tool call is open (text only). **Decision (review
  fix): `[DONE]` without a `finish_reason` while tool-call fragments were seen is a cut, mapped to
  `Disconnected` exactly like an end of body there**: a proxy or gateway can abort a generation
  mid call and still write `[DONE]`, and the consumer must not get `Done { ToolCalls }` with
  truncated arguments (a SideEffect tool could run on them). The conservative side was chosen
  because a well-formed tool-call turn always carries a `finish_reason` on all five vendors'
  documented streams; a custom server that omits it for tool calls will see `Disconnected`.
- **Decision: tool-call shape, `finish_reason` variants and error body shapes are read by one
  tolerant path, not selected by per-vendor flags** (the original plan asked for quirk flags for
  them). Every shape the docs list is accepted at once, so a preset cannot be wrong about them
  and a custom server needs no flag; `Quirks` keeps only what changes the request or the wait
  (`max_tokens_field`, `request_stream_usage`, `usage`, `parallel_tool_calls`, `tool_choice`) and
  the one read-side switch `synthesize_missing_ids` (off: a call without an id is a protocol
  error). Not offered: forcing fragmented-only parsing, or making `stop` with tool calls an
  error. Add a flag when a live run shows a vendor whose stream the tolerant path misreads.
- **URL assembly.** The chat URL is built with a real URL parser, not by string concatenation:
  `chat_path` is appended to the path of `base_url`, the query of `base_url` (`?key=...`) stays
  a query after the path, a query on `chat_path` is appended to it with `&`, and fragments are
  dropped (they are never sent). `https://gw.example/v1?key=abc` + `/chat/completions` posts to
  `/v1/chat/completions?key=abc`. A base URL that cannot carry a path (`mailto:`) is refused at
  construction.
- **Secrets in the endpoint.** The base URL and chat path may carry credentials. They are never
  logged (transport errors are logged by class, not with reqwest's URL-bearing Display), and
  `Debug` of the configuration prints scheme, host and port plus header and body-field *names*.
  `extra_body` must not set `temperature` / `parallel_tool_calls` while the adapter sets them
  (no duplicate keys in the body).
- **Never spoken / never used.** `delta.reasoning`, `reasoning_content`, `internal_content`,
  `refusal`, `annotations`, `x_groq` extras, `timings`, `warnings`, the chunk `id`. A `refusal`
  is dropped, so a model that refuses answers with silence (UNVERIFIED whether any of the five
  sends refusals on Chat Completions; revisit with the live run).
- **Sent only when configured**: `stream_options.include_usage` (Groq, vLLM, Ollama presets),
  `parallel_tool_calls`, `tool_choice` (no preset sets them: Ollama has no `tool_choice`, the
  parallel-call support of Groq differs per model), `max_completion_tokens` instead of
  `max_tokens` (Groq preset). `tools` is omitted when empty. Replayed assistant tool calls with
  empty arguments are sent as `"{}"` (a server that parses the string back would reject `""`),
  an assistant turn that only called tools has `content: null`. `Message::provider_data` is
  ignored.
- **Errors.** Pre-stream non-2xx is `Rejected { status }` unchanged (including Together's 403
  for context overflow, Groq 498/499, Fireworks 520, Together 524/529). `Retry-After` and the
  sanitised error `type` / `code` are logged, the message never. In-stream `error` (object or
  string): a numeric `code` / `status` 400-599 is that status; `rate_limit_*` -> 429,
  `server_error` / `api_error` -> 500, `overloaded` / `service_unavailable` -> 503,
  `invalid_request_error` -> 400, `authentication_error` -> 401; anything else is `Protocol`.
  Ollama's native mid-stream NDJSON error line (`{"error": "..."}` without `data:`) is not SSE:
  the SSE parser ignores it and the response ends as `Disconnected` (UNVERIFIED whether the
  `/v1` path ever emits it).
- **Warm-up** is an unauthenticated `HEAD` to the chat URL (UNVERIFIED that every vendor answers
  it; any HTTP status counts, a refused connection is `Disconnected`). It does not load an
  Ollama model: send a real tiny request before the call (see Ollama above).
- **Usage cached tokens** are read from `usage.prompt_tokens_details.cached_tokens` only
  (OpenAI, Ollama); other locations Together mentions are UNVERIFIED, so those vendors report 0.
- **Bounds**: 128 tool calls, 1 MiB arguments per call, 4 MiB per response, SSE event cap 1 MiB
  (all `Protocol` when exceeded), event channel 64 with back-pressure.
- **Plain http**: allowed for loopback; a vLLM / Ollama on another host needs
  `allow_insecure_http`.
- **Live**: `BRISVOX_LIVE_TESTS=1` plus `GROQ_API_KEY` / `TOGETHER_API_KEY` /
  `FIREWORKS_API_KEY` and/or `OPENAI_COMPATIBLE_BASE_URL` + `OPENAI_COMPATIBLE_MODEL`
  (+ `OPENAI_COMPATIBLE_API_KEY`). It prints the number of tool-call fragments and the time to
  first token per vendor; use the output to fill in the UNVERIFIED items below
.

### Live-test acceptance
Blocked: needs API keys (Groq, Together, Fireworks) and a local vLLM / Ollama.
- `BRISVOX_LIVE_TESTS=1` with the keys in `.env`: `tests/openai_compatible/live.rs` passes per
  vendor (streaming text and a one-tool loop).
- Confirm or correct every item in the UNVERIFIED list below, and fix the mock dialects in
  `tests/openai_compatible/wire.rs` where a live capture differs: tool-call fragmentation per
  vendor, usage placement and `stream_options` acceptance, `finish_reason` words, mid-stream
  error body shapes, whether the five vendors send `[DONE]` after a tool-call `finish_reason`,
  `HEAD` warm-up answers.

## UNVERIFIED (consolidated)
- Wire captures of `delta.tool_calls` for every vendor (no live run; docs have no chunk JSON
  except Together's single "later delta" and the OpenAI first chunk).
- Groq: placement of streaming usage (top-level vs `x_groq.usage`), `stream_options.include_usage`
  acceptance (not in groq-python params), whether tool-call arguments are fragmented, mid-stream
  error delivery (`x_groq.error`), Authorization header spelled out as text, exact
  `failed_generation` JSON values, `reasoning_format` values other than `parsed`.
- Together: `stream_options` / `include_usage` and `parallel_tool_calls` parameters, usage in the
  last chunk, rate-limit headers, JSON error body, streaming `index` always integer, 400 vs 403 for
  context overflow, verbatim wording of the overview-page streaming sentence.
- Fireworks: which usage placement is real (pages conflict), error JSON body, response rate-limit
  headers, small/fast model list, `parallel_tool_calls` per-model support, named `tool_choice` form
  (conflict `{"type":"function","name"}` vs nested), `tool_choice` enumeration, `buffer_ms`,
  `prompt_cache_isolation_key`, dedicated-deployment model id format.
- vLLM: default host/port, auth header name, streaming fragmentation per parser (only `hf`
  documented as whole), exact failure behaviour without `--enable-auto-tool-choice`, mid-stream
  error body shape, `stream_options` acceptance in docs (source shows `include_usage`),
  `max_tokens` vs `max_completion_tokens` handling, 429/503 behaviour, full `vllm serve` flags.
- Ollama: whether streamed tool calls are always whole, whether `max_completion_tokens` /
  `parallel_tool_calls` are ignored or rejected, mid-stream error shape on `/v1`, mapping of
  `tool_call_id` back to a call, `Authorization` handling for a reverse-proxied local server,
  `handleNativeChat` tool-call id assignment.
- Turkish support of every listed model and vendor; regions/data residency of every vendor;
  keepalive/idle timeouts of every vendor; pricing; TTFT numbers.
- openai-python `_deltas.py` merge rules (404 in the fetch); the openai.md claim about GPT-6 and
  Responses.

## Sources (all fetched 2026-10-08)
- OpenAI chunk reference: https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events
- OpenAI SDK: https://raw.githubusercontent.com/openai/openai-python/main/src/openai/types/chat/chat_completion_chunk.py ,
  .../types/chat/completion_create_params.py , .../lib/streaming/chat/_completions.py ,
  https://developers.openai.com/api/docs/guides/function-calling
- Groq: https://console.groq.com/docs/openai , https://console.groq.com/docs/tool-use ,
  https://console.groq.com/docs/tool-use/local-tool-calling , https://console.groq.com/docs/api-reference ,
  https://console.groq.com/docs/rate-limits , https://console.groq.com/docs/errors ,
  https://console.groq.com/docs/models , https://console.groq.com/docs/text-chat ,
  https://raw.githubusercontent.com/groq/groq-python/main/src/groq/types/chat/chat_completion_chunk.py ,
  .../completion_create_params.py
- Together: https://docs.together.ai/docs/openai-api-compatibility ,
  https://docs.together.ai/docs/function-calling ,
  https://docs.together.ai/docs/inference/function-calling/single-call.md ,
  .../parallel.md , .../overview.md , .../best-practices.md ,
  https://docs.together.ai/reference/chat-completions.md ,
  https://docs.together.ai/docs/error-codes.md ,
  https://docs.together.ai/docs/serverless/rate-limits.md , https://docs.together.ai/docs/rate-limits ,
  https://docs.together.ai/docs/serverless/models.md ,
  https://docs.together.ai/docs/inference/chat/parameters.md , https://docs.together.ai/llms.txt
- Fireworks: https://docs.fireworks.ai/tools-sdks/openai-compatibility ,
  https://docs.fireworks.ai/api-reference/post-chatcompletions ,
  https://docs.fireworks.ai/guides/function-calling ,
  https://docs.fireworks.ai/guides/querying-text-models ,
  https://docs.fireworks.ai/guides/quotas_usage/rate-limits ,
  https://docs.fireworks.ai/guides/inference-error-codes ,
  https://docs.fireworks.ai/getting-started/quickstart , https://docs.fireworks.ai/models/overview
- OpenRouter: https://openrouter.ai/docs/api-reference/overview (fetched 2026-10-14)
- vLLM: https://docs.vllm.ai/en/latest/serving/online_serving/openai_compatible_server/ ,
  https://docs.vllm.ai/en/latest/features/tool_calling/ ,
  https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/entrypoints/openai/chat_completion/serving.py ,
  https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/entrypoints/chat_utils.py
- Ollama: https://docs.ollama.com/api/openai-compatibility , https://docs.ollama.com/api/authentication ,
  https://docs.ollama.com/api/errors , https://docs.ollama.com/api/usage ,
  https://docs.ollama.com/capabilities/tool-calling , https://docs.ollama.com/capabilities/streaming ,
  https://docs.ollama.com/faq ,
  https://raw.githubusercontent.com/ollama/ollama/main/openai/openai.go ,
  .../middleware/openai.go , .../server/routes.go , .../api/types.go

Fact-checked 2026-10-08: 78 claims checked, 13 corrected. Corrections: (1) Groq local-tool-calling page has no "Accumulate `delta.content` and `delta.tool_calls`" sentence, only a code example (`collected_tool_calls.extend`); (2) Groq "tool_call_id must match" quote not found verbatim, replaced by the page's actual wording; (3) Groq rate limits: values are "base Developer plan" limits, header sample values "illustrative", `retry-after` sample 2; (4) Groq `delta.reasoning` only populated when `reasoning_format` is `parsed`; (5) groq-python params do not define `stream_options` (only the API reference does); (6) Groq errors page does show a `{message, type}` body example; Groq tool_choice 400 behaviours and `qwen`/`minimax` models (preview, not production) clarified; (7) Fireworks: "finish_reason set" and "Fireworks extension" wording is from querying-text-models only, compat page says "very last chunk"; (8) Together context overflow: reference says 400 for `context_length_exceeded_behavior=error`, error-codes page says 403 (conflict recorded); Together 429/524/529 meanings and Llama 3.1 8B statement (page silent) fixed; (9) Mistral 9-digit ids apply to the Transformers tokenization backend only and are not in `chat_utils.py`; (10) vLLM serialisation is `exclude_none` (the `exclude_unset` claim unconfirmed), `include_continuous_usage` also covers initial/echo chunks; vLLM parser list is longer; (11) openai-python partial-JSON parsing applies only to strict tools; (12) Ollama chunk id is `chatcmpl-%d` with `rand.Intn(999)`; `timings` only on the usage chunk; stream_options only `include_usage`; extra model examples `qwen3-vl:8b`; (13) Together `Qwen3.8-Flash` context is 1,000,000 and prices added; Fireworks 6,000 RPM is a fixed ceiling and the "requests still count towards the account limit" claim is unconfirmed. Items downgraded to UNVERIFIED: see the consolidated list above.
