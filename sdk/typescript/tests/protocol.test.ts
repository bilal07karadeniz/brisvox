/** Message (de)serialization against the examples and tables of docs/AGENT_PROTOCOL.md. */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, it } from "node:test";
import * as protocol from "../src/protocol.js";
import { ErrorCode, ProtocolError, parseServerMessage } from "../src/protocol.js";

// dist/tests/protocol.test.js -> repository root
const DOC_PATH = new URL("../../../../docs/AGENT_PROTOCOL.md", import.meta.url);
let DOC = "";
try {
  DOC = readFileSync(DOC_PATH, "utf8");
} catch {
  DOC = "";
}
const docOnly = { skip: DOC === "" ? "docs/AGENT_PROTOCOL.md is not next to the SDK" : false };

// The two JSON examples of the doc ("Version negotiation and handshake").
const DOC_SESSION_START = `{"seq":1,"t_ms":0,"type":"session.start","protocol":"brisvox.agent","versions":[1],
 "session_id":"42","agent":"luigi","capabilities":{"transfer":true,"tools":true}}`;
const DOC_SESSION_ACCEPT = `{"type":"session.accept","version":1,
 "system_prompt":"You are Luigi's host.",
 "context":[{"role":"system","text":"The caller is a regular."}],
 "greeting":"Hi, Luigi's here. How can I help?",
 "events":["user.text","turn.bot_ended"],
 "tools":[{"name":"book_table","description":"Books a table.",
           "parameters":{"type":"object","properties":{"time":{"type":"string"}},"required":["time"]},
           "kind":"side_effect","timeout_ms":4000}]}`;

function sample(kind: string): string {
  const extra: Record<string, Record<string, unknown>> = {
    "session.start": {
      protocol: "brisvox.agent",
      versions: [1],
      session_id: "1",
      agent: "a",
      capabilities: { transfer: true, tools: true },
    },
    "session.end": { reason: "hangup" },
    "user.transcript": { text: "hi", final: false },
    "user.text": { epoch: 1, text: "hi" },
    "bot.text": { epoch: 1, text: "hi" },
    "bot.tts_text": { epoch: 1, text: "hi" },
    "turn.metrics": { epoch: 1, metrics: { response_ms: 400.5 } },
    error: { stage: "llm", message: "boom", fatal: false },
    "events.lost": { count: 3 },
    "tool.call": {
      id: 1,
      call_id: "c",
      name: "t",
      arguments: {},
      kind: "read_only",
      epoch: 1,
      idempotency_key: "00ff00ff00ff00ff",
      timeout_ms: 5000,
    },
    "tool.cancel": { id: 1, reason: "timeout" },
    "command.result": { id: "x", ok: true },
  };
  return JSON.stringify({ seq: 7, t_ms: 99, type: kind, ...(extra[kind] ?? { epoch: 1 }) });
}

describe("doc examples", () => {
  it("parses session.start", () => {
    const message = parseServerMessage(DOC_SESSION_START);
    assert.equal(message.type, "session.start");
    assert.ok(message.type === "session.start");
    assert.deepEqual([message.seq, message.tMs], [1, 0]);
    assert.equal(message.protocol, protocol.PROTOCOL);
    assert.deepEqual(message.versions, [1]);
    assert.deepEqual([message.sessionId, message.agent], ["42", "luigi"]);
    assert.equal(message.capabilities.transfer, true);
    assert.equal(message.capabilities.tools, true);
  });

  it("parses session.start without a call identity", () => {
    const message = parseServerMessage(DOC_SESSION_START);
    assert.ok(message.type === "session.start");
    assert.equal(message.call, undefined);
  });

  it("parses the caller identity of session.start", () => {
    const base = {
      seq: 1,
      t_ms: 0,
      type: "session.start",
      protocol: "brisvox.agent",
      versions: [1],
      session_id: "42",
      agent: "luigi",
      capabilities: { transfer: true, tools: true },
    };
    const full = parseServerMessage(
      JSON.stringify({
        ...base,
        call: { carrier: "twilio", carrier_call_id: "CA1", from: "+15551230001", to: "+15551230002" },
      }),
    );
    assert.ok(full.type === "session.start");
    assert.deepEqual(full.call, {
      carrier: "twilio",
      carrierCallId: "CA1",
      from: "+15551230001",
      to: "+15551230002",
      piiRedacted: false,
    });
    // Numbers withheld by the server's PII redaction: no from/to keys at all.
    const redacted = parseServerMessage(
      JSON.stringify({
        ...base,
        call: { carrier: "sip", carrier_call_id: "abc@h", pii_redacted: true },
      }),
    );
    assert.ok(redacted.type === "session.start");
    assert.deepEqual(redacted.call, { carrier: "sip", carrierCallId: "abc@h", piiRedacted: true });
    // A call object that is not one is a bad message.
    for (const bad of ["yes", { carrier: "twilio" }, { carrier: 1, carrier_call_id: "x" }]) {
      assert.throws(() => parseServerMessage(JSON.stringify({ ...base, call: bad })), ProtocolError);
    }
  });

  it("builds a session.accept equal to the doc example", () => {
    const built = protocol.sessionAccept(1, {
      systemPrompt: "You are Luigi's host.",
      context: [{ role: "system", text: "The caller is a regular." }],
      greeting: "Hi, Luigi's here. How can I help?",
      events: ["user.text", "turn.bot_ended"],
      tools: [
        {
          name: "book_table",
          description: "Books a table.",
          parameters: { type: "object", properties: { time: { type: "string" } }, required: ["time"] },
          kind: "side_effect",
          timeout_ms: 4000,
        },
      ],
    });
    assert.deepEqual(built, JSON.parse(DOC_SESSION_ACCEPT));
    assert.deepEqual(JSON.parse(protocol.encode(built)), built);
  });

  it("hardcodes exactly the doc's own JSON blocks", docOnly, () => {
    const block = /```json\n([\s\S]*?)```/.exec(DOC);
    assert.ok(block?.[1]);
    const [start, accept] = block[1].trim().split("\n\n");
    assert.deepEqual(JSON.parse(start ?? ""), JSON.parse(DOC_SESSION_START));
    assert.deepEqual(JSON.parse(accept ?? ""), JSON.parse(DOC_SESSION_ACCEPT));
  });

  it("has exactly the error codes of the doc's table", docOnly, () => {
    const section = DOC.split("## Error codes")[1]?.split("\n## ")[0] ?? "";
    const documented = new Set([...section.matchAll(/^\| `([a-z_]+)` \|/gm)].map((m) => m[1]));
    assert.deepEqual(documented, new Set(Object.values(ErrorCode)));
  });

  it("has exactly the event types of the doc's table, each parsing into a typed message", docOnly, () => {
    const events = DOC.split("### Events")[1]?.split("### Tool calls")[0] ?? "";
    const documented = new Set([...events.matchAll(/^\| `([a-z_.]+)` \|/gm)].map((m) => m[1] as string));
    documented.delete("type");
    const subscribable = new Set(documented);
    for (const kind of ["error", "events.lost", "session.end"]) {
      subscribable.delete(kind);
    }
    assert.deepEqual(subscribable, new Set(protocol.EVENT_TYPES));
    for (const kind of documented) {
      const message = parseServerMessage(sample(kind));
      assert.notEqual(message.type, "unknown", kind);
      assert.equal(message.type, kind);
    }
  });

  it("documents the commands and tool messages the SDK builds", docOnly, () => {
    const commands = DOC.split("### Commands")[1]?.split("### How the commands act")[0] ?? "";
    const documented = new Set([...commands.matchAll(/^\| `([a-z_.]+)` \|/gm)].map((m) => m[1] as string));
    const built = [
      protocol.say("x"),
      protocol.end(),
      protocol.transfer("+4930123456"),
      protocol.contextUpdate(),
      protocol.toolsSet(null),
      protocol.toolResult(1, { result: 1 }),
    ].map((m) => m.type);
    documented.delete("type");
    assert.deepEqual(new Set(built), documented);
  });

  it("matches the limits of the doc", docOnly, () => {
    assert.ok(DOC.includes("**64 KiB**"));
    assert.equal(protocol.MAX_MESSAGE_BYTES, 64 * 1024);
    assert.ok(DOC.includes("at most 4 KiB"));
    assert.equal(protocol.MAX_LINE_BYTES, 4096);
    assert.ok(DOC.includes("At most 64."));
    assert.equal(protocol.MAX_TOOLS, 64);
    assert.ok(DOC.includes("at most 32"));
    assert.equal(protocol.MAX_CONTEXT_MESSAGES, 32);
    assert.ok(DOC.includes("at most 30 000"));
    assert.equal(protocol.MAX_TOOL_TIMEOUT_MS, 30_000);
    assert.ok(DOC.includes("at most 64 bytes"));
    assert.equal(protocol.MAX_COMMAND_ID_BYTES, 64);
  });
});

describe("server messages", () => {
  it("reads the envelope and the fields", () => {
    const message = parseServerMessage(sample("user.text"));
    assert.deepEqual(message, { type: "user.text", seq: 7, tMs: 99, epoch: 1, text: "hi" });
  });

  it("reads the final flag of a transcript", () => {
    const message = parseServerMessage(
      JSON.stringify({ type: "user.transcript", seq: 1, t_ms: 1, text: "a", final: true }),
    );
    assert.ok(message.type === "user.transcript");
    assert.equal(message.final, true);
  });

  it("leaves absent metrics out", () => {
    const message = parseServerMessage(sample("turn.metrics"));
    assert.ok(message.type === "turn.metrics");
    assert.deepEqual(message.metrics, { responseMs: 400.5 });
    const empty = parseServerMessage('{"type":"turn.metrics","epoch":2}');
    assert.ok(empty.type === "turn.metrics");
    assert.deepEqual(empty.metrics, {});
  });

  it("ignores unknown fields", () => {
    const message = parseServerMessage('{"type":"bot.tts_stopped","epoch":3,"extra":{"a":1}}');
    assert.deepEqual(message, { type: "bot.tts_stopped", seq: 0, tMs: 0, epoch: 3 });
  });

  it("keeps an unknown type instead of failing", () => {
    const message = parseServerMessage('{"type":"future.thing","seq":4,"x":1}');
    assert.ok(message.type === "unknown");
    assert.equal(message.wireType, "future.thing");
    assert.deepEqual(message.raw, { type: "future.thing", seq: 4, x: 1 });
  });

  it("reads a tool.call", () => {
    const message = parseServerMessage(sample("tool.call"));
    assert.ok(message.type === "tool.call");
    assert.deepEqual(
      [message.id, message.callId, message.name, message.kind, message.idempotencyKey, message.timeoutMs],
      [1, "c", "t", "read_only", "00ff00ff00ff00ff", 5000],
    );
    assert.deepEqual(message.arguments, {});
  });

  it("reads the tool.cancel reasons of the doc", () => {
    for (const reason of ["interrupted", "timeout", "abandoned"]) {
      const message = parseServerMessage(`{"type":"tool.cancel","id":3,"reason":"${reason}"}`);
      assert.ok(message.type === "tool.cancel");
      assert.equal(message.reason, reason);
    }
  });

  it("reads command.result errors, known and unknown codes", () => {
    const known = parseServerMessage(
      '{"type":"command.result","id":"a","ok":false,"error":{"code":"queue_full","message":"later"}}',
    );
    assert.ok(known.type === "command.result");
    assert.deepEqual(known.error, { code: ErrorCode.QueueFull, message: "later" });
    const future = parseServerMessage(
      '{"type":"command.result","id":"a","ok":false,"error":{"code":"brand_new","message":"?"}}',
    );
    assert.ok(future.type === "command.result");
    assert.equal(future.error?.code, "brand_new");
    const ok = parseServerMessage('{"type":"command.result","id":"a","ok":true}');
    assert.ok(ok.type === "command.result");
    assert.equal(ok.error, undefined);
  });

  it("reads error, events.lost and session.end", () => {
    const error = parseServerMessage(sample("error"));
    assert.ok(error.type === "error");
    assert.deepEqual([error.stage, error.message, error.fatal], ["llm", "boom", false]);
    const lost = parseServerMessage(sample("events.lost"));
    assert.ok(lost.type === "events.lost");
    assert.equal(lost.count, 3);
    const end = parseServerMessage(sample("session.end"));
    assert.ok(end.type === "session.end");
    assert.equal(end.reason, "hangup");
  });

  it("answers malformed messages with bad_message", () => {
    const bad: (string | object)[] = [
      "not json",
      "[]",
      "3",
      "null",
      "{}",
      '{"type":5}',
      '{"type":"user.text","epoch":1}',
      '{"type":"user.text","epoch":"1","text":"x"}',
      '{"type":"user.text","epoch":1.5,"text":"x"}',
      '{"type":"user.text","epoch":true,"text":"x"}',
      '{"type":"user.transcript","text":"x"}',
      '{"type":"session.start","protocol":"brisvox.agent","versions":["1"],"session_id":"1","agent":"a"}',
      '{"type":"tool.call","id":1,"call_id":"c","name":"t","arguments":[],"kind":"read_only","epoch":1,"idempotency_key":"k","timeout_ms":1}',
      '{"type":"tool.call","id":1,"call_id":"c","name":"t","arguments":{},"kind":"sometimes","epoch":1,"idempotency_key":"k","timeout_ms":1}',
      '{"type":"command.result","id":3,"ok":true}',
    ];
    for (const text of bad) {
      assert.throws(
        () => parseServerMessage(text),
        (error: unknown) => error instanceof ProtocolError && error.code === ErrorCode.BadMessage,
        String(text),
      );
    }
  });

  it("refuses binary frames", () => {
    assert.throws(() => parseServerMessage(new Uint8Array([0, 1])), /binary frames/);
  });
});

describe("agent messages", () => {
  it("builds say", () => {
    assert.deepEqual(protocol.say("hi", { id: "a", re: 4, interrupt: true }), {
      type: "say",
      id: "a",
      re: 4,
      text: "hi",
      interrupt: true,
    });
    assert.deepEqual(protocol.say("hi"), { type: "say", text: "hi" });
  });

  it("builds end", () => {
    assert.deepEqual(protocol.end({ say: "Bye", reason: "done", id: "e" }), {
      type: "end",
      id: "e",
      say: "Bye",
      reason: "done",
    });
    assert.deepEqual(protocol.end(), { type: "end" });
  });

  it("builds transfer", () => {
    assert.deepEqual(
      protocol.transfer("+4930123456", { mode: "warm", summary: "s", reason: "r", id: "t1" }),
      { type: "transfer", id: "t1", destination: "+4930123456", mode: "warm", summary: "s", reason: "r" },
    );
  });

  it("builds context.update and tools.set", () => {
    assert.deepEqual(
      protocol.contextUpdate({ systemPrompt: "p", append: [{ role: "user", text: "u" }], id: "c" }),
      { type: "context.update", id: "c", system_prompt: "p", append: [{ role: "user", text: "u" }] },
    );
    assert.deepEqual(protocol.toolsSet(["a", "b"]), { type: "tools.set", enabled: ["a", "b"] });
    assert.deepEqual(protocol.toolsSet(null), { type: "tools.set", enabled: null });
    assert.deepEqual(protocol.toolsSet([]), { type: "tools.set", enabled: [] });
  });

  it("builds tool.result: error wins, undefined is null", () => {
    assert.deepEqual(protocol.toolResult(5, { result: { a: 1 } }), { type: "tool.result", id: 5, result: { a: 1 } });
    assert.deepEqual(protocol.toolResult(5, { result: "text" }), { type: "tool.result", id: 5, result: "text" });
    assert.deepEqual(protocol.toolResult(5), { type: "tool.result", id: 5, result: null });
    assert.deepEqual(protocol.toolResult(5, { result: 1, error: "boom" }), {
      type: "tool.result",
      id: 5,
      error: "boom",
    });
  });

  it("builds session.reject", () => {
    assert.deepEqual(protocol.sessionReject(ErrorCode.UnsupportedVersion, "no"), {
      type: "session.reject",
      code: "unsupported_version",
      message: "no",
    });
    assert.deepEqual(protocol.sessionReject(), { type: "session.reject" });
  });

  it("checks the rules of the doc before anything is sent", () => {
    const invalid = (fn: () => unknown, code: ErrorCode): void => {
      assert.throws(fn, (error: unknown) => error instanceof ProtocolError && error.code === code);
    };
    invalid(() => protocol.say(""), ErrorCode.InvalidArgument);
    invalid(() => protocol.say("   "), ErrorCode.InvalidArgument);
    invalid(() => protocol.say("x".repeat(4097)), ErrorCode.InvalidArgument);
    invalid(() => protocol.say("é".repeat(2049)), ErrorCode.InvalidArgument); // bytes, not characters
    invalid(() => protocol.say("x", { id: "" }), ErrorCode.InvalidArgument);
    invalid(() => protocol.say("x", { id: "i".repeat(65) }), ErrorCode.InvalidArgument);
    invalid(() => protocol.end({ say: " " }), ErrorCode.InvalidArgument);
    invalid(() => protocol.end({ reason: "r".repeat(513) }), ErrorCode.InvalidArgument);
    invalid(() => protocol.transfer("0049301234"), ErrorCode.InvalidArgument);
    invalid(() => protocol.transfer("+1234567"), ErrorCode.InvalidArgument);
    invalid(() => protocol.transfer("+1234567890123456"), ErrorCode.InvalidArgument);
    invalid(() => protocol.transfer("+4930123456", { summary: "s".repeat(513) }), ErrorCode.InvalidArgument);
    invalid(
      () => protocol.transfer("+4930123456", { mode: "hot" as unknown as "cold" }),
      ErrorCode.InvalidArgument,
    );
    const many = Array.from({ length: 33 }, () => ({ role: "user" as const, text: "x" }));
    invalid(() => protocol.contextUpdate({ append: many }), ErrorCode.TooLarge);
    invalid(() => protocol.sessionAccept(1, { context: many }), ErrorCode.TooLarge);
    invalid(
      () => protocol.contextUpdate({ append: [{ role: "robot" as unknown as "user", text: "x" }] }),
      ErrorCode.InvalidArgument,
    );
    invalid(() => protocol.contextUpdate({ append: [{ role: "user", text: " " }] }), ErrorCode.InvalidArgument);
    invalid(() => protocol.contextUpdate({ systemPrompt: "x".repeat(64 * 1024 + 1) }), ErrorCode.InvalidArgument);
    const tools = Array.from({ length: 65 }, (_, n) => ({
      name: `t${n}`,
      description: "d",
      parameters: { type: "object" },
      kind: "read_only" as const,
    }));
    invalid(() => protocol.sessionAccept(1, { tools }), ErrorCode.TooLarge);
    invalid(() => protocol.sessionAccept(1, { greeting: "g".repeat(4097) }), ErrorCode.InvalidArgument);
  });

  it("accepts values on the boundaries", () => {
    protocol.say("x".repeat(4096), { id: "i".repeat(64) });
    protocol.say("é".repeat(2048));
    protocol.transfer("+12345678");
    protocol.transfer("+123456789012345", { summary: "s".repeat(512), reason: "r".repeat(512) });
    protocol.contextUpdate({ append: Array.from({ length: 32 }, () => ({ role: "system" as const, text: "x" })) });
  });

  it("validates tool names", () => {
    assert.equal(protocol.validateToolName("book_table-2"), "book_table-2");
    for (const name of ["", "a b", "x".repeat(65), "naïve", "a.b"]) {
      assert.throws(() => protocol.validateToolName(name), RangeError, name);
    }
  });

  it("cannot put NaN, Infinity or a cycle on the wire", () => {
    for (const value of [Number.NaN, Number.POSITIVE_INFINITY, { values: [1, Number.NEGATIVE_INFINITY] }]) {
      assert.throws(
        () => protocol.encode(protocol.toolResult(1, { result: value })),
        (error: unknown) => error instanceof ProtocolError && error.code === ErrorCode.InvalidArgument,
      );
    }
    const cycle: Record<string, unknown> = {};
    cycle["self"] = cycle;
    assert.throws(() => protocol.encode(protocol.toolResult(1, { result: cycle })), ProtocolError);
    assert.throws(() => protocol.encode(protocol.toolResult(1, { result: 10n })), ProtocolError);
  });

  it("refuses a message above 64 KiB with too_large and sends one at the limit", () => {
    assert.throws(
      () => protocol.encode(protocol.toolResult(1, { result: "x".repeat(70_000) })),
      (error: unknown) => error instanceof ProtocolError && error.code === ErrorCode.TooLarge,
    );
    const overhead = Buffer.byteLength(protocol.encode(protocol.toolResult(1, { result: "" })), "utf8");
    const exact = protocol.encode(protocol.toolResult(1, { result: "x".repeat(protocol.MAX_MESSAGE_BYTES - overhead) }));
    assert.equal(Buffer.byteLength(exact, "utf8"), protocol.MAX_MESSAGE_BYTES);
    assert.throws(
      () => protocol.encode(protocol.toolResult(1, { result: "x".repeat(protocol.MAX_MESSAGE_BYTES - overhead + 1) })),
      ProtocolError,
    );
  });
});
