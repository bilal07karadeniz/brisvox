/** The handshake, the per-call setup, the bearer token and the limits, over a real WebSocket. */

import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { describe, it, mock } from "node:test";
import type { WebSocket } from "ws";
import { Agent, ErrorCode, Reject, isLoopbackHost, listeningMessage, s } from "../src/index.js";
import type { Accept, Session } from "../src/index.js";
import { FakeServer, HttpRefusal, START_BODY } from "./fake-server.js";
import { Harness, CollectingLogger, settle, until, using } from "./helpers.js";


function luigi(options: ConstructorParameters<typeof Harness>[0] = {}): Harness {
  const h = new Harness({
    systemPrompt: "You are Luigi's host.",
    greeting: "Hi, Luigi's here.",
    ...options,
  });
  h.agent.tool({
    name: "book_table",
    description: "Books a table.",
    kind: "side_effect",
    timeoutMs: 4000,
    parameters: s.object({ time: s.string() }),
    handler: () => "ok",
  });
  h.agent.onUserText(() => undefined);
  h.agent.onSessionEnd(() => undefined);
  h.agent.onTurn(() => undefined);
  return h;
}

describe("handshake", () => {
  it("accepts with prompt, greeting, tools and the events that have handlers", async () => {
    const h = luigi({ context: [{ role: "system", text: "The caller is a regular." }] });
    await using(h, async () => {
      const [, answer] = await h.call();
      assert.deepEqual(answer, {
        type: "session.accept",
        version: 1,
        tools: [
          {
            name: "book_table",
            description: "Books a table.",
            parameters: { type: "object", properties: { time: { type: "string" } }, required: ["time"] },
            kind: "side_effect",
            timeout_ms: 4000,
          },
        ],
        system_prompt: "You are Luigi's host.",
        greeting: "Hi, Luigi's here.",
        context: [{ role: "system", text: "The caller is a regular." }],
        // only types with handlers, in the protocol's order; session.end is not subscribable
        events: [
          "turn.user_started",
          "turn.user_ended",
          "turn.bot_started",
          "turn.bot_ended",
          "turn.interrupted",
          "turn.metrics",
          "user.text",
        ],
      });
    });
  });

  it("subscribes to nothing when there are no handlers", async () => {
    const h = new Harness({});
    await using(h, async () => {
      const [, answer] = await h.call();
      assert.equal(answer["type"], "session.accept");
      assert.deepEqual(answer["events"], []);
      assert.deepEqual(answer["tools"], []);
      assert.equal("system_prompt" in answer, false);
      assert.equal("greeting" in answer, false);
    });
  });

  it("subscribes to every event with events: null, to the listed ones with an array", async () => {
    const all = new Harness({ events: null });
    await using(all, async () => {
      const [, answer] = await all.call();
      assert.equal("events" in answer, false);
    });
    const some = new Harness({ events: ["bot.text"] });
    await using(some, async () => {
      const [, answer] = await some.call();
      assert.deepEqual(answer["events"], ["bot.text"]);
    });
  });

  it("picks the highest common version", async () => {
    const h = luigi();
    await using(h, async () => {
      const [, answer] = await h.call({ versions: [3, 1, 2] });
      assert.equal(answer["type"], "session.accept");
      assert.equal(answer["version"], 1);
    });
  });

  it("rejects with unsupported_version when no version is common", async () => {
    const h = luigi();
    await using(h, async () => {
      const [fake, answer] = await h.call({ versions: [2, 3] });
      assert.equal(answer["type"], "session.reject");
      assert.equal(answer["code"], "unsupported_version");
      assert.match(String(answer["message"]), /speaks versions \[1\]/);
      await until("the agent closed the link", () => fake.closed);
      assert.equal(fake.closeCode, 1000);
    });
  });

  it("closes the link on a wrong protocol or a first message that is not session.start", async () => {
    const h = luigi();
    await using(h, async () => {
      const wrong = await FakeServer.connect(h.url);
      await wrong.send({ type: "session.start", ...START_BODY, protocol: "other" });
      await until("close", () => wrong.closed);
      assert.equal(wrong.closeCode, 1002);

      const notStart = await FakeServer.connect(h.url);
      await notStart.send({ type: "user.text", epoch: 1, text: "hi" });
      await until("close", () => notStart.closed);
      assert.equal(notStart.closeCode, 1002);

      const binary = await FakeServer.connect(h.url);
      await binary.raw(Buffer.from([0, 1, 2]));
      await until("close", () => binary.closed);
      assert.equal(binary.closeCode, 1002);

      const garbage = await FakeServer.connect(h.url);
      await garbage.raw("not json");
      await until("close", () => garbage.closed);
      assert.equal(garbage.closeCode, 1002);
    });
  });

  it("gives up on a link that never sends session.start", async () => {
    const h = luigi({ handshakeTimeoutMs: 100 });
    await using(h, async () => {
      const silent = await FakeServer.connect(h.url);
      await until("close", () => silent.closed);
      assert.equal(silent.closeCode, 1008);
      assert.ok(h.logger.has("no session.start"));
    });
  });

  it("refuses a plain HTTP request with 426", async () => {
    const h = luigi();
    await using(h, async () => {
      const response = await fetch(h.url.replace("ws://", "http://"));
      assert.equal(response.status, 426);
    });
  });
});

describe("onSessionStart", () => {
  it("keeps the defaults when it returns nothing, and knows the call", async () => {
    const h = luigi();
    const seen: Session[] = [];
    h.agent.onSessionStart((session) => {
      seen.push(session);
    });
    await using(h, async () => {
      const [, answer] = await h.call();
      assert.equal(answer["system_prompt"], "You are Luigi's host.");
      const session = seen[0];
      assert.ok(session);
      assert.equal(session.id, "42");
      assert.equal(session.agentName, "luigi");
      assert.equal(session.version, 1);
      assert.deepEqual(session.capabilities, { transfer: false, tools: true });
    });
  });

  it("exposes who is on the line when the server says", async () => {
    const h = luigi();
    const seen: (Session["call"])[] = [];
    h.agent.onSessionStart((session) => {
      seen.push(session.call);
    });
    await using(h, async () => {
      await h.call();
      await h.call({
        call: { carrier: "telnyx", carrier_call_id: "v3:abc", from: "+15551230001", to: "+15551230002" },
      });
      assert.deepEqual(seen, [
        undefined,
        { carrier: "telnyx", carrierCallId: "v3:abc", from: "+15551230001", to: "+15551230002", piiRedacted: false },
      ]);
    });
  });

  it("can change prompt, context, greeting, events and tools for one call", async () => {
    const h = luigi();
    h.agent.tool({ name: "other", description: "Other.", handler: () => "x" });
    h.agent.onSessionStart((session): Accept | undefined => {
      if (session.id === "7") {
        return {
          systemPrompt: "Special.",
          greeting: null,
          context: [{ role: "user", text: "hello" }],
          events: ["bot.text"],
          tools: ["other"],
        };
      }
      return undefined;
    });
    await using(h, async () => {
      const [, special] = await h.call({ session_id: "7" });
      assert.equal(special["system_prompt"], "Special.");
      assert.equal("greeting" in special, false);
      assert.deepEqual(special["context"], [{ role: "user", text: "hello" }]);
      assert.deepEqual(special["events"], ["bot.text"]);
      assert.deepEqual(
        (special["tools"] as { name: string }[]).map((t) => t.name),
        ["other"],
      );
      const [, normal] = await h.call({ session_id: "8" });
      assert.equal(normal["system_prompt"], "You are Luigi's host.");
      assert.equal((normal["tools"] as unknown[]).length, 2);
    });
  });

  it("sends session.reject when the handler throws Reject", async () => {
    const h = luigi();
    h.agent.onSessionStart(() => {
      throw new Reject(ErrorCode.Unavailable, "no calls tonight");
    });
    await using(h, async () => {
      const [fake, answer] = await h.call();
      assert.deepEqual(answer, { type: "session.reject", code: "unavailable", message: "no calls tonight" });
      await until("close", () => fake.closed);
    });
  });

  it("rejects instead of hanging when the handler throws something else", async () => {
    const h = luigi();
    h.agent.onSessionStart(() => {
      throw new Error("secret internal detail");
    });
    await using(h, async () => {
      const [, answer] = await h.call();
      assert.equal(answer["type"], "session.reject");
      assert.equal(answer["code"], "unavailable");
      assert.equal(JSON.stringify(answer).includes("secret"), false);
    });
  });

  it("rejects when the handler asks for an undeclared tool", async () => {
    const h = luigi();
    h.agent.onSessionStart(() => ({ tools: ["nope"] }));
    await using(h, async () => {
      const [, answer] = await h.call();
      assert.equal(answer["type"], "session.reject");
      assert.ok(h.logger.has("session.accept is invalid"));
    });
  });

  it("is cut at the handshake budget", async () => {
    const h = luigi({ handshakeTimeoutMs: 150 });
    h.agent.onSessionStart(() => new Promise<void>(() => undefined));
    await using(h, async () => {
      const started = Date.now();
      const [, answer] = await h.call();
      assert.equal(answer["type"], "session.reject");
      assert.equal(answer["message"], "handshake budget exceeded");
      assert.ok(Date.now() - started < 1500);
    });
  });

  it("refuses a second handler", () => {
    const agent = new Agent();
    agent.onSessionStart(() => undefined);
    assert.throws(() => agent.onSessionStart(() => undefined), /already registered/);
  });
});

describe("bearer token", () => {
  it("is required, compared exactly, and answered with 401 before any frame", async () => {
    const h = luigi();
    await using(
      h,
      async () => {
        for (const [token, authorization] of [
          [undefined, undefined],
          ["wrong", undefined],
          [undefined, "Basic s3cret"],
          [undefined, "Bearer"],
          [undefined, "Bearer s3cret and more"],
          ["s3cre", undefined],
        ] as const) {
          await assert.rejects(
            FakeServer.connect(h.url, token, authorization),
            (error: unknown) => error instanceof HttpRefusal && error.status === 401,
          );
        }
        const [, answer] = await h.call({}, "s3cret");
        assert.equal(answer["type"], "session.accept");
        const fake = await FakeServer.connect(h.url, undefined, "bearer s3cret");
        const lower = await fake.start();
        assert.equal(lower["type"], "session.accept");
        await fake.close();
      },
      "s3cret",
    );
  });

  it("reads BRISVOX_AGENT_TOKEN by default", async () => {
    const agent = new Agent({ logger: new CollectingLogger() });
    const saved = process.env["BRISVOX_AGENT_TOKEN"];
    process.env["BRISVOX_AGENT_TOKEN"] = "from-env";
    try {
      const server = await agent.listen({ port: 0 });
      try {
        await assert.rejects(FakeServer.connect(server.url), HttpRefusal);
        const fake = await FakeServer.connect(server.url, "from-env");
        assert.equal((await fake.start())["type"], "session.accept");
        await fake.close();
      } finally {
        await server.close();
      }
    } finally {
      if (saved === undefined) {
        delete process.env["BRISVOX_AGENT_TOKEN"];
      } else {
        process.env["BRISVOX_AGENT_TOKEN"] = saved;
      }
    }
  });
});

describe("limits", () => {
  it("rejects more calls than maxSessions with unavailable, and frees the slot when a call ends", async () => {
    const h = luigi({ maxSessions: 1 });
    await using(h, async () => {
      const [first, accepted] = await h.call();
      assert.equal(accepted["type"], "session.accept");
      const [, second] = await h.call({ session_id: "43" });
      assert.equal(second["type"], "session.reject");
      assert.equal(second["code"], "unavailable");
      await first.close();
      await settle(100);
      const [, third] = await h.call({ session_id: "44" });
      assert.equal(third["type"], "session.accept");
    });
  });

  it("isolates calls: one closing does not touch another", async () => {
    const h = luigi({ events: null });
    const seen: string[] = [];
    h.agent.onBotText((session, event) => {
      seen.push(`${session.id}:${event.text}`);
    });
    await using(h, async () => {
      const [a] = await h.call({ session_id: "a" });
      const [b] = await h.call({ session_id: "b" });
      await a.close();
      await b.send({ type: "bot.text", epoch: 1, text: "x" });
      await until("b's event", () => seen.includes("b:x"));
      assert.equal(b.closed, false);
    });
  });

  it("takes down a link that goes silent and ends the session", async () => {
    const h = luigi({ idleTimeoutMs: 120 });
    const reasons: string[] = [];
    h.agent.onSessionEnd((_session, event) => {
      reasons.push(event.reason);
    });
    await using(h, async () => {
      const [fake] = await h.call();
      await until("the watchdog", () => fake.closed, 3000);
      await until("the end", () => reasons.length === 1);
      assert.deepEqual(reasons, ["link_closed"]);
      assert.ok(h.logger.has("nothing received"));
    });
  });

  it("keeps a link that keeps talking", async () => {
    const h = luigi({ idleTimeoutMs: 200 });
    await using(h, async () => {
      const [fake] = await h.call();
      for (let n = 0; n < 6; n += 1) {
        await settle(60);
        await fake.send({ type: "bot.text", epoch: 1, text: "ping" });
      }
      assert.equal(fake.closed, false);
    });
  });

  it("validates its settings", () => {
    assert.throws(() => new Agent({ eventQueue: 0 }), RangeError);
    assert.throws(() => new Agent({ maxSessions: 0 }), RangeError);
    assert.throws(() => new Agent({ maxConcurrentTools: 0 }), RangeError);
    assert.throws(() => new Agent({ handshakeTimeoutMs: -1 }), RangeError);
    assert.throws(() => new Agent({ versions: [2] }), /speaks versions/);
    assert.throws(() => new Agent({ versions: [] }), RangeError);
    assert.throws(() => new Agent({ events: ["session.end" as never] }), /unsubscribable/);
  });
});

describe("handshake budget", () => {
  it("bounds the session.accept write", async () => {
    // A socket that delivers session.start and then never completes a write.
    class StuckSocket extends EventEmitter {
      readyState = 1;
      terminated = false;
      send(): void {
        // never calls back
      }
      close(): void {
        this.readyState = 3;
        this.emit("close");
      }
      terminate(): void {
        this.terminated = true;
        this.readyState = 3;
        this.emit("close");
      }
    }
    const logger = new CollectingLogger();
    const agent = new Agent({ name: "luigi", handshakeTimeoutMs: 200, writeTimeoutMs: 10_000, logger });
    const socket = new StuckSocket();
    const started = Date.now();
    const done = agent.handle(socket as unknown as WebSocket);
    socket.emit("message", Buffer.from(JSON.stringify({ seq: 1, t_ms: 0, type: "session.start", ...START_BODY })), false);
    await done;
    assert.ok(Date.now() - started < 2000, "the write was bounded by the handshake budget, not by writeTimeoutMs");
    assert.ok(socket.terminated, "a write that cannot finish takes the link down");
    assert.ok(logger.has("handshake failed"));
  });
});

describe("startup line", () => {
  it("recognises the loopback binds", () => {
    for (const host of ["127.0.0.1", "127.1.2.3", "::1", "[::1]", "localhost", "::ffff:127.0.0.1", "0:0:0:0:0:0:0:1"]) {
      assert.equal(isLoopbackHost(host), true, host);
    }
    for (const host of ["0.0.0.0", "::", "192.168.1.5", "10.0.0.2", "agent.internal", "", "fd00::1"]) {
      assert.equal(isLoopbackHost(host), false, host);
    }
  });

  it("names the url and hints only on a loopback bind", () => {
    const local = listeningMessage("luigi", "127.0.0.1", "ws://127.0.0.1:8765");
    assert.match(local, /'luigi' listening on ws:\/\/127\.0\.0\.1:8765/);
    assert.match(local, /reachable only from this machine/);
    assert.match(local, /0\.0\.0\.0 or ::/);
    const open = listeningMessage("luigi", "::", "ws://[::]:8765");
    assert.match(open, /listening on ws:\/\/\[::\]:8765/);
    assert.doesNotMatch(open, /loopback/);
  });

  const runOnce = async (agent: Agent, host?: string): Promise<void> => {
    const stop = new AbortController();
    await agent.run({ port: 0, token: null, signal: stop.signal, onReady: () => stop.abort(), ...(host === undefined ? {} : { host }) });
  };

  it("run() logs the bound address through the agent's logger", async () => {
    const logger = new CollectingLogger();
    await runOnce(new Agent({ name: "luigi", logger }));
    assert.ok(logger.has("info: agent 'luigi' listening on ws://127.0.0.1:"));
    assert.ok(logger.has("reachable only from this machine"));
    const open = new CollectingLogger();
    await runOnce(new Agent({ name: "luigi", logger: open }), "0.0.0.0");
    assert.ok(open.has("listening on ws://0.0.0.0:"));
    assert.equal(open.has("loopback"), false);
  });

  it("run() prints the address to stderr when the default logger would drop it", async () => {
    const saved = process.env["BRISVOX_AGENT_LOG"];
    delete process.env["BRISVOX_AGENT_LOG"];
    const printed = mock.method(console, "error", () => undefined);
    try {
      await runOnce(new Agent({ name: "luigi" }));
      const lines = printed.mock.calls.map((call) => String(call.arguments[0]));
      assert.ok(lines.some((line) => line.includes("listening on ws://127.0.0.1:") && line.includes("reachable only from this machine")), lines.join("\n"));
    } finally {
      printed.mock.restore();
      if (saved !== undefined) {
        process.env["BRISVOX_AGENT_LOG"] = saved;
      }
    }
  });
});

describe("declaration", () => {
  const handler = (): string => "x";

  it("refuses duplicate and illegal tools when they are declared", () => {
    const agent = new Agent();
    agent.tool({ name: "a", description: "d", handler });
    assert.throws(() => agent.tool({ name: "a", description: "d", handler }), /declared twice/);
    assert.throws(() => agent.tool({ name: "bad name", description: "d", handler }), RangeError);
  });

  it("allows at most 64 tools", () => {
    const agent = new Agent();
    for (let n = 0; n < 64; n += 1) {
      agent.tool({ name: `t${n}`, description: "d", handler });
    }
    assert.throws(() => agent.tool({ name: "t64", description: "d", handler }), /at most 64 tools/);
  });

  it("refuses an unknown event type", () => {
    const agent = new Agent();
    assert.throws(() => agent.on("nope" as never, () => undefined), /unknown event type/);
  });
});
