/**
 * A reconnecting agent: `resume: true` keeps a call's state when its link drops.
 *
 * The brisvox server reopens a lost link of an agent that declared `resume` (backoff, at most
 * five attempts, a 15 s window) and sends `session.start` again with `resume`. The SDK re-attaches
 * the call to its parked `Session`: `session.data` is still there, `onSessionStart` does not run
 * again, nothing is greeted twice, and `onResumed` tells the app about the gap.
 *
 * - `remember` is a `side_effect` tool that keeps a note in `session.data`.
 * - `recall` is a `read_only` tool that reports the notes, how often the call started in this
 *   process (once, however often the link dropped) and the gaps `onResumed` saw.
 *
 * Run (from sdk/typescript, after `npm run build`):
 *
 *     node dist/examples/resumable.js --port 8765
 */

import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
// An application imports from "@brisvox/agent".
import { Agent, s } from "../src/index.js";

interface Gap {
  attempt: number;
  previous_seq: number;
  seen_seq: number;
  down_ms: number;
  seq: number;
}

export const agent = new Agent({
  name: "luigi",
  systemPrompt: "You are Luigi's host. Use remember to keep what the caller asks for, recall to repeat it.",
  greeting: "Hi, Luigi's here.",
  resume: true, // declare `resume` in session.accept: a lost link is taken up again
  resumeWindowMs: 15_000, // how long a lost call is kept for the server (the server's own window)
});

agent.onSessionStart((session) => {
  // Runs once per call, never again for a resumption.
  session.data["notes"] = [] as string[];
  session.data["starts"] = 1;
  session.data["gaps"] = [] as Gap[];
});

agent.tool({
  name: "remember",
  description: "Keep a note about the caller's wishes for the rest of the call.",
  kind: "side_effect",
  parameters: s.object({ note: s.string({ description: "What to remember." }) }),
  handler: ({ note }, ctx) => {
    const notes = ctx.session.data["notes"] as string[];
    notes.push(note);
    return `noted (${notes.length})`;
  },
});

agent.tool({
  name: "recall",
  description: "Repeat the notes of this call and what the link did meanwhile.",
  parameters: s.object({}),
  handler: (_args, ctx) => {
    const data = ctx.session.data;
    const gaps = [...(data["gaps"] as Gap[])];
    return {
      notes: [...(data["notes"] as string[])],
      starts: data["starts"],
      resumes: ctx.session.resumes,
      gaps,
      // The server's count continues across a resumption: the last message this agent read on
      // the lost link is the one the server says it wrote last (`previous_seq`), and the resuming
      // session.start is the message right after it.
      seq_continues: gaps.every((gap) => gap.seen_seq === gap.previous_seq && gap.seq === gap.previous_seq + 1),
    };
  },
});

// The link was lost and is back. `event.seq` continues the server's count after
// `event.previousSeq`; whatever the server could not deliver meanwhile arrives as `events.lost`.
agent.onResumed((session, event) => {
  (session.data["gaps"] as Gap[]).push({
    attempt: event.attempt,
    previous_seq: event.previousSeq,
    seen_seq: event.seenSeq,
    down_ms: event.downMs,
    seq: event.seq,
  });
  console.error(`call ${session.id}: back after ${event.downMs} ms (attempt ${event.attempt})`);
});

agent.onUserText(async (session, event) => {
  if (event.text.toLowerCase().includes("goodbye")) {
    await session.end({ say: "Goodbye, and thank you for calling!", reason: "caller said goodbye" });
  }
});

agent.onSessionEnd((session, event) => {
  console.error(`call ${session.id} over (${event.reason}) after ${session.resumes} resumption(s)`);
});

async function main(): Promise<void> {
  const { values } = parseArgs({
    options: { host: { type: "string", default: "127.0.0.1" }, port: { type: "string", default: "8765" } },
  });
  await agent.run({
    host: values.host ?? "127.0.0.1",
    port: Number(values.port ?? "8765"),
    // One line on stdout for scripts (and the Rust integration test) to read the port from.
    onReady: (server) => console.log(`LISTENING ${server.url}`),
  });
}

if (process.argv[1] !== undefined && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error: unknown) => {
    console.error(error);
    process.exit(1);
  });
}
