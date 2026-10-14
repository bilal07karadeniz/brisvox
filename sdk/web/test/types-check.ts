// Not run by the test runner: `tsc --noEmit --strict --module nodenext --lib es2022,dom types-check.ts`
// proves brisvox-call.d.ts accepts the documented usage.
import { BrisvoxCall, BrisvoxCallError, connect, type CallEvent } from "../brisvox-call.js";

const call = new BrisvoxCall({
  serverUrl: "https://voice.example.com",
  tokenProvider: async () => "token",
  transport: "auto",
  fallbackAfterMs: 4000,
  audio: { deviceId: "default" },
  onEvent: (event: CallEvent) => {
    switch (event.type) {
      case "state": console.log(event.state, event.previous, event.transport); break;
      case "transport": console.log(event.transport, event.fallback); break;
      case "error": console.log(event.error.code, event.error.status); break;
      case "clear": console.log(event.epoch); break;
      default: break;
    }
  },
});
const stop = call.subscribe(() => {});
stop();
void call.start().then((c) => c.hangup());
const levels: { mic: number; bot: number } = call.getLevels();
void levels;
void connect({ serverUrl: "http://localhost:8088", token: "dev" });
void (new BrisvoxCallError("x", "config") instanceof Error);
