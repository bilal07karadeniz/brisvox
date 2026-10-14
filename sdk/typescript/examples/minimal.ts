/**
 * The smallest useful agent: the TypeScript twin of the Rust "minimal agent" in
 * docs/AGENT_PROTOCOL.md. It answers one question itself and offers one tool.
 *
 * Run it, then point the brisvox server at it:
 *
 *     node dist/examples/minimal.js                  # listens on ws://127.0.0.1:8765
 *     BRISVOX_AGENT_URL=ws://127.0.0.1:8765 brisvox-server ...
 */

import path from "node:path";
import { fileURLToPath } from "node:url";
// An application imports from "@brisvox/agent".
import { Agent, s } from "../src/index.js";

export const agent = new Agent({ name: "minimal", systemPrompt: "You are a helpful phone assistant." });

const FACTS: Record<string, string> = { hours: "We open at noon.", address: "12 Harbour Street." };

agent.tool({
  name: "lookup",
  description: "Look up a short fact about the restaurant (hours, address).",
  parameters: s.object({ topic: s.string() }),
  handler: ({ topic }) => FACTS[topic.toLowerCase()] ?? "I have nothing on that.",
});

// React to the user's turn. `re` (the event's seq) is attached automatically.
agent.onUserText(async (session, event) => {
  if (event.text.toLowerCase().includes("hours")) {
    await session.say("We open at noon.");
  }
});

if (process.argv[1] !== undefined && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await agent.run({ port: 8765 });
}
