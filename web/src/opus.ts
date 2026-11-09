// Opus audio (mono, 48 kHz, 20 ms frames), the same as the desktop app. Uses the
// browser's own codec (WebCodecs) where it has Opus, otherwise libopus compiled to
// WebAssembly (from @evan/opus), downloaded only when needed (mostly iPhones).

export const SAMPLE_RATE = 48_000;
export const FRAME = 960;

export interface Encoder {
  encode(frame: Float32Array): void;
  close(): void;
}
export interface Decoder {
  decode(packet: Uint8Array): void;
  close(): void;
}
export type Backend = "webcodecs" | "wasm";

const ENC_CONFIG: AudioEncoderConfig = { codec: "opus", sampleRate: SAMPLE_RATE, numberOfChannels: 1, bitrate: 32_000 };
const DEC_CONFIG: AudioDecoderConfig = { codec: "opus", sampleRate: SAMPLE_RATE, numberOfChannels: 1 };

async function webcodecsWorks(): Promise<{ enc: boolean; dec: boolean }> {
  const enc =
    typeof AudioEncoder !== "undefined" &&
    !!(await AudioEncoder.isConfigSupported(ENC_CONFIG).catch(() => ({ supported: false }))).supported;
  const dec =
    typeof AudioDecoder !== "undefined" &&
    !!(await AudioDecoder.isConfigSupported(DEC_CONFIG).catch(() => ({ supported: false }))).supported;
  return { enc, dec };
}

// ---- WebCodecs ----

class WcEncoder implements Encoder {
  private enc: AudioEncoder;
  private ts = 0;
  constructor(output: (packet: Uint8Array) => void) {
    this.enc = new AudioEncoder({
      output: (chunk) => {
        const b = new Uint8Array(chunk.byteLength);
        chunk.copyTo(b);
        output(b);
      },
      error: (e) => console.warn("opus encoder", e),
    });
    // 20 ms frames like the desktop app; "voip" tuning where the browser allows it.
    const opus = { frameDuration: 20_000, application: "voip", usedtx: false } as unknown;
    try {
      this.enc.configure({ ...ENC_CONFIG, opus } as AudioEncoderConfig);
    } catch {
      this.enc.configure(ENC_CONFIG);
    }
  }
  encode(frame: Float32Array): void {
    if (this.enc.state !== "configured") return;
    const data = new AudioData({
      format: "f32-planar",
      sampleRate: SAMPLE_RATE,
      numberOfFrames: frame.length,
      numberOfChannels: 1,
      timestamp: this.ts,
      data: frame as Float32Array<ArrayBuffer>,
    });
    this.ts += (frame.length * 1_000_000) / SAMPLE_RATE;
    this.enc.encode(data);
    data.close();
  }
  close(): void {
    if (this.enc.state !== "closed") this.enc.close();
  }
}

class WcDecoder implements Decoder {
  private dec: AudioDecoder;
  private ts = 0;
  constructor(output: (pcm: Float32Array) => void) {
    this.dec = new AudioDecoder({
      output: (ad) => {
        const pcm = new Float32Array(ad.numberOfFrames);
        try {
          ad.copyTo(pcm, { planeIndex: 0, format: "f32-planar" });
        } catch {
          ad.copyTo(pcm, { planeIndex: 0 });
        }
        ad.close();
        output(pcm);
      },
      error: (e) => console.warn("opus decoder", e),
    });
    this.dec.configure(DEC_CONFIG);
  }
  decode(packet: Uint8Array): void {
    if (this.dec.state !== "configured") return;
    this.dec.decode(new EncodedAudioChunk({ type: "key", timestamp: this.ts, data: packet }));
    this.ts += 20_000;
  }
  close(): void {
    if (this.dec.state !== "closed") this.dec.close();
  }
}

// ---- WebAssembly libopus (adapted from @evan/opus/wasm/index.js for browsers) ----

interface OpusWasm {
  memory: WebAssembly.Memory;
  malloc(n: number): number;
  free(p: number): void;
  opus_encoder_get_size(ch: number): number;
  opus_encoder_init(p: number, rate: number, ch: number, app: number): number;
  opus_encode(p: number, pcm: number, frames: number, out: number, max: number): number;
  opus_decoder_get_size(ch: number): number;
  opus_decoder_init(p: number, rate: number, ch: number): number;
  opus_decode(p: number, data: number, len: number, pcm: number, frames: number, fec: number): number;
  opus_encoder_ctl_set(p: number, cmd: number, arg: number): number;
}

let wasmPromise: Promise<OpusWasm> | null = null;
const PACKET_BUF = 2 ** 13;
const PCM_BUF = 2 ** 15;

function loadWasm(url: string): Promise<OpusWasm> {
  wasmPromise ??= (async () => {
    let w!: OpusWasm;
    const imports = {
      wasi_snapshot_preview1: { fd_seek() {}, fd_write() {}, fd_close() {}, proc_exit() {} },
      env: { emscripten_notify_memory_growth() {} },
    };
    const res = await fetch(url);
    const { instance } = await WebAssembly.instantiate(await res.arrayBuffer(), imports);
    w = instance.exports as unknown as OpusWasm;
    return w;
  })();
  return wasmPromise;
}

class WasmEncoder implements Encoder {
  private st: number;
  private pcm: number;
  private out: number;
  constructor(private w: OpusWasm, private output: (packet: Uint8Array) => void) {
    this.st = w.malloc(w.opus_encoder_get_size(1));
    const VOIP = 2048;
    if (w.opus_encoder_init(this.st, SAMPLE_RATE, 1, VOIP) < 0) throw new Error("opus encoder init failed");
    const SET_BITRATE = 4002;
    w.opus_encoder_ctl_set(this.st, SET_BITRATE, 32_000);
    this.pcm = w.malloc(FRAME * 2);
    this.out = w.malloc(PACKET_BUF);
  }
  encode(frame: Float32Array): void {
    const i16 = new Int16Array(this.w.memory.buffer, this.pcm, FRAME);
    for (let i = 0; i < FRAME; i++) i16[i] = Math.max(-1, Math.min(1, frame[i] ?? 0)) * 32767;
    const n = this.w.opus_encode(this.st, this.pcm, FRAME, this.out, PACKET_BUF);
    if (n > 0) this.output(new Uint8Array(this.w.memory.buffer, this.out, n).slice());
  }
  close(): void {
    for (const p of [this.st, this.pcm, this.out]) if (p) this.w.free(p);
    this.st = this.pcm = this.out = 0;
  }
}

class WasmDecoder implements Decoder {
  private st: number;
  private inp: number;
  private pcm: number;
  constructor(private w: OpusWasm, private output: (pcm: Float32Array) => void) {
    this.st = w.malloc(w.opus_decoder_get_size(1));
    if (w.opus_decoder_init(this.st, SAMPLE_RATE, 1) < 0) throw new Error("opus decoder init failed");
    this.inp = w.malloc(PACKET_BUF);
    this.pcm = w.malloc(PCM_BUF);
  }
  decode(packet: Uint8Array): void {
    if (!this.st || packet.length > PACKET_BUF) return;
    new Uint8Array(this.w.memory.buffer, this.inp, packet.length).set(packet);
    const frames = this.w.opus_decode(this.st, this.inp, packet.length, this.pcm, PCM_BUF / 2, 0);
    if (frames <= 0) return;
    const i16 = new Int16Array(this.w.memory.buffer, this.pcm, frames);
    const out = new Float32Array(frames);
    for (let i = 0; i < frames; i++) out[i] = i16[i] / 32768;
    this.output(out);
  }
  close(): void {
    for (const p of [this.st, this.inp, this.pcm]) if (p) this.w.free(p);
    this.st = this.inp = this.pcm = 0;
  }
}

/** Opus for this browser. `wasmUrl` is where the WebAssembly fallback lives. */
export class Codec {
  private constructor(private encKind: Backend, private decKind: Backend, private w: OpusWasm | null) {}

  static async create(wasmUrl: string, force?: Backend): Promise<Codec> {
    const wc = force === "wasm" ? { enc: false, dec: false } : await webcodecsWorks();
    const w = !wc.enc || !wc.dec ? await loadWasm(wasmUrl) : null;
    return new Codec(wc.enc ? "webcodecs" : "wasm", wc.dec ? "webcodecs" : "wasm", w);
  }

  get description(): string {
    return this.encKind === this.decKind ? this.encKind : `${this.encKind} encoder, ${this.decKind} decoder`;
  }

  encoder(output: (packet: Uint8Array) => void): Encoder {
    return this.encKind === "webcodecs" ? new WcEncoder(output) : new WasmEncoder(this.w!, output);
  }

  decoder(output: (pcm: Float32Array) => void): Decoder {
    return this.decKind === "webcodecs" ? new WcDecoder(output) : new WasmDecoder(this.w!, output);
  }
}
