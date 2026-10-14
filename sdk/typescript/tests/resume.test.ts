/**
 * Reconnecting: `new Agent({ resume: true })` declares `resume`, parks a call whose link
 * was lost and re-attaches it when the server's `session.start.resume` arrives
 * (docs/AGENT_PROTOCOL.md, "Reconnecting").
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { Agent, ErrorCode, ProtocolError, SessionClosed, parseServerMessage, s } from "../src/index.js";
import type { Resumed, Session } from "../src/index.js";
import { sessionAccept } from "../src/protocol.js";
import { FakeServer, START_BODY } from "./fake-server.js";
import type { Wire } from "./fake-server.js";
import { Harness, settle, toolCall, until, using } from "./helpers.js";

const RESUME = { attempt: 1, previous_seq: 7, down_ms: 120 };

interface Rig {
  h: Harness;
  log: unknown[][];
  sessions: Session[];
  resumedEvents: Resumed[];
}

/** An agent that keeps notes in `session.data` and records everything it is told. */
function rig(options: ConstructorParameters<typeof Harness>[0] = {}): Rig {
  const h = new Harness({ greeting: "Hi!", resume: true, ...options });
  const log: unknown[][] = [];
  const sessions: Session[] = [];
  const resumedEvents: Resumed[] = [];
  h.agent.onSessionStart((session) => {
    log.push(["start", session.id]);
    session.data["notes"] = [] as string[];
    sessions.push(session);
  });
  h.agent.tool({
    name: "note",
    description: "Keeps a note on the call.",
    parameters: s.object({ text: s.string() }),
    handler: ({ text }, ctx) => {
      const notes = ctx.session.data["notes"] as string[];
      notes.push(text);
      return `${notes.length} notes`;
    },
  });
  h.agent.tool({
    name: "hang",
    description: "Waits until the link is lost.",
    parameters: s.object({}),
    handler: (_args, ctx) =>
      new Promise<string>((resolve) => {
        ctx.signal.addEventListener("abort", () => {
          log.push(["hang", String(ctx.signal.reason)]);
          resolve("unreachable");
        });
      }),
  });
  h.agent.onUserText((session, event) => {
    log.push(["user", event.text]);
    (session.data["notes"] as string[]).push(event.text);
  });
  h.agent.onResumed((_session, event) => {
    resumedEvents.push(event);
    log.push(["resumed", event.attempt, event.previousSeq, event.downMs, event.seq]);
  });
  h.agent.onEventsLost((_session, event) => {
    log.push(["lost", event.count]);
  });
  h.agent.onSessionEnd((_session, event) => {
    log.push(["end", event.reason]);
  });
  return { h, log, sessions, resumedEvents };
}

function entries(r: Rig, kind: string): unknown[][] {
  return r.log.filter((entry) => entry[0] === kind);
}

async function connect(h: Harness): Promise<FakeServer> {
  const fake = await FakeServer.connect(h.url);
  h.fakes.push(fake);
  return fake;
}

async function firstLink(h: Harness): Promise<[FakeServer, Wire]> {
  const fake = await connect(h);
  return [fake, await fake.start()];
}

async function resumeLink(h: Harness, resume: Wire = {}): Promise<[FakeServer, Wire]> {
  const info = { ...RESUME, ...resume };
  const fake = await connect(h);
  fake.seq = info["previous_seq"] as number; // the count continues on the new link
  return [fake, await fake.start({ resume: info })];
}

describe("resume: declaration", () => {
  it("declares resume in the accept only when opted in", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [, answer] = await firstLink(r.h);
      assert.equal(answer["resume"], true);
    });
    const plain = new Harness({});
    await using(plain, async () => {
      const [, answer] = await plain.call();
      assert.equal("resume" in answer, false);
    });
  });

  it("validates the window", () => {
    assert.throws(() => new Agent({ resume: true, resumeWindowMs: 0 }), RangeError);
    assert.throws(() => new Agent({ resume: true, resumeWindowMs: Number.NaN }), RangeError);
  });
});

describe("resume: a resumption is not a new call", () => {
  it("re-attaches the parked session: state kept, no second start, no greeting", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [fake, first] = await firstLink(r.h);
      await fake.send({ type: "user.text", epoch: 1, text: "table for two" });
      await until("the first event", () => entries(r, "user").length === 1);
      await fake.close();
      const session = r.sessions[0] as Session;
      await until("the session is parked", () => session.parked);
      assert.equal(session.closed, true);

      const [again, answer] = await resumeLink(r.h);
      assert.equal(answer["type"], "session.accept");
      assert.equal(answer["resume"], true);
      assert.equal(answer["version"], 1);
      assert.equal("greeting" in answer, false, "a resumed call is not greeted again");
      assert.deepEqual(answer["events"], first["events"], "the subscription is kept");
      await until("the resumed hook", () => entries(r, "resumed").length === 1);
      assert.deepEqual(entries(r, "start"), [["start", "42"]], "onSessionStart ran once");
      assert.equal(r.sessions.length, 1);
      assert.equal(session.parked, false);
      assert.equal(session.closed, false);
      assert.equal(session.resumes, 1);
      assert.deepEqual(session.data["notes"], ["table for two"], "the call's state is kept");
      assert.deepEqual(entries(r, "resumed"), [["resumed", 1, 7, 120, 8]]);
      assert.ok(
        r.log.findIndex((e) => e[0] === "user") < r.log.findIndex((e) => e[0] === "resumed"),
        "onResumed runs after the events queued before the loss",
      );
      await again.close();
    });
  });

  it("serves events, commands and tools on the resumed link", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      await fake.close();
      const session = r.sessions[0] as Session;
      await until("parked", () => session.parked);
      const [again] = await resumeLink(r.h);
      await until("resumed", () => !session.parked);

      await again.send({ type: "user.text", epoch: 2, text: "hello again" });
      await until("an event on the new link", () => entries(r, "user").length === 1);

      const say = session.say("Back with you.");
      const command = await again.recvType("say");
      assert.equal(command["text"], "Back with you.");
      await again.ok(command["id"] as string);
      await say;

      await again.send(toolCall(9, "note", { text: "kept" }, "side_effect"));
      const reply = await again.recvType("tool.result");
      assert.deepEqual(reply, { type: "tool.result", id: 9, result: "2 notes" });
      assert.deepEqual(session.data["notes"], ["hello again", "kept"]);
    });
  });

  it("delivers the end once, with the server's events.lost in place", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      await fake.close();
      const [again] = await resumeLink(r.h);
      await until("resumed", () => entries(r, "resumed").length === 1);
      await again.send({ type: "events.lost", count: 3 });
      await again.send({ type: "session.end", reason: "hangup" });
      await until("the end", () => entries(r, "end").length === 1);
      await again.close();
      await settle();
      assert.deepEqual(entries(r, "end"), [["end", "hangup"]]);
      assert.deepEqual(entries(r, "lost"), [["lost", 3]]);
      assert.ok(r.log.findIndex((e) => e[0] === "resumed") < r.log.findIndex((e) => e[0] === "lost"));
    });
  });

  it("survives two losses and two resumptions", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      const session = r.sessions[0] as Session;
      await fake.close();
      const [second] = await resumeLink(r.h, { attempt: 1, previous_seq: 7 });
      await until("first resumption", () => session.resumes === 1);
      await second.close();
      await until("parked again", () => session.parked);
      await resumeLink(r.h, { attempt: 2, previous_seq: 20 });
      await until("second resumption", () => session.resumes === 2);
      assert.deepEqual(
        entries(r, "resumed").map((e) => e[1]),
        [1, 2],
      );
      assert.deepEqual(entries(r, "start"), [["start", "42"]]);
    });
  });
});

describe("resume: parking", () => {
  it("fails commands while parked, pending ones at the loss", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      const session = r.sessions[0] as Session;
      const waiting = session.say("Hello?");
      const outcome = assert.rejects(waiting, SessionClosed);
      await fake.recvType("say");
      await fake.close();
      await outcome;
      await until("parked", () => session.parked);
      await assert.rejects(session.say("Anyone there?"), SessionClosed);
    });
  });

  it("tells a running tool about the loss and serves tools after the resumption", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      await fake.send(toolCall(1, "hang", {}));
      await settle();
      await fake.close();
      const session = r.sessions[0] as Session;
      await until("the tool heard the loss", () => entries(r, "hang").length === 1);
      assert.deepEqual(entries(r, "hang"), [["hang", "link_closed"]]);
      await until("parked", () => session.parked);
      const [again] = await resumeLink(r.h);
      await again.send(toolCall(2, "note", { text: "x" }));
      const reply = await again.recvType("tool.result");
      assert.equal(reply["id"], 2);
    });
  });

  it("treats the server's session.end as final", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      await fake.send({ type: "session.end", reason: "hangup" });
      await until("the end", () => entries(r, "end").length === 1);
      await fake.close();
      await settle();
      assert.equal((r.sessions[0] as Session).parked, false);
      const [, answer] = await resumeLink(r.h);
      assert.deepEqual(answer, { type: "session.reject", code: "unknown_session", message: "no such call" });
      assert.deepEqual(entries(r, "end"), [["end", "hangup"]]);
    });
  });

  it("rejects the resumption of a call it does not know", async () => {
    const r = rig();
    await using(r.h, async () => {
      const fake = await connect(r.h);
      fake.seq = 3;
      const answer = await fake.start({ session_id: "never-seen", resume: RESUME });
      assert.equal(answer["type"], "session.reject");
      assert.equal(answer["code"], "unknown_session");
      assert.deepEqual(r.log, [], "no handler ran");
    });
  });

  it("rejects a resumption when the agent did not opt in", async () => {
    const plain = new Harness({});
    await using(plain, async () => {
      const fake = await connect(plain);
      const answer = await fake.start({ resume: RESUME });
      assert.equal(answer["code"], "unknown_session");
    });
  });

  it("ends the call at once when the agent did not opt in", async () => {
    const plain = new Harness({});
    const ends: string[] = [];
    plain.agent.onSessionEnd((_session, event) => {
      ends.push(event.reason);
    });
    await using(plain, async () => {
      const [fake] = await plain.call();
      await fake.close();
      await until("the end", () => ends.length === 1);
      assert.deepEqual(ends, ["link_closed"]);
    });
  });

  it("takes over a link the agent still holds (a half-open one)", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [old] = await firstLink(r.h);
      const session = r.sessions[0] as Session;
      const [again, answer] = await resumeLink(r.h); // the old link: no close seen yet
      assert.equal(answer["resume"], true);
      await until("resumed", () => entries(r, "resumed").length === 1);
      assert.equal(session.resumes, 1);
      assert.equal(session.closed, false);
      await until("the old link closed by the agent", () => old.closed);
      await settle();
      assert.equal(session.parked, false, "the old link closing must not park the resumed call");
      assert.deepEqual(entries(r, "end"), []);
      await again.send({ type: "user.text", epoch: 1, text: "still here" });
      await until("the event", () => entries(r, "user").length === 1);
    });
  });

  it("ends the parked calls when the server stops", async () => {
    const r = rig();
    await r.h.start();
    const [fake] = await firstLink(r.h);
    await fake.close();
    const session = r.sessions[0] as Session;
    await until("parked", () => session.parked);
    await r.h.stop();
    assert.deepEqual(entries(r, "end"), [["end", "link_closed"]]);
    assert.equal(session.parked, false);
  });
});

describe("resume: bookkeeping", () => {
  it("reports the last seq this side read on the lost link", async () => {
    const r = rig();
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h); // session.start was seq 1
      await fake.send({ type: "user.text", epoch: 1, text: "one" });
      await fake.send({ type: "user.text", epoch: 1, text: "two" });
      await until("both events", () => entries(r, "user").length === 2);
      await fake.close();
      const session = r.sessions[0] as Session;
      await until("parked", () => session.parked);
      await resumeLink(r.h, { previous_seq: 3 }); // the server's count: start, one, two
      await until("resumed", () => r.resumedEvents.length === 1);
      const event = r.resumedEvents[0] as Resumed;
      assert.deepEqual([event.previousSeq, event.seenSeq, event.seq], [3, 3, 4]);
      assert.equal(session.lastSeq, 4);
    });
  });

  it("counts parked calls against maxSessions", async () => {
    const r = rig({ maxSessions: 1 });
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      const session = r.sessions[0] as Session;
      await fake.close();
      await until("parked", () => session.parked);
      const [, answer] = await r.h.call({ session_id: "43" });
      assert.equal(answer["type"], "session.reject");
      assert.equal(answer["code"], "unavailable");
      // Once resumed it is one call again: the slot is not counted twice.
      await resumeLink(r.h);
      await until("resumed", () => !session.parked);
    });
  });
});

describe("resume: expiry", () => {
  it("ends a parked call that is not resumed once after the window", async () => {
    const r = rig({ resumeWindowMs: 150 });
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      await fake.close();
      const session = r.sessions[0] as Session;
      await until("parked", () => session.parked);
      assert.deepEqual(entries(r, "end"), []);
      await until("the end", () => entries(r, "end").length === 1);
      assert.deepEqual(entries(r, "end"), [["end", "link_closed"]]);
      assert.equal(session.parked, false);
      // Too late: the server's resumption is refused and nothing runs again.
      const [, answer] = await resumeLink(r.h);
      assert.equal(answer["code"], "unknown_session");
      await settle();
      assert.deepEqual(entries(r, "end"), [["end", "link_closed"]]);
      assert.deepEqual(entries(r, "resumed"), []);
    });
  });

  it("a timely resumption cancels the expiry", async () => {
    const r = rig({ resumeWindowMs: 150 });
    await using(r.h, async () => {
      const [fake] = await firstLink(r.h);
      await fake.close();
      const session = r.sessions[0] as Session;
      await until("parked", () => session.parked);
      await resumeLink(r.h);
      await until("resumed", () => !session.parked);
      await settle(450);
      assert.deepEqual(entries(r, "end"), [], "the old timer did not end the resumed call");
      assert.equal(session.closed, false);
    });
  });
});

describe("resume: wire types", () => {
  it("session.start parses resume", () => {
    const start = parseServerMessage(
      JSON.stringify({ seq: 57, t_ms: 41200, type: "session.start", ...START_BODY, resume: RESUME }),
    );
    assert.equal(start.type, "session.start");
    if (start.type === "session.start") {
      assert.deepEqual(start.resume, { attempt: 1, previousSeq: 7, downMs: 120 });
    }
    const first = parseServerMessage(JSON.stringify({ seq: 1, t_ms: 0, type: "session.start", ...START_BODY }));
    assert.equal(first.type === "session.start" && first.resume === undefined, true);
  });

  it("a malformed resume is a bad message", () => {
    for (const bad of ["yes", { attempt: 1 }, { attempt: "1", previous_seq: 1, down_ms: 1 }]) {
      assert.throws(
        () =>
          parseServerMessage(
            JSON.stringify({ seq: 1, t_ms: 0, type: "session.start", ...START_BODY, resume: bad }),
          ),
        ProtocolError,
      );
    }
  });

  it("session.accept carries the flag only when asked", () => {
    assert.equal("resume" in sessionAccept(1), false);
    assert.equal(sessionAccept(1, { resume: true })["resume"], true);
    assert.equal(ErrorCode.UnknownSession, "unknown_session");
  });
});
