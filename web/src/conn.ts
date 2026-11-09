// The WebSocket to the server: JSON messages both ways, binary frames for voice.
// Reconnects by itself and signs back in with the saved session token.

import type { ClientMsg, ServerMsg } from "./proto";

const TOKEN_KEY = "celerity.token";
const PING_MS = 30_000;

export type ConnState = "connecting" | "open" | "closed";

export class Conn {
  private ws: WebSocket | null = null;
  private retry = 0;
  private pinger: number | undefined;
  private wanted = false;
  /** Called for every message from the server. */
  onMessage: (msg: ServerMsg) => void = () => {};
  /** Called for every binary (voice) frame. */
  onBinary: (data: Uint8Array) => void = () => {};
  onState: (s: ConnState) => void = () => {};
  /** First message to send after (re)connecting; usually Resume with the saved token. */
  hello: () => ClientMsg | null = () => null;

  static get token(): string | null {
    try { return localStorage.getItem(TOKEN_KEY); } catch { return null; }
  }
  static set token(t: string | null) {
    try {
      if (t) localStorage.setItem(TOKEN_KEY, t);
      else localStorage.removeItem(TOKEN_KEY);
    } catch { /* private browsing: stay signed in for this visit only */ }
  }

  connect(): void {
    this.wanted = true;
    if (this.ws && this.ws.readyState <= WebSocket.OPEN) return;
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${scheme}://${location.host}/ws`);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.onState("connecting");
    ws.onopen = () => {
      this.retry = 0;
      this.onState("open");
      const first = this.hello();
      if (first) this.send(first);
      clearInterval(this.pinger);
      this.pinger = window.setInterval(() => this.send({ op: "Ping" }), PING_MS);
    };
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        try { this.onMessage(JSON.parse(ev.data) as ServerMsg); } catch (e) { console.warn("bad message", e); }
      } else {
        this.onBinary(new Uint8Array(ev.data as ArrayBuffer));
      }
    };
    ws.onclose = () => {
      clearInterval(this.pinger);
      if (this.ws === ws) this.ws = null;
      this.onState("closed");
      if (!this.wanted) return;
      // Back off: 1 s, 2 s, 4 s ... up to 30 s.
      const delay = Math.min(30_000, 1000 * 2 ** this.retry++);
      setTimeout(() => this.connect(), delay);
    };
  }

  /** Reconnect straight away (for example when the phone wakes the app up). */
  nudge(): void {
    if (!this.ws || this.ws.readyState > WebSocket.OPEN) {
      this.retry = 0;
      this.connect();
    }
  }

  close(): void {
    this.wanted = false;
    this.ws?.close();
  }

  get open(): boolean {
    return this.ws?.readyState === WebSocket.OPEN;
  }

  send(msg: ClientMsg): void {
    if (this.open) this.ws!.send(JSON.stringify(msg));
  }

  sendBinary(data: Uint8Array): void {
    // Drop audio rather than queue it if the connection is backed up.
    if (this.open && this.ws!.bufferedAmount < 64_000) this.ws!.send(data);
  }
}
