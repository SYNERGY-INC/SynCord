import assert from "node:assert/strict";
import { test } from "node:test";
import { blocks, inline, linkEnd, pieces } from "../src/markdown";

test("bold, italic and code", () => {
  const segs = inline("**hi** *there* `x`");
  assert.ok(segs.some((s) => s.style.bold && s.text === "hi"));
  assert.ok(segs.some((s) => s.style.italic && s.text === "there"));
  assert.ok(segs.some((s) => s.style.code && s.text === "x"));
});

test("underscores in links aren't italics", () => {
  const segs = inline("see https://a.com/_x_/y");
  assert.ok(!segs.some((s) => s.style.italic));
});

test("blocks: heading, list, quote, code", () => {
  const b = blocks("# Title\n- one\n> quoted\n```\ncode\n```");
  assert.deepEqual(b.map((x) => x.kind), ["h", "li", "quote", "code"]);
});

test("mentions become pieces", () => {
  const p = pieces("hey <@12> and <@&3> @everyone");
  assert.deepEqual(p.filter((x) => x.kind !== "text").map((x) => x.kind), ["user", "role", "everyone"]);
});

test("links end before trailing punctuation", () => {
  const s = "https://example.com/a).";
  assert.equal(s.slice(0, linkEnd(s)), "https://example.com/a");
});
