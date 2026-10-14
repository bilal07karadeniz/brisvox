/**
 * One call, seen from the agent: {@link Session}, the commands to act on it, and the plumbing
 * around one WebSocket link (a writer with a timeout, a reader inbox, a bounded event queue).
 *
 * @module
 */

import { AsyncLocalStorage } from "node:async_hooks";
import { WebSocket } from "ws";
import type { RawData } from "ws";
import * as protocol from "./protocol.js";
import { ErrorCode, ProtocolError } from "./protocol.js";
import type {
  AgentEvent,
  CallIdentity,
  Capabilities,
  CommandFields,
  CommandResult,
  ContextMessage,
  EventsLost,
  SessionEnd,
  SessionStart,
  WireMessage,
} from "./protocol.js";
import type { CancelReason, ToolContext } from "./tools.js";

/** A diagnostic sink. The default writes warnings and errors to `console.error`. */
export interface Logger {
  /** Details for debugging. */
  debug(message: string, ...details: unknown[]): void;
  /** Normal events (a call started). */
  info(message: string, ...details: unknown[]): void;
  /** Something went wrong but the call goes on. */
  warn(message: string, ...details: unknown[]): void;
  /** A handler or tool failed. */
  error(message: string, ...details: unknown[]): void;
}

/**
 * A logger printing to `console.error` from `level` up (`BRISVOX_AGENT_LOG=debug|info|warn|error|silent`
 * overrides `level` in the default logger).
 */
export function consoleLogger(level: "debug" | "info" | "warn" | "error" | "silent" = "warn"): Logger {
  const order = ["debug", "info", "warn", "error", "silent"] as const;
  const threshold = order.indexOf(level);
  const make =
    (name: (typeof order)[number]) =>
    (message: string, ...details: unknown[]): void => {
      if (order.indexOf(name) >= threshold) {
        console.error(`[brisvox-agent] ${name}: ${message}`, ...details);
      }
    };
  return { debug: make("debug"), info: make("info"), warn: make("warn"), error: make("error") };
}

/** The call's link is closed: nothing more can be sent. */
export class SessionClosed extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SessionClosed";
  }
}

/**
 * A command failed: the server answered `command.result` with `ok: false`, or the command broke
 * a protocol rule before it was sent. `code` is an {@link ErrorCode} (or the raw string of a code
 * this SDK does not know yet).
 */
export class CommandError extends Error {
  /** The error code. */
  readonly code: ErrorCode | (string & {});
  /** The explanation without the code. */
  readonly detail: string;

  constructor(code: ErrorCode | (string & {}), message: string) {
    super(`${code}: ${message}`);
    this.name = "CommandError";
    this.code = code;
    this.detail = message;
  }
}

/**
 * No `command.result` came in time. The command may still have been applied; for a `transfer`
 * that is `outcome_unknown`: retry only with the same `id`.
 */
export class CommandTimeout extends CommandError {
  /** The id of the command that got no answer. */
  readonly commandId: string;

  constructor(commandId: string, timeoutMs: number) {
    super(
      ErrorCode.OutcomeUnknown,
      `no result for command '${commandId}' within ${timeoutMs} ms; it may or may not have been applied`,
    );
    this.name = "CommandTimeout";
    this.commandId = commandId;
  }
}

/** A command that was sent with `wait: false`: its id and the promise of its result. */
export interface PendingCommand {
  /** The command's id. */
  readonly id: string;
  /**
   * Resolves with the server's `command.result` (also when `ok` is false: check it) and rejects
   * with {@link SessionClosed} when the link closes first. There is no timeout.
   */
  readonly result: Promise<CommandResult>;
}

/** Options every command takes. */
export interface CommandOptions {
  /**
   * The command's id (1 to 64 bytes; echoed in the result). Auto-generated when absent.
   * A `transfer` should get a reproducible one (`handoff-${session.id}`): a retry with the same
   * id cannot start a second transfer.
   */
  id?: string;
  /**
   * The `seq` of the event the command reacts to (the server measures the hop latency with it).
   * Taken from the event the current handler is reacting to when absent; `null` sends none.
   */
  re?: number | null;
  /** Wait for the `command.result` (default `true`). With `false` the call returns a {@link PendingCommand}. */
  wait?: boolean;
  /** How long to wait for the result, in milliseconds (default: the agent's `commandTimeoutMs`, longer for `end` and `transfer`). */
  timeoutMs?: number;
}

/** Options of {@link Session.say}. */
export interface SayOptions extends CommandOptions {
  /** Cut a running response like a barge-in (never the user). Default false. */
  interrupt?: boolean;
}

/** Options of {@link Session.end}. */
export interface EndOptions extends CommandOptions {
  /** A farewell the bot speaks before the call ends. */
  say?: string;
  /** Why (at most 512 bytes; for the logs). */
  reason?: string;
}

/** Options of {@link Session.transfer}. */
export interface TransferOptions extends CommandOptions {
  /** `cold` (default) or `warm`. */
  mode?: "cold" | "warm";
  /** What the receiving agent should know (at most 512 bytes). */
  summary?: string;
  /** Why (at most 512 bytes). */
  reason?: string;
}

/** Options of {@link Session.updateContext}. */
export interface UpdateContextOptions extends CommandOptions {
  /** Replaces the system prompt. */
  systemPrompt?: string;
  /** Messages to append (at most 32). */
  append?: readonly ContextMessage[];
}

/** Settings a session needs from its agent. */
export interface SessionSettings {
  /** Default wait for a `command.result`. */
  commandTimeoutMs: number;
  /** Bound on one WebSocket write. */
  writeTimeoutMs: number;
  /** Events queued per call for the handlers. */
  eventQueue: number;
  /** Where diagnostics go. */
  logger: Logger;
}

/** Runs the handlers of the agent for one delivered event. */
export type Deliver = (session: Session, event: AgentEvent | CommandResult) => Promise<void>;

/** The `seq` of the event the current handler is reacting to. */
const currentRe = new AsyncLocalStorage<number | undefined>();

/** Run `fn` as a reaction to the event with sequence number `seq` (commands inside carry `re`). */
export function reactingTo<T>(seq: number | undefined, fn: () => T): T {
  return currentRe.run(seq, fn);
}

interface Deferred<T> {
  promise: Promise<T>;
  resolve(value: T): void;
  reject(reason: unknown): void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** Resolve with `promise`'s outcome, or reject with `onTimeout()` after `ms`; the timer never keeps the process alive. */
export function withTimeout<T>(promise: Promise<T>, ms: number, onTimeout: () => Error): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(onTimeout()), Math.max(0, ms));
    timer.unref();
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error: unknown) => {
        clearTimeout(timer);
        reject(error instanceof Error ? error : new Error(String(error)));
      },
    );
  });
}

/** Write one text frame, bounded by `timeoutMs`; a write that does not finish in time takes the link down. */
export function writeText(ws: WebSocket, text: string, timeoutMs: number): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    if (ws.readyState !== WebSocket.OPEN) {
      reject(new SessionClosed("the link is closed"));
      return;
    }
    const timer = setTimeout(() => {
      reject(new SessionClosed("a write timed out; the link is considered down"));
      ws.terminate();
    }, Math.max(0, timeoutMs));
    timer.unref();
    try {
      ws.send(text, { compress: false, binary: false }, (error) => {
        clearTimeout(timer);
        if (error) {
          reject(new SessionClosed(`the link closed: ${error.message}`));
        } else {
          resolve();
        }
      });
    } catch (error) {
      clearTimeout(timer);
      reject(new SessionClosed(`the link closed: ${error instanceof Error ? error.message : String(error)}`));
    }
  });
}

/** Close a WebSocket politely and terminate it when the peer does not answer within `timeoutMs`. */
export function closeWs(ws: WebSocket, code: number, reason: string, timeoutMs = 2000): Promise<void> {
  return new Promise<void>((resolve) => {
    if (ws.readyState === WebSocket.CLOSED) {
      resolve();
      return;
    }
    const timer = setTimeout(() => {
      ws.terminate();
      resolve();
    }, timeoutMs);
    timer.unref();
    ws.once("close", () => {
      clearTimeout(timer);
      resolve();
    });
    try {
      ws.close(code, reason);
    } catch {
      ws.terminate();
    }
  });
}

/** One received frame. */
export interface Frame {
  /** The text of a text frame (empty for a binary one). */
  readonly text: string;
  /** Whether the frame was binary (not part of the protocol). */
  readonly binary: boolean;
}

/** The result of waiting for the next frame. */
export type Received =
  | { readonly kind: "frame"; readonly frame: Frame }
  | { readonly kind: "closed" }
  | { readonly kind: "timeout" };

function rawToText(data: RawData): string {
  if (Buffer.isBuffer(data)) {
    return data.toString("utf8");
  }
  if (Array.isArray(data)) {
    return Buffer.concat(data).toString("utf8");
  }
  return Buffer.from(data).toString("utf8");
}

/**
 * The frames of one link, in order. During the handshake (`receive`) it is bounded: past
 * `capacity` unread frames the newest are dropped and counted in `dropped` (the server sends next
 * to nothing before `session.accept`). After the handshake `forward` hands every frame straight to
 * a synchronous sink, so nothing is queued here and a burst of frames (a stalled event loop, one
 * TCP chunk holding hundreds of frames) can never lose a control frame or `session.end`; the real
 * backpressure is the handler queue behind the sink.
 */
export class Inbox {
  readonly #queue: Frame[] = [];
  readonly #capacity: number;
  #waiter: ((received: Received) => void) | undefined;
  #sink: ((frame: Frame) => void) | undefined;
  #onClosed: (() => void) | undefined;
  #closed = false;
  /** Frames dropped because the inbox was full (handshake only). */
  dropped = 0;

  constructor(ws: WebSocket, capacity = 256) {
    this.#capacity = capacity;
    ws.on("message", (data: RawData, isBinary: boolean) => {
      const frame: Frame = isBinary ? { text: "", binary: true } : { text: rawToText(data), binary: false };
      const sink = this.#sink;
      if (sink !== undefined) {
        sink(frame);
        return;
      }
      const waiter = this.#waiter;
      if (waiter !== undefined) {
        this.#waiter = undefined;
        waiter({ kind: "frame", frame });
      } else if (this.#queue.length < this.#capacity) {
        this.#queue.push(frame);
      } else {
        this.dropped += 1;
      }
    });
    ws.on("close", () => {
      this.#closed = true;
      const onClosed = this.#onClosed;
      if (onClosed !== undefined) {
        this.#onClosed = undefined;
        onClosed();
      }
      const waiter = this.#waiter;
      if (waiter !== undefined) {
        this.#waiter = undefined;
        waiter({ kind: "closed" });
      }
    });
    // A failing socket emits `error` and then `close`; the close path handles it.
    ws.on("error", () => undefined);
    if (ws.readyState === WebSocket.CLOSED) {
      this.#closed = true;
    }
  }

  /**
   * Switch to direct delivery: frames already queued go to `sink` first, in order, then every later
   * frame is passed to it synchronously from the socket's `message` event. `sink` must not throw
   * and must not run user code. Resolves when the link closes (at once if it already has).
   */
  forward(sink: (frame: Frame) => void): Promise<void> {
    return new Promise<void>((resolve) => {
      for (let frame = this.#queue.shift(); frame !== undefined; frame = this.#queue.shift()) {
        sink(frame);
      }
      if (this.#closed) {
        resolve();
        return;
      }
      this.#sink = sink;
      this.#onClosed = resolve;
    });
  }

  /** The next frame, or `closed` (frames received before the close are still delivered first), or `timeout`. */
  receive(timeoutMs: number): Promise<Received> {
    const queued = this.#queue.shift();
    if (queued !== undefined) {
      return Promise.resolve({ kind: "frame", frame: queued });
    }
    if (this.#closed) {
      return Promise.resolve({ kind: "closed" });
    }
    return new Promise<Received>((resolve) => {
      const timer = Number.isFinite(timeoutMs)
        ? setTimeout(() => {
            if (this.#waiter === settle) {
              this.#waiter = undefined;
            }
            resolve({ kind: "timeout" });
          }, Math.max(0, timeoutMs))
        : undefined;
      const settle = (received: Received): void => {
        if (timer !== undefined) {
          clearTimeout(timer);
        }
        resolve(received);
      };
      this.#waiter = settle;
    });
  }

  /** Whether the socket closed. */
  get closed(): boolean {
    return this.#closed;
  }
}

/** A running tool call, tracked by its `tool.call.id`. */
export interface ToolEntry {
  /** The context the tool sees. */
  readonly ctx: ToolContext;
  /** Resolves when the SDK stops waiting for the tool (cancelled, or the link closed). */
  readonly abandoned: Deferred<void>;
  /** Whether the tool finished (or was abandoned). */
  done: boolean;
  /** The grace timer of a `side_effect` tool after the link closed. */
  graceTimer: NodeJS.Timeout | undefined;
}

/**
 * One call, seen from the agent: its identity, the commands to act on it, and a `data` object
 * for the handlers' own per-call state.
 *
 * Created by the {@link Agent} when a call connects; handlers and tools receive it.
 */
export class Session {
  /** The `session.start` message (versions offered, capabilities, ...). */
  readonly start: SessionStart;
  /** The protocol version picked for this call. */
  readonly version: number;
  /** Free space for the handlers of this call. */
  readonly data: Record<string, unknown> = {};
  /** The `session.end` reason once the call is over (`link_closed` when the link dropped without one). */
  endReason: string | undefined;

  /** How many times the call's link was taken up again after a loss. */
  resumes = 0;

  /**
   * Reconnecting (`Agent({ resume: true })`): the link was lost and the session waits for the
   * server until `parkDeadline` (`performance.now()` time). @internal
   */
  parked = false;
  /** A resumption is being answered; the expiry timer must not end the session meanwhile. @internal */
  resuming = false;
  /** When the parked session expires, on the `performance.now()` clock. @internal */
  parkDeadline = 0;
  /** The expiry timer of a parked session. @internal */
  parkTimer: NodeJS.Timeout | undefined;
  /** The event subscription of the accept, repeated in the answer to a resumption. @internal */
  eventsSubscription: readonly string[] | undefined;
  /**
   * The `seq` of the last server message read on the current (or last) link: what a resuming
   * `session.start.resume.previousSeq` must continue from. @internal
   */
  lastSeq: number;

  #ws: WebSocket;
  readonly #settings: SessionSettings;
  readonly #deliver: Deliver;
  readonly #pending = new Map<string, Deferred<CommandResult>>();
  #closedGate = deferred<void>();
  #nextId = 0;
  #closed = false;

  // The event queue of the handlers. Overflow policy: the newest event is dropped and counted
  // (what the server does on its side); EventsLost is reported in front of the first event that
  // fits after the gap. `session.end` is held aside and delivered last: it is never dropped.
  readonly #events: (AgentEvent | CommandResult)[] = [];
  #lost = 0;
  #enqueued = 0;
  #delivered = 0;
  #gapAfter = 0;
  #end: SessionEnd | undefined;
  #endStarted = false;
  #pumping: Promise<void> | undefined;
  #abandoned = false;

  /** The tool calls in flight, by `tool.call.id`. @internal */
  readonly tools = new Map<number, ToolEntry>();

  /** @internal */
  constructor(ws: WebSocket, start: SessionStart, version: number, settings: SessionSettings, deliver: Deliver) {
    this.#ws = ws;
    this.start = start;
    this.lastSeq = start.seq;
    this.version = version;
    this.#settings = settings;
    this.#deliver = deliver;
    this.#closedGate.promise.catch(() => undefined);
  }

  // -- identity --------------------------------------------------------------------------------

  /** The server's id of the call. */
  get id(): string {
    return this.start.sessionId;
  }

  /** The name of the agent definition the call runs. */
  get agentName(): string {
    return this.start.agent;
  }

  /** What the server can do on this call (`transfer`, `tools`). */
  get capabilities(): Capabilities {
    return this.start.capabilities;
  }

  /**
   * Who is on the line: the carrier, the carrier's call id and the caller's and called numbers.
   * `undefined` unless the agent definition opted in (`[agent_link] caller_identity = true` or
   * `BRISVOX_AGENT_CALLER_IDENTITY=1`) and the call comes through a carrier (Twilio, Telnyx,
   * SIP). The numbers are absent when the server redacts phone numbers (`call.piiRedacted`).
   * Personal data: keep it out of your logs.
   */
  get call(): CallIdentity | undefined {
    return this.start.call;
  }

  /** Whether the link is closed (also true while the session is {@link Session.parked}). */
  get closed(): boolean {
    return this.#closed;
  }

  /** Whether the server's `session.end` (or a synthetic one) was received. @internal */
  get hasEnded(): boolean {
    return this.#end !== undefined;
  }

  /** The WebSocket of the current link. @internal */
  get socket(): WebSocket {
    return this.#ws;
  }

  /** Resolves when the link is closed (a lost link counts; a resumed one is open again). */
  waitClosed(): Promise<void> {
    return this.#closedGate.promise;
  }

  // -- commands --------------------------------------------------------------------------------

  /**
   * The bot speaks `text` verbatim (a *bot line*, no LLM request).
   *
   * The line waits (in order) while the user speaks or a response runs. With `interrupt: true` it
   * cuts a running response, like a barge-in (never the user). Resolves with the
   * {@link CommandResult} once the server has queued the line; rejects with {@link CommandError}
   * if it refused (`queue_full`, `call_ended`, `invalid_argument`).
   */
  say(text: string, options?: SayOptions & { wait?: true }): Promise<CommandResult>;
  say(text: string, options: SayOptions & { wait: false }): Promise<PendingCommand>;
  say(text: string, options?: SayOptions): Promise<CommandResult | PendingCommand>;
  say(text: string, options: SayOptions = {}): Promise<CommandResult | PendingCommand> {
    return this.#command(
      (fields) => protocol.say(text, { ...fields, interrupt: options.interrupt === true }),
      options,
      this.#settings.commandTimeoutMs,
    );
  }

  /**
   * End the call: speak the optional farewell `say`, wait until the bot is silent, then hang up
   * (the server bounds this at 30 s). The result comes when the call is being ended;
   * `session.end` (reason `ended_by_agent`) follows.
   */
  end(options?: EndOptions & { wait?: true }): Promise<CommandResult>;
  end(options: EndOptions & { wait: false }): Promise<PendingCommand>;
  end(options?: EndOptions): Promise<CommandResult | PendingCommand>;
  end(options: EndOptions = {}): Promise<CommandResult | PendingCommand> {
    const rest: { say?: string; reason?: string } = {};
    if (options.say !== undefined) rest.say = options.say;
    if (options.reason !== undefined) rest.reason = options.reason;
    return this.#command(
      (fields) => protocol.end({ ...fields, ...rest }),
      options,
      this.#settings.commandTimeoutMs + 35_000,
    );
  }

  /**
   * Transfer the caller to the E.164 number `destination` through the carrier.
   *
   * Needs `session.capabilities.transfer` (else the server answers `unsupported`). The command
   * always carries an `id` (generated when you pass none), because the carrier's idempotency key
   * derives from it: after a {@link CommandTimeout} (outcome unknown) or a lost result, repeat
   * the transfer **with the same** `id` and it cannot start twice.
   */
  transfer(destination: string, options?: TransferOptions & { wait?: true }): Promise<CommandResult>;
  transfer(destination: string, options: TransferOptions & { wait: false }): Promise<PendingCommand>;
  transfer(destination: string, options?: TransferOptions): Promise<CommandResult | PendingCommand>;
  transfer(destination: string, options: TransferOptions = {}): Promise<CommandResult | PendingCommand> {
    const rest: { mode?: "cold" | "warm"; summary?: string; reason?: string } = {};
    if (options.mode !== undefined) rest.mode = options.mode;
    if (options.summary !== undefined) rest.summary = options.summary;
    if (options.reason !== undefined) rest.reason = options.reason;
    return this.#command(
      (fields) => protocol.transfer(destination, { ...fields, ...rest }),
      { ...options, id: options.id ?? this.#newId() },
      this.#settings.commandTimeoutMs + 50_000,
    );
  }

  /**
   * Edit the conversation from the next response on: replace the system prompt and/or append up
   * to 32 messages (all of the command or nothing).
   */
  updateContext(options?: UpdateContextOptions & { wait?: true }): Promise<CommandResult>;
  updateContext(options: UpdateContextOptions & { wait: false }): Promise<PendingCommand>;
  updateContext(options?: UpdateContextOptions): Promise<CommandResult | PendingCommand>;
  updateContext(options: UpdateContextOptions = {}): Promise<CommandResult | PendingCommand> {
    const rest: { systemPrompt?: string; append?: readonly ContextMessage[] } = {};
    if (options.systemPrompt !== undefined) rest.systemPrompt = options.systemPrompt;
    if (options.append !== undefined) rest.append = options.append;
    return this.#command(
      (fields) => protocol.contextUpdate({ ...fields, ...rest }),
      options,
      this.#settings.commandTimeoutMs,
    );
  }

  /**
   * Offer only the tools named in `enabled` (local and remote) to the LLM from the next response
   * on; `null` offers all of them again.
   */
  setTools(enabled: readonly string[] | null, options?: CommandOptions & { wait?: true }): Promise<CommandResult>;
  setTools(enabled: readonly string[] | null, options: CommandOptions & { wait: false }): Promise<PendingCommand>;
  setTools(enabled: readonly string[] | null, options?: CommandOptions): Promise<CommandResult | PendingCommand>;
  setTools(enabled: readonly string[] | null, options: CommandOptions = {}): Promise<CommandResult | PendingCommand> {
    return this.#command((fields) => protocol.toolsSet(enabled, fields), options, this.#settings.commandTimeoutMs);
  }

  // -- internals -------------------------------------------------------------------------------

  #newId(): string {
    this.#nextId += 1;
    return `c${this.#nextId}`;
  }

  /** Write one message, bounded by the agent's write timeout. @internal */
  async send(message: WireMessage): Promise<void> {
    const text = protocol.encode(message);
    if (this.#closed) {
      throw new SessionClosed("the link is closed");
    }
    await writeText(this.#ws, text, this.#settings.writeTimeoutMs);
  }

  async #command(
    build: (fields: CommandFields) => WireMessage,
    options: CommandOptions,
    defaultTimeoutMs: number,
  ): Promise<CommandResult | PendingCommand> {
    const re = options.re === undefined ? currentRe.getStore() : (options.re ?? undefined);
    // Without waiting there is nobody to read the result, but an id is still sent so a failure
    // is logged instead of lost.
    const id = options.id ?? this.#newId();
    if (this.#pending.has(id)) {
      throw new RangeError(`command id '${id}' is already waiting for its result`);
    }
    const fields: CommandFields = re === undefined ? { id } : { id, re };
    let message: WireMessage;
    try {
      message = build(fields);
    } catch (error) {
      throw asCommandError(error);
    }
    const slot = deferred<CommandResult>();
    this.#pending.set(id, slot);
    try {
      await this.send(message);
    } catch (error) {
      this.#pending.delete(id);
      throw asCommandError(error);
    }
    if (options.wait === false) {
      const kind = String(message["type"]);
      slot.promise.then(
        (result) => {
          if (!result.ok && result.error !== undefined) {
            this.#settings.logger.warn(
              `command ${kind} ${id} failed: ${result.error.code}: ${result.error.message}`,
            );
          }
        },
        (error: unknown) => {
          this.#settings.logger.debug(`command ${kind} ${id} was not answered: ${String(error)}`);
        },
      );
      return { id, result: slot.promise };
    }
    const timeoutMs = options.timeoutMs ?? defaultTimeoutMs;
    let result: CommandResult;
    try {
      result = await withTimeout(slot.promise, timeoutMs, () => new CommandTimeout(id, timeoutMs));
    } catch (error) {
      this.#pending.delete(id);
      throw error;
    }
    if (!result.ok) {
      const error = result.error;
      throw new CommandError(error?.code ?? ErrorCode.BadMessage, error?.message ?? "failed");
    }
    return result;
  }

  /** A `command.result` arrived. @internal */
  resolveCommand(result: CommandResult): void {
    const slot = this.#pending.get(result.id);
    if (slot !== undefined) {
      this.#pending.delete(result.id);
      slot.resolve(result);
    }
  }

  /** The link closed: nothing more can be sent and waiting commands fail. @internal */
  markClosed(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    const error = new SessionClosed("the link closed before the command was answered");
    const pending = [...this.#pending.values()];
    this.#pending.clear();
    for (const slot of pending) {
      slot.reject(error);
    }
    this.#closedGate.resolve();
  }

  /** A resumed link took the call up again: move the session onto it. @internal */
  reopen(ws: WebSocket): void {
    if (this.parkTimer !== undefined) {
      clearTimeout(this.parkTimer);
      this.parkTimer = undefined;
    }
    this.parked = false;
    this.resuming = false;
    this.#ws = ws;
    this.resumes += 1;
    if (this.#closed) {
      this.#closed = false;
      this.#closedGate = deferred<void>();
      this.#closedGate.promise.catch(() => undefined);
    }
  }

  // -- the handlers' queue ---------------------------------------------------------------------

  /** Queue an event for the handlers (drops the newest when the queue is full). @internal */
  enqueue(event: AgentEvent | CommandResult): void {
    if (this.#end !== undefined) {
      return; // the call is over; nothing is delivered after session.end
    }
    if (this.#events.length >= this.#settings.eventQueue) {
      // Overflow policy: drop the newest and count it (the server does the same). The first drop
      // of a gap remembers how many items were queued ahead of it.
      if (this.#lost === 0) {
        this.#gapAfter = this.#enqueued;
      }
      this.#lost += 1;
      return;
    }
    this.#events.push(event);
    this.#enqueued += 1;
    this.#kick();
  }

  /** Queue the session end (once). It is held aside, so a full queue can never drop it. @internal */
  finish(end: SessionEnd): void {
    if (this.#end !== undefined) {
      return;
    }
    this.#end = end;
    this.endReason = end.reason;
    this.#kick();
  }

  #kick(): void {
    if (this.#pumping === undefined && !this.#abandoned) {
      this.#pumping = this.#pump();
    }
  }

  async #pump(): Promise<void> {
    // The exit check and the reset of `#pumping` happen in one synchronous step (`finally` runs
    // right at the `break`), so an event queued after the last check always starts a new pump.
    try {
      // Yield first: `#kick` stores this promise, and `finally` must not run before it is stored.
      await Promise.resolve();
      for (;;) {
        if (this.#abandoned) {
          break;
        }
        let event: AgentEvent | CommandResult | undefined = this.#events.shift();
        let isEnd = false;
        if (event === undefined) {
          if (this.#end !== undefined && !this.#endStarted) {
            event = this.#end;
            isEnd = true;
          } else {
            break;
          }
        }
        // EventsLost goes in front of the next event that fits after the gap, i.e. once the
        // events queued ahead of the first drop were delivered (the server's rule too).
        if (this.#lost > 0 && this.#delivered >= this.#gapAfter) {
          const count = this.#lost;
          this.#lost = 0;
          this.#settings.logger.warn(`session ${this.id}: handlers fell behind, ${count} events dropped`);
          const lost: EventsLost = { type: "events.lost", seq: 0, tMs: 0, count };
          await this.#deliver(this, lost);
        }
        if (isEnd) {
          this.#endStarted = true;
        } else {
          this.#delivered += 1;
        }
        await this.#deliver(this, event);
      }
    } finally {
      this.#pumping = undefined;
    }
  }

  /**
   * Wait for the queue to drain (the end included) for at most `timeoutMs`. Past that the handlers
   * are abandoned (JavaScript cannot cancel them) and, if it has not run yet, `session.end` gets
   * `endGraceMs` of its own. @internal
   */
  async drain(timeoutMs: number, endGraceMs: number): Promise<void> {
    const pumping = this.#pumping;
    if (pumping === undefined) {
      return;
    }
    try {
      await withTimeout(pumping, timeoutMs, () => new Error("timeout"));
    } catch {
      this.#settings.logger.warn(`session ${this.id}: handlers did not finish in ${timeoutMs} ms; abandoned`);
      this.#abandoned = true;
      const end = this.#end;
      if (end !== undefined && !this.#endStarted) {
        this.#endStarted = true;
        try {
          await withTimeout(this.#deliver(this, end), endGraceMs, () => new Error("timeout"));
        } catch {
          this.#settings.logger.warn(`session ${this.id}: on_session_end did not finish in ${endGraceMs} ms`);
        }
      }
    }
  }

  /** Tell every running tool the link is gone. @internal */
  windDownTools(graceMs: number): void {
    for (const entry of this.tools.values()) {
      entry.ctx.cancel("link_closed" satisfies CancelReason);
      if (entry.ctx.kind === "read_only") {
        entry.abandoned.resolve();
      } else if (entry.graceTimer === undefined && !entry.done) {
        // A side_effect tool may wind down by itself; a hung one is not waited for forever.
        entry.graceTimer = setTimeout(() => entry.abandoned.resolve(), graceMs);
        entry.graceTimer.unref();
      }
    }
  }
}

function asCommandError(error: unknown): unknown {
  return error instanceof ProtocolError ? new CommandError(error.code, error.message) : error;
}

/** Create the tool entry for a `tool.call` (registered by the caller before the tool first runs). @internal */
export function newToolEntry(ctx: ToolContext): ToolEntry {
  return { ctx, abandoned: deferred<void>(), done: false, graceTimer: undefined };
}
