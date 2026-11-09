// Voice over the WebSocket (browsers can't send UDP), encrypted the same way as the
// desktop app (crates/proto/src/voice.rs): ChaCha20-Poly1305 with a per-session key,
// nonce = [direction, 0, 0, 0, counter as u64 big-endian].

import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { Codec, FRAME, SAMPLE_RATE } from "./opus";
import type { Decoder, Encoder } from "./opus";

const SEALED_UP = 0x05;
const SEALED_DOWN = 0x06;
const AUDIO_DOWN = 0x04;

export function hexToBytes(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function nonce(direction: 0 | 1, counter: bigint): Uint8Array {
  const n = new Uint8Array(12);
  n[0] = direction;
  new DataView(n.buffer).setBigUint64(4, counter);
  return n;
}

/** Encrypt one audio frame (sequence number + Opus data). */
export function seal(key: Uint8Array, direction: 0 | 1, counter: bigint, seq: number, opus: Uint8Array): Uint8Array {
  const plain = new Uint8Array(2 + opus.length);
  new DataView(plain.buffer).setUint16(0, seq);
  plain.set(opus, 2);
  return chacha20poly1305(key, nonce(direction, counter)).encrypt(plain);
}

/** Decrypt one audio frame; null if it was tampered with. */
export function open(key: Uint8Array, direction: 0 | 1, counter: bigint, sealed: Uint8Array): { seq: number; opus: Uint8Array } | null {
  try {
    const plain = chacha20poly1305(key, nonce(direction, counter)).decrypt(sealed);
    if (plain.length < 3) return null;
    return { seq: new DataView(plain.buffer, plain.byteOffset).getUint16(0), opus: plain.subarray(2) };
  } catch {
    return null;
  }
}

/** [0x05][token: 16][counter: u64][sealed] */
export function sealedUp(token: Uint8Array, counter: bigint, sealed: Uint8Array): Uint8Array {
  const out = new Uint8Array(1 + 16 + 8 + sealed.length);
  out[0] = SEALED_UP;
  out.set(token, 1);
  new DataView(out.buffer).setBigUint64(17, counter);
  out.set(sealed, 25);
  return out;
}

// The two audio worklets, kept as source so the build stays a single script.
const WORKLETS = `
class Capture extends AudioWorkletProcessor {
  constructor(o) { super(); this.ratio = o.processorOptions.ratio; this.buf = new Float32Array(${FRAME}); this.n = 0; this.pos = 0; this.prev = 0; }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (!ch) return true;
    // Resample from the device rate to 48 kHz (linear; ratio is 1 on most devices).
    for (let i = 0; i < ch.length; i++) {
      const s = ch[i];
      while (this.pos < 1) {
        this.buf[this.n++] = this.prev + (s - this.prev) * this.pos;
        if (this.n === ${FRAME}) { this.port.postMessage(this.buf, [this.buf.buffer]); this.buf = new Float32Array(${FRAME}); this.n = 0; }
        this.pos += this.ratio;
      }
      this.pos -= 1;
      this.prev = s;
    }
    return true;
  }
}
class Player extends AudioWorkletProcessor {
  constructor(o) {
    super();
    this.step = o.processorOptions.step; // 48 kHz samples per output sample
    this.q = new Map(); // ssrc -> {frames: Float32Array[], off, primed}
    this.port.onmessage = (e) => {
      const { ssrc, pcm, clear } = e.data;
      if (clear) { this.q.clear(); return; }
      let s = this.q.get(ssrc);
      if (!s) this.q.set(ssrc, (s = { frames: [], off: 0, primed: false }));
      s.frames.push(pcm);
      while (s.frames.length > 12) { s.frames.shift(); s.off = 0; } // never more than ~240 ms behind
    };
  }
  process(_, outputs) {
    const out = outputs[0][0];
    out.fill(0);
    for (const [ssrc, s] of this.q) {
      if (!s.primed) { if (s.frames.length >= 3) s.primed = true; else continue; } // ~60 ms cushion
      for (let i = 0; i < out.length; i++) {
        let f = s.frames[0];
        if (!f) { s.primed = false; break; }
        const k = s.off | 0;
        const a = f[k], b = k + 1 < f.length ? f[k + 1] : (s.frames[1] ? s.frames[1][0] : a);
        out[i] += a + (b - a) * (s.off - k);
        s.off += this.step;
        if (s.off >= f.length) { s.off -= f.length; s.frames.shift(); }
      }
      if (!s.frames.length && !s.primed) this.q.delete(ssrc);
    }
    for (let c = 1; c < outputs[0].length; c++) outputs[0][c].set(out);
    for (let i = 0; i < out.length; i++) out[i] = Math.max(-1, Math.min(1, out[i]));
    return true;
  }
}
registerProcessor("celerity-capture", Capture);
registerProcessor("celerity-player", Player);
`;

/** Below this loudness (RMS) the mic counts as quiet... */
const SPEECH_RMS = 0.012;
/** ...and audio keeps flowing this many frames after speech stops (300 ms). */
const HANGOVER = 15;

export interface VoiceEvents {
  send(packet: Uint8Array): void;
  /** Someone's audio arrived (for the speaking indicator). */
  heard(ssrc: number): void;
  /** You started or stopped sending audio. */
  speaking(on: boolean): void;
}

export class VoiceSession {
  private ctx: AudioContext;
  private stream: MediaStream;
  private capture: AudioWorkletNode | null = null;
  private player: AudioWorkletNode | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private encoder: Encoder | null = null;
  private decoders = new Map<number, Decoder>();
  private token = new Uint8Array(16);
  private key = new Uint8Array(32);
  private counter = 1n;
  private seq = 0;
  private hangover = 0;
  private codec: Codec | null = null;
  muted = false;
  deafened = false;
  /** Last time (ms) we heard each ssrc. */
  lastHeard = new Map<number, number>();
  codecName = "";
  /** For troubleshooting: frames sent and decoded, and the loudness of the last one heard. */
  stats = { sent: 0, decoded: 0, level: 0, peak: 0 };

  private constructor(ctx: AudioContext, stream: MediaStream, private events: VoiceEvents) {
    this.ctx = ctx;
    this.stream = stream;
  }

  /** Ask for the microphone (call from a tap). Throws with a readable message if refused. */
  static async prepare(events: VoiceEvents): Promise<VoiceSession> {
    if (!navigator.mediaDevices?.getUserMedia) {
      throw new Error(
        location.protocol === "https:" ? "This browser can't use a microphone." : "Voice needs a secure (https) address.",
      );
    }
    let ctx: AudioContext;
    try {
      ctx = new AudioContext({ sampleRate: SAMPLE_RATE, latencyHint: "interactive" });
    } catch {
      ctx = new AudioContext({ latencyHint: "interactive" });
    }
    void ctx.resume();
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true, channelCount: 1 },
      });
    } catch (e) {
      void ctx.close();
      throw new Error(`Microphone not available: ${(e as Error).message || e}`);
    }
    return new VoiceSession(ctx, stream, events);
  }

  /** The server said where to send audio: start. */
  async start(token: string, key: string, wasmUrl: string): Promise<void> {
    this.token = hexToBytes(token) as Uint8Array<ArrayBuffer>;
    this.key = hexToBytes(key) as Uint8Array<ArrayBuffer>;
    // "celerity.codec" = "wasm" forces the WebAssembly codec (what iPhones use), for testing.
    let force: "wasm" | undefined;
    try { force = localStorage.getItem("celerity.codec") === "wasm" ? "wasm" : undefined; } catch { /* ignore */ }
    this.codec = await Codec.create(wasmUrl, force);
    this.codecName = this.codec.description;
    const url = URL.createObjectURL(new Blob([WORKLETS], { type: "text/javascript" }));
    await this.ctx.audioWorklet.addModule(url);
    URL.revokeObjectURL(url);
    const rate = this.ctx.sampleRate;
    this.capture = new AudioWorkletNode(this.ctx, "celerity-capture", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      processorOptions: { ratio: rate / SAMPLE_RATE },
    });
    // Some browsers only run nodes that lead to the speakers: connect it through silence.
    const silent = this.ctx.createGain();
    silent.gain.value = 0;
    this.capture.connect(silent).connect(this.ctx.destination);
    this.player = new AudioWorkletNode(this.ctx, "celerity-player", {
      numberOfInputs: 0,
      outputChannelCount: [2],
      processorOptions: { step: SAMPLE_RATE / rate },
    });
    this.player.connect(this.ctx.destination);
    this.source = this.ctx.createMediaStreamSource(this.stream);
    this.source.connect(this.capture);
    this.encoder = this.codec.encoder((opus) => this.sendOpus(opus));
    this.capture.port.onmessage = (e) => this.onFrame(e.data as Float32Array);
  }

  private onFrame(frame: Float32Array): void {
    if (this.muted || this.deafened || !this.encoder) return;
    let sum = 0;
    for (let i = 0; i < frame.length; i++) sum += frame[i] * frame[i];
    const loud = Math.sqrt(sum / frame.length) > SPEECH_RMS;
    const wasTalking = this.hangover > 0;
    this.hangover = loud ? HANGOVER : Math.max(0, this.hangover - 1);
    if (this.hangover > 0 !== wasTalking) this.events.speaking(this.hangover > 0);
    if (this.hangover > 0) this.encoder.encode(frame);
  }

  private sendOpus(opus: Uint8Array): void {
    const counter = this.counter++;
    const sealed = seal(this.key, 0, counter, this.seq, opus);
    this.seq = (this.seq + 1) & 0xffff;
    this.events.send(sealedUp(this.token, counter, sealed));
    this.stats.sent++;
  }

  /** A binary frame from the server. */
  receive(data: Uint8Array): void {
    if (this.deafened || !this.codec || data.length < 7) return;
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
    let ssrc: number;
    let opus: Uint8Array;
    if (data[0] === SEALED_DOWN && data.length > 13) {
      ssrc = view.getUint32(1);
      const opened = open(this.key, 1, view.getBigUint64(5), data.subarray(13));
      if (!opened) return;
      opus = opened.opus;
    } else if (data[0] === AUDIO_DOWN) {
      ssrc = view.getUint32(1);
      opus = data.subarray(7);
    } else {
      return; // video and acknowledgements aren't used here
    }
    this.lastHeard.set(ssrc, performance.now());
    this.events.heard(ssrc);
    let dec = this.decoders.get(ssrc);
    if (!dec) {
      dec = this.codec.decoder((pcm) => {
        this.stats.decoded++;
        let sum = 0;
        for (const x of pcm) sum += x * x;
        this.stats.level = Math.sqrt(sum / pcm.length);
        this.stats.peak = Math.max(this.stats.peak, this.stats.level);
        this.player?.port.postMessage({ ssrc, pcm }, [pcm.buffer]);
      });
      this.decoders.set(ssrc, dec);
    }
    dec.decode(opus);
  }

  setMuted(muted: boolean, deafened: boolean): void {
    this.muted = muted;
    this.deafened = deafened;
    if (muted || deafened) {
      if (this.hangover > 0) this.events.speaking(false);
      this.hangover = 0;
    }
    if (deafened) this.player?.port.postMessage({ clear: true });
    for (const t of this.stream.getAudioTracks()) t.enabled = !muted && !deafened;
  }

  /** Someone left: free their decoder. */
  forget(ssrc: number): void {
    this.decoders.get(ssrc)?.close();
    this.decoders.delete(ssrc);
  }

  stop(): void {
    for (const t of this.stream.getTracks()) t.stop();
    this.encoder?.close();
    for (const d of this.decoders.values()) d.close();
    this.decoders.clear();
    this.source?.disconnect();
    this.capture?.disconnect();
    this.player?.disconnect();
    void this.ctx.close();
  }
}
