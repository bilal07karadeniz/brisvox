// Types for brisvox-call.js.

export type CallState = "idle" | "connecting" | "connected" | "ended" | "failed";
export type TransportName = "webrtc" | "websocket";
export type TransportChoice = "auto" | TransportName;

/** Why a call failed. See `BrisvoxCallError`. */
export type ErrorCode =
  | "config"
  | "unsupported"
  | "microphone"
  | "token"
  | "signaling"
  | "webrtc"
  | "websocket"
  | "timeout"
  | "connection";

export class BrisvoxCallError extends Error {
  constructor(message: string, code: ErrorCode, details?: { status?: number; cause?: unknown });
  readonly name: "BrisvoxCallError";
  readonly code: ErrorCode;
  /** The HTTP status, when the server answered one (for example 401, 403, 503). */
  readonly status?: number;
}

/** Browser objects the module uses. Every field defaults to the global; set one to use another (tests, an iframe). */
export interface Platform {
  fetch?: typeof fetch;
  WebSocket?: typeof WebSocket;
  RTCPeerConnection?: typeof RTCPeerConnection;
  AudioContext?: typeof AudioContext;
  AudioWorkletNode?: typeof AudioWorkletNode;
  MediaStream?: typeof MediaStream;
  Audio?: typeof Audio;
  getUserMedia?: (constraints: MediaStreamConstraints) => Promise<MediaStream>;
}

export interface BrisvoxCallOptions {
  /** The server's signaling address, `https://voice.example.com` (or `http://localhost:8088`). */
  serverUrl: string;
  /** A fixed bearer token (the development token, or one session token for one call). */
  token?: string;
  /**
   * Returns a bearer token (a session token your backend minted). Called for every connection
   * attempt, because session tokens are single use: once for WebRTC and again for the WebSocket
   * fallback. Do not combine with `token`.
   */
  tokenProvider?: () => string | undefined | Promise<string | undefined>;
  /** `auto` (default): WebRTC, then the WebSocket if WebRTC cannot connect. */
  transport?: TransportChoice;
  /** How long WebRTC gets to connect, after the server accepted its offer, before the WebSocket takes over. Default 4000. */
  fallbackAfterMs?: number;
  /** Bound on the WHIP request. Default 10000. */
  signalingTimeoutMs?: number;
  /** With `transport: "webrtc"` (no fallback): how long the connection may take. Default 20000. */
  connectTimeoutMs?: number;
  /** From opening the WebSocket to the server's `started`. Default 8000. */
  startTimeoutMs?: number;
  /** The WHIP path on `serverUrl`. Default `/whip`; `null` or `""` turns WebRTC off. */
  whipPath?: string | null;
  /** The WebSocket path on `serverUrl`. Default `/ws`; `null` or `""` turns the fallback off. */
  wsPath?: string | null;
  /** Merged over the microphone constraints (default: echo cancellation, noise suppression, auto gain, mono). */
  audio?: MediaTrackConstraints;
  /** The element that plays the assistant over WebRTC. Default: a new `Audio()`. */
  audioElement?: HTMLMediaElement;
  /** Load the AudioWorklet code from this URL instead of a Blob URL (for a strict Content-Security-Policy). `WORKLET_SOURCE` is its content. */
  workletUrl?: string;
  /** Extra `start.metadata` for the WebSocket protocol (string values). */
  metadata?: Record<string, string>;
  /** Receives every event (see `CallEvent`). */
  onEvent?: (event: CallEvent) => void;
  platform?: Platform;
}

export type CallEvent =
  /** The call changed state. `transport` is set while it is connected (or reconnecting); `reason` and `error` on `ended` and `failed`. */
  | { type: "state"; state: CallState; previous: CallState; transport?: TransportName; reason?: string; error?: BrisvoxCallError }
  /** A transport carries the call. `fallback` is true when WebRTC was tried first and failed; `reason` says why. */
  | { type: "transport"; transport: TransportName; fallback: boolean; reason?: string }
  /** WebRTC could not connect; the WebSocket is next. */
  | { type: "fallback"; from: "webrtc"; to: "websocket"; reason: string }
  /** The WebSocket call is open (the server's `started`). */
  | { type: "started"; callId?: string; sampleRate: number; encoding: string }
  /** Barge-in: the server told the client to drop all audio it has not played yet (WebSocket transport; WebRTC does this on the server). */
  | { type: "clear"; epoch?: number }
  /** The server's `mark` (WebSocket transport); the client echoes it once the audio before it has played. */
  | { type: "mark"; id: string }
  /** The server ended the call (`ended` or `shutdown`). */
  | { type: "stop"; reason: string }
  /** A server `error` message. Fatal ones are followed by the end of the call. */
  | { type: "notice"; code: string; message: string; fatal: boolean }
  /** A server message of a type this version does not know (transcripts, if a server sends them). */
  | { type: "message"; message: Record<string, unknown> }
  /** The microphone was muted or unmuted. */
  | { type: "muted"; muted: boolean }
  /** The call failed. Followed by `state` `failed`. */
  | { type: "error"; error: BrisvoxCallError; code: ErrorCode; message: string; fatal: true }
  /** A line for a connection log. */
  | { type: "log"; text: string };

export class BrisvoxCall {
  constructor(options: BrisvoxCallOptions);
  readonly state: CallState;
  readonly transport: TransportName | null;
  readonly muted: boolean;
  /** The call's AudioContext (null before `start()` and after `hangup()`). */
  readonly audioContext: AudioContext | null;
  /** Opens the microphone and connects. Call it from a click handler. Rejects with `BrisvoxCallError`, or an `AbortError` if `hangup()` came first. */
  start(): Promise<this>;
  /** Ends the call and frees everything. Safe to call at any time, more than once. */
  hangup(): Promise<void>;
  setMuted(muted: boolean): void;
  /** Microphone and assistant levels, 0 to 1, for a meter. */
  getLevels(): { mic: number; bot: number };
  /** Adds a listener for events; returns a function that removes it. */
  subscribe(listener: (event: CallEvent) => void): () => void;
}

/** `new BrisvoxCall(options)` followed by `start()`. */
export function connect(options: BrisvoxCallOptions): Promise<BrisvoxCall>;

/** The AudioWorklet module the WebSocket transport loads. */
export const WORKLET_SOURCE: string;
