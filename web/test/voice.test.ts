import assert from "node:assert/strict";
import { test } from "node:test";
import { hexToBytes, open, seal } from "../src/voice";

const key = hexToBytes("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
const hex = (b: Uint8Array) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");

// The same frame sealed by the Rust code (celerity_proto::voice::seal, test `web_app_vector`).
const RUST_UP = "2a47cbeef06cf484ccea757ca0106d2e989237880e88";

test("seal matches the server's encryption", () => {
  const sealed = seal(key, 0, 7n, 513, new Uint8Array([0xde, 0xad, 0xbe, 0xef]));
  assert.equal(hex(sealed), RUST_UP);
});

test("open reverses seal and rejects the wrong direction", () => {
  const sealed = seal(key, 1, 42n, 9, new Uint8Array([1, 2, 3]));
  assert.deepEqual(open(key, 1, 42n, sealed), { seq: 9, opus: new Uint8Array([1, 2, 3]) });
  assert.equal(open(key, 0, 42n, sealed), null);
});
