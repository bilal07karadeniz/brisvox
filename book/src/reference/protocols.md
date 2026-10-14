# Protocols

brisvox speaks three protocols to the outside, each with its own reference page:

| Protocol | For | Direction | Reference |
|---|---|---|---|
| Websocket protocol (version 1) | Browsers, apps and test clients: one websocket connection is one call. Binary frames carry audio, text frames carry JSON control messages (`start`, `started`, `clear`, `mark`, `dtmf`, `stop`, ...). | Client connects to the server (`BRISVOX_WS_ADDR`) | [WebSocket protocol](websocket-protocol.md) |
| Agent protocol (version 1) | Your application logic in another process: tools, events, commands. The Python and TypeScript SDKs implement it. | The server connects **out** to your agent (`BRISVOX_AGENT_URL`) | [Agent protocol](agent-protocol.md) |
| RTVI | Pipecat's client SDKs talking to brisvox. | Client connects to the server (`BRISVOX_RTVI_ADDR`) | [RTVI](rtvi.md) |

Carrier and browser transports have their own pages: [Twilio Media Streams](../telephony/twilio.md),
[Telnyx media streaming](../telephony/telnyx.md), [SIP trunk](../telephony/sip.md) and
[WebRTC](providers/webrtc.md). All listeners and their settings are in
[Operations](../operations.md).
