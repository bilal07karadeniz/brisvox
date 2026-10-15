/** Event handlers (order, backpressure, `re`) and commands (shape, results, timeouts). */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { CommandError, CommandTimeout, ErrorCode, SessionClosed, s } from "../src/index.js";
import type { AgentOptions, Session } from "../src/index.js";
import type { FakeServer, Wire } from "./fake-server.js";
import { Harness, gate, settle, until, using, toolCall } from "./helpers.js";

type Seen = (string | number | boolean | null | undefined)[];

function eventsSetup(options: AgentOptions = {}) {
  const h = new Harness(options);
  const seen: Seen[] = [];
  const ended = gate();
  const slow = gate();
  h.agent.onUserText(async (session, event) => {
    seen.push(["user", event.text, event.seq]);
    if (event.text === "slow") {
      await slow.promise;
    }
    if (event.text === "hours") {
      await session.say("We open at noon."); // `re` is taken from the event
    }
  });
  h.agent.onBotText((_session, event) => {
    seen.push(["bot", event.text]);
  });
  h.agent.onTranscript((_session, event) => {
    seen.push(["transcript", event.text, event.final]); // a plain function works too
  });
  h.agent.onError(() => {
    seen.push(["error"]);
    throw new Error("a failing handler must not kill the call");
  });
  h.agent.onEventsLost((_session, event) => {
    seen.push(["lost", event.count]);
  });
  h.agent.onSessionUsage((_session, event) => {
    seen.push(["usage", event.turns, event.usage.llm?.inputTokens]);
  });
  h.agent.onSessionEnd((session, event) => {
    seen.push(["end", event.reason, session.endReason]);
    ended.open();
  });
  return { h, seen, ended, slow };
}

const names = (seen: Seen[]): unknown[] => seen.map((entry) => entry[0]);

describe("events", () => {
  it("arrive in order, and a failing handler is survived", async () => {
    const { h, seen, ended } = eventsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "user.transcript", text: "hel", final: false });
      await fake.send({ type: "error", stage: "llm", message: "x", fatal: false });
      await fake.send({ type: "bot.text", epoch: 1, text: "Sure." });
      await fake.send({ type: "user.text", epoch: 1, text: "hello" });
      await fake.send({ type: "session.end", reason: "hangup" });
      await ended.promise;
      assert.deepEqual(names(seen), ["transcript", "error", "bot", "user", "end"]);
      assert.deepEqual(seen[0], ["transcript", "hel", false]);
      assert.deepEqual(seen.at(-1), ["end", "hangup", "hangup"]);
      assert.ok(h.logger.has("handler for error threw"));
    });
  });

  it("deliver session.usage before the session end, without a subscription", async () => {
    const { h, seen, ended } = eventsSetup();
    await using(h, async () => {
      const [fake, accept] = await h.call();
      // `session.usage` is always sent: it is not a subscribable event.
      assert.equal((accept["events"] as string[]).includes("session.usage"), false);
      await fake.send({
        type: "session.usage",
        duration_ms: 5000,
        turns: 2,
        interrupted_turns: 0,
        latency: {},
        usage: {
          llm: {
            provider: "openai",
            model: "m",
            input_tokens: 77,
            cached_input_tokens: 0,
            output_tokens: 3,
            responses_without_usage: 0,
          },
        },
      });
      await fake.send({ type: "session.end", reason: "hangup" });
      await ended.promise;
      assert.deepEqual(names(seen), ["usage", "end"]);
      assert.deepEqual(seen[0], ["usage", 2, 77]);
    });
  });

  it("make say() react to the event with re and wait for the result", async () => {
    const { h } = eventsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      const seq = await fake.send({ type: "user.text", epoch: 1, text: "hours" });
      const command = await fake.recvType("say");
      assert.equal(command["text"], "We open at noon.");
      assert.equal(command["re"], seq);
      assert.equal(typeof command["id"], "string");
      assert.equal("interrupt" in command, false);
      await fake.ok(command["id"]);
    });
  });

  it("do not block command results or tools behind a slow handler", async () => {
    const { h, slow } = eventsSetup();
    h.agent.tool({ name: "ping", description: "Ping.", handler: () => "pong" });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "user.text", epoch: 1, text: "slow" });
      await fake.send(toolCall(5, "ping", {}));
      const reply = await fake.recvType("tool.result");
      assert.deepEqual(reply, { type: "tool.result", id: 5, result: "pong" });
      slow.open();
    });
  });

  it("still end the session once when the link drops", async () => {
    const { h, seen, ended } = eventsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.close();
      await ended.promise;
      assert.deepEqual(seen.at(-1), ["end", "link_closed", "link_closed"]);
      await settle(50);
      assert.equal(names(seen).filter((n) => n === "end").length, 1);
    });
  });

  it("drop the newest when the handler queue is full and report events.lost", async () => {
    const { h, seen, ended, slow } = eventsSetup({ eventQueue: 2 });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "user.text", epoch: 1, text: "slow" }); // parks the dispatcher
      await settle();
      for (let n = 0; n < 5; n += 1) {
        await fake.send({ type: "bot.text", epoch: 1, text: `b${n}` }); // 2 fit, 3 are dropped
      }
      await settle();
      slow.open();
      await fake.send({ type: "session.end", reason: "hangup" });
      await ended.promise;
      assert.deepEqual(
        seen.filter((entry) => entry[0] === "lost"),
        [["lost", 3]],
      );
      assert.equal(names(seen).filter((n) => n === "bot").length, 2);
    });
  });

  it("never drop session.end", async () => {
    const { h, seen, ended, slow } = eventsSetup({ eventQueue: 2 });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "user.text", epoch: 1, text: "slow" });
      await settle();
      for (let n = 0; n < 5; n += 1) {
        await fake.send({ type: "user.text", epoch: 1, text: `u${n}` });
      }
      await fake.send({ type: "session.end", reason: "hangup" });
      await settle();
      assert.deepEqual(names(seen), ["user"]); // the handler is still parked
      slow.open();
      await ended.promise;
      assert.deepEqual(seen.at(-1), ["end", "hangup", "hangup"]);
      assert.equal(names(seen).filter((n) => n === "end").length, 1);
    });
  });

  it("put events.lost where the gap is", async () => {
    const { h, seen, ended, slow } = eventsSetup({ eventQueue: 2 });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "user.text", epoch: 1, text: "slow" });
      await settle();
      for (let n = 0; n < 5; n += 1) {
        await fake.send({ type: "user.text", epoch: 1, text: `u${n}` }); // u0, u1 fit; u2..u4 dropped
      }
      await settle();
      slow.open();
      await fake.send({ type: "session.end", reason: "hangup" });
      await ended.promise;
      assert.deepEqual(
        seen.map((entry) => entry.slice(0, 2)),
        [["user", "slow"], ["user", "u0"], ["user", "u1"], ["lost", 3], ["end", "hangup"]],
      );
    });
  });

  it("put events.lost in front of the next event that fits after the gap", async () => {
    const { h, seen, ended, slow } = eventsSetup({ eventQueue: 2 });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "user.text", epoch: 1, text: "slow" });
      await settle();
      for (let n = 0; n < 4; n += 1) {
        await fake.send({ type: "user.text", epoch: 1, text: `u${n}` }); // u0, u1 fit; u2, u3 dropped
      }
      await settle();
      slow.open();
      await settle(100); // drained: room again
      await fake.send({ type: "user.text", epoch: 1, text: "after" });
      await fake.send({ type: "session.end", reason: "hangup" });
      await ended.promise;
      assert.deepEqual(
        seen.map((entry) => entry.slice(0, 2)),
        [["user", "slow"], ["user", "u0"], ["user", "u1"], ["lost", 2], ["user", "after"], ["end", "hangup"]],
      );
    });
  });

  it("survive unknown types and malformed messages", async () => {
    const { h, seen, ended } = eventsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "future.thing", x: 1 });
      await fake.raw("not json");
      await fake.raw(Buffer.from([0, 1]));
      await fake.send({ type: "bot.text", epoch: 1 }); // missing text
      await fake.send({ type: "bot.text", epoch: 1, text: "still here" });
      await fake.send({ type: "session.start", ...{ protocol: "brisvox.agent", versions: [1], session_id: "2", agent: "x" } });
      await fake.send({ type: "session.end", reason: "hangup" });
      await ended.promise;
      assert.deepEqual(
        seen.filter((entry) => entry[0] === "bot"),
        [["bot", "still here"]],
      );
      assert.ok(h.logger.has("unreadable server message"));
      assert.ok(h.logger.has("binary frame ignored"));
      assert.ok(h.logger.has("second session.start ignored"));
    });
  });

  it("deliver nothing after session.end", async () => {
    const { h, seen, ended } = eventsSetup();
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "session.end", reason: "hangup" });
      await fake.send({ type: "bot.text", epoch: 1, text: "late" });
      await ended.promise;
      await settle(50);
      assert.deepEqual(names(seen), ["end"]);
    });
  });

  it("do not let a handler hog the end of the call forever", async () => {
    const h = new Harness({ events: [] });
    const ended = gate();
    h.agent.onUserText(() => new Promise<void>(() => undefined)); // never returns
    h.agent.onSessionEnd(() => {
      ended.open();
    });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send({ type: "user.text", epoch: 1, text: "stuck" });
      await settle();
      await fake.close();
      await ended.promise; // after the drain budget (5 s), the end still gets its turn
      assert.ok(h.logger.has("handlers did not finish"));
    });
  });
});

describe("commands", () => {
  async function newSession(options: AgentOptions = {}): Promise<{ h: Harness; fake: FakeServer; session: Session }> {
    const h = new Harness(options);
    const sessions: Session[] = [];
    h.agent.onSessionStart((session) => {
      sessions.push(session);
    });
    await h.start();
    const [fake] = await h.call();
    await until("the session", () => sessions.length === 1);
    return { h, fake, session: sessions[0] as Session };
  }

  async function withSession(body: (c: { h: Harness; fake: FakeServer; session: Session }) => Promise<void>, options: AgentOptions = {}): Promise<void> {
    const c = await newSession(options);
    try {
      await body(c);
    } finally {
      await c.h.stop();
    }
  }

  it("have their doc shape on the wire", async () => {
    await withSession(async ({ fake, session }) => {
      const results = [
        session.say("hi", { interrupt: true, re: 4 }),
        session.updateContext({ systemPrompt: "p", append: [{ role: "user", text: "u" }] }),
        session.setTools(["a"]),
        session.setTools(null),
        session.end({ say: "Bye", reason: "done" }),
        session.transfer("+4930123456", { mode: "warm", summary: "s", id: "tr1" }),
      ];
      const commands: Record<string, Record<string, any>> = {};
      for (let n = 0; n < results.length; n += 1) {
        const command = await fake.recv();
        commands[`${command["type"]}${command["type"] === "tools.set" ? JSON.stringify(command["enabled"]) : ""}`] = command;
        await fake.ok(command["id"]);
      }
      for (const result of await Promise.all(results)) {
        assert.equal(result.ok, true);
      }
      const say = commands["say"];
      assert.deepEqual([say?.["text"], say?.["interrupt"], say?.["re"]], ["hi", true, 4]);
      assert.deepEqual(commands["context.update"]?.["append"], [{ role: "user", text: "u" }]);
      assert.equal(commands["context.update"]?.["system_prompt"], "p");
      assert.deepEqual(commands['tools.set["a"]']?.["enabled"], ["a"]);
      assert.equal(commands["tools.setnull"]?.["enabled"], null);
      assert.equal(commands["end"]?.["say"], "Bye");
      assert.equal(commands["end"]?.["reason"], "done");
      const transfer = commands["transfer"];
      assert.deepEqual(
        [transfer?.["id"], transfer?.["destination"], transfer?.["mode"], transfer?.["summary"]],
        ["tr1", "+4930123456", "warm", "s"],
      );
    });
  });

  it("get unique ids, and a transfer always has one", async () => {
    await withSession(async ({ fake, session }) => {
      const first = session.transfer("+4930123456");
      const t1 = await fake.recv();
      await fake.ok(t1["id"]);
      await first;
      const second = session.say("x");
      const s1 = await fake.recv();
      await fake.ok(s1["id"]);
      await second;
      assert.equal(typeof t1["id"], "string");
      assert.notEqual(t1["id"], s1["id"]);
    });
  });

  it("raise CommandError with the code of a refusal", async () => {
    await withSession(async ({ fake, session }) => {
      const pending = session.transfer("+4930123456");
      // Expect the rejection before the refusal is sent: it can land before the next line runs.
      const refused = assert.rejects(pending, (error: unknown) => {
        return (
          error instanceof CommandError && error.code === ErrorCode.Unsupported && error.detail === "no call-control"
        );
      });
      const command = await fake.recv();
      await fake.fail(command["id"], "unsupported", "no call-control");
      await refused;
    });
  });

  it("check the rules before anything is sent", async () => {
    await withSession(async ({ fake, session }) => {
      for (const attempt of [
        () => session.say(""),
        () => session.transfer("0049301234"),
        () => session.say("x".repeat(5000)),
        () => session.say("x", { id: "i".repeat(65) }),
      ]) {
        await assert.rejects(
          attempt(),
          (error: unknown) => error instanceof CommandError && error.code === ErrorCode.InvalidArgument,
        );
      }
      await assert.rejects(
        session.updateContext({ append: Array.from({ length: 33 }, () => ({ role: "user" as const, text: "x" })) }),
        (error: unknown) => error instanceof CommandError && error.code === ErrorCode.TooLarge,
      );
      await assert.rejects(fake.recv(150), /no message/); // nothing went out
    });
  });

  it("raise CommandTimeout when no result comes, keeping the id for a retry", async () => {
    await withSession(async ({ fake, session }) => {
      await assert.rejects(
        session.transfer("+4930123456", { id: "tr-9", timeoutMs: 100 }),
        (error: unknown) =>
          error instanceof CommandTimeout &&
          error.commandId === "tr-9" &&
          error.code === ErrorCode.OutcomeUnknown &&
          error instanceof CommandError,
      );
      const first = await fake.recv();
      const retry = session.transfer("+4930123456", { id: "tr-9" }); // the same id is allowed again
      const again = await fake.recv();
      assert.equal(again["id"], first["id"]);
      await fake.ok("tr-9");
      assert.equal((await retry).ok, true);
    });
  });

  it("return a pending command with wait: false", async () => {
    await withSession(async ({ fake, session }) => {
      const pending = await session.say("hi", { wait: false });
      const command = await fake.recv();
      assert.equal(command["id"], pending.id);
      let settled = false;
      void pending.result.then(() => {
        settled = true;
      });
      await settle(30);
      assert.equal(settled, false);
      await fake.ok(command["id"]);
      assert.equal((await pending.result).ok, true);
    });
  });

  it("do not leak an unhandled rejection for a failed or unanswered unwaited command", async () => {
    await withSession(async ({ h, fake, session }) => {
      const failed = await session.say("hi", { wait: false });
      const command = await fake.recv();
      await fake.fail(command["id"], "queue_full");
      assert.equal((await failed.result).ok, false);
      assert.ok(h.logger.has("queue_full"));

      const unanswered = await session.say("again", { wait: false });
      await fake.recv();
      await fake.close();
      await assert.rejects(unanswered.result, SessionClosed);
      await settle(30); // an unhandled rejection would fail the test run here
    });
  });

  it("fail waiting commands when the link closes, and refuse new ones", async () => {
    await withSession(async ({ fake, session }) => {
      const waiting = session.say("hi");
      // Expect the rejection before the link closes: it can land before the next line runs.
      const rejected = assert.rejects(waiting, SessionClosed);
      await fake.recv();
      await fake.close();
      await rejected;
      await session.waitClosed();
      assert.equal(session.closed, true);
      await assert.rejects(session.say("again"), SessionClosed);
    });
  });

  it("refuse a duplicate id that is still waiting", async () => {
    await withSession(async ({ fake, session }) => {
      const first = session.say("a", { id: "same" });
      await fake.recv();
      await assert.rejects(session.say("b", { id: "same" }), RangeError);
      await fake.ok("same");
      await first;
    });
  });

  it("send no re when re is null, and the explicit re otherwise", async () => {
    const h = new Harness({});
    h.agent.onUserText(async (session) => {
      await session.say("auto");
      await session.say("none", { re: null });
      await session.say("explicit", { re: 99 });
    });
    await using(h, async () => {
      const [fake] = await h.call();
      const seq = await fake.send({ type: "user.text", epoch: 1, text: "go" });
      const auto = await fake.ackNext("say");
      const none = await fake.ackNext("say");
      const explicit = await fake.ackNext("say");
      assert.equal(auto["re"], seq);
      assert.equal("re" in none, false);
      assert.equal(explicit["re"], 99);
    });
  });

  it("send no re from outside a handler", async () => {
    await withSession(async ({ fake, session }) => {
      const pending = session.say("x");
      const command = await fake.recv();
      assert.equal("re" in command, false);
      await fake.ok(command["id"]);
      await pending;
    });
  });

  it("give commands from a tool no re either", async () => {
    const h = new Harness({});
    h.agent.tool({
      name: "speak",
      description: "Speaks.",
      parameters: s.object({}),
      handler: async (_args, ctx) => {
        await ctx.session.say("from a tool");
        return "spoken";
      },
    });
    await using(h, async () => {
      const [fake] = await h.call();
      await fake.send(toolCall(1, "speak", {}));
      const command = await fake.ackNext("say");
      assert.equal("re" in command, false);
      assert.equal((await fake.recvType("tool.result"))["result"], "spoken");
    });
  });

  it("lose no frame in a burst: 600 events, then a tool call and session.end, all in one write", async () => {
    const h = new Harness();
    let botStarted = 0;
    const ended = gate();
    h.agent.tool({ name: "ping", description: "Pings.", handler: () => "pong" });
    h.agent.onTurn(() => {
      botStarted += 1;
    });
    h.agent.onSessionEnd((_session, event) => {
      assert.equal(event.reason, "hangup");
      ended.open();
    });
    await using(h, async () => {
      const [fake] = await h.call();
      const frames: Wire[] = [];
      for (let n = 0; n < 600; n += 1) {
        frames.push({ type: "turn.bot_started", epoch: 1 });
      }
      frames.push(toolCall(7, "ping", {}));
      frames.push({ type: "session.end", reason: "hangup" });
      await fake.burst(frames);
      const reply = await fake.recvType("tool.result");
      assert.deepEqual(reply, { type: "tool.result", id: 7, result: "pong" });
      await ended.promise; // the end was not lost
      assert.equal(h.logger.has("dropped"), false);
      assert.ok(botStarted > 0);
    });
  });
});
