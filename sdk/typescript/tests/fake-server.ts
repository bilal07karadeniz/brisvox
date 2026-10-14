/**
 * A fake brisvox server for the tests: connects out to an agent like the real one does and
 * speaks the server side of docs/AGENT_PROTOCOL.md (envelope with `seq` and `t_ms`,
 * `session.start` first, `command.result` for commands that carry an `id`).
 */

import { WebSocket } from "ws";
import type { RawData } from "ws";

/** A decoded agent message. */
export type Wire = Record<string, any>;

export const START_BODY = {
  protocol: "brisvox.agent",
  versions: [1],
  session_id: "42",
  agent: "luigi",
  capabilities: { transfer: false, tools: true },
};

export class FakeServer {
  seq = 0;
  readonly received: Wire[] = [];
  readonly #queue: Wire[] = [];
  readonly #waiters: ((message: Wire | null) => void)[] = [];
  closeCode: number | undefined;
  closed = false;

  private constructor(readonly ws: WebSocket) {
    ws.on("message", (data: RawData) => {
      const message = JSON.parse(data.toString("utf8")) as Wire;
      this.received.push(message);
      const waiter = this.#waiters.shift();
      if (waiter) {
        waiter(message);
      } else {
        this.#queue.push(message);
      }
    });
    ws.on("close", (code) => {
      this.closed = true;
      this.closeCode = code;
      for (const waiter of this.#waiters.splice(0)) {
        waiter(null);
      }
    });
    ws.on("error", () => undefined);
  }

  /** Connect to an agent; rejects with the HTTP status when the upgrade is refused. */
  static async connect(url: string, token?: string, authorization?: string): Promise<FakeServer> {
    const headers: Record<string, string> = {};
    if (authorization !== undefined) {
      headers["Authorization"] = authorization;
    } else if (token !== undefined) {
      headers["Authorization"] = `Bearer ${token}`;
    }
    const ws = new WebSocket(url, { headers, handshakeTimeout: 5000 });
    await new Promise<void>((resolve, reject) => {
      ws.once("open", () => resolve());
      ws.once("unexpected-response", (_request, response) => {
        reject(new HttpRefusal(response.statusCode ?? 0));
        response.resume();
      });
      ws.once("error", reject);
    });
    return new FakeServer(ws);
  }

  /** Send a server message with the envelope; returns its `seq`. */
  async send(body: Wire): Promise<number> {
    this.seq += 1;
    await this.raw(JSON.stringify({ seq: this.seq, t_ms: this.seq * 10, ...body }));
    return this.seq;
  }

  /**
   * Send many server messages in one TCP write (the socket is corked), so the agent's `ws` parses
   * them in one synchronous run, as after an event-loop stall. Returns the `seq` of each.
   */
  async burst(bodies: Wire[]): Promise<number[]> {
    // `_socket` is ws's (undocumented) net.Socket; cork() batches the writes into one chunk.
    const socket = (this.ws as unknown as { _socket: { cork(): void; uncork(): void } })._socket;
    const seqs: number[] = [];
    const sends: Promise<void>[] = [];
    socket.cork();
    try {
      for (const body of bodies) {
        this.seq += 1;
        seqs.push(this.seq);
        sends.push(this.raw(JSON.stringify({ seq: this.seq, t_ms: this.seq * 10, ...body })));
      }
    } finally {
      socket.uncork();
    }
    await Promise.all(sends);
    return seqs;
  }

  /** Send a raw frame. */
  raw(data: string | Buffer): Promise<void> {
    return new Promise((resolve, reject) => {
      this.ws.send(data, (error) => (error ? reject(error) : resolve()));
    });
  }

  /** Send `session.start` and return the agent's answer. */
  async start(overrides: Wire = {}): Promise<Wire> {
    await this.send({ type: "session.start", ...START_BODY, ...overrides });
    return await this.recv();
  }

  /** The next message from the agent. */
  recv(timeoutMs = 5000): Promise<Wire> {
    const queued = this.#queue.shift();
    if (queued) {
      return Promise.resolve(queued);
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        const at = this.#waiters.indexOf(waiter);
        if (at >= 0) {
          this.#waiters.splice(at, 1);
        }
        reject(new Error(`no message from the agent within ${timeoutMs} ms`));
      }, timeoutMs);
      const waiter = (message: Wire | null): void => {
        clearTimeout(timer);
        if (message === null) {
          reject(new Error("the agent closed the link"));
        } else {
          resolve(message);
        }
      };
      this.#waiters.push(waiter);
    });
  }

  /** The next message of `type` (others are skipped, but kept in `received`). */
  async recvType(type: string, timeoutMs = 5000): Promise<Wire> {
    for (;;) {
      const message = await this.recv(timeoutMs);
      if (message["type"] === type) {
        return message;
      }
    }
  }

  async ok(commandId: string): Promise<void> {
    await this.send({ type: "command.result", id: commandId, ok: true });
  }

  async fail(commandId: string, code: string, message = "nope"): Promise<void> {
    await this.send({ type: "command.result", id: commandId, ok: false, error: { code, message } });
  }

  /** Receive the next `type` command and answer it ok. */
  async ackNext(type: string): Promise<Wire> {
    const command = await this.recvType(type);
    if (typeof command["id"] === "string") {
      await this.ok(command["id"]);
    }
    return command;
  }

  /** Close the link and wait for it. */
  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    await new Promise<void>((resolve) => {
      this.ws.once("close", () => resolve());
      this.ws.close();
    });
  }
}

/** The WebSocket upgrade was answered with an HTTP status instead. */
export class HttpRefusal extends Error {
  constructor(readonly status: number) {
    super(`HTTP ${status}`);
  }
}
