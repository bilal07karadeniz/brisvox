# brisvox (npm)

```sh
npm i brisvox
```

The JavaScript and TypeScript SDKs of [brisvox](https://github.com/bilal07karadeniz/brisvox), a Rust
engine for real-time voice agents, under the short package name. `brisvox` has no code of its own:
it depends on [`@brisvox/agent`](https://www.npmjs.com/package/@brisvox/agent) and
[`@brisvox/web`](https://www.npmjs.com/package/@brisvox/web) (the same release, pinned exactly) and
re-exports them. Install either form; the SDK references use the scoped names.

| Import | Is | For |
|---|---|---|
| `brisvox` | `@brisvox/agent` | The agent side, in Node 20+: the application logic of a call (tools, events, commands) |
| `brisvox/web` | `@brisvox/web` | The browser side: a "Talk to us" button, WebRTC with a WebSocket fallback |

```ts
// Node: the agent the brisvox server connects out to, one WebSocket per call.
import { Agent, s } from "brisvox";

const agent = new Agent({ name: "luigi", systemPrompt: "You are Luigi's host.", greeting: "Hi!" });

agent.tool({
  name: "opening_hours",
  description: "Opening hours of the restaurant on a weekday.",
  parameters: s.object({ day: s.enum(["mon", "tue", "wed", "thu", "fri", "sat", "sun"]) }),
  handler: ({ day }) => (day === "mon" ? "Closed on Monday." : "12:00 to 22:00."),
});

await agent.run({ port: 8765 }); // then: BRISVOX_AGENT_URL=ws://127.0.0.1:8765
```

```js
// Browser: the call from a web page (bundle it, or import it from your own server).
import { connect } from "brisvox/web";

// Your backend mints a single-use session token per connection attempt.
const tokenProvider = () => fetch("/api/voice-token").then((r) => r.text());
const call = await connect({ serverUrl: "https://voice.example.com", tokenProvider });
```

The server is a separate program: the prebuilt `brisvox-server` binary, the container image, or the
`brisvox` crate for a server with your own Rust tools. Pages that need no bundler can copy
`brisvox-call.js` from `@brisvox/web`. Do not import plain `brisvox` in a browser: it is the Node
agent SDK (a WebSocket server); the browser entry is `brisvox/web`.

* [Documentation](https://bilal07karadeniz.github.io/brisvox/)
* [Agent SDK](https://github.com/bilal07karadeniz/brisvox/tree/main/sdk/typescript) and
  [browser client](https://github.com/bilal07karadeniz/brisvox/tree/main/sdk/web)

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT
license ([LICENSE-MIT](LICENSE-MIT)) at your option.
