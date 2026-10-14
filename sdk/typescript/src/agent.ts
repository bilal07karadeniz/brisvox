/**
 * The async agent API: an {@link Agent} is a WebSocket server that brisvox connects to.
 *
 * One connection is one call. The agent answers the handshake, then runs three things per call:
 *
 * - a **reader** that never runs user code: it resolves command results, starts tool calls and
 *   queues events;
 * - a **dispatcher** that runs the event handlers one at a time, in the server's order (a bounded
 *   queue; when a slow handler lets it fill up, the newest events are dropped and reported to the
 *   handlers as a synthetic `events.lost`, mirroring what the server does on its side);
 * - one **promise per tool call**, so tools run concurrently and a slow event handler never
 *   delays a tool result.
 *
 * Everything that waits on the network or on the server has a timeout.
 *
 * **Reconnecting** (opt-in, `new Agent({ resume: true })`): the agent then declares `resume` in
 * `session.accept`, and when a call's link is lost the {@link Session} is *parked* instead of
 * ended: its `data`, queued events and handler state stay for `resumeWindowMs`. The server
 * reconnects with a `session.start` carrying `resume`; the agent re-attaches the parked session to
 * the new link (no greeting, no `onSessionStart`), runs the `onResumed` handlers and goes on. A
 * parked session nobody resumes ends cleanly after the window (`onSessionEnd` with reason
 * `link_closed`).
 *
 * @module
 */

import { createHash, timingSafeEqual } from "node:crypto";
import http from "node:http";
import https from "node:https";
import { isIP } from "node:net";
import type { AddressInfo } from "node:net";
import type { Duplex } from "node:stream";
import { performance } from "node:perf_hooks";
import { WebSocketServer } from "ws";
import type { WebSocket } from "ws";
import * as protocol from "./protocol.js";
import { ErrorCode, EVENT_TYPES, HANDLER_TYPES, ProtocolError } from "./protocol.js";
import type {
  AgentEvent,
  CommandResult,
  ContextMessage,
  EventMap,
  EventType,
  SubscribableEventType,
  ToolCall,
  ToolCancel,
  WireMessage,
} from "./protocol.js";
import {
  Inbox,
  Session,
  SessionClosed,
  closeWs,
  consoleLogger,
  newToolEntry,
  reactingTo,
  withTimeout,
  writeText,
} from "./session.js";
import type { Frame, Logger, ToolEntry } from "./session.js";
import { Tool, ToolContext, ToolError, errorName } from "./tools.js";
import type { RawToolDefinition, ToolDefinition, TypedToolDefinition } from "./tools.js";
import type { Schema } from "./schema.js";

/** Raise from the `onSessionStart` handler to decline the call (`session.reject`). The call then runs on the server's agent definition alone. */
export class Reject extends Error {
  /** The code sent in `session.reject`. */
  readonly code: ErrorCode | (string & {});

  constructor(code: ErrorCode | (string & {}) = ErrorCode.Unavailable, message = "") {
    super(message || code);
    this.name = "Reject";
    this.code = code;
  }
}

/** What an `onSessionStart` handler may change for this call: any field left out keeps the agent's default. */
export interface Accept {
  /** The system prompt of this call (`null`: none, the server's agent definition keeps its own). */
  systemPrompt?: string | null;
  /** The messages the conversation starts with. */
  context?: readonly ContextMessage[];
  /** The greeting of this call (`null`: none). */
  greeting?: string | null;
  /** Event types to receive (`null`: all; `[]`: none). */
  events?: readonly SubscribableEventType[] | null;
  /** Names of the declared tools to offer on this call (`null`: all of them). */
  tools?: readonly string[] | null;
}

/** A handler of one event type. A plain function works as well as an async one. */
export type Handler<E> = (session: Session, event: E) => void | Promise<void>;

/** The handshake handler. */
export type StartHandler = (session: Session) => Accept | void | Promise<Accept | void>;

/** The settings of an {@link Agent}. */
export interface AgentOptions {
  /** The agent's name (for logs only; the server names the definition itself). */
  name?: string;
  /** Replaces the agent definition's system prompt (at most 64 KiB). */
  systemPrompt?: string;
  /** Messages the conversation starts with (at most 32). */
  context?: readonly ContextMessage[];
  /** A line the bot speaks at once (at most 4 KiB). */
  greeting?: string;
  /**
   * `"auto"` (default) subscribes only to the event types you registered handlers for; `null`
   * to every event; an array to exactly those. The always-delivered messages (`session.*`,
   * `tool.*`, `command.result`, `events.lost`, `error`) are not subscribable.
   */
  events?: "auto" | readonly SubscribableEventType[] | null;
  /**
   * Budget in milliseconds for `session.start` to arrive and the answer to be written (every
   * handshake write is bounded by it). The server gives up 2000 ms after connecting, so keep it
   * below that. Default 1500.
   */
  handshakeTimeoutMs?: number;
  /** Default wait for a `command.result`, in milliseconds. Default 10000. */
  commandTimeoutMs?: number;
  /** Bound on one WebSocket write, in milliseconds. Default 10000. */
  writeTimeoutMs?: number;
  /**
   * How long a `side_effect` tool may keep running after the link closed before the SDK stops
   * waiting for it, in milliseconds (the tool sees `ctx.signal` abort with reason `link_closed` at
   * once; `read_only` tools are abandoned immediately). Default 30000.
   */
  sideEffectGraceMs?: number;
  /**
   * A link from which nothing (no message, no ping, no pong) arrived for this long is taken down,
   * in milliseconds. The server pings every 10 s and itself gives up after 30 s, so the default
   * 60000 only frees links of servers that vanished without closing. 0 turns the watchdog off.
   */
  idleTimeoutMs?: number;
  /** Events queued per call for the handlers (newest dropped when full). Default 1024. */
  eventQueue?: number;
  /** Calls served at once; more are answered with `session.reject` (`unavailable`). Default 256. */
  maxSessions?: number;
  /** Tool calls running at once per call; more fail at once (the server's own bound is 32). Default 64. */
  maxConcurrentTools?: number;
  /** Protocol versions to offer back (default: all this SDK speaks). */
  versions?: readonly number[];
  /**
   * Opt in to reconnecting. The agent declares `resume` in `session.accept`; when a call's link is
   * lost the session is parked (`session.data`, queued events and the handlers' view of the call
   * are kept) and the server's reconnection with `session.start.resume` re-attaches it, with no
   * greeting and no `onSessionStart`. Handle the gap in `onResumed`. Off by default: an agent that
   * keeps no state across a lost link must not declare it.
   */
  resume?: boolean;
  /**
   * How long a parked session waits for the server, in milliseconds (default 15000, the server's
   * own reconnect window). After it the call ends for the handlers (`onSessionEnd` with reason
   * `link_closed`) and a later resumption is rejected with `unknown_session`. Needs `resume`.
   */
  resumeWindowMs?: number;
  /** Where diagnostics go (default: warnings and errors to `console.error`). */
  logger?: Logger;
}

/** Options of {@link Agent.serve}. */
export interface ServeOptions {
  /** The address to bind. Default `127.0.0.1`. */
  host?: string;
  /** The port (`0`: a free one, see {@link AgentServer.port}). Default 8765. */
  port?: number;
  /**
   * The bearer token the brisvox server must present (`Authorization: Bearer <token>`). The
   * default `"env"` reads `BRISVOX_AGENT_TOKEN` (unset or empty: no authentication, so bind a
   * loopback address or a private network then); `null` turns authentication off.
   */
  token?: string | null | "env";
  /** TLS options (`key`, `cert`, ...): serve `wss://`. */
  tls?: https.ServerOptions;
}

const ALL_EVENT_TYPES: ReadonlySet<string> = HANDLER_TYPES;
const TURN_EVENTS = [
  "turn.user_started",
  "turn.user_ended",
  "turn.bot_started",
  "turn.bot_ended",
  "turn.interrupted",
  "turn.metrics",
] as const;
const ABANDONED = Symbol("abandoned");
/** Write time a `session.reject` gets even when the handshake budget is spent. */
const REJECT_GRACE_MS = 250;

function defaultLogLevel(): "debug" | "info" | "warn" | "error" | "silent" {
  const level = process.env["BRISVOX_AGENT_LOG"];
  const known = ["debug", "info", "warn", "error", "silent"] as const;
  return known.find((candidate) => candidate === level) ?? "warn";
}

function defaultLogger(): Logger {
  return consoleLogger(defaultLogLevel());
}

/**
 * Whether binding `host` makes the agent reachable only from this machine: `localhost`, an
 * address in `127.0.0.0/8`, `::1`, or an IPv4-mapped loopback address.
 */
export function isLoopbackHost(host: string): boolean {
  const name = host.trim().replace(/^\[|\]$/g, "").toLowerCase();
  if (name === "localhost" || name.endsWith(".localhost")) {
    return true;
  }
  const family = isIP(name);
  if (family === 4) {
    return name.startsWith("127.");
  }
  if (family === 6) {
    if (name === "::1" || /^(0{1,4}:){7}0{0,3}1$/.test(name)) {
      return true;
    }
    const mapped = /^(?:0{1,4}:){5}ffff:(\d+\.\d+\.\d+\.\d+)$|^::ffff:(\d+\.\d+\.\d+\.\d+)$/.exec(name);
    const v4 = mapped?.[1] ?? mapped?.[2];
    return v4 !== undefined && v4.startsWith("127.");
  }
  return false;
}

/** The hint appended to the startup line of an agent bound to a loopback address. */
export const LOOPBACK_HINT =
  "loopback: reachable only from this machine; bind 0.0.0.0 or :: for a server in another container";

/**
 * The startup line of {@link Agent.run}: the address the agent bound, with a hint when it is a
 * loopback address.
 */
export function listeningMessage(name: string, host: string, url: string): string {
  const base = `agent '${name}' listening on ${url}`;
  return isLoopbackHost(host) ? `${base} (${LOOPBACK_HINT})` : base;
}

function checkEventTypes(events: readonly string[]): void {
  const known: readonly string[] = EVENT_TYPES;
  const unknown = events.filter((type) => !known.includes(type));
  if (unknown.length > 0) {
    throw new RangeError(`unknown or unsubscribable event types: ${unknown.join(", ")}`);
  }
}

/**
 * A brisvox agent: the application logic of a call.
 *
 * Declare tools and handlers, then `await agent.run({ port: 8765 })` (until Ctrl+C) or
 * `const server = await agent.listen(...)` (embedding):
 *
 * ```ts
 * const agent = new Agent({ name: "luigi", systemPrompt: "You are Luigi's host.", greeting: "Hi!" });
 *
 * agent.tool({
 *   name: "book_table",
 *   description: "Book a table.",
 *   kind: "side_effect",
 *   parameters: s.object({ time: s.string(), partySize: s.optional(s.integer(), 2) }),
 *   handler: async ({ time, partySize }, ctx) => `Booked ${partySize} at ${time} (${ctx.idempotencyKey}).`,
 * });
 *
 * agent.onUserText(async (session, event) => {
 *   if (event.text.includes("bye")) await session.end({ say: "Goodbye!" });
 * });
 *
 * await agent.run({ port: 8765 });
 * ```
 */
export class Agent {
  /** The agent's name. */
  readonly name: string;
  /** The default system prompt. */
  readonly systemPrompt: string | undefined;
  /** The default starting context. */
  readonly context: readonly ContextMessage[];
  /** The default greeting. */
  readonly greeting: string | undefined;
  /** The default event subscription. */
  readonly events: "auto" | readonly SubscribableEventType[] | null;
  /** Handshake budget in milliseconds. */
  readonly handshakeTimeoutMs: number;
  /** Default wait for a `command.result` in milliseconds. */
  readonly commandTimeoutMs: number;
  /** Bound on one WebSocket write in milliseconds. */
  readonly writeTimeoutMs: number;
  /** Grace for `side_effect` tools after the link closed, in milliseconds. */
  readonly sideEffectGraceMs: number;
  /** Idle time after which a link is taken down, in milliseconds (0: never). */
  readonly idleTimeoutMs: number;
  /** Events queued per call for the handlers. */
  readonly eventQueue: number;
  /** Calls served at once. */
  readonly maxSessions: number;
  /** Tool calls running at once per call. */
  readonly maxConcurrentTools: number;
  /** Protocol versions this agent offers. */
  readonly versions: readonly number[];
  /** Whether the agent declares `resume` and keeps lost calls for the server. */
  readonly resume: boolean;
  /** How long a lost call is kept for the server, in milliseconds. */
  readonly resumeWindowMs: number;
  /** Where diagnostics go. */
  readonly logger: Logger;

  readonly #tools = new Map<string, Tool>();
  readonly #handlers = new Map<string, Handler<never>[]>();
  #startHandler: StartHandler | undefined;
  #active = 0;
  /** The calls a resumption can name: live ones and parked ones (only with `resume`). */
  readonly #sessions = new Map<string, Session>();
  #draining = false;
  readonly #background = new Set<Promise<void>>();
  /** The default logger is on and would drop the startup line (its level is above `info`). */
  readonly #announceOnConsole: boolean;

  /**
   * @throws {RangeError} a setting is out of range, or an event type / protocol version is unknown.
   */
  constructor(options: AgentOptions = {}) {
    this.name = options.name ?? "agent";
    this.systemPrompt = options.systemPrompt;
    this.context = [...(options.context ?? [])];
    this.greeting = options.greeting;
    this.events = options.events === undefined ? "auto" : options.events;
    this.handshakeTimeoutMs = options.handshakeTimeoutMs ?? 1500;
    this.commandTimeoutMs = options.commandTimeoutMs ?? 10_000;
    this.writeTimeoutMs = options.writeTimeoutMs ?? 10_000;
    this.sideEffectGraceMs = options.sideEffectGraceMs ?? 30_000;
    this.idleTimeoutMs = options.idleTimeoutMs ?? 60_000;
    this.eventQueue = options.eventQueue ?? 1024;
    this.maxSessions = options.maxSessions ?? 256;
    this.maxConcurrentTools = options.maxConcurrentTools ?? 64;
    this.versions = [...(options.versions ?? protocol.SUPPORTED_VERSIONS)];
    this.resume = options.resume === true;
    this.resumeWindowMs = options.resumeWindowMs ?? 15_000;
    this.logger = options.logger ?? defaultLogger();
    const level = defaultLogLevel();
    this.#announceOnConsole = options.logger === undefined && (level === "warn" || level === "error");
    for (const [what, value] of [
      ["eventQueue", this.eventQueue],
      ["maxSessions", this.maxSessions],
      ["maxConcurrentTools", this.maxConcurrentTools],
    ] as const) {
      if (!Number.isInteger(value) || value < 1) {
        throw new RangeError(`${what} must be an integer, 1 or more`);
      }
    }
    for (const [what, value] of [
      ["handshakeTimeoutMs", this.handshakeTimeoutMs],
      ["commandTimeoutMs", this.commandTimeoutMs],
      ["writeTimeoutMs", this.writeTimeoutMs],
      ["sideEffectGraceMs", this.sideEffectGraceMs],
      ["idleTimeoutMs", this.idleTimeoutMs],
    ] as const) {
      if (!Number.isFinite(value) || value < 0) {
        throw new RangeError(`${what} must be a number of milliseconds, 0 or more`);
      }
    }
    if (!Number.isFinite(this.resumeWindowMs) || this.resumeWindowMs <= 0) {
      throw new RangeError("resumeWindowMs must be a positive number of milliseconds");
    }
    const unsupported = this.versions.filter((v) => !protocol.SUPPORTED_VERSIONS.includes(v));
    if (unsupported.length > 0 || this.versions.length === 0) {
      throw new RangeError(
        `this SDK speaks versions ${JSON.stringify(protocol.SUPPORTED_VERSIONS)}, not ${JSON.stringify(this.versions)}`,
      );
    }
    if (Array.isArray(this.events)) {
      checkEventTypes(this.events);
    }
  }

  // -- declaration -----------------------------------------------------------------------------

  /**
   * Declare a remote tool.
   *
   * With `parameters: s.object({...})` the handler's `args` are typed from the schema and
   * validated; with a hand-written JSON-schema object they arrive as the server sent them.
   * `kind: "side_effect"` is for anything that changes the world: it never runs speculatively and
   * never twice for one call; pass `ctx.idempotencyKey` to APIs that deduplicate.
   *
   * @throws {RangeError} a duplicate or illegal name, more than 64 tools, a bad timeout, a missing description.
   */
  tool<P extends Schema<Record<string, unknown>>>(definition: TypedToolDefinition<P>): this;
  tool<Args extends Record<string, unknown> = Record<string, unknown>>(
    definition: RawToolDefinition<Args>,
  ): this;
  tool(definition: ToolDefinition): this {
    const declared = new Tool(definition);
    if (this.#tools.has(declared.name)) {
      throw new RangeError(`tool '${declared.name}' is declared twice`);
    }
    if (this.#tools.size >= protocol.MAX_TOOLS) {
      throw new RangeError(`at most ${protocol.MAX_TOOLS} tools`);
    }
    this.#tools.set(declared.name, declared);
    return this;
  }

  /** The declared tools by name. */
  get tools(): ReadonlyMap<string, Tool> {
    return new Map(this.#tools);
  }

  /**
   * Register an event handler by wire type (`"user.text"`). Handlers of one call run one at a
   * time, in the order of the events. A handler that throws is logged and the call goes on.
   *
   * @throws {RangeError} an unknown event type.
   */
  on<K extends EventType>(type: K, handler: Handler<EventMap[K]>): this {
    if (!ALL_EVENT_TYPES.has(type)) {
      throw new RangeError(`unknown event type '${String(type)}'`);
    }
    const list = this.#handlers.get(type) ?? [];
    list.push(handler as Handler<never>);
    this.#handlers.set(type, list);
    return this;
  }

  /**
   * The handshake handler. It runs before `session.accept` is sent, inside the handshake budget.
   * Return an {@link Accept} to change the prompt, context, greeting, events or tools for this
   * call, or throw {@link Reject} to decline it. One handler per agent.
   */
  onSessionStart(handler: StartHandler): this {
    if (this.#startHandler !== undefined) {
      throw new Error("onSessionStart is already registered");
    }
    this.#startHandler = handler;
    return this;
  }

  /**
   * The call is over. Runs exactly once per call, after all earlier events were handled; if the
   * link dropped without a `session.end` the reason is `"link_closed"`.
   */
  onSessionEnd(handler: Handler<EventMap["session.end"]>): this {
    return this.on("session.end", handler);
  }

  /** STT segments (interim and final). */
  onTranscript(handler: Handler<EventMap["user.transcript"]>): this {
    return this.on("user.transcript", handler);
  }

  /** The whole user turn the LLM answers. */
  onUserText(handler: Handler<EventMap["user.text"]>): this {
    return this.on("user.text", handler);
  }

  /** Streamed LLM deltas (a bot line is one). */
  onBotText(handler: Handler<EventMap["bot.text"]>): this {
    return this.on("bot.text", handler);
  }

  /** Every `turn.*` event (user started/ended, bot started/ended, interrupted, metrics). */
  onTurn(handler: Handler<EventMap[(typeof TURN_EVENTS)[number]]>): this {
    for (const type of TURN_EVENTS) {
      this.on(type, handler as Handler<EventMap[typeof type]>);
    }
    return this;
  }

  /** A stage of the call reported an error. */
  onError(handler: Handler<EventMap["error"]>): this {
    return this.on("error", handler);
  }

  /**
   * The call's lost link was taken up again (`attempt`, `previousSeq`, `downMs`). Runs after the
   * events queued before the loss; the server then reports what it could not deliver as
   * `events.lost`. Needs `resume: true`.
   */
  onResumed(handler: Handler<EventMap["session.resumed"]>): this {
    return this.on("session.resumed", handler);
  }

  /** Events were dropped, by the server or by this SDK's own queue. */
  onEventsLost(handler: Handler<EventMap["events.lost"]>): this {
    return this.on("events.lost", handler);
  }

  // -- serving ---------------------------------------------------------------------------------

  /** The WebSocket server for the agent, not yet listening: `await server.start()`. */
  serve(options: ServeOptions = {}): AgentServer {
    const token = options.token === undefined || options.token === "env"
      ? (process.env["BRISVOX_AGENT_TOKEN"] || null)
      : options.token;
    return new AgentServer(this, options.host ?? "127.0.0.1", options.port ?? 8765, token, options.tls);
  }

  /** Serve and start listening: `const server = await agent.listen({ port: 0 }); server.url`. */
  async listen(options: ServeOptions = {}): Promise<AgentServer> {
    return await this.serve(options).start();
  }

  /**
   * Serve until `SIGINT`/`SIGTERM` (or `options.signal` aborts), then close the live links and
   * resolve. `onReady` is called with the listening server (its `url` holds the bound port).
   *
   * The address it bound is logged at `info` (printed to stderr when the default logger would
   * drop it). On a loopback `host` (the default `127.0.0.1`) the line says the agent is reachable
   * only from this machine and that a server in another container needs `0.0.0.0` or `::`.
   */
  async run(options: ServeOptions & { onReady?: (server: AgentServer) => void; signal?: AbortSignal } = {}): Promise<void> {
    const server = await this.listen(options);
    const message = listeningMessage(this.name, server.host, server.url);
    if (this.#announceOnConsole) {
      // The default logger shows warnings only; the address is what a deployer needs.
      console.error(`[brisvox-agent] ${message}`);
    } else {
      this.logger.info(message);
    }
    options.onReady?.(server);
    await new Promise<void>((resolve) => {
      const stop = (): void => {
        process.off("SIGINT", stop);
        process.off("SIGTERM", stop);
        options.signal?.removeEventListener("abort", stop);
        resolve();
      };
      process.once("SIGINT", stop);
      process.once("SIGTERM", stop);
      if (options.signal?.aborted === true) {
        stop();
      } else {
        options.signal?.addEventListener("abort", stop, { once: true });
      }
    });
    await server.close();
  }

  // -- one call --------------------------------------------------------------------------------

  #negotiate(offered: readonly number[]): number | undefined {
    const common = offered.filter((v) => this.versions.includes(v));
    return common.length > 0 ? Math.max(...common) : undefined;
  }

  #subscription(): SubscribableEventType[] | undefined {
    if (this.events === "auto") {
      return EVENT_TYPES.filter((type) => (this.#handlers.get(type)?.length ?? 0) > 0);
    }
    return this.events === null ? undefined : [...this.events];
  }

  /**
   * Serve one connection (one call) to its end. {@link AgentServer} calls this; call it yourself
   * to embed the agent in another WebSocket server. Never rejects.
   */
  async handle(ws: WebSocket): Promise<void> {
    const deadline = performance.now() + this.handshakeTimeoutMs;
    this.#active += 1;
    try {
      await this.#serveCall(ws, deadline);
    } catch (error) {
      this.logger.error("call failed", error);
      await closeWs(ws, 1011, "internal error");
    } finally {
      this.#active -= 1;
    }
  }

  async #serveCall(ws: WebSocket, deadline: number): Promise<void> {
    const inbox = new Inbox(ws);
    let session: Session | undefined;
    try {
      session = await this.#handshake(ws, inbox, deadline);
    } catch (error) {
      this.logger.error("handshake failed", error);
      await closeWs(ws, 1011, "internal error");
      return;
    }
    if (session === undefined) {
      return;
    }
    const stopWatching = this.#watchIdle(ws, session);
    try {
      if (inbox.dropped > 0) {
        this.logger.warn(`session ${session.id}: ${inbox.dropped} frame(s) received during the handshake were dropped`);
      }
      // Frames are routed straight from the socket's message event: the reader never runs user code
      // (handlers are queued, tools run as tasks), so no frame can wait behind another.
      await inbox.forward((frame) => {
        try {
          this.#process(session, frame);
        } catch (error) {
          this.logger.error(`session ${session.id}: reader failed`, error);
        }
      });
    } finally {
      stopWatching();
      if (session.socket !== ws) {
        // A resumed link took the session over (this one was half-open): not ours any more.
        await closeWs(ws, 1000, "");
      } else {
        // Nothing awaits between the loss and the decision to park, so a resumption cannot slip in.
        this.#linkLost(session);
        if (this.#mayPark(session)) {
          this.#park(session);
        } else {
          await this.#finalize(session);
        }
        await closeWs(ws, 1000, "");
      }
    }
  }

  // -- reconnecting ----------------------------------------------------------------------------

  /** The link is gone: commands fail, running tools hear it. */
  #linkLost(session: Session): void {
    session.markClosed();
    session.windDownTools(this.sideEffectGraceMs);
  }

  /** Parked calls: no connection of their own, so not part of `#active`. */
  #parkedCount(): number {
    let count = 0;
    for (const session of this.#sessions.values()) {
      if (session.parked && !session.resuming) count += 1;
    }
    return count;
  }

  /** A lost link is only waited out for a resumable agent whose call did not end. */
  #mayPark(session: Session): boolean {
    return this.resume && !this.#draining && !session.hasEnded;
  }

  #park(session: Session): void {
    session.parked = true;
    session.parkDeadline = performance.now() + this.resumeWindowMs;
    this.#armPark(session);
    this.logger.info(`session ${session.id}: link lost; waiting ${this.resumeWindowMs} ms for the server to resume`);
  }

  #armPark(session: Session): void {
    if (session.parkTimer !== undefined) {
      clearTimeout(session.parkTimer); // never two timers: a stale one would end a later window
      session.parkTimer = undefined;
    }
    const delay = Math.max(0, session.parkDeadline - performance.now());
    session.parkTimer = setTimeout(() => {
      const task: Promise<void> = this.#expire(session)
        .catch((error: unknown) => this.logger.error(`session ${session.id}: expiry failed`, error))
        .finally(() => this.#background.delete(task));
      this.#background.add(task);
    }, delay);
    session.parkTimer.unref();
  }

  /** The window ran out (or the server stops): the call ends for the handlers. */
  async #expire(session: Session): Promise<void> {
    if (!session.parked || session.resuming) {
      return;
    }
    session.parked = false;
    session.parkTimer = undefined;
    this.logger.info(`session ${session.id}: not resumed in time; the call ends`);
    await this.#finalize(session);
  }

  /** The call is over for the handlers: queue the end behind everything else and wait (bounded). */
  async #finalize(session: Session): Promise<void> {
    if (this.#sessions.get(session.id) === session) {
      this.#sessions.delete(session.id);
    }
    session.finish({ type: "session.end", seq: 0, tMs: 0, reason: "link_closed" }); // no-op when the server's arrived
    // on_session_end runs exactly once, also when the handler queue overflowed or a handler hogs
    // the budget: the end is held aside and gets its own turn.
    await session.drain(5000, 1000);
  }

  /** @internal Server shutdown: no more resumptions are taken (`true`), or taken again (`false`). */
  setDraining(draining: boolean): void {
    this.#draining = draining;
  }

  /** @internal Server shutdown: end every parked call now. */
  async endParked(): Promise<void> {
    for (const session of [...this.#sessions.values()]) {
      if (session.parked) {
        if (session.parkTimer !== undefined) {
          clearTimeout(session.parkTimer);
          session.parkTimer = undefined;
        }
        session.resuming = false;
        await this.#expire(session);
      }
    }
  }

  /** Answer a `session.start` that resumes a call: re-attach the parked session. */
  async #resume(
    ws: WebSocket,
    start: protocol.SessionStart,
    version: number,
    writeBudget: () => number,
    rejectCall: (code: ErrorCode | (string & {}), message?: string) => Promise<undefined>,
  ): Promise<Session | undefined> {
    const resume = start.resume;
    const session = this.#draining ? undefined : this.#sessions.get(start.sessionId);
    if (resume === undefined || session === undefined || session.hasEnded || session.resuming) {
      this.logger.warn(`session ${start.sessionId}: resumption of a call this agent does not know`);
      return await rejectCall(ErrorCode.UnknownSession, "no such call");
    }
    // From here the expiry timer must not end the session under the answer being written.
    session.resuming = true;
    const options: protocol.SessionAcceptOptions = { resume: true };
    if (session.eventsSubscription !== undefined) options.events = session.eventsSubscription;
    try {
      await writeText(ws, protocol.encode(protocol.sessionAccept(version, options)), writeBudget());
    } catch (error) {
      session.resuming = false;
      if (session.parked) {
        this.#armPark(session); // the same deadline: the window is not extended
      }
      throw new SessionClosed(`session.accept was not written: ${error instanceof Error ? error.message : String(error)}`);
    }
    const old = session.socket;
    if (!session.closed) {
      // The server resumed while this side still holds the old link (a half-open one).
      this.#linkLost(session);
      void closeWs(old, 1000, "superseded by a resumed link");
    }
    const seenSeq = session.lastSeq; // what this side read on the lost link
    session.reopen(ws);
    session.lastSeq = start.seq;
    this.logger.info(`session ${session.id}: resumed (attempt ${resume.attempt}, down ${resume.downMs} ms)`);
    if ((this.#handlers.get("session.resumed")?.length ?? 0) > 0) {
      session.enqueue({
        type: "session.resumed",
        seq: start.seq,
        tMs: start.tMs,
        attempt: resume.attempt,
        previousSeq: resume.previousSeq,
        seenSeq,
        downMs: resume.downMs,
      });
    }
    return session;
  }

  /** Take the link down when nothing arrives for `idleTimeoutMs`; returns the function that stops watching. */
  #watchIdle(ws: WebSocket, session: Session): () => void {
    if (this.idleTimeoutMs === 0) {
      return () => undefined;
    }
    let last = performance.now();
    const touch = (): void => {
      last = performance.now();
    };
    ws.on("message", touch);
    ws.on("ping", touch);
    ws.on("pong", touch);
    const timer = setInterval(
      () => {
        if (performance.now() - last > this.idleTimeoutMs) {
          this.logger.warn(`session ${session.id}: nothing received for ${this.idleTimeoutMs} ms; link taken down`);
          ws.terminate();
        }
      },
      Math.max(10, Math.min(5000, this.idleTimeoutMs / 4)),
    );
    timer.unref();
    return () => {
      clearInterval(timer);
      ws.off("message", touch);
      ws.off("ping", touch);
      ws.off("pong", touch);
    };
  }

  async #handshake(ws: WebSocket, inbox: Inbox, deadline: number): Promise<Session | undefined> {
    const remaining = (): number => Math.max(0, deadline - performance.now());
    const first = await inbox.receive(remaining());
    if (first.kind === "closed") {
      return undefined;
    }
    if (first.kind === "timeout") {
      this.logger.warn(`no session.start within ${this.handshakeTimeoutMs} ms`);
      await closeWs(ws, 1008, "no session.start");
      return undefined;
    }
    let start: protocol.SessionStart | undefined;
    if (!first.frame.binary) {
      try {
        const parsed = protocol.parseServerMessage(first.frame.text);
        if (parsed.type === "session.start" && parsed.protocol === protocol.PROTOCOL) {
          start = parsed;
        }
      } catch (error) {
        this.logger.warn(`bad first message: ${error instanceof Error ? error.message : String(error)}`);
      }
    }
    if (start === undefined) {
      this.logger.warn(`first message was not a ${protocol.PROTOCOL} session.start`);
      await closeWs(ws, 1002, "expected session.start");
      return undefined;
    }
    // A handshake write is bounded by the handshake budget and by one write timeout.
    const writeBudget = (): number => Math.min(remaining(), this.writeTimeoutMs);
    const sendBestEffort = async (message: WireMessage): Promise<void> => {
      // A rejection is best effort: past the budget the server has probably given up already, so
      // it only gets a short grace to be written (a stuck peer must not hold the call).
      const budget = Math.min(this.writeTimeoutMs, Math.max(remaining(), REJECT_GRACE_MS));
      try {
        await writeText(ws, protocol.encode(message), budget);
      } catch (error) {
        this.logger.debug(`session ${start.sessionId}: rejection not written: ${String(error)}`);
      }
    };
    const rejectCall = async (code: ErrorCode | (string & {}), message?: string): Promise<undefined> => {
      await sendBestEffort(protocol.sessionReject(code, message));
      await closeWs(ws, 1000, "");
      return undefined;
    };

    const version = this.#negotiate(start.versions);
    if (version === undefined) {
      this.logger.warn(
        `session ${start.sessionId} offers versions ${JSON.stringify(start.versions)}, this agent speaks ${JSON.stringify(this.versions)}`,
      );
      return await rejectCall(
        ErrorCode.UnsupportedVersion,
        `this agent speaks versions ${JSON.stringify(this.versions)}`,
      );
    }
    if (start.resume !== undefined) {
      return await this.#resume(ws, start, version, writeBudget, rejectCall);
    }
    // Connected calls (this one included) plus the parked ones, which hold state too.
    if (this.#active + this.#parkedCount() > this.maxSessions) {
      return await rejectCall(ErrorCode.Unavailable, "too many calls");
    }

    const session = new Session(
      ws,
      start,
      version,
      {
        commandTimeoutMs: this.commandTimeoutMs,
        writeTimeoutMs: this.writeTimeoutMs,
        eventQueue: this.eventQueue,
        logger: this.logger,
      },
      (target, event) => this.#deliver(target, event),
    );
    let accept: Accept = {};
    if (this.#startHandler !== undefined) {
      try {
        const returned = await withTimeout(
          Promise.resolve().then(() => this.#startHandler?.(session)),
          remaining(),
          () => new HandshakeBudgetExceeded(),
        );
        if (returned !== undefined && returned !== null) {
          if (typeof returned !== "object") {
            throw new TypeError("onSessionStart must return an Accept or nothing");
          }
          accept = returned;
        }
      } catch (error) {
        if (error instanceof Reject) {
          return await rejectCall(error.code, error.message || undefined);
        }
        if (error instanceof HandshakeBudgetExceeded) {
          this.logger.warn(`session ${start.sessionId}: onSessionStart exceeded the handshake budget`);
          return await rejectCall(ErrorCode.Unavailable, "handshake budget exceeded");
        }
        this.logger.error(`session ${start.sessionId}: onSessionStart threw; rejecting`, error);
        return await rejectCall(ErrorCode.Unavailable, "agent error");
      }
    }

    let message: WireMessage;
    try {
      message = this.#acceptMessage(version, accept);
    } catch (error) {
      this.logger.error(`session ${start.sessionId}: the session.accept is invalid; rejecting`, error);
      return await rejectCall(ErrorCode.Unavailable, "agent error");
    }
    try {
      await writeText(ws, protocol.encode(message), writeBudget());
    } catch (error) {
      throw new SessionClosed(`session.accept was not written: ${error instanceof Error ? error.message : String(error)}`);
    }
    if (this.resume) {
      const events = message["events"];
      session.eventsSubscription = Array.isArray(events) ? (events as string[]) : undefined;
      this.#sessions.set(session.id, session);
    }
    return session;
  }

  #acceptMessage(version: number, accept: Accept): WireMessage {
    const names = accept.tools === undefined ? null : accept.tools;
    let tools: Tool[];
    if (names === null) {
      tools = [...this.#tools.values()];
    } else {
      const missing = names.filter((name) => !this.#tools.has(name));
      if (missing.length > 0) {
        throw new RangeError(`Accept.tools names undeclared tools: ${missing.join(", ")}`);
      }
      tools = names.flatMap((name) => this.#tools.get(name) ?? []);
    }
    const events = accept.events === undefined ? this.#subscription() : accept.events;
    if (events !== undefined && events !== null) {
      checkEventTypes(events);
    }
    const systemPrompt = accept.systemPrompt === undefined ? this.systemPrompt : accept.systemPrompt;
    const greeting = accept.greeting === undefined ? this.greeting : accept.greeting;
    const options: protocol.SessionAcceptOptions = {
      tools: tools.map((tool) => tool.declaration()),
      context: accept.context ?? this.context,
    };
    if (systemPrompt !== undefined && systemPrompt !== null) options.systemPrompt = systemPrompt;
    if (greeting !== undefined && greeting !== null) options.greeting = greeting;
    if (events !== undefined && events !== null) options.events = events;
    if (this.resume) options.resume = true;
    return protocol.sessionAccept(version, options);
  }

  #process(session: Session, frame: Frame): void {
    if (frame.binary) {
      this.logger.warn(`session ${session.id}: binary frame ignored`);
      return;
    }
    let message: protocol.ServerMessage;
    try {
      message = protocol.parseServerMessage(frame.text);
    } catch (error) {
      if (error instanceof ProtocolError) {
        this.logger.warn(`session ${session.id}: unreadable server message: ${error.message}`);
        return;
      }
      throw error;
    }
    session.lastSeq = message.seq;
    switch (message.type) {
      case "tool.call":
        this.#startTool(session, message);
        return;
      case "tool.cancel":
        this.#cancelTool(session, message);
        return;
      case "command.result":
        session.resolveCommand(message);
        if (this.#handlers.has("command.result")) {
          session.enqueue(message);
        }
        return;
      case "session.start":
        this.logger.warn(`session ${session.id}: second session.start ignored`);
        return;
      case "unknown":
        this.logger.debug(`session ${session.id}: unknown message type '${message.wireType}' ignored`);
        return;
      case "session.end":
        // Never dropped: kept aside and delivered last.
        session.finish(message);
        return;
      default:
        if ((this.#handlers.get(message.type)?.length ?? 0) > 0) {
          session.enqueue(message);
        } else if (message.type === "error") {
          this.logger.warn(`session ${session.id}: server reported an error in ${message.stage}: ${message.message}`);
        }
    }
  }

  async #deliver(session: Session, event: AgentEvent | CommandResult): Promise<void> {
    await reactingTo(event.seq || undefined, async () => {
      for (const handler of this.#handlers.get(event.type) ?? []) {
        try {
          await (handler as Handler<typeof event>)(session, event);
        } catch (error) {
          if (error instanceof SessionClosed) {
            this.logger.debug(`session ${session.id}: handler for ${event.type} hit a closed link`);
          } else {
            this.logger.error(`session ${session.id}: handler for ${event.type} threw`, error);
          }
        }
      }
    });
  }

  // -- tools -----------------------------------------------------------------------------------

  #startTool(session: Session, call: ToolCall): void {
    if (session.tools.size >= this.maxConcurrentTools) {
      void this.#answer(session, protocol.toolResult(call.id, { error: "too many tool calls at once" }));
      return;
    }
    const ctx = new ToolContext({
      session,
      callId: call.callId,
      requestId: call.id,
      name: call.name,
      kind: call.kind,
      epoch: call.epoch,
      idempotencyKey: call.idempotencyKey,
      timeoutMs: call.timeoutMs,
    });
    const entry = newToolEntry(ctx);
    // Registered before the tool first runs, so a tool.cancel right behind the tool.call finds it.
    session.tools.set(call.id, entry);
    const task: Promise<void> = this.#runTool(session, call, entry).finally(() => {
      entry.done = true;
      if (entry.graceTimer !== undefined) {
        clearTimeout(entry.graceTimer);
      }
      if (session.tools.get(call.id) === entry) {
        session.tools.delete(call.id);
      }
      this.#background.delete(task);
    });
    this.#background.add(task);
  }

  #cancelTool(session: Session, cancel: ToolCancel): void {
    const entry = session.tools.get(cancel.id);
    if (entry === undefined) {
      return;
    }
    entry.ctx.cancel(cancel.reason);
    if (entry.ctx.kind === "read_only") {
      entry.abandoned.resolve();
    }
    // side_effect: advice only. The tool finishes; its result becomes a late note.
  }

  async #runTool(session: Session, call: ToolCall, entry: ToolEntry): Promise<void> {
    const tool = this.#tools.get(call.name);
    if (tool === undefined) {
      await this.#answer(session, protocol.toolResult(call.id, { error: `unknown tool '${call.name}'` }));
      return;
    }
    let reply: WireMessage;
    try {
      const outcome = await Promise.race([
        tool.run(call.arguments, entry.ctx),
        entry.abandoned.promise.then(() => ABANDONED),
      ]);
      if (outcome === ABANDONED) {
        // The server stopped waiting (or the link is gone); a result would be ignored.
        this.logger.debug(`tool ${call.name} (${call.id}) abandoned: ${entry.ctx.cancelReason ?? "?"}`);
        return;
      }
      reply = protocol.toolResult(call.id, { result: outcome });
    } catch (error) {
      if (entry.ctx.cancelled && entry.ctx.kind === "read_only") {
        return; // it stopped because it was told to
      }
      if (error instanceof ToolError) {
        reply = protocol.toolResult(call.id, { error: error.message || "tool failed" });
      } else {
        this.logger.error(`tool ${call.name} (${call.id}) failed`, error);
        reply = protocol.toolResult(call.id, { error: `tool failed: ${errorName(error)}` });
      }
    }
    await this.#answer(session, reply);
  }

  async #answer(session: Session, reply: WireMessage): Promise<void> {
    const id = reply["id"] as number;
    try {
      await session.send(reply);
    } catch (error) {
      if (error instanceof ProtocolError) {
        // Too large, or not strict JSON (NaN / Infinity / a cycle): the server still gets an answer.
        const text = error.code === ErrorCode.TooLarge ? "tool result is too large" : "tool result is not valid JSON";
        try {
          await session.send(protocol.toolResult(id, { error: text }));
        } catch {
          // The link is gone; nothing more to do.
        }
      } else if (error instanceof SessionClosed) {
        this.logger.debug(`session ${session.id}: tool result dropped, the link is closed`);
      } else {
        throw error;
      }
    }
  }
}

class HandshakeBudgetExceeded extends Error {
  constructor() {
    super("handshake budget exceeded");
    this.name = "HandshakeBudgetExceeded";
  }
}

function sha256(text: string): Buffer {
  return createHash("sha256").update(text).digest();
}

/**
 * The listening side of an {@link Agent} (`await agent.listen(...)`).
 *
 * Rejects handshakes without the right bearer token with HTTP 401 before any WebSocket frame is
 * exchanged.
 */
export class AgentServer {
  /** The agent this server serves. */
  readonly agent: Agent;
  /** The bound host. */
  readonly host: string;

  readonly #port: number;
  readonly #tokenDigest: Buffer | null;
  readonly #tls: https.ServerOptions | undefined;
  #server: http.Server | undefined;
  #sockets: WebSocketServer | undefined;

  /** @internal Use {@link Agent.serve}. */
  constructor(agent: Agent, host: string, port: number, token: string | null, tls: https.ServerOptions | undefined) {
    this.agent = agent;
    this.host = host;
    this.#port = port;
    this.#tokenDigest = token === null || token === "" ? null : sha256(token);
    this.#tls = tls;
  }

  /** The bound port (useful with `port: 0`). */
  get port(): number {
    const address = this.#server?.address();
    return address !== null && address !== undefined && typeof address === "object"
      ? (address as AddressInfo).port
      : this.#port;
  }

  /** The `ws://` (or `wss://`) URL for `BRISVOX_AGENT_URL`. */
  get url(): string {
    const host = this.host.includes(":") ? `[${this.host}]` : this.host;
    return `${this.#tls === undefined ? "ws" : "wss"}://${host}:${this.port}`;
  }

  #authorized(request: http.IncomingMessage): boolean {
    if (this.#tokenDigest === null) {
      return true;
    }
    const header = request.headers.authorization ?? "";
    const space = header.indexOf(" ");
    const scheme = space < 0 ? header : header.slice(0, space);
    const presented = space < 0 ? "" : header.slice(space + 1).trim();
    // Compare digests: constant time and no length leak.
    return scheme.toLowerCase() === "bearer" && timingSafeEqual(sha256(presented), this.#tokenDigest);
  }

  /** Start listening. */
  async start(): Promise<this> {
    if (this.#server !== undefined) {
      return this;
    }
    this.agent.setDraining(false);
    const log = this.agent.logger;
    const wss = new WebSocketServer({
      noServer: true,
      maxPayload: 2 * protocol.MAX_MESSAGE_BYTES,
      perMessageDeflate: false,
    });
    const handler: http.RequestListener = (_request, response) => {
      response.writeHead(426, { "Content-Type": "text/plain", Connection: "close" });
      response.end("this is a brisvox agent: connect with a WebSocket\n");
    };
    const server: http.Server = this.#tls === undefined ? http.createServer(handler) : https.createServer(this.#tls, handler);
    server.headersTimeout = this.agent.handshakeTimeoutMs + 3000;
    server.on("upgrade", (request: http.IncomingMessage, socket: Duplex, head: Buffer) => {
      socket.on("error", () => undefined);
      if (!this.#authorized(request)) {
        socket.end(
          "HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\nContent-Length: 13\r\nConnection: close\r\n\r\nunauthorized\n",
        );
        return;
      }
      wss.handleUpgrade(request, socket, head, (ws) => {
        void this.agent.handle(ws).catch((error: unknown) => log.error("call failed", error));
      });
    });
    await new Promise<void>((resolve, reject) => {
      const onError = (error: Error): void => reject(error);
      server.once("error", onError);
      server.listen({ host: this.host, port: this.#port }, () => {
        server.off("error", onError);
        resolve();
      });
    });
    server.on("error", (error) => log.error("agent server error", error));
    this.#server = server;
    this.#sockets = wss;
    return this;
  }

  /** Stop listening and close the live calls' links. */
  async close(): Promise<void> {
    const server = this.#server;
    const wss = this.#sockets;
    this.#server = undefined;
    this.#sockets = undefined;
    if (server === undefined || wss === undefined) {
      return;
    }
    this.agent.setDraining(true);
    const closed = new Promise<void>((resolve) => server.close(() => resolve()));
    await Promise.all([...wss.clients].map((ws) => closeWs(ws, 1001, "going away", 3000)));
    wss.close();
    server.closeAllConnections();
    await withTimeout(closed, 5000, () => new Error("server close timed out")).catch(() => undefined);
    await this.agent.endParked(); // calls waiting for a resumption end now
  }
}

export type { RawToolDefinition, ToolDefinition, TypedToolDefinition };
