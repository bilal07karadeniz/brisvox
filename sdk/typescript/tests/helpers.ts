/** Shared test plumbing: an agent served on a free port, fake brisvox servers connected to it. */

import { Agent } from "../src/index.js";
import type { AgentOptions, AgentServer, Logger, Session } from "../src/index.js";
import { FakeServer } from "./fake-server.js";
import type { Wire } from "./fake-server.js";

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Let pending I/O and timers run. */
export function settle(ms = 60): Promise<void> {
  return sleep(ms);
}

/** Resolves when `condition()` holds; fails after `timeoutMs`. */
export async function until(what: string, condition: () => boolean, timeoutMs = 5000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!condition()) {
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await sleep(5);
  }
}

export interface Gate {
  promise: Promise<void>;
  open(): void;
  readonly isOpen: boolean;
}

export function gate(): Gate {
  let open!: () => void;
  let isOpen = false;
  const promise = new Promise<void>((resolve) => {
    open = () => {
      isOpen = true;
      resolve();
    };
  });
  return {
    promise,
    open,
    get isOpen() {
      return isOpen;
    },
  };
}

/** A logger that keeps what it is told, so a test can assert on warnings. */
export class CollectingLogger implements Logger {
  readonly lines: string[] = [];
  debug(message: string): void {
    this.lines.push(`debug: ${message}`);
  }
  info(message: string): void {
    this.lines.push(`info: ${message}`);
  }
  warn(message: string): void {
    this.lines.push(`warn: ${message}`);
  }
  error(message: string): void {
    this.lines.push(`error: ${message}`);
  }
  has(text: string): boolean {
    return this.lines.some((line) => line.includes(text));
  }
}

/** An agent served on a free loopback port, plus the fake servers connected to it. */
export class Harness {
  readonly agent: Agent;
  readonly logger = new CollectingLogger();
  server: AgentServer | undefined;
  readonly fakes: FakeServer[] = [];
  readonly sessions: Session[] = [];

  constructor(options: AgentOptions = {}) {
    this.agent = new Agent({ name: "luigi", logger: this.logger, ...options });
  }

  /** Start listening (`token`: the bearer token the agent requires). */
  async start(token?: string): Promise<this> {
    this.server = await this.agent.listen({ host: "127.0.0.1", port: 0, token: token ?? null });
    return this;
  }

  get url(): string {
    if (this.server === undefined) {
      throw new Error("not started");
    }
    return this.server.url;
  }

  /** Connect a fake server, send `session.start` and return it with the agent's answer. */
  async call(overrides: Wire = {}, token?: string): Promise<[FakeServer, Wire]> {
    const fake = await FakeServer.connect(this.url, token);
    this.fakes.push(fake);
    const answer = await fake.start(overrides);
    return [fake, answer];
  }

  async stop(): Promise<void> {
    for (const fake of this.fakes) {
      fake.ws.terminate();
    }
    await this.server?.close();
  }
}

/** Start `h`, run `body`, stop `h`. */
export async function using(h: Harness, body: () => Promise<void>, token?: string): Promise<void> {
  await h.start(token);
  try {
    await body();
  } finally {
    await h.stop();
  }
}

/** Run `body` with a started harness and stop it afterwards. */
export async function withHarness(
  options: AgentOptions,
  setup: (h: Harness) => void,
  body: (h: Harness) => Promise<void>,
  token?: string,
): Promise<void> {
  const h = new Harness(options);
  setup(h);
  await h.start(token);
  try {
    await body(h);
  } finally {
    await h.stop();
  }
}

export function toolCall(
  id: number,
  name: string,
  args: Record<string, unknown>,
  kind: "read_only" | "side_effect" = "read_only",
): Wire {
  return {
    type: "tool.call",
    id,
    call_id: `call-${id}`,
    name,
    arguments: args,
    kind,
    epoch: 1,
    idempotency_key: "0123456789abcdef",
    timeout_ms: 5000,
  };
}
