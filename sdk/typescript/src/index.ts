/**
 * Brisvox agent SDK: write the application logic of a voice call in TypeScript.
 *
 * The brisvox server connects out to your agent over a WebSocket (protocol version 1,
 * `docs/AGENT_PROTOCOL.md`); this package is the agent side:
 *
 * ```ts
 * import { Agent, s } from "@brisvox/agent";
 *
 * const agent = new Agent({ name: "luigi", systemPrompt: "You are Luigi's host.", greeting: "Hi!" });
 *
 * agent.tool({
 *   name: "opening_hours",
 *   description: "Opening hours of the restaurant on a weekday.",
 *   parameters: s.object({ day: s.enum(["mon", "tue", "wed", "thu", "fri", "sat", "sun"]) }),
 *   handler: ({ day }) => (day === "mon" ? "Closed on Monday." : "12:00 to 22:00."),
 * });
 *
 * await agent.run({ port: 8765 });
 * ```
 *
 * @module
 */

export { Agent, AgentServer, Reject, isLoopbackHost, listeningMessage } from "./agent.js";
export type { Accept, AgentOptions, Handler, ServeOptions, StartHandler } from "./agent.js";
export { CommandError, CommandTimeout, Session, SessionClosed, consoleLogger } from "./session.js";
export type {
  CommandOptions,
  EndOptions,
  Logger,
  PendingCommand,
  SayOptions,
  TransferOptions,
  UpdateContextOptions,
} from "./session.js";
export { Tool, ToolArgumentError, ToolContext, ToolError, defineTool } from "./tools.js";
export type {
  CancelReason,
  RawToolDefinition,
  ToolDefinition,
  ToolOptions,
  ToolReturn,
  TypedToolDefinition,
} from "./tools.js";
export { MIN_SESSION_SECRET_BYTES, SESSION_TOKEN_AUDIENCES, mintSessionToken } from "./session-token.js";
export type { SessionTokenAudience, SessionTokenOptions } from "./session-token.js";
export { Schema, SchemaViolation, s } from "./schema.js";
export type { ArrayOptions, Infer, JsonSchema, NumberOptions, ObjectOutput, StringOptions } from "./schema.js";
export {
  ErrorCode,
  EVENT_TYPES,
  MAX_COMMAND_ID_BYTES,
  MAX_CONTEXT_MESSAGES,
  MAX_LINE_BYTES,
  MAX_MESSAGE_BYTES,
  MAX_NOTE_BYTES,
  MAX_SYSTEM_PROMPT_BYTES,
  MAX_TOOLS,
  MAX_TOOL_TIMEOUT_MS,
  PROTOCOL,
  ProtocolError,
  SUPPORTED_VERSIONS,
  parseServerMessage,
} from "./protocol.js";
export type {
  AgentEvent,
  BotLlmStarted,
  BotLlmStopped,
  BotText,
  BotTtsStopped,
  BotTtsText,
  CallIdentity,
  LanguageSwitched,
  Capabilities,
  CommandResult,
  ContextMessage,
  ErrorEvent,
  EventMap,
  EventType,
  EventsLost,
  LatencySummary,
  LlmUsage,
  Metrics,
  Resumed,
  ResumeInfo,
  ServerMessage,
  SessionEnd,
  SessionStart,
  SessionUsage,
  StageLatency,
  SttUsage,
  SubscribableEventType,
  TokenUsage,
  ToolCall,
  ToolCancel,
  ToolKind,
  TtsUsage,
  TurnBotEnded,
  TurnBotStarted,
  TurnInterrupted,
  TurnMetrics,
  TurnUserEnded,
  TurnUserStarted,
  UnknownMessage,
  UsageSummary,
  UserText,
  UserTranscript,
  WireError,
} from "./protocol.js";

/** The SDK's version. */
export const VERSION = "0.1.3";
