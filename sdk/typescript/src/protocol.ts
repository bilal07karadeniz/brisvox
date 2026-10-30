/**
 * Typed messages of the brisvox agent protocol, version 1.
 *
 * This module is the TypeScript image of `docs/AGENT_PROTOCOL.md` (and of
 * `crates/brisvox-agent-link/src/protocol.rs`, the executable definition): every message the
 * server sends is a readonly object with a `type` discriminant, every message the agent sends
 * is built by a function that returns the wire object. Both directions are plain JSON objects.
 *
 * Field names are camelCase here (`tMs`, `sessionId`, `idempotencyKey`) and snake_case on the
 * wire; the parser and the builders are the only places that know both.
 *
 * Parsing is strict about what the protocol requires (a missing or mistyped field throws a
 * {@link ProtocolError} with code `bad_message`) and lenient about everything else (extra fields
 * are ignored, an unknown `type` becomes an {@link UnknownMessage}).
 *
 * @module
 */

/** The `protocol` field of `session.start`. */
export const PROTOCOL = "brisvox.agent";

/** The protocol versions this SDK speaks. */
export const SUPPORTED_VERSIONS: readonly number[] = [1];

/** Largest message either way; the server closes a link that sends a larger one. */
export const MAX_MESSAGE_BYTES = 64 * 1024;
/** Largest `say.text` (and `end.say`, and the greeting). */
export const MAX_LINE_BYTES = 4 * 1024;
/** Largest `transfer.summary` / `transfer.reason` / `end.reason`. */
export const MAX_NOTE_BYTES = 512;
/** Largest command `id`. */
export const MAX_COMMAND_ID_BYTES = 64;
/** Most tools in `session.accept`. */
export const MAX_TOOLS = 64;
/** Most messages in `session.accept.context` and in one `context.update`. */
export const MAX_CONTEXT_MESSAGES = 32;
/** Longest system prompt. */
export const MAX_SYSTEM_PROMPT_BYTES = 64 * 1024;
/** Largest `timeout_ms` of a tool declaration. */
export const MAX_TOOL_TIMEOUT_MS = 30_000;

/** The event types an agent may subscribe to in `session.accept.events`. */
export const EVENT_TYPES = [
  "turn.user_started",
  "turn.user_ended",
  "turn.bot_started",
  "turn.bot_ended",
  "turn.interrupted",
  "turn.metrics",
  "user.transcript",
  "user.text",
  "bot.llm_started",
  "bot.text",
  "bot.llm_stopped",
  "bot.tts_text",
  "bot.tts_stopped",
  "language.switched",
] as const;

/** A subscribable event type (see {@link EVENT_TYPES}). */
export type SubscribableEventType = (typeof EVENT_TYPES)[number];

/** The error codes of the protocol (`command.result.error.code`, `session.reject.code`). */
export const ErrorCode = {
  /** Not JSON, a missing field, a field of the wrong type. */
  BadMessage: "bad_message",
  /** A `type` this version does not know. */
  UnknownType: "unknown_type",
  /** A value breaks a rule (an empty or too long text, a bad phone number). */
  InvalidArgument: "invalid_argument",
  /** A count bound was hit (messages of one `context.update`, tool names). */
  TooLarge: "too_large",
  /** The call's input queue is full: retry later. */
  QueueFull: "queue_full",
  /** The call cannot do this (no call-control for `transfer`). */
  Unsupported: "unsupported",
  /** The call is over. */
  CallEnded: "call_ended",
  /** A handshake message after the handshake. */
  NotReady: "not_ready",
  /** The carrier refused the transfer; nothing changed. */
  Rejected: "rejected",
  /** The carrier could not be reached; nothing changed. */
  Unavailable: "unavailable",
  /** The transfer may or may not have started (a timeout): do not repeat it blindly. */
  OutcomeUnknown: "outcome_unknown",
  /** For `session.reject`: the agent speaks none of the offered versions. */
  UnsupportedVersion: "unsupported_version",
  /** For `session.reject` of a resumption: the agent does not know the call (it restarted, or the parked call expired). */
  UnknownSession: "unknown_session",
} as const;

/** One of the protocol's error codes. */
export type ErrorCode = (typeof ErrorCode)[keyof typeof ErrorCode];

const ERROR_CODE_VALUES: ReadonlySet<string> = new Set(Object.values(ErrorCode));

/** A message that breaks the protocol (either direction). `code` is what the server would answer with. */
export class ProtocolError extends Error {
  /** The code the server would answer with. */
  readonly code: ErrorCode;

  constructor(message: string, code: ErrorCode = ErrorCode.BadMessage) {
    super(message);
    this.name = "ProtocolError";
    this.code = code;
  }
}

/** `command.result.error`: a machine-readable `code` and a human `message`. */
export interface WireError {
  /** An {@link ErrorCode} when the SDK knows it, else the raw string (a code added within version 1). */
  readonly code: ErrorCode | (string & {});
  /** The server's explanation. */
  readonly message: string;
}

/** What the server can do on this call (`session.start.capabilities`). */
export interface Capabilities {
  /** Whether `transfer` can work (a carrier call-control is bound). */
  readonly transfer: boolean;
  /** Whether remote tools are available. */
  readonly tools: boolean;
}

/** Turn latencies in milliseconds (`turn.metrics`); a field is absent when unknown. */
export interface Metrics {
  /** End of the user's speech to the end-of-turn decision. */
  readonly endOfTurnMs?: number;
  /** End of the user's speech to the STT final. */
  readonly sttFinalMs?: number;
  /** LLM time to first token. */
  readonly llmTtftMs?: number;
  /** TTS time to first byte. */
  readonly ttsTtfbMs?: number;
  /** End of the user's speech to the first audio out. */
  readonly responseMs?: number;
}

/** Provider-reported LLM token usage of a response (`bot.llm_stopped.usage`). */
export interface TokenUsage {
  /** Prompt tokens. */
  readonly inputTokens: number;
  /** Of `inputTokens`, the tokens served from the provider's prompt cache. */
  readonly cachedInputTokens: number;
  /** Generated tokens. */
  readonly outputTokens: number;
}

/** Average and worst value of one latency stage over the turns of a call, in milliseconds. */
export interface StageLatency {
  readonly avgMs: number;
  readonly maxMs: number;
}

/** The per-call latency stages of `session.usage`; a stage no turn had is absent. */
export interface LatencySummary {
  /** The end-of-turn policy's wait after the caller stopped speaking. */
  readonly endOfTurn?: StageLatency;
  /** End of the caller's speech to the final transcript. */
  readonly sttFinal?: StageLatency;
  /** LLM request to first token. */
  readonly llmTtft?: StageLatency;
  /** TTS request to first audio. */
  readonly ttsTtfb?: StageLatency;
  /** End of the caller's speech to the first bot audio (server side). */
  readonly response?: StageLatency;
}

/** What the LLM provider billed in a call (discarded speculative responses included). */
export interface LlmUsage {
  readonly provider: string;
  readonly model: string;
  readonly inputTokens: number;
  readonly cachedInputTokens: number;
  readonly outputTokens: number;
  /** Responses that reported no usage: the token counts are lower bounds by those. */
  readonly responsesWithoutUsage: number;
}

/** The speech-to-text audio sent in a call. */
export interface SttUsage {
  readonly provider: string;
  readonly model: string;
  readonly audioSeconds: number;
}

/** The text-to-speech characters sent in a call. */
export interface TtsUsage {
  readonly provider: string;
  readonly model: string;
  readonly characters: number;
}

/**
 * The provider units of a call; a part is absent when the call did not run that stage (a
 * speech-to-speech call has no separate STT, LLM or TTS). Units, not money.
 */
export interface UsageSummary {
  readonly llm?: LlmUsage;
  readonly stt?: SttUsage;
  readonly tts?: TtsUsage;
}

// ---------------------------------------------------------------------------------------------
// Server -> agent
// ---------------------------------------------------------------------------------------------

/** Envelope of a server message. */
export interface Envelope {
  /** The message number on the link (1 for `session.start`). */
  readonly seq: number;
  /** Milliseconds since the link opened, on the call's clock. */
  readonly tMs: number;
}

/**
 * Who is on the line (`session.start.call`): present only for a telephony call (Twilio, Telnyx,
 * SIP) whose agent definition opted in (`[agent_link] caller_identity = true`). The numbers are
 * what the carrier says; a caller ID can be spoofed, so do not treat it as proof of identity on
 * its own.
 */
export interface CallIdentity {
  /** `twilio`, `telnyx` or `sip`. */
  readonly carrier: string;
  /** The carrier's id of the call: Twilio `CallSid`, Telnyx `call_control_id`, SIP `Call-ID`. */
  readonly carrierCallId: string;
  /**
   * The caller's number (the SIP user part of `From`). Absent when the carrier did not say, and
   * when the server redacts phone numbers ({@link CallIdentity.piiRedacted}). On an outbound
   * call it is the number the call is placed from.
   */
  readonly from?: string;
  /** The called number, with the same absence rules as `from`. */
  readonly to?: string;
  /** `true`: the numbers were withheld because the server redacts phone numbers. */
  readonly piiRedacted: boolean;
}

/** `session.start.resume`: this `session.start` takes a call up again on a new link. */
export interface ResumeInfo {
  /** Which reconnection attempt of this outage it is (1 for the first). */
  readonly attempt: number;
  /** The last `seq` written on the lost link; the new link continues the count. */
  readonly previousSeq: number;
  /** Milliseconds since the loss. */
  readonly downMs: number;
}

/** The first message: a call started and offers its protocol versions. */
export interface SessionStart extends Envelope {
  readonly type: "session.start";
  /** Always {@link PROTOCOL}. */
  readonly protocol: string;
  /** The versions the server speaks. */
  readonly versions: readonly number[];
  /** The server's id of the call. */
  readonly sessionId: string;
  /** The name of the agent definition the call runs. */
  readonly agent: string;
  /** What the server can do on this call. */
  readonly capabilities: Capabilities;
  /** Set when the server reconnects to a call that lost its link (see "Reconnecting"). */
  readonly resume?: ResumeInfo;
  /**
   * Who is on the line. Absent unless the agent definition opted in and the call has a carrier
   * (see "Caller identity").
   */
  readonly call?: CallIdentity;
}

/** The last message (`reason`: `call_ended`, `hangup`, `ended_by_agent`, ...). */
export interface SessionEnd extends Envelope {
  readonly type: "session.end";
  readonly reason: string;
}

/**
 * What the call used and how fast it was, sent once right before `session.end` (always, whatever
 * the agent subscribed to; missing when the link was down at the end of the call). Units, not
 * money: multiply by your providers' prices.
 */
export interface SessionUsage extends Envelope {
  readonly type: "session.usage";
  /** From the start to the end of the call, on the server's clock. */
  readonly durationMs: number;
  /** Turns that finished. */
  readonly turns: number;
  /** Of `turns`, the ones the caller interrupted. */
  readonly interruptedTurns: number;
  readonly latency: LatencySummary;
  readonly usage: UsageSummary;
}

/** A user turn started (speech; also a confirmed barge-in). */
export interface TurnUserStarted extends Envelope {
  readonly type: "turn.user_started";
  readonly epoch: number;
}

/** The user turn ended; the engine answers it now. */
export interface TurnUserEnded extends Envelope {
  readonly type: "turn.user_ended";
  readonly epoch: number;
}

/** The bot's audio of `epoch` started playing. */
export interface TurnBotStarted extends Envelope {
  readonly type: "turn.bot_started";
  readonly epoch: number;
}

/** The bot's audio of `epoch` was played to its end. */
export interface TurnBotEnded extends Envelope {
  readonly type: "turn.bot_ended";
  readonly epoch: number;
}

/** A barge-in: the call moved to `epoch`; everything older was dropped. */
export interface TurnInterrupted extends Envelope {
  readonly type: "turn.interrupted";
  readonly epoch: number;
}

/** The turn's latencies when its first audio went out. */
export interface TurnMetrics extends Envelope {
  readonly type: "turn.metrics";
  readonly epoch: number;
  readonly metrics: Metrics;
}

/** A transcript segment from the STT (interim when `final` is false). */
export interface UserTranscript extends Envelope {
  readonly type: "user.transcript";
  readonly text: string;
  readonly final: boolean;
}

/** The whole text of the user turn the LLM answers (spoken finals and typed text). */
export interface UserText extends Envelope {
  readonly type: "user.text";
  readonly epoch: number;
  readonly text: string;
}

/** The LLM started a response round (a tool loop has several). */
export interface BotLlmStarted extends Envelope {
  readonly type: "bot.llm_started";
  readonly epoch: number;
}

/** A streamed LLM delta. A bot line arrives as one delta. */
export interface BotText extends Envelope {
  readonly type: "bot.text";
  readonly epoch: number;
  readonly text: string;
}

/** The LLM finished the response of `epoch` (the end of its last round). */
export interface BotLlmStopped extends Envelope {
  readonly type: "bot.llm_stopped";
  readonly epoch: number;
  /**
   * What the provider reported for the response, summed over the rounds of the turn; absent when
   * it reported none (an older server sends none either).
   */
  readonly usage?: TokenUsage;
}

/** A chunk sent to the TTS. */
export interface BotTtsText extends Envelope {
  readonly type: "bot.tts_text";
  readonly epoch: number;
  readonly text: string;
}

/** The TTS produced the response's last audio. */
export interface BotTtsStopped extends Envelope {
  readonly type: "bot.tts_stopped";
  readonly epoch: number;
}

/**
 * The agent follows the caller into another language: the replies from the next turn on are in
 * `to` (language codes such as `en`, `tr`).
 */
export interface LanguageSwitched extends Envelope {
  readonly type: "language.switched";
  readonly from: string;
  readonly to: string;
}

/**
 * A stage of the call reported an error, or the link refused something without a command id
 * (`stage === "agent_link"`, the message then starts with the error code).
 */
export interface ErrorEvent extends Envelope {
  readonly type: "error";
  readonly stage: string;
  readonly message: string;
  readonly fatal: boolean;
}

/** `count` events never reached the agent (see "Backpressure" in the protocol doc). */
export interface EventsLost extends Envelope {
  readonly type: "events.lost";
  readonly count: number;
}

/**
 * Not a wire message: the SDK's own event, delivered to `onResumed` handlers once a lost link of
 * the call was taken up again (after the events queued before the loss). `seq` is that of the
 * resuming `session.start`.
 */
export interface Resumed extends Envelope {
  readonly type: "session.resumed";
  /** Which reconnection attempt of the outage it was. */
  readonly attempt: number;
  /** The last `seq` written on the lost link. */
  readonly previousSeq: number;
  /**
   * The `seq` of the last server message this agent itself read on the lost link; equal to
   * `previousSeq` when nothing was lost in flight.
   */
  readonly seenSeq: number;
  /** Milliseconds the link was down. */
  readonly downMs: number;
}

/** The outcome of a command that carried an `id`. */
export interface CommandResult extends Envelope {
  readonly type: "command.result";
  readonly id: string;
  readonly ok: boolean;
  readonly error?: WireError;
}

/** Run the tool and answer with `tool.result` echoing `id`. */
export interface ToolCall extends Envelope {
  readonly type: "tool.call";
  readonly id: number;
  readonly callId: string;
  readonly name: string;
  readonly arguments: Readonly<Record<string, unknown>>;
  readonly kind: ToolKind;
  readonly epoch: number;
  /** 16 hex digits, stable for the tool call of the call: pass it to APIs that deduplicate. */
  readonly idempotencyKey: string;
  readonly timeoutMs: number;
}

/**
 * The server stopped waiting for tool call `id` (`reason`: `interrupted`, `timeout`,
 * `abandoned`): a later result is ignored. Advice only for a `side_effect` tool.
 */
export interface ToolCancel extends Envelope {
  readonly type: "tool.cancel";
  readonly id: number;
  readonly reason: string;
}

/** A message whose `type` this SDK does not know (an addition within version 1). */
export interface UnknownMessage extends Envelope {
  readonly type: "unknown";
  /** The wire `type`. */
  readonly wireType: string;
  /** The whole object as received. */
  readonly raw: Readonly<Record<string, unknown>>;
}

/** Whether a tool only reads (`read_only`) or changes the world (`side_effect`). */
export type ToolKind = "read_only" | "side_effect";

/** Every message the server sends to an agent that handlers can see as an event. */
export type AgentEvent =
  | SessionEnd
  | SessionUsage
  | TurnUserStarted
  | TurnUserEnded
  | TurnBotStarted
  | TurnBotEnded
  | TurnInterrupted
  | TurnMetrics
  | UserTranscript
  | UserText
  | BotLlmStarted
  | BotText
  | BotLlmStopped
  | BotTtsText
  | BotTtsStopped
  | LanguageSwitched
  | ErrorEvent
  | EventsLost
  | Resumed;

/** Every message the server can send. */
export type ServerMessage =
  | SessionStart
  | Exclude<AgentEvent, Resumed>
  | CommandResult
  | ToolCall
  | ToolCancel
  | UnknownMessage;

/** Wire `type` to message type, for every message a handler can be registered on. */
export interface EventMap {
  "session.end": SessionEnd;
  "session.usage": SessionUsage;
  "turn.user_started": TurnUserStarted;
  "turn.user_ended": TurnUserEnded;
  "turn.bot_started": TurnBotStarted;
  "turn.bot_ended": TurnBotEnded;
  "turn.interrupted": TurnInterrupted;
  "turn.metrics": TurnMetrics;
  "user.transcript": UserTranscript;
  "user.text": UserText;
  "bot.llm_started": BotLlmStarted;
  "bot.text": BotText;
  "bot.llm_stopped": BotLlmStopped;
  "bot.tts_text": BotTtsText;
  "bot.tts_stopped": BotTtsStopped;
  "language.switched": LanguageSwitched;
  error: ErrorEvent;
  "events.lost": EventsLost;
  "session.resumed": Resumed;
  "command.result": CommandResult;
}

/** A key of {@link EventMap}. */
export type EventType = keyof EventMap;

/** Every wire type a handler can be registered on (the events, `command.result`). */
export const HANDLER_TYPES: ReadonlySet<string> = new Set<EventType>([
  ...EVENT_TYPES,
  "session.end",
  "session.usage",
  "error",
  "events.lost",
  "session.resumed",
  "command.result",
]);

// --- parsing ---------------------------------------------------------------------------------

type Obj = Readonly<Record<string, unknown>>;

function isObject(value: unknown): value is Obj {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function needInt(obj: Obj, key: string, what: string): number {
  const value = field(obj, key, what);
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new ProtocolError(`${what}: field '${key}' has the wrong type`);
  }
  return value;
}

function needStr(obj: Obj, key: string, what: string): string {
  const value = field(obj, key, what);
  if (typeof value !== "string") {
    throw new ProtocolError(`${what}: field '${key}' has the wrong type`);
  }
  return value;
}

function needBool(obj: Obj, key: string, what: string): boolean {
  const value = field(obj, key, what);
  if (typeof value !== "boolean") {
    throw new ProtocolError(`${what}: field '${key}' has the wrong type`);
  }
  return value;
}

function field(obj: Obj, key: string, what: string): unknown {
  if (!Object.hasOwn(obj, key)) {
    throw new ProtocolError(`${what}: missing field '${key}'`);
  }
  return obj[key];
}

function optNumber(obj: Obj, key: string): number | undefined {
  const value = obj[key];
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function optStr(obj: Obj, key: string): string | undefined {
  const value = obj[key];
  return typeof value === "string" ? value : undefined;
}

function parseCall(raw: unknown): CallIdentity | undefined {
  if (raw === undefined || raw === null) {
    return undefined;
  }
  if (!isObject(raw)) {
    throw new ProtocolError("session.start: call must be an object");
  }
  const from = optStr(raw, "from");
  const to = optStr(raw, "to");
  return {
    carrier: needStr(raw, "carrier", "session.start.call"),
    carrierCallId: needStr(raw, "carrier_call_id", "session.start.call"),
    ...(from === undefined ? {} : { from }),
    ...(to === undefined ? {} : { to }),
    piiRedacted: raw["pii_redacted"] === true,
  };
}

function parseMetrics(raw: unknown): Metrics {
  if (!isObject(raw)) {
    return {};
  }
  const out: {
    -readonly [K in keyof Metrics]: Metrics[K];
  } = {};
  const pairs: [keyof Metrics, string][] = [
    ["endOfTurnMs", "end_of_turn_ms"],
    ["sttFinalMs", "stt_final_ms"],
    ["llmTtftMs", "llm_ttft_ms"],
    ["ttsTtfbMs", "tts_ttfb_ms"],
    ["responseMs", "response_ms"],
  ];
  for (const [name, wire] of pairs) {
    const value = optNumber(raw, wire);
    if (value !== undefined) {
      out[name] = value;
    }
  }
  return out;
}

function optCount(obj: Obj, key: string): number {
  const value = obj[key];
  return typeof value === "number" && Number.isInteger(value) ? value : 0;
}

function parseTokenUsage(raw: unknown): TokenUsage | undefined {
  // An optional addition: a malformed one is ignored, the event still counts.
  if (!isObject(raw)) {
    return undefined;
  }
  return {
    inputTokens: optCount(raw, "input_tokens"),
    cachedInputTokens: optCount(raw, "cached_input_tokens"),
    outputTokens: optCount(raw, "output_tokens"),
  };
}

function parseStageLatency(raw: unknown): StageLatency | undefined {
  if (!isObject(raw)) {
    return undefined;
  }
  const avgMs = optNumber(raw, "avg_ms");
  const maxMs = optNumber(raw, "max_ms");
  return avgMs === undefined || maxMs === undefined ? undefined : { avgMs, maxMs };
}

function parseLatencySummary(raw: unknown): LatencySummary {
  if (!isObject(raw)) {
    return {};
  }
  const out: { -readonly [K in keyof LatencySummary]: LatencySummary[K] } = {};
  const pairs: [keyof LatencySummary, string][] = [
    ["endOfTurn", "end_of_turn"],
    ["sttFinal", "stt_final"],
    ["llmTtft", "llm_ttft"],
    ["ttsTtfb", "tts_ttfb"],
    ["response", "response"],
  ];
  for (const [name, wire] of pairs) {
    const stage = parseStageLatency(raw[wire]);
    if (stage !== undefined) {
      out[name] = stage;
    }
  }
  return out;
}

function parseUsageSummary(raw: unknown): UsageSummary {
  if (!isObject(raw)) {
    return {};
  }
  const names = (part: Obj): { provider: string; model: string } => ({
    provider: optStr(part, "provider") ?? "",
    model: optStr(part, "model") ?? "",
  });
  const { llm, stt, tts } = raw;
  return {
    ...(isObject(llm)
      ? {
          llm: {
            ...names(llm),
            inputTokens: optCount(llm, "input_tokens"),
            cachedInputTokens: optCount(llm, "cached_input_tokens"),
            outputTokens: optCount(llm, "output_tokens"),
            responsesWithoutUsage: optCount(llm, "responses_without_usage"),
          },
        }
      : {}),
    ...(isObject(stt)
      ? { stt: { ...names(stt), audioSeconds: optNumber(stt, "audio_seconds") ?? 0 } }
      : {}),
    ...(isObject(tts) ? { tts: { ...names(tts), characters: optCount(tts, "characters") } } : {}),
  };
}

function parseBody(kind: string, obj: Obj, env: Envelope): ServerMessage {
  const epoch = (): number => needInt(obj, "epoch", kind);
  const text = (): string => needStr(obj, "text", kind);
  switch (kind) {
    case "session.start": {
      const caps = isObject(obj["capabilities"]) ? obj["capabilities"] : {};
      const versions = field(obj, "versions", kind);
      if (
        !Array.isArray(versions) ||
        !versions.every((v) => typeof v === "number" && Number.isInteger(v))
      ) {
        throw new ProtocolError(`${kind}: versions must be an array of integers`);
      }
      const rawResume = obj["resume"];
      let resume: ResumeInfo | undefined;
      if (rawResume !== undefined && rawResume !== null) {
        if (!isObject(rawResume)) {
          throw new ProtocolError(`${kind}: resume must be an object`);
        }
        resume = {
          attempt: needInt(rawResume, "attempt", "session.start.resume"),
          previousSeq: needInt(rawResume, "previous_seq", "session.start.resume"),
          downMs: needInt(rawResume, "down_ms", "session.start.resume"),
        };
      }
      const call = parseCall(obj["call"]);
      return {
        type: "session.start",
        ...env,
        protocol: needStr(obj, "protocol", kind),
        versions: versions as number[],
        sessionId: needStr(obj, "session_id", kind),
        agent: needStr(obj, "agent", kind),
        capabilities: { transfer: caps["transfer"] === true, tools: caps["tools"] === true },
        ...(resume === undefined ? {} : { resume }),
        ...(call === undefined ? {} : { call }),
      };
    }
    case "session.end":
      return { type: kind, ...env, reason: needStr(obj, "reason", kind) };
    case "session.usage":
      return {
        type: kind,
        ...env,
        durationMs: needInt(obj, "duration_ms", kind),
        turns: needInt(obj, "turns", kind),
        interruptedTurns: needInt(obj, "interrupted_turns", kind),
        latency: parseLatencySummary(obj["latency"]),
        usage: parseUsageSummary(obj["usage"]),
      };
    case "bot.llm_stopped": {
      const usage = parseTokenUsage(obj["usage"]);
      return { type: kind, ...env, epoch: epoch(), ...(usage === undefined ? {} : { usage }) };
    }
    case "turn.user_started":
    case "turn.user_ended":
    case "turn.bot_started":
    case "turn.bot_ended":
    case "turn.interrupted":
    case "bot.llm_started":
    case "bot.tts_stopped":
      return { type: kind, ...env, epoch: epoch() };
    case "language.switched":
      return { type: kind, ...env, from: needStr(obj, "from", kind), to: needStr(obj, "to", kind) };
    case "turn.metrics":
      return { type: kind, ...env, epoch: epoch(), metrics: parseMetrics(obj["metrics"]) };
    case "user.transcript":
      return { type: kind, ...env, text: text(), final: needBool(obj, "final", kind) };
    case "user.text":
    case "bot.text":
    case "bot.tts_text":
      return { type: kind, ...env, epoch: epoch(), text: text() };
    case "error":
      return {
        type: kind,
        ...env,
        stage: needStr(obj, "stage", kind),
        message: needStr(obj, "message", kind),
        fatal: needBool(obj, "fatal", kind),
      };
    case "events.lost":
      return { type: kind, ...env, count: needInt(obj, "count", kind) };
    case "tool.call": {
      const toolKind = needStr(obj, "kind", kind);
      if (toolKind !== "read_only" && toolKind !== "side_effect") {
        throw new ProtocolError(`${kind}: unknown tool kind '${toolKind}'`);
      }
      const args = field(obj, "arguments", kind);
      if (!isObject(args)) {
        throw new ProtocolError(`${kind}: field 'arguments' has the wrong type`);
      }
      return {
        type: kind,
        ...env,
        id: needInt(obj, "id", kind),
        callId: needStr(obj, "call_id", kind),
        name: needStr(obj, "name", kind),
        arguments: args,
        kind: toolKind,
        epoch: epoch(),
        idempotencyKey: needStr(obj, "idempotency_key", kind),
        timeoutMs: needInt(obj, "timeout_ms", kind),
      };
    }
    case "tool.cancel":
      return {
        type: kind,
        ...env,
        id: needInt(obj, "id", kind),
        reason: needStr(obj, "reason", kind),
      };
    case "command.result": {
      const error = obj["error"];
      const base = { type: kind, ...env, id: needStr(obj, "id", kind), ok: needBool(obj, "ok", kind) } as const;
      if (isObject(error)) {
        const code = error["code"];
        return {
          ...base,
          error: {
            code: typeof code === "string" ? code : "",
            message: typeof error["message"] === "string" ? error["message"] : "",
          },
        };
      }
      return base;
    }
    default:
      return { type: "unknown", ...env, wireType: kind, raw: obj };
  }
}

/**
 * Parse one server message (a JSON text or an already decoded object).
 *
 * Fields the SDK does not know are ignored; an unknown `type` gives an {@link UnknownMessage}.
 *
 * @throws {ProtocolError} not JSON, not an object, no `type`, a binary frame, or a required
 * field is missing or has the wrong type.
 */
export function parseServerMessage(data: string | Uint8Array | object): ServerMessage {
  let obj: unknown;
  if (typeof data === "string") {
    try {
      obj = JSON.parse(data);
    } catch (error) {
      throw new ProtocolError(`not JSON: ${error instanceof Error ? error.message : String(error)}`);
    }
  } else if (data instanceof Uint8Array) {
    throw new ProtocolError("binary frames are not part of the protocol");
  } else {
    obj = data;
  }
  if (!isObject(obj)) {
    throw new ProtocolError("a message must be a JSON object");
  }
  const kind = obj["type"];
  if (typeof kind !== "string") {
    throw new ProtocolError("missing string field 'type'");
  }
  const seq = obj["seq"];
  const tMs = obj["t_ms"];
  const env: Envelope = {
    seq: typeof seq === "number" && Number.isInteger(seq) ? seq : 0,
    tMs: typeof tMs === "number" && Number.isInteger(tMs) ? tMs : 0,
  };
  return parseBody(kind, obj, env);
}

/** Turn a wire error code into an {@link ErrorCode} when it is one this SDK knows, else keep the string. */
export function isKnownErrorCode(code: string): code is ErrorCode {
  return ERROR_CODE_VALUES.has(code);
}

// ---------------------------------------------------------------------------------------------
// Agent -> server
// ---------------------------------------------------------------------------------------------

/** A wire object the agent sends. */
export type WireMessage = Record<string, unknown> & { type: string };

const TOOL_NAME = /^[A-Za-z0-9_-]{1,64}$/;
const E164 = /^\+[0-9]{8,15}$/;

/** Return `name` if it is a legal tool name (1 to 64 of `A-Z a-z 0-9 _ -`). */
export function validateToolName(name: string): string {
  if (!TOOL_NAME.test(name)) {
    throw new RangeError(`illegal tool name '${name}': 1 to 64 of A-Z a-z 0-9 _ -`);
  }
  return name;
}

/** Return `number` if it is E.164 as the protocol wants it (`+` and 8 to 15 digits). */
export function validatePhoneNumber(number: string): string {
  if (!E164.test(number)) {
    throw new ProtocolError(
      `invalid destination '${number}': E.164 is '+' and 8 to 15 digits`,
      ErrorCode.InvalidArgument,
    );
  }
  return number;
}

/** The UTF-8 length of `text` in bytes. */
export function byteLength(text: string): number {
  return Buffer.byteLength(text, "utf8");
}

function bounded(value: string | undefined, limit: number, what: string): void {
  if (value !== undefined && byteLength(value) > limit) {
    throw new ProtocolError(`${what} is longer than ${limit} bytes`, ErrorCode.InvalidArgument);
  }
}

function command(
  type: string,
  id: string | undefined,
  re: number | undefined,
  fields: Record<string, unknown>,
): WireMessage {
  if (id !== undefined && (id.length === 0 || byteLength(id) > MAX_COMMAND_ID_BYTES)) {
    throw new ProtocolError(
      `command id must be 1 to ${MAX_COMMAND_ID_BYTES} bytes`,
      ErrorCode.InvalidArgument,
    );
  }
  const out: WireMessage = { type };
  if (id !== undefined) {
    out["id"] = id;
  }
  if (re !== undefined) {
    out["re"] = re;
  }
  for (const [key, value] of Object.entries(fields)) {
    if (value !== undefined) {
      out[key] = value;
    }
  }
  return out;
}

/**
 * Serialize an agent message as one compact JSON text.
 *
 * @throws {ProtocolError} `too_large` above {@link MAX_MESSAGE_BYTES}; `invalid_argument` when
 * the value is not strict JSON (a NaN or infinite number, a cycle, a bigint).
 */
export function encode(message: object): string {
  let text: string;
  try {
    text = JSON.stringify(message, (_key, value: unknown) => {
      if (typeof value === "number" && !Number.isFinite(value)) {
        throw new TypeError(`${String(value)} is not valid JSON`);
      }
      return value;
    });
  } catch (error) {
    throw new ProtocolError(
      `not valid JSON: ${error instanceof Error ? error.message : String(error)}`,
      ErrorCode.InvalidArgument,
    );
  }
  if (text === undefined) {
    throw new ProtocolError("not valid JSON: nothing to send", ErrorCode.InvalidArgument);
  }
  if (byteLength(text) > MAX_MESSAGE_BYTES) {
    throw new ProtocolError(`message is larger than ${MAX_MESSAGE_BYTES} bytes`, ErrorCode.TooLarge);
  }
  return text;
}

/** A tool declaration as it goes on the wire (`session.accept.tools[]`). */
export interface WireToolDeclaration {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
  kind: ToolKind;
  timeout_ms?: number;
  filler_after_ms?: number;
  /** Hold the tool for the user's confirmation (`side_effect` tools only). */
  confirm?: boolean;
  /**
   * An identical call may repeat in a later response of the same turn (`side_effect` tools
   * only). Absent means `false`.
   */
  repeatable?: boolean;
}

/** A conversation message: `role` is `system`, `user` or `assistant`. */
export interface ContextMessage {
  readonly role: "system" | "user" | "assistant";
  readonly text: string;
}

/** Options of {@link sessionAccept}. */
export interface SessionAcceptOptions {
  /** The declared remote tools (at most 64). */
  tools?: readonly WireToolDeclaration[];
  /** Replaces the agent definition's prompt (at most 64 KiB). */
  systemPrompt?: string;
  /** Messages the conversation starts with (at most 32). */
  context?: readonly ContextMessage[];
  /** A line the bot speaks at once (at most 4 KiB). */
  greeting?: string;
  /** Event types to receive: absent subscribes to every event, `[]` to none. */
  events?: readonly string[];
  /**
   * `true` promises to keep the call's state after a lost link (and is required in the answer to a
   * resuming `session.start`).
   */
  resume?: boolean;
}

/** Build `session.accept`. */
export function sessionAccept(version: number, options: SessionAcceptOptions = {}): WireMessage {
  const tools = options.tools ?? [];
  const context = options.context ?? [];
  if (tools.length > MAX_TOOLS) {
    throw new ProtocolError(`more than ${MAX_TOOLS} tools`, ErrorCode.TooLarge);
  }
  if (context.length > MAX_CONTEXT_MESSAGES) {
    throw new ProtocolError(
      `more than ${MAX_CONTEXT_MESSAGES} context messages`,
      ErrorCode.TooLarge,
    );
  }
  checkMessages(context);
  bounded(options.systemPrompt, MAX_SYSTEM_PROMPT_BYTES, "system_prompt");
  bounded(options.greeting, MAX_LINE_BYTES, "greeting");
  const message: WireMessage = { type: "session.accept", version, tools: [...tools] };
  if (options.systemPrompt !== undefined) {
    message["system_prompt"] = options.systemPrompt;
  }
  if (options.greeting !== undefined) {
    message["greeting"] = options.greeting;
  }
  if (context.length > 0) {
    message["context"] = context.map((m) => ({ role: m.role, text: m.text }));
  }
  if (options.events !== undefined) {
    message["events"] = [...options.events];
  }
  if (options.resume === true) {
    message["resume"] = true;
  }
  return message;
}

/** Build `session.reject`: the agent does not take this call. */
export function sessionReject(code?: ErrorCode | (string & {}), message?: string): WireMessage {
  const out: WireMessage = { type: "session.reject" };
  if (code !== undefined) {
    out["code"] = code;
  }
  if (message !== undefined) {
    out["message"] = message;
  }
  return out;
}

/** Fields every command may carry. */
export interface CommandFields {
  /** Echoed in `command.result` (1 to 64 bytes); `command.result` is sent only when present. */
  id?: string;
  /** The `seq` of the event the command reacts to (hop latency). */
  re?: number;
}

/** Build `say`: the bot speaks `text` verbatim, without an LLM request. */
export function say(text: string, options: CommandFields & { interrupt?: boolean } = {}): WireMessage {
  if (text.trim().length === 0) {
    throw new ProtocolError("say.text is empty", ErrorCode.InvalidArgument);
  }
  bounded(text, MAX_LINE_BYTES, "say.text");
  const message = command("say", options.id, options.re, { text });
  if (options.interrupt === true) {
    message["interrupt"] = true;
  }
  return message;
}

/** Build `end`: speak the optional farewell `say`, then end the call. */
export function end(options: CommandFields & { say?: string; reason?: string } = {}): WireMessage {
  if (options.say !== undefined && options.say.trim().length === 0) {
    throw new ProtocolError("end.say is empty", ErrorCode.InvalidArgument);
  }
  bounded(options.say, MAX_LINE_BYTES, "end.say");
  bounded(options.reason, MAX_NOTE_BYTES, "end.reason");
  return command("end", options.id, options.re, { say: options.say, reason: options.reason });
}

/**
 * Build `transfer` (E.164 `destination`; `mode` `cold` by default).
 *
 * Always give a transfer an `id`: a repeat with the same `id` carries the same carrier
 * idempotency key, a transfer without one is a new transfer every time.
 */
export function transfer(
  destination: string,
  options: CommandFields & { mode?: "cold" | "warm"; summary?: string; reason?: string } = {},
): WireMessage {
  validatePhoneNumber(destination);
  if (options.mode !== undefined && options.mode !== "cold" && options.mode !== "warm") {
    throw new ProtocolError("mode must be 'cold' or 'warm'", ErrorCode.InvalidArgument);
  }
  bounded(options.summary, MAX_NOTE_BYTES, "transfer.summary");
  bounded(options.reason, MAX_NOTE_BYTES, "transfer.reason");
  return command("transfer", options.id, options.re, {
    destination,
    mode: options.mode,
    summary: options.summary,
    reason: options.reason,
  });
}

function checkMessages(messages: readonly ContextMessage[]): void {
  for (const item of messages) {
    const role: unknown = item.role;
    if (role !== "system" && role !== "user" && role !== "assistant") {
      throw new ProtocolError(`unknown role '${String(role)}'`, ErrorCode.InvalidArgument);
    }
    if (typeof item.text !== "string" || item.text.trim().length === 0) {
      throw new ProtocolError("a context message is empty", ErrorCode.InvalidArgument);
    }
  }
}

/**
 * Build `context.update`: replace the system prompt and/or append messages (roles `system`,
 * `user`, `assistant`; at most 32; all of the command or nothing).
 */
export function contextUpdate(
  options: CommandFields & { systemPrompt?: string; append?: readonly ContextMessage[] } = {},
): WireMessage {
  const append = options.append ?? [];
  if (append.length > MAX_CONTEXT_MESSAGES) {
    throw new ProtocolError(`more than ${MAX_CONTEXT_MESSAGES} messages`, ErrorCode.TooLarge);
  }
  checkMessages(append);
  bounded(options.systemPrompt, MAX_SYSTEM_PROMPT_BYTES, "system_prompt");
  const message = command("context.update", options.id, options.re, {
    system_prompt: options.systemPrompt,
  });
  if (append.length > 0) {
    message["append"] = append.map((m) => ({ role: m.role, text: m.text }));
  }
  return message;
}

/** Build `tools.set`: offer only the `enabled` tools (local and remote); `null`: all of them. */
export function toolsSet(enabled: readonly string[] | null, options: CommandFields = {}): WireMessage {
  const message = command("tools.set", options.id, options.re, {});
  message["enabled"] = enabled === null ? null : [...enabled];
  return message;
}

/**
 * Build `tool.result` answering `tool.call` `id`.
 *
 * A string `result` reaches the LLM as it is, any other JSON in compact form; `error` wins over
 * `result` and reaches the LLM as a `tool_failed` error result. An `undefined` result is `null`.
 */
export function toolResult(id: number, options: { result?: unknown; error?: string } = {}): WireMessage {
  if (options.error !== undefined) {
    return { type: "tool.result", id, error: options.error };
  }
  return { type: "tool.result", id, result: options.result === undefined ? null : options.result };
}
