// `brisvox` is the short name of the agent SDK: everything @brisvox/agent exports (Node 20+).
// The browser client is `brisvox/web`; it is a separate entry because it has no Node dependencies
// and a page should not pull in the agent's WebSocket server.
export * from "@brisvox/agent";
