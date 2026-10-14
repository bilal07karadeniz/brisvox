/** The shipped examples: they load, declare what the docs say, and work end to end. */

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { fileURLToPath } from "node:url";
import { agent as minimal } from "../examples/minimal.js";
import { agent as restaurant } from "../examples/restaurant.js";
import { agent as resumable } from "../examples/resumable.js";
import { FakeServer } from "./fake-server.js";
import { sleep, toolCall } from "./helpers.js";

describe("examples", () => {
  it("both import without starting a server and have an agent", () => {
    assert.equal(minimal.name, "minimal");
    assert.equal(restaurant.name, "luigi");
    assert.deepEqual([...minimal.tools.keys()], ["lookup"]);
    assert.deepEqual([...restaurant.tools.keys()].sort(), ["book_table", "opening_hours"]);
  });

  it("declares the book_table schema the Rust integration test relies on", () => {
    const declaration = restaurant.tools.get("book_table")?.declaration();
    assert.equal(declaration?.kind, "side_effect");
    assert.equal(declaration?.timeout_ms, 4000);
    const parameters = declaration?.parameters as {
      required: string[];
      properties: Record<string, Record<string, unknown>>;
    };
    assert.deepEqual(parameters.required, ["time"]);
    assert.equal(parameters.properties["party_size"]?.["default"], 2);
    assert.deepEqual(Object.keys(parameters.properties), ["time", "party_size", "contact"]);
  });

  it("restaurant works end to end against a fake server", async () => {
    const server = await restaurant.listen({ port: 0, token: null });
    try {
      const fake = await FakeServer.connect(server.url);
      const accept = await fake.start();
      assert.equal(accept["greeting"], "Hi, Luigi's here. How can I help?");
      assert.deepEqual(accept["events"], [
        "turn.user_started",
        "turn.user_ended",
        "turn.bot_started",
        "turn.bot_ended",
        "turn.interrupted",
        "turn.metrics",
        "user.text",
      ]);

      await fake.send(toolCall(1, "book_table", { time: "19:30", party_size: 4, contact: { name: "Ann" } }, "side_effect"));
      const booked = await fake.recvType("tool.result");
      assert.deepEqual(booked["result"], { status: "booked", time: "19:30", party_size: 4, name: "Ann" });

      await fake.send(toolCall(2, "book_table", { time: "20:00", party_size: 99 }, "side_effect"));
      assert.equal((await fake.recvType("tool.result"))["error"], "We seat parties of 1 to 12.");

      // The same idempotency key books once: a repeat returns the first booking.
      await fake.send(toolCall(6, "book_table", { time: "21:00", party_size: 2 }, "side_effect"));
      assert.equal((await fake.recvType("tool.result"))["result"].time, "19:30");

      const other = { ...toolCall(3, "book_table", { time: "20:00" }, "side_effect"), idempotency_key: "1111111111111111" };
      await fake.send(other);
      assert.equal((await fake.recvType("tool.result"))["result"].party_size, 2, "the default applies");

      await fake.send(toolCall(4, "opening_hours", { day: "mon" }));
      assert.equal((await fake.recvType("tool.result"))["result"], "Closed on Monday.");
      await fake.send(toolCall(5, "opening_hours", { day: "funday" }));
      assert.match((await fake.recvType("tool.result"))["error"], /\$\.day/);

      const seq = await fake.send({ type: "user.text", epoch: 1, text: "ok goodbye" });
      const end = await fake.ackNext("end");
      assert.equal(end["say"], "Goodbye, and thank you for calling!");
      assert.equal(end["re"], seq);

      // No transfer on this server: the example offers to take a message instead.
      await fake.send({ type: "user.text", epoch: 2, text: "i want a human" });
      const update = await fake.ackNext("context.update");
      assert.equal(update["append"][0].role, "system");

      await fake.send({ type: "user.text", epoch: 3, text: "let us just chat" });
      assert.deepEqual((await fake.ackNext("tools.set"))["enabled"], []);
      await fake.close();
    } finally {
      await server.close();
    }
  });
});

describe("resumable example", () => {
  it("keeps its state across a dropped link", async () => {
    const server = await resumable.listen({ port: 0, token: null });
    try {
      const fake = await FakeServer.connect(server.url);
      const accept = await fake.start();
      assert.equal(accept["resume"], true);
      assert.equal(accept["greeting"], "Hi, Luigi's here.");
      await fake.send(toolCall(1, "remember", { note: "window seat" }, "side_effect"));
      assert.equal((await fake.recvType("tool.result"))["result"], "noted (1)");
      await fake.close();

      const again = await FakeServer.connect(server.url);
      again.seq = 2; // the agent read two messages on the first link (session.start, tool.call)
      const answer = await again.start({ resume: { attempt: 1, previous_seq: 2, down_ms: 50 } });
      assert.equal(answer["type"], "session.accept");
      assert.equal("greeting" in answer, false);
      await sleep(50); // onResumed runs behind the reader
      await again.send(toolCall(2, "recall", {}));
      assert.deepEqual((await again.recvType("tool.result"))["result"], {
        notes: ["window seat"],
        starts: 1,
        resumes: 1,
        gaps: [{ attempt: 1, previous_seq: 2, seen_seq: 2, down_ms: 50, seq: 3 }],
        seq_continues: true,
      });
      await again.close();
    } finally {
      await server.close();
    }
  });

  it("notices a server count that does not match what it read", async () => {
    const server = await resumable.listen({ port: 0, token: null });
    try {
      const fake = await FakeServer.connect(server.url);
      await fake.start();
      await fake.close();
      const again = await FakeServer.connect(server.url);
      again.seq = 10; // the agent only ever read session.start (seq 1)
      await again.start({ resume: { attempt: 1, previous_seq: 10, down_ms: 50 } });
      await sleep(50);
      await again.send(toolCall(2, "recall", {}));
      assert.equal((await again.recvType("tool.result"))["result"].seq_continues, false);
      await again.close();
    } finally {
      await server.close();
    }
  });
});

describe("examples/Dockerfile", () => {
  // The image builds from the repository root: every path it copies has to exist there.
  const repository = new URL("../../../../", import.meta.url);
  const dockerfile = readFileSync(new URL("sdk/typescript/examples/Dockerfile", repository), "utf8");
  const instructions = dockerfile
    .replace(/\\\r?\n/g, " ")
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== "" && !line.startsWith("#"));

  it("copies only paths that exist in the repository", () => {
    const sources = instructions
      .filter((line) => line.startsWith("COPY ") && !line.includes("--from="))
      .flatMap((line) => line.split(/\s+/).slice(1, -1))
      .filter((arg) => !arg.startsWith("--"));
    assert.ok(sources.includes("sdk/typescript/src"), `${sources}`);
    assert.ok(sources.includes("sdk/typescript/examples"), `${sources}`);
    for (const source of sources) {
      assert.ok(existsSync(fileURLToPath(new URL(source, repository))), `COPY ${source}: not in the repository`);
    }
  });

  it("runs a compiled example and binds all interfaces", () => {
    const cmd = instructions.find((line) => line.startsWith("CMD "));
    assert.ok(cmd?.includes('"dist/examples/restaurant.js"'), cmd);
    assert.ok(cmd?.includes('"0.0.0.0"'), cmd);
    assert.ok(existsSync(fileURLToPath(new URL("sdk/typescript/examples/restaurant.ts", repository))));
  });
});
