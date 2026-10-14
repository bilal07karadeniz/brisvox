/**
 * A tiny typed JSON-schema builder for tool parameters.
 *
 * A schema built with {@link s} does three jobs at once: it is the JSON Schema that goes into
 * `session.accept.tools[].parameters`, it validates the `arguments` of a `tool.call` (the server
 * validates them too, so this is a second line of defence that also applies defaults), and it
 * gives the tool handler its argument type through {@link Infer}.
 *
 * ```ts
 * const params = s.object({
 *   time: s.string({ description: "HH:MM, 24 hours" }),
 *   partySize: s.optional(s.integer({ minimum: 1, maximum: 12 }), 2),
 * });
 * type Args = Infer<typeof params>; // { time: string; partySize: number }
 * ```
 *
 * Why not zod: a tool schema needs a handful of node kinds, and zod would be the SDK's only
 * heavy dependency (and its JSON-Schema output a second API to pin). Tools whose schema is
 * written by hand as a plain JSON-schema object work too (see `ToolDefinition.parameters`);
 * the server validates those, the handler gets the arguments as received.
 *
 * @module
 */

/** A JSON Schema object (the subset this builder emits; hand-written schemas may use any keyword). */
export type JsonSchema = Record<string, unknown>;

/** A value that does not fit its schema; `path` locates it (`$.contact.name`). */
export class SchemaViolation extends Error {
  /** Where in the arguments the value sits. */
  readonly path: string;

  constructor(path: string, message: string) {
    super(`${path}: ${message}`);
    this.name = "SchemaViolation";
    this.path = path;
  }
}

/**
 * A schema node. `T` is the TypeScript type of a valid value, `Opt` marks a property that may be
 * absent from its object (see {@link s.optional}).
 */
export abstract class Schema<T, Opt extends boolean = false> {
  /** Phantom: the type of a valid value. Never set at runtime. */
  declare readonly _type: T;
  /** Phantom: whether the property may be absent. Never set at runtime. */
  declare readonly _optional: Opt;

  /** Whether an object property of this schema may be absent. */
  readonly optional: boolean = false;
  /** The default applied when an object property is absent (`undefined`: none). */
  readonly defaultValue: unknown = undefined;

  /** The JSON Schema of this node. */
  abstract jsonSchema(): JsonSchema;

  /** Check `value` and return it typed; throws {@link SchemaViolation}. */
  abstract parse(value: unknown, path: string): T;
}

/** The type a schema accepts. */
export type Infer<S> = S extends Schema<infer T, boolean> ? T : never;

type OptionalKeys<Shape extends Record<string, Schema<unknown, boolean>>> = {
  [K in keyof Shape]: Shape[K] extends Schema<unknown, true> ? K : never;
}[keyof Shape];

type Simplify<T> = { [K in keyof T]: T[K] } & {};

/** The value type of an object schema: optional properties are optional keys. */
export type ObjectOutput<Shape extends Record<string, Schema<unknown, boolean>>> = Simplify<
  { [K in Exclude<keyof Shape, OptionalKeys<Shape>>]: Infer<Shape[K]> } & {
    [K in OptionalKeys<Shape>]?: Infer<Shape[K]>;
  }
>;

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function describe(base: JsonSchema, description: string | undefined): JsonSchema {
  if (description !== undefined) {
    base["description"] = description;
  }
  return base;
}

function describeType(value: unknown): string {
  if (value === null) {
    return "null";
  }
  return Array.isArray(value) ? "array" : typeof value;
}

/** Options of {@link s.string}. */
export interface StringOptions {
  /** For the LLM. */
  description?: string;
  /** Fewest characters (code points). */
  minLength?: number;
  /** Most characters (code points). */
  maxLength?: number;
  /** A regular expression the whole string must match (JSON Schema `pattern`: unanchored search). */
  pattern?: string;
  /** A JSON Schema `format` (`date`, `date-time`, `email`, ...): advice for the server's validator. */
  format?: string;
}

class StringSchema extends Schema<string> {
  readonly #options: StringOptions;
  readonly #pattern: RegExp | undefined;

  constructor(options: StringOptions) {
    super();
    this.#options = options;
    this.#pattern = options.pattern === undefined ? undefined : new RegExp(options.pattern, "u");
  }

  jsonSchema(): JsonSchema {
    const o = this.#options;
    const out = describe({ type: "string" }, o.description);
    if (o.minLength !== undefined) out["minLength"] = o.minLength;
    if (o.maxLength !== undefined) out["maxLength"] = o.maxLength;
    if (o.pattern !== undefined) out["pattern"] = o.pattern;
    if (o.format !== undefined) out["format"] = o.format;
    return out;
  }

  parse(value: unknown, path: string): string {
    if (typeof value !== "string") {
      throw new SchemaViolation(path, `expected a string, got ${describeType(value)}`);
    }
    const o = this.#options;
    if (o.minLength !== undefined || o.maxLength !== undefined) {
      let length = 0;
      for (const _ of value) {
        length += 1;
      }
      if (o.minLength !== undefined && length < o.minLength) {
        throw new SchemaViolation(path, `shorter than ${o.minLength} characters`);
      }
      if (o.maxLength !== undefined && length > o.maxLength) {
        throw new SchemaViolation(path, `longer than ${o.maxLength} characters`);
      }
    }
    if (this.#pattern !== undefined && !this.#pattern.test(value)) {
      throw new SchemaViolation(path, `does not match the pattern ${o.pattern ?? ""}`);
    }
    return value;
  }
}

/** Options of {@link s.number} and {@link s.integer}. */
export interface NumberOptions {
  /** For the LLM. */
  description?: string;
  /** Smallest value. */
  minimum?: number;
  /** Largest value. */
  maximum?: number;
}

class NumberSchema extends Schema<number> {
  readonly #integer: boolean;
  readonly #options: NumberOptions;

  constructor(integer: boolean, options: NumberOptions) {
    super();
    this.#integer = integer;
    this.#options = options;
  }

  jsonSchema(): JsonSchema {
    const o = this.#options;
    const out = describe({ type: this.#integer ? "integer" : "number" }, o.description);
    if (o.minimum !== undefined) out["minimum"] = o.minimum;
    if (o.maximum !== undefined) out["maximum"] = o.maximum;
    return out;
  }

  parse(value: unknown, path: string): number {
    if (typeof value !== "number" || !Number.isFinite(value)) {
      throw new SchemaViolation(
        path,
        `expected ${this.#integer ? "an integer" : "a number"}, got ${describeType(value)}`,
      );
    }
    if (this.#integer && !Number.isInteger(value)) {
      throw new SchemaViolation(path, `expected an integer, got ${value}`);
    }
    const o = this.#options;
    if (o.minimum !== undefined && value < o.minimum) {
      throw new SchemaViolation(path, `${value} is below the minimum ${o.minimum}`);
    }
    if (o.maximum !== undefined && value > o.maximum) {
      throw new SchemaViolation(path, `${value} is above the maximum ${o.maximum}`);
    }
    return value;
  }
}

class BooleanSchema extends Schema<boolean> {
  readonly #description: string | undefined;

  constructor(description: string | undefined) {
    super();
    this.#description = description;
  }

  jsonSchema(): JsonSchema {
    return describe({ type: "boolean" }, this.#description);
  }

  parse(value: unknown, path: string): boolean {
    if (typeof value !== "boolean") {
      throw new SchemaViolation(path, `expected a boolean, got ${describeType(value)}`);
    }
    return value;
  }
}

class EnumSchema<V extends string | number> extends Schema<V> {
  readonly #values: readonly V[];
  readonly #description: string | undefined;

  constructor(values: readonly V[], description: string | undefined) {
    super();
    this.#values = values;
    this.#description = description;
  }

  jsonSchema(): JsonSchema {
    return describe({ enum: [...this.#values] }, this.#description);
  }

  parse(value: unknown, path: string): V {
    const found = this.#values.find((candidate) => candidate === value);
    if (found === undefined) {
      throw new SchemaViolation(path, `${JSON.stringify(value)} is not one of ${JSON.stringify(this.#values)}`);
    }
    return found;
  }
}

/** Options of {@link s.array}. */
export interface ArrayOptions {
  /** For the LLM. */
  description?: string;
  /** Fewest items. */
  minItems?: number;
  /** Most items. */
  maxItems?: number;
}

class ArraySchema<T> extends Schema<T[]> {
  readonly #item: Schema<T, boolean>;
  readonly #options: ArrayOptions;

  constructor(item: Schema<T, boolean>, options: ArrayOptions) {
    super();
    this.#item = item;
    this.#options = options;
  }

  jsonSchema(): JsonSchema {
    const o = this.#options;
    const out = describe({ type: "array", items: this.#item.jsonSchema() }, o.description);
    if (o.minItems !== undefined) out["minItems"] = o.minItems;
    if (o.maxItems !== undefined) out["maxItems"] = o.maxItems;
    return out;
  }

  parse(value: unknown, path: string): T[] {
    if (!Array.isArray(value)) {
      throw new SchemaViolation(path, `expected an array, got ${describeType(value)}`);
    }
    const o = this.#options;
    if (o.minItems !== undefined && value.length < o.minItems) {
      throw new SchemaViolation(path, `fewer than ${o.minItems} items`);
    }
    if (o.maxItems !== undefined && value.length > o.maxItems) {
      throw new SchemaViolation(path, `more than ${o.maxItems} items`);
    }
    return value.map((item: unknown, index) => this.#item.parse(item, `${path}[${index}]`));
  }
}

/** An object schema; its properties are the `shape`. */
export class ObjectSchema<Shape extends Record<string, Schema<unknown, boolean>>> extends Schema<
  ObjectOutput<Shape>
> {
  readonly #shape: Shape;
  readonly #description: string | undefined;

  constructor(shape: Shape, description: string | undefined) {
    super();
    this.#shape = shape;
    this.#description = description;
  }

  jsonSchema(): JsonSchema {
    const properties: Record<string, JsonSchema> = {};
    const required: string[] = [];
    for (const [key, schema] of Object.entries(this.#shape)) {
      const node = schema.jsonSchema();
      if (schema.defaultValue !== undefined) {
        node["default"] = schema.defaultValue;
      }
      properties[key] = node;
      if (!schema.optional) {
        required.push(key);
      }
    }
    const out = describe({ type: "object", properties }, this.#description);
    if (required.length > 0) {
      out["required"] = required;
    }
    return out;
  }

  parse(value: unknown, path: string): ObjectOutput<Shape> {
    if (!isObject(value)) {
      throw new SchemaViolation(path, `expected an object, got ${describeType(value)}`);
    }
    const out: Record<string, unknown> = {};
    for (const [key, schema] of Object.entries(this.#shape)) {
      const at = path === "$" ? `$.${key}` : `${path}.${key}`;
      const present = Object.hasOwn(value, key) && value[key] !== undefined;
      if (present) {
        out[key] = schema.parse(value[key], at);
      } else if (schema.defaultValue !== undefined) {
        out[key] = structuredClone(schema.defaultValue);
      } else if (!schema.optional) {
        throw new SchemaViolation(at, "is required");
      }
    }
    return out as ObjectOutput<Shape>;
  }
}

class NullableSchema<T> extends Schema<T | null> {
  readonly #inner: Schema<T, boolean>;

  constructor(inner: Schema<T, boolean>) {
    super();
    this.#inner = inner;
  }

  jsonSchema(): JsonSchema {
    const inner = this.#inner.jsonSchema();
    const description = inner["description"];
    const rest = { ...inner };
    delete rest["description"];
    return describe({ anyOf: [rest, { type: "null" }] }, typeof description === "string" ? description : undefined);
  }

  parse(value: unknown, path: string): T | null {
    return value === null ? null : this.#inner.parse(value, path);
  }
}

/** Wraps a schema so the property may be absent (with an optional default). */
class OptionalSchema<T, Opt extends boolean> extends Schema<T, Opt> {
  override readonly optional: boolean = true;
  override readonly defaultValue: unknown;
  readonly #inner: Schema<T, boolean>;

  constructor(inner: Schema<T, boolean>, defaultValue: unknown) {
    super();
    this.#inner = inner;
    this.defaultValue = defaultValue;
  }

  jsonSchema(): JsonSchema {
    return this.#inner.jsonSchema();
  }

  parse(value: unknown, path: string): T {
    return this.#inner.parse(value, path);
  }
}

/** The schema builders. */
export const s = {
  /** A string. */
  string: (options: StringOptions = {}): Schema<string> => new StringSchema(options),
  /** A number (any finite JSON number). */
  number: (options: NumberOptions = {}): Schema<number> => new NumberSchema(false, options),
  /** An integer. */
  integer: (options: NumberOptions = {}): Schema<number> => new NumberSchema(true, options),
  /** A boolean. */
  boolean: (options: { description?: string } = {}): Schema<boolean> =>
    new BooleanSchema(options.description),
  /** One of a fixed list of strings (or numbers): `s.enum(["mon", "tue"] as const)`. */
  enum: <const V extends string | number>(
    values: readonly [V, ...V[]],
    options: { description?: string } = {},
  ): Schema<V> => new EnumSchema<V>(values, options.description),
  /** An array whose items fit `item`. */
  array: <T>(item: Schema<T, boolean>, options: ArrayOptions = {}): Schema<T[]> =>
    new ArraySchema(item, options),
  /** An object with the properties of `shape`; extra properties in the arguments are dropped. */
  object: <const Shape extends Record<string, Schema<unknown, boolean>>>(
    shape: Shape,
    options: { description?: string } = {},
  ): ObjectSchema<Shape> => new ObjectSchema(shape, options.description),
  /** The value or `null`. */
  nullable: <T>(inner: Schema<T, boolean>): Schema<T | null> => new NullableSchema(inner),
  /**
   * An object property that may be absent. With a `defaultValue` the handler always gets a value
   * (the default is applied when the property is absent and advertised in the schema).
   */
  optional: optionalSchema,
};

function optionalSchema<T>(inner: Schema<T, boolean>): Schema<T, true>;
function optionalSchema<T>(inner: Schema<T, boolean>, defaultValue: T): Schema<T, false>;
function optionalSchema<T>(inner: Schema<T, boolean>, ...rest: [T?]): Schema<T, boolean> {
  return new OptionalSchema<T, boolean>(inner, rest.length > 0 ? rest[0] : undefined);
}
