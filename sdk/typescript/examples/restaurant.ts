/**
 * A restaurant host: typed tools, per-call setup, event handlers and every command.
 *
 * - `opening_hours` is a `read_only` tool: the engine may run it speculatively and cancels it
 *   when the caller barges in.
 * - `book_table` is a `side_effect` tool: it never runs twice for one call, and
 *   `ctx.idempotencyKey` goes to the booking system so even a retried request cannot book twice.
 * - `onSessionStart` personalises the call (and shows how to decline one).
 * - The handlers show `say`, `end`, `transfer`, `updateContext` and `setTools`.
 *
 * Run (from sdk/typescript, after `npm run build`):
 *
 *     node dist/examples/restaurant.js --port 8765
 *     BRISVOX_AGENT_URL=ws://127.0.0.1:8765 BRISVOX_AGENT_TOKEN=... brisvox-server ...
 *
 * Set `BRISVOX_AGENT_TOKEN` for the agent as well to require the bearer token. Property names
 * of a tool schema are the names the LLM sees, so they are snake_case here.
 */

import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
// An application imports from "@brisvox/agent".
import { Agent, CommandError, ErrorCode, Reject, ToolError, s } from "../src/index.js";
import type { Accept, Session } from "../src/index.js";

export const agent = new Agent({
  name: "luigi",
  systemPrompt:
    "You are Luigi's host on the phone. Be brief and warm. " +
    "Use the tools for opening hours and bookings; never invent a booking.",
  greeting: "Hi, Luigi's here. How can I help?",
});

const WEEKDAYS = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] as const;

/** Bookings by idempotency key (a stand-in for the booking system's own deduplication). */
const BOOKED = new Map<string, { time: string; party_size: number; name: string | null }>();

agent.tool({
  name: "opening_hours",
  description: "Opening hours of the restaurant on a weekday.",
  parameters: s.object({ day: s.enum(WEEKDAYS, { description: "The weekday, as three letters." }) }),
  handler: ({ day }) => (day === "mon" ? "Closed on Monday." : "12:00 to 22:00."),
});

agent.tool({
  name: "book_table",
  description: "Book a table.",
  kind: "side_effect",
  timeoutMs: 4000,
  parameters: s.object({
    time: s.string({ description: "The time as HH:MM, 24 hours." }),
    party_size: s.optional(s.integer({ description: "How many people (1 to 12)." }), 2),
    contact: s.optional(
      s.object(
        {
          name: s.string({ description: "The caller's name." }),
          phone: s.optional(s.string({ description: "A number in any format." })),
        },
        { description: "Who to call back, if the caller gave a name." },
      ),
    ),
  }),
  handler: ({ time, party_size, contact }, ctx) => {
    if (party_size < 1 || party_size > 12) {
      throw new ToolError("We seat parties of 1 to 12."); // the LLM reads this and apologises
    }
    // The key is stable for this tool call: a repeated request books once.
    let booking = BOOKED.get(ctx.idempotencyKey);
    if (booking === undefined) {
      booking = { time, party_size, name: contact?.name ?? null };
      BOOKED.set(ctx.idempotencyKey, booking);
    }
    return { status: "booked", ...booking };
  },
});

agent.onSessionStart((session): Accept | undefined => {
  // Runs inside the handshake budget (about 1.5 s): keep it fast.
  if (session.agentName === "closed-for-private-event") {
    throw new Reject(ErrorCode.Unavailable, "no calls tonight");
  }
  session.data["turns"] = 0;
  if (!session.capabilities.transfer) {
    console.error(`call ${session.id}: no transfer on this server`);
  }
  return undefined; // keep the agent's defaults; return { ... } to change them for this call
});

agent.onUserText(async (session: Session, event) => {
  const text = event.text.toLowerCase();
  session.data["turns"] = Number(session.data["turns"] ?? 0) + 1;
  if (text.includes("goodbye") || text.split(/\s+/).includes("bye")) {
    await session.end({ say: "Goodbye, and thank you for calling!", reason: "caller said goodbye" });
  } else if (text.includes("manager") || text.includes("human")) {
    if (session.capabilities.transfer) {
      try {
        await session.transfer("+4930123456789", {
          mode: "warm",
          summary: "Caller asked for a person.",
          id: `handoff-${session.id}`, // the same id on a retry means no second transfer
        });
      } catch (error) {
        if (!(error instanceof CommandError)) {
          throw error;
        }
        console.error(`transfer failed: ${error.message}`);
        await session.say("I could not reach the manager, sorry. Can I help instead?");
      }
    } else {
      await session.updateContext({
        append: [
          {
            role: "system",
            text: "The caller wants a person but transfer is unavailable; offer to take a message.",
          },
        ],
      });
    }
  } else if (text.includes("just chat")) {
    await session.setTools([]); // no tools from the next response on
  } else if (text.includes("tools back")) {
    await session.setTools(null);
  }
});

agent.onTurn((session, event) => {
  if (event.type === "turn.metrics" && event.metrics.responseMs !== undefined) {
    console.error(`call ${session.id} epoch ${event.epoch}: first audio after ${Math.round(event.metrics.responseMs)} ms`);
  }
});

agent.onSessionEnd((session, event) => {
  console.error(`call ${session.id} over (${event.reason}) after ${String(session.data["turns"])} user turns`);
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
