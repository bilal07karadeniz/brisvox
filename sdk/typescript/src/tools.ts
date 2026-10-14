/**
 * Typed remote tools: a function the LLM of the call may call.
 *
 * A tool is declared with `agent.tool({...})` (see {@link Agent}). Its JSON schema is either built
 * with {@link s} (then the handler's argument type is inferred and the arguments are validated)
 * or written by hand as a plain JSON-schema object (then the handler gets the arguments as the
 * server validated and sent them).
 *
 * @module
 */

import { MAX_TOOL_TIMEOUT_MS, validateToolName } from "./protocol.js";
import type { ToolKind, WireToolDeclaration } from "./protocol.js";
import { Schema, SchemaViolation } from "./schema.js";
import type { Infer, JsonSchema } from "./schema.js";
import type { Session } from "./session.js";

/**
 * Throw from a tool to fail it with a message the LLM gets verbatim.
 *
 * Any other error fails the tool too, but the LLM only learns the error's class name (the
 * details, which may be internal, go to the log).
 */
export class ToolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ToolError";
  }
}

/**
 * The arguments of a `tool.call` do not fit the schema (the server validates them against the
 * declared schema first, so this is rare).
 */
export class ToolArgumentError extends ToolError {
  constructor(message: string) {
    super(message);
    this.name = "ToolArgumentError";
  }
}

/** Why a tool call was cancelled: the server's `tool.cancel` reasons, or `link_closed`. */
export type CancelReason = "interrupted" | "timeout" | "abandoned" | "link_closed" | (string & {});

/**
 * What a tool may want to know about the call it runs for.
 *
 * `idempotencyKey` is stable for the tool call of the call (16 hex digits): pass it to APIs that
 * deduplicate requests, so a `side_effect` tool never acts twice.
 */
export class ToolContext {
  /** The call this tool runs for. */
  readonly session: Session;
  /** The engine's id of the LLM tool call. */
  readonly callId: string;
  /** The link's id of this request (`tool.call.id`). */
  readonly requestId: number;
  /** The tool's name. */
  readonly name: string;
  /** `read_only` or `side_effect`. */
  readonly kind: ToolKind;
  /** The turn epoch the call belongs to. */
  readonly epoch: number;
  /** 16 hex digits, stable for this tool call: pass it to APIs that deduplicate. */
  readonly idempotencyKey: string;
  /** How long the server waits for the result. */
  readonly timeoutMs: number;

  readonly #controller = new AbortController();

  /** @internal */
  constructor(fields: {
    session: Session;
    callId: string;
    requestId: number;
    name: string;
    kind: ToolKind;
    epoch: number;
    idempotencyKey: string;
    timeoutMs: number;
  }) {
    this.session = fields.session;
    this.callId = fields.callId;
    this.requestId = fields.requestId;
    this.name = fields.name;
    this.kind = fields.kind;
    this.epoch = fields.epoch;
    this.idempotencyKey = fields.idempotencyKey;
    this.timeoutMs = fields.timeoutMs;
  }

  /**
   * Aborted when the server sent `tool.cancel` or the link closed. A `read_only` tool should stop
   * (the SDK stops waiting for it and sends no result); for a `side_effect` tool this is advice
   * only: it may finish and its result still goes out (it becomes a late note), as long as
   * the link is up.
   */
  get signal(): AbortSignal {
    return this.#controller.signal;
  }

  /** Whether {@link signal} is aborted. */
  get cancelled(): boolean {
    return this.#controller.signal.aborted;
  }

  /** `interrupted`, `timeout`, `abandoned` or `link_closed` once {@link cancelled}. */
  get cancelReason(): CancelReason | undefined {
    const reason: unknown = this.#controller.signal.reason;
    return this.#controller.signal.aborted && typeof reason === "string" ? reason : undefined;
  }

  /** @internal Abort the signal (once) with `reason`. */
  cancel(reason: CancelReason): void {
    if (!this.#controller.signal.aborted) {
      this.#controller.abort(reason);
    }
  }
}

/** What a tool handler may return: any JSON value (a string reaches the LLM as it is). */
export type ToolReturn = unknown;

/** The options every tool takes. */
export interface ToolOptions {
  /** 1 to 64 of `A-Z a-z 0-9 _ -`. */
  name: string;
  /** For the LLM (required: the model decides from it when to call the tool). */
  description: string;
  /**
   * `read_only` (may run speculatively, cancelled on barge-in or timeout; the default) or
   * `side_effect` (never speculative, never run twice for one call: use
   * `ctx.idempotencyKey` against APIs that deduplicate).
   */
  kind?: ToolKind;
  /** The server's wait for the result (default 5000, at most 30000). */
  timeoutMs?: number;
  /** The bot says a filler phrase after this long without a result (default 700; 0 disables). */
  fillerAfterMs?: number;
  /**
   * Hold the tool for the user's confirmation (`side_effect` tools only): when the model calls
   * it, the server does not; it tells the model to ask the user, and calls the tool only after
   * the user's next reply confirms. Use it for a transfer, a payment, a booking.
   */
  confirm?: boolean;
  /**
   * Allow an identical call (same arguments) to run again in a later response of the same turn
   * (`side_effect` tools only). By default the server refuses such a repeat as `already_done`
   * once the first one succeeded, since a model that read the result and asks again is
   * repeating itself (a second booking). Set it where doing the same thing twice is
   * legitimate, such as pressing the same IVR digit twice.
   */
  repeatable?: boolean;
}

/** A tool whose parameters are a schema built with {@link s}: arguments are typed and validated. */
export interface TypedToolDefinition<P extends Schema<Record<string, unknown>>> extends ToolOptions {
  /** An object schema: `s.object({...})`. */
  parameters: P;
  /** Runs the tool. Plain functions work, but a blocking one stalls every call: use worker threads. */
  handler: (args: Infer<P>, ctx: ToolContext) => ToolReturn | Promise<ToolReturn>;
}

/** A tool whose parameters are a hand-written JSON-schema object (`"type": "object"`). */
export interface RawToolDefinition<Args extends Record<string, unknown> = Record<string, unknown>>
  extends ToolOptions {
  /** A JSON Schema with `"type": "object"` (no remote `$ref`). */
  parameters?: JsonSchema;
  /** Runs the tool with the arguments as the server sent them (the server validated them). */
  handler: (args: Args, ctx: ToolContext) => ToolReturn | Promise<ToolReturn>;
}

/** Either form of tool definition. */
export type ToolDefinition = TypedToolDefinition<Schema<Record<string, unknown>>> | RawToolDefinition;

const EMPTY_PARAMETERS: JsonSchema = { type: "object", properties: {} };

/** A declared tool: its wire declaration and the function behind it. */
export class Tool {
  /** The tool's name. */
  readonly name: string;
  /** The description for the LLM. */
  readonly description: string;
  /** The JSON Schema of the parameters. */
  readonly parameters: JsonSchema;
  /** `read_only` or `side_effect`. */
  readonly kind: ToolKind;
  /** The server's wait for the result, when set. */
  readonly timeoutMs: number | undefined;
  /** The filler delay, when set. */
  readonly fillerAfterMs: number | undefined;
  /** Whether the server holds the tool for the user's confirmation. */
  readonly confirm: boolean;
  /** Whether an identical call may repeat within a turn. */
  readonly repeatable: boolean;

  readonly #schema: Schema<Record<string, unknown>> | undefined;
  readonly #handler: (args: never, ctx: ToolContext) => unknown;

  /**
   * Declare a tool.
   *
   * @throws {RangeError} an illegal name, kind, timeout or description, or parameters that are not
   * an object schema.
   */
  constructor(definition: ToolDefinition) {
    this.name = validateToolName(definition.name);
    const kind = definition.kind ?? "read_only";
    if (kind !== "read_only" && kind !== "side_effect") {
      throw new RangeError(`tool kind must be 'read_only' or 'side_effect', not '${String(kind)}'`);
    }
    this.kind = kind;
    const timeout = definition.timeoutMs;
    if (timeout !== undefined && !(Number.isInteger(timeout) && timeout > 0 && timeout <= MAX_TOOL_TIMEOUT_MS)) {
      throw new RangeError(`timeoutMs must be an integer in 1..${MAX_TOOL_TIMEOUT_MS}`);
    }
    this.timeoutMs = timeout;
    const filler = definition.fillerAfterMs;
    if (filler !== undefined && !(Number.isInteger(filler) && filler >= 0)) {
      throw new RangeError("fillerAfterMs must be an integer, 0 or more");
    }
    this.fillerAfterMs = filler;
    this.confirm = definition.confirm === true;
    if (this.confirm && kind !== "side_effect") {
      throw new RangeError("confirm needs kind 'side_effect' (a read-only tool changes nothing)");
    }
    this.repeatable = definition.repeatable === true;
    if (this.repeatable && kind !== "side_effect") {
      throw new RangeError("repeatable needs kind 'side_effect' (a read-only tool may always repeat)");
    }
    const description = definition.description.trim();
    if (description.length === 0) {
      throw new RangeError(`tool '${this.name}' needs a description`);
    }
    this.description = description;
    if (typeof definition.handler !== "function") {
      throw new TypeError(`tool '${this.name}' needs a handler function`);
    }
    this.#handler = definition.handler as (args: never, ctx: ToolContext) => unknown;
    const parameters: unknown = definition.parameters;
    if (parameters instanceof Schema) {
      const json = parameters.jsonSchema();
      if (json["type"] !== "object") {
        throw new RangeError(`tool '${this.name}': parameters must be an object schema (s.object)`);
      }
      this.#schema = parameters as Schema<Record<string, unknown>>;
      this.parameters = json;
    } else {
      const json = parameters === undefined ? EMPTY_PARAMETERS : (parameters as JsonSchema);
      if (json["type"] !== "object") {
        throw new RangeError(`tool '${this.name}': parameters must be a JSON schema with "type": "object"`);
      }
      this.#schema = undefined;
      this.parameters = json;
    }
  }

  /** The entry of `session.accept.tools`. */
  declaration(): WireToolDeclaration {
    const out: WireToolDeclaration = {
      name: this.name,
      description: this.description,
      parameters: this.parameters,
      kind: this.kind,
    };
    if (this.timeoutMs !== undefined) {
      out.timeout_ms = this.timeoutMs;
    }
    if (this.fillerAfterMs !== undefined) {
      out.filler_after_ms = this.fillerAfterMs;
    }
    if (this.confirm) {
      out.confirm = true;
    }
    if (this.repeatable) {
      out.repeatable = true;
    }
    return out;
  }

  /**
   * Validate the JSON `args` (typed schemas only) and call the handler.
   *
   * @throws {ToolArgumentError} the arguments do not fit the schema.
   */
  async run(args: Readonly<Record<string, unknown>>, ctx: ToolContext): Promise<unknown> {
    let bound: unknown = args;
    if (this.#schema !== undefined) {
      try {
        bound = this.#schema.parse(args, "$");
      } catch (error) {
        if (error instanceof SchemaViolation) {
          throw new ToolArgumentError(`invalid arguments: ${error.message}`);
        }
        throw error;
      }
    }
    return await (this.#handler as (a: unknown, c: ToolContext) => unknown)(bound, ctx);
  }
}

/**
 * Declare a tool without an agent (the typed form infers the handler's arguments from
 * `parameters`). `agent.tool(...)` does this and registers it.
 */
export function defineTool<P extends Schema<Record<string, unknown>>>(definition: TypedToolDefinition<P>): Tool;
export function defineTool<Args extends Record<string, unknown> = Record<string, unknown>>(
  definition: RawToolDefinition<Args>,
): Tool;
export function defineTool(definition: ToolDefinition): Tool {
  return new Tool(definition);
}

/** The class name of an error, for "tool failed: <Name>" (never its message). */
export function errorName(error: unknown): string {
  if (error instanceof Error) {
    return error.constructor.name || error.name || "Error";
  }
  return typeof error;
}
