/** Remote tools over a real WebSocket: results, errors, cancellation, the link closing. */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { ToolError, s } from "../src/index.js";
import type { AgentOptions, Session, ToolContext } from "../src/index.js";
import { Harness, gate, settle, until, using, toolCall } from "./helpers.js";

function toolsSetup(options: AgentOptions = {}) {
  const h = new Harness(options);
  const state = {
    aborted: [] as (string | undefined)[],
    finished: gate(),
    release: gate(),
    keys: [] as string[],
    contexts: [] as ToolContext[],
    sessions: [] as Session[],
  };
  h.agent.onSessionStart((session) => {
    state.sessions.push(session);
  });
  h.agent.tool({
    name: "echo",
    description: "Echo.",
    parameters: s.object({ text: s.string(), times: s.optional(s.integer(), 1) }),
    handler: async ({ text, times }) => ({ text: text.repeat(times) }),
  });
  h.agent.tool({
    name: "plain",
    description: "Add.",
    parameters: s.object({ a: s.integer(), b: s.integer() }),
    handler: ({ a, b }) => a + b,
  });
  h.agent.tool({
    name: "raw",
    description: "Hand-written schema.",
    parameters: { type: "object", properties: { n: { type: "integer" } } },
    handler: (args) => ({ n: args["n"] }),
  });
  h.agent.tool({
    name: "refuses",
    description: "Refuses.",
    handler: () => {
      throw new ToolError("no tables left");
    },
  });
  h.agent.tool({
    name: "explodes",
    description: "Explodes.",
    handler: async () => {
      throw new RangeError("secret internal detail");
    },
  });
  h.agent.tool({
    name: "hangs",
    description: "Hangs until cancelled.",
    handler: (_args, ctx) =>
      new Promise<string>((_resolve, reject) => {
        ctx.signal.addEventListener("abort", () => {
          state.aborted.push(ctx.cancelReason);
          reject(new Error("aborted"));
        });
      }),
  });
  h.agent.tool({
    name: "deaf",
    description: "Ignores the abort signal and never finishes.",
    handler: () => new Promise<string>(() => undefined),
  });
  h.agent.tool({
    name: "book",
    description: "Books.",
    kind: "side_effect",
    parameters: s.object({ time: s.string() }),
    handler: async ({ time }, ctx) => {
      state.keys.push(ctx.idempotencyKey);
      state.contexts.push(ctx);
      if (time === "wait") {
        await new Promise<void>((resolve) => ctx.signal.addEventListener("abort", () => resolve()));
      }
      state.finished.open();
      return `booked ${time} (${ctx.cancelReason})`;
    },
  });
  h.agent.tool({
    name: "stubborn",
    description: "Ignores the abort signal.",
    kind: "side_effect",
    handler: async () => {
      await state.release.promise;
      return "late";
    },
  });
  h.agent.tool({ name: "nan_mean", description: "Mean of nothing.", handler: () => Number.NaN });
  h.agent.tool({ name: "nested_inf", description: "Infinity.", handler: () => ({ values: [1, Number.POSITIVE_INFINITY] }) });
  h.agent.tool({ name: "huge", description: "Huge.", handler: () => "x".repeat(100_000) });
  h.agent.tool({
    name: "cyclic",
    description: "Cycle.",
    handler: () => {
      const loop: Record<string, unknown> = {};
      loop["self"] = loop;
      return loop;
    },
  });
  h.agent.tool({ name: "bigint", description: "BigInt.", handler: () => 10n });
  h.agent.tool({ name: "nothing", description: "Returns nothing.", handler: () => undefined });
  h.agent.tool({ name: "when", description: "A date.", handler: () => new Date(Date.UTC(2026, 9, 13)) });
  return { h, state };
}

describe("tool calls", () => {
  it("answer async and plain handlers, typed and raw schemas", async () => {
    const { h } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "echo", { text: "ab", times: 2 }));
      await fake.send(toolCall(2, "plain", { a: 2, b: 3 }));
      await fake.send(toolCall(3, "raw", { n: 7, extra: true }));
      const replies = new Map<number, unknown>();
      for (let n = 0; n < 3; n += 1) {
        const reply = await fake.recvType("tool.result");
        replies.set(reply["id"], reply);
      }
      assert.deepEqual(replies.get(1), { type: "tool.result", id: 1, result: { text: "abab" } });
      assert.deepEqual(replies.get(2), { type: "tool.result", id: 2, result: 5 });
      assert.deepEqual(replies.get(3), { type: "tool.result", id: 3, result: { n: 7 } });
    });
  });

  it("send null for no result and ISO text for a date", async () => {
    const { h } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "nothing", {}));
      assert.deepEqual(await fake.recvType("tool.result"), { type: "tool.result", id: 1, result: null });
      await fake.send(toolCall(2, "when", {}));
      assert.deepEqual(await fake.recvType("tool.result"), {
        type: "tool.result",
        id: 2,
        result: "2026-10-13T00:00:00.000Z",
      });
    });
  });

  it("tell the LLM what a ToolError says and nothing else", async () => {
    const { h } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "refuses", {}));
      await fake.send(toolCall(2, "explodes", {}));
      await fake.send(toolCall(3, "nonexistent", {}));
      await fake.send(toolCall(4, "echo", { text: 5 })); // wrong type
      await fake.send(toolCall(5, "cyclic", {}));
      await fake.send(toolCall(6, "bigint", {}));
      const replies = new Map<number, Record<string, unknown>>();
      for (let n = 0; n < 6; n += 1) {
        const reply = await fake.recvType("tool.result");
        replies.set(reply["id"], reply);
      }
      assert.equal(replies.get(1)?.["error"], "no tables left");
      assert.equal(replies.get(2)?.["error"], "tool failed: RangeError");
      assert.equal(JSON.stringify(replies.get(2)).includes("secret"), false);
      assert.match(String(replies.get(3)?.["error"]), /unknown tool/);
      assert.match(String(replies.get(4)?.["error"]), /\$\.text: expected a string/);
      assert.equal(replies.get(5)?.["error"], "tool result is not valid JSON");
      assert.equal(replies.get(6)?.["error"], "tool result is not valid JSON");
      assert.equal("result" in (replies.get(5) ?? {}), false);
    });
  });

  it("turn an oversized result into an error", async () => {
    const { h } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "huge", {}));
      assert.deepEqual(await fake.recvType("tool.result"), {
        type: "tool.result",
        id: 1,
        error: "tool result is too large",
      });
    });
  });

  it("answer NaN and Infinity results with an error", async () => {
    const { h } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "nan_mean", {}));
      await fake.send(toolCall(2, "nested_inf", {}));
      const replies = new Map<number, Record<string, unknown>>();
      for (let n = 0; n < 2; n += 1) {
        const reply = await fake.recvType("tool.result", 1500);
        replies.set(reply["id"], reply);
      }
      for (const id of [1, 2]) {
        assert.equal(replies.get(id)?.["error"], "tool result is not valid JSON");
        assert.equal("result" in (replies.get(id) ?? {}), false);
      }
    });
  });

  it("give the handler the call's context", async () => {
    const { h, state } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ ...toolCall(8, "book", { time: "19:30" }, "side_effect"), epoch: 4 });
      await fake.recvType("tool.result");
      const ctx = state.contexts[0];
      assert.ok(ctx);
      assert.deepEqual(
        [ctx.requestId, ctx.callId, ctx.name, ctx.kind, ctx.epoch, ctx.idempotencyKey, ctx.timeoutMs],
        [8, "call-8", "book", "side_effect", 4, "0123456789abcdef", 5000],
      );
      assert.equal(ctx.session, state.sessions[0]);
    });
  });

  it("fail calls beyond maxConcurrentTools at once", async () => {
    const { h, state } = toolsSetup({ maxConcurrentTools: 2 });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "stubborn", {}, "side_effect"));
      await fake.send(toolCall(2, "stubborn", {}, "side_effect"));
      await fake.send(toolCall(3, "plain", { a: 1, b: 1 }));
      const reply = await fake.recvType("tool.result");
      assert.deepEqual(reply, { type: "tool.result", id: 3, error: "too many tool calls at once" });
      state.release.open();
      await fake.recvType("tool.result");
      await fake.recvType("tool.result");
      await until("the slots are free", () => state.sessions[0]?.tools.size === 0);
      await fake.send(toolCall(4, "plain", { a: 1, b: 1 }));
      assert.equal((await fake.recvType("tool.result"))["result"], 2);
    });
  });
});

describe("tool.cancel", () => {
  it("aborts a read_only tool and sends no result for it", async () => {
    const { h, state } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "hangs", {}));
      await settle();
      await fake.send({ type: "tool.cancel", id: 1, reason: "interrupted" });
      await until("the abort", () => state.aborted.length === 1);
      assert.deepEqual(state.aborted, ["interrupted"]);
      // No result is sent for a cancelled call: the next tool.result is call 2's.
      await fake.send(toolCall(2, "plain", { a: 1, b: 1 }));
      assert.equal((await fake.recvType("tool.result"))["id"], 2);
      await until("nothing left running", () => state.sessions[0]?.tools.size === 0);
    });
  });

  it("stops waiting for a read_only tool that ignores the abort signal", async () => {
    const { h, state } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "deaf", {}));
      await until("the call is running", () => state.sessions[0]?.tools.size === 1);
      await fake.send({ type: "tool.cancel", id: 1, reason: "abandoned" });
      await until("the SDK stopped waiting", () => state.sessions[0]?.tools.size === 0);
      await fake.send(toolCall(2, "plain", { a: 1, b: 1 }));
      assert.equal((await fake.recvType("tool.result"))["id"], 2);
    });
  });

  it("finds the tool when the cancel comes right behind the call", async () => {
    const { h, state } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "hangs", {}));
      await fake.send({ type: "tool.cancel", id: 1, reason: "timeout" });
      await fake.send(toolCall(2, "plain", { a: 1, b: 1 }));
      assert.equal((await fake.recvType("tool.result"))["id"], 2); // id 1 produced nothing
      await until("nothing left running", () => state.sessions[0]?.tools.size === 0);
      assert.deepEqual(state.aborted, ["timeout"]);
    });
  });

  it("ignores a cancel for an unknown id", async () => {
    const { h } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "tool.cancel", id: 77, reason: "timeout" });
      await fake.send(toolCall(1, "plain", { a: 1, b: 2 }));
      assert.equal((await fake.recvType("tool.result"))["result"], 3);
    });
  });

  it("only advises a side_effect tool: it finishes and its result still goes out", async () => {
    const { h, state } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "book", { time: "wait" }, "side_effect"));
      await settle();
      await fake.send({ type: "tool.cancel", id: 1, reason: "timeout" });
      const reply = await fake.recvType("tool.result");
      assert.deepEqual(reply, { type: "tool.result", id: 1, result: "booked wait (timeout)" });
      assert.deepEqual(state.keys, ["0123456789abcdef"]);
    });
  });
});

describe("the link closing", () => {
  it("tells a side_effect tool and lets it finish", async () => {
    const { h, state } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "book", { time: "wait" }, "side_effect"));
      await settle();
      assert.equal(state.finished.isOpen, false);
      await fake.close();
      await state.finished.promise; // ctx.signal was aborted
      assert.equal(state.contexts[0]?.cancelReason, "link_closed");
      await until("nothing leaks", () => state.sessions[0]?.tools.size === 0);
    });
  });

  it("abandons a read_only tool at once", async () => {
    const { h, state } = toolsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "hangs", {}));
      await settle();
      await fake.close();
      await until("the abort", () => state.aborted.length === 1);
      assert.deepEqual(state.aborted, ["link_closed"]);
      await until("nothing leaks", () => state.sessions[0]?.tools.size === 0);
    });
  });

  it("stops waiting for a side_effect tool that ignores the link closing, after its grace", async () => {
    const { h, state } = toolsSetup({ sideEffectGraceMs: 50 });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "stubborn", {}, "side_effect"));
      await settle();
      assert.equal(state.sessions[0]?.tools.size, 1);
      await fake.close();
      await until("abandoned after the grace", () => state.sessions[0]?.tools.size === 0);
      state.release.open();
    });
  });

  it("keeps waiting for a side_effect tool within its grace", async () => {
    const { h, state } = toolsSetup({ sideEffectGraceMs: 30_000 });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "stubborn", {}, "side_effect"));
      await settle();
      await fake.close();
      await settle(150);
      assert.equal(state.sessions[0]?.tools.size, 1, "not abandoned at once");
      state.release.open();
      await until("finished by itself", () => state.sessions[0]?.tools.size === 0);
      assert.ok(h.logger.has("tool result dropped"));
    });
  });
});
