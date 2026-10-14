/** Schema generation, argument validation and the tool declaration. */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { defineTool, ToolArgumentError, ToolContext, ToolError, s } from "../src/index.js";
import type { Infer, Session } from "../src/index.js";

function context(): ToolContext {
  return new ToolContext({
    session: {} as Session,
    callId: "c1",
    requestId: 1,
    name: "t",
    kind: "read_only",
    epoch: 1,
    idempotencyKey: "0123456789abcdef",
    timeoutMs: 5000,
  });
}

describe("schema builder", () => {
  const params = s.object({
    time: s.string({ description: "HH:MM", pattern: "^\\d\\d:\\d\\d$" }),
    party_size: s.optional(s.integer({ minimum: 1, maximum: 12 }), 2),
    day: s.enum(["mon", "tue"] as const, { description: "weekday" }),
    note: s.optional(s.string()),
    contact: s.optional(s.object({ name: s.string(), phone: s.nullable(s.string()) })),
    tags: s.optional(s.array(s.string(), { maxItems: 3 })),
    vip: s.optional(s.boolean(), false),
    ratio: s.optional(s.number({ minimum: 0 })),
  });

  it("emits the JSON Schema the server compiles", () => {
    assert.deepEqual(params.jsonSchema(), {
      type: "object",
      properties: {
        time: { type: "string", description: "HH:MM", pattern: "^\\d\\d:\\d\\d$" },
        party_size: { type: "integer", minimum: 1, maximum: 12, default: 2 },
        day: { enum: ["mon", "tue"], description: "weekday" },
        note: { type: "string" },
        contact: {
          type: "object",
          properties: { name: { type: "string" }, phone: { anyOf: [{ type: "string" }, { type: "null" }] } },
          required: ["name", "phone"],
        },
        tags: { type: "array", items: { type: "string" }, maxItems: 3 },
        vip: { type: "boolean", default: false },
        ratio: { type: "number", minimum: 0 },
      },
      required: ["time", "day"],
    });
  });

  it("types the arguments (compile-time check) and applies defaults", () => {
    type Args = Infer<typeof params>;
    const parsed: Args = params.parse({ time: "19:30", day: "mon", extra: "dropped" }, "$");
    // The next lines fail to compile if the inferred type is wrong.
    const size: number = parsed.party_size;
    const vip: boolean = parsed.vip;
    const note: string | undefined = parsed.note;
    const day: "mon" | "tue" = parsed.day;
    assert.deepEqual(parsed, { time: "19:30", party_size: 2, day: "mon", vip: false });
    assert.deepEqual([size, vip, note, day], [2, false, undefined, "mon"]);
    assert.equal("extra" in parsed, false);
  });

  it("keeps values that are given", () => {
    const parsed = params.parse(
      { time: "08:00", day: "tue", party_size: 4, contact: { name: "Ann", phone: null }, tags: ["a"], vip: true },
      "$",
    );
    assert.deepEqual(parsed, {
      time: "08:00",
      day: "tue",
      party_size: 4,
      contact: { name: "Ann", phone: null },
      tags: ["a"],
      vip: true,
    });
  });

  it("names the path of a violation", () => {
    const cases: [unknown, RegExp][] = [
      [{ day: "mon" }, /\$\.time: is required/],
      [{ time: 5, day: "mon" }, /\$\.time: expected a string, got number/],
      [{ time: "xx", day: "mon" }, /\$\.time: does not match/],
      [{ time: "19:30", day: "fri" }, /\$\.day: "fri" is not one of/],
      [{ time: "19:30", day: "mon", party_size: 0 }, /\$\.party_size: 0 is below the minimum 1/],
      [{ time: "19:30", day: "mon", party_size: 2.5 }, /\$\.party_size: expected an integer/],
      [{ time: "19:30", day: "mon", party_size: 13 }, /above the maximum 12/],
      [{ time: "19:30", day: "mon", contact: { phone: null } }, /\$\.contact\.name: is required/],
      [{ time: "19:30", day: "mon", tags: ["a", "b", "c", "d"] }, /more than 3 items/],
      [{ time: "19:30", day: "mon", tags: ["a", 1] }, /\$\.tags\[1\]: expected a string/],
      [{ time: "19:30", day: "mon", vip: "yes" }, /\$\.vip: expected a boolean/],
      [{ time: "19:30", day: "mon", ratio: -1 }, /below the minimum/],
      [[], /expected an object, got array/],
      [null, /expected an object, got null/],
    ];
    for (const [value, message] of cases) {
      assert.throws(() => params.parse(value, "$"), message, JSON.stringify(value));
    }
  });

  it("counts string length in characters, not UTF-16 units", () => {
    const short = s.string({ maxLength: 2 });
    short.parse("😀😀", "$");
    assert.throws(() => short.parse("😀😀😀", "$"), /longer than 2/);
    assert.throws(() => s.string({ minLength: 3 }).parse("ab", "$"), /shorter than 3/);
  });

  it("rejects NaN and Infinity as numbers", () => {
    assert.throws(() => s.number().parse(Number.NaN, "$"), /expected a number/);
    assert.throws(() => s.integer().parse(Number.POSITIVE_INFINITY, "$"), /expected an integer/);
  });
});

describe("Tool", () => {
  const base = { name: "lookup", description: "Look something up." };

  it("declares confirm only when set, and only on a side_effect tool", () => {
    const handler = (): string => "x";
    const held = defineTool({ name: "transfer", description: "d", kind: "side_effect", confirm: true, handler });
    assert.equal(held.declaration().confirm, true);
    const plain = defineTool({ name: "transfer", description: "d", kind: "side_effect", handler });
    assert.equal("confirm" in plain.declaration(), false);
    assert.throws(() => defineTool({ name: "t", description: "d", confirm: true, handler }), RangeError);
  });

  it("declares repeatable only when set, and only on a side_effect tool", () => {
    const handler = (): string => "x";
    const press = defineTool({ name: "press", description: "d", kind: "side_effect", repeatable: true, handler });
    assert.equal(press.repeatable, true);
    assert.equal(press.declaration().repeatable, true);
    const plain = defineTool({ name: "press", description: "d", kind: "side_effect", handler });
    assert.equal(plain.repeatable, false);
    assert.equal("repeatable" in plain.declaration(), false);
    assert.throws(() => defineTool({ name: "t", description: "d", repeatable: true, handler }), RangeError);
    assert.throws(
      () => defineTool({ name: "t", description: "d", kind: "read_only", repeatable: true, handler }),
      RangeError,
    );
  });

  it("declares a typed tool", () => {
    const tool = defineTool({
      ...base,
      kind: "side_effect",
      timeoutMs: 4000,
      fillerAfterMs: 0,
      parameters: s.object({ q: s.string() }),
      handler: ({ q }) => q,
    });
    assert.deepEqual(tool.declaration(), {
      name: "lookup",
      description: "Look something up.",
      parameters: { type: "object", properties: { q: { type: "string" } }, required: ["q"] },
      kind: "side_effect",
      timeout_ms: 4000,
      filler_after_ms: 0,
    });
  });

  it("defaults to read_only with an empty parameter object and leaves optional numbers out", () => {
    const tool = defineTool({ ...base, handler: () => "x" });
    assert.deepEqual(tool.declaration(), {
      name: "lookup",
      description: "Look something up.",
      parameters: { type: "object", properties: {} },
      kind: "read_only",
    });
  });

  it("takes a hand-written JSON schema and passes the arguments through", async () => {
    const parameters = { type: "object", properties: { n: { type: "integer" } }, required: ["n"] };
    const tool = defineTool({ ...base, parameters, handler: (args: Record<string, unknown>) => ({ got: args["n"] }) });
    assert.deepEqual(tool.declaration().parameters, parameters);
    assert.deepEqual(await tool.run({ n: 3, other: true }, context()), { got: 3 });
  });

  it("validates typed arguments and reports a ToolArgumentError", async () => {
    const tool = defineTool({
      ...base,
      parameters: s.object({ n: s.integer() }),
      handler: ({ n }) => n + 1,
    });
    assert.equal(await tool.run({ n: 1 }, context()), 2);
    await assert.rejects(tool.run({ n: "1" }, context()), (error: unknown) => {
      return error instanceof ToolArgumentError && error instanceof ToolError && /\$\.n: expected an integer/.test(error.message);
    });
  });

  it("awaits async handlers and turns a synchronous throw into a rejection", async () => {
    const slow = defineTool({ ...base, handler: async () => "done" });
    assert.equal(await slow.run({}, context()), "done");
    const throws = defineTool({
      ...base,
      handler: () => {
        throw new ToolError("no tables left");
      },
    });
    await assert.rejects(throws.run({}, context()), /no tables left/);
  });

  it("refuses bad declarations", () => {
    const handler = (): string => "x";
    assert.throws(() => defineTool({ name: "bad name", description: "d", handler }), RangeError);
    assert.throws(() => defineTool({ name: "", description: "d", handler }), RangeError);
    assert.throws(() => defineTool({ name: "t", description: "  ", handler }), /needs a description/);
    assert.throws(() => defineTool({ name: "t", description: "d", timeoutMs: 30_001, handler }), RangeError);
    assert.throws(() => defineTool({ name: "t", description: "d", timeoutMs: 0, handler }), RangeError);
    assert.throws(() => defineTool({ name: "t", description: "d", fillerAfterMs: -1, handler }), RangeError);
    assert.throws(
      () => defineTool({ name: "t", description: "d", kind: "sometimes" as unknown as "read_only", handler }),
      RangeError,
    );
    assert.throws(
      () => defineTool({ name: "t", description: "d", parameters: { type: "array" }, handler }),
      /"type": "object"/,
    );
    assert.throws(
      () => defineTool({ name: "t", description: "d", parameters: s.object({}), handler: undefined as never }),
      TypeError,
    );
    defineTool({ name: "t", description: "d", timeoutMs: 30_000, handler });
  });
});

describe("ToolContext", () => {
  it("aborts its signal once with the reason", () => {
    const ctx = context();
    assert.equal(ctx.cancelled, false);
    assert.equal(ctx.cancelReason, undefined);
    ctx.cancel("timeout");
    ctx.cancel("interrupted");
    assert.equal(ctx.signal.aborted, true);
    assert.equal(ctx.cancelReason, "timeout");
  });
});
