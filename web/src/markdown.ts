// Discord-style Markdown, ported from the desktop app (crates/client/src/markdown.rs)
// so messages look the same everywhere: **bold**, *italic*, __underline__, ~~strike~~,
// `code`, ```code blocks```, > quotes, # headings, - lists and -# small text.
// Links and @mentions are split out the same way as crates/client/src/view_messages.rs.

export interface Style { bold: boolean; italic: boolean; underline: boolean; strike: boolean; code: boolean }
export interface Seg { text: string; style: Style }
export type Block =
  | { kind: "p"; segs: Seg[] }
  | { kind: "h"; level: number; segs: Seg[] }
  | { kind: "sub"; segs: Seg[] }
  | { kind: "li"; depth: number; segs: Seg[] }
  | { kind: "quote"; blocks: Block[] }
  | { kind: "code"; text: string };

const PLAIN: Style = { bold: false, italic: false, underline: false, strike: false, code: false };
const MARKERS = ["**", "__", "~~", "*", "_", "`"];

const same = (a: Style, b: Style) =>
  a.bold === b.bold && a.italic === b.italic && a.underline === b.underline && a.strike === b.strike && a.code === b.code;
const isWord = (c: string | undefined) => c !== undefined && /[\p{L}\p{N}]/u.test(c);

/** Links: `http(s)://` up to whitespace, without trailing punctuation (keeping balanced brackets). */
export function linkEnd(s: string): number {
  let end = s.search(/[\s<>"]/);
  if (end < 0) end = s.length;
  while (end > 0) {
    const last = s[end - 1];
    const part = s.slice(0, end);
    const count = (ch: string) => part.split(ch).length - 1;
    const trim =
      ".,!?:;'*_~".includes(last) ||
      (last === ")" && count("(") < count(")")) ||
      (last === "]" && count("[") < count("]"));
    if (!trim) break;
    end--;
  }
  return end;
}

export function inline(s: string): Seg[] {
  const out: Seg[] = [];
  parseInline(s, PLAIN, out);
  const merged: Seg[] = [];
  for (const seg of out) {
    const last = merged[merged.length - 1];
    if (last && same(last.style, seg.style)) last.text += seg.text;
    else if (seg.text) merged.push(seg);
  }
  return merged;
}

function push(out: Seg[], text: string, style: Style) {
  if (text) out.push({ text, style });
}

function parseInline(s: string, style: Style, out: Seg[]) {
  let plainStart = 0;
  let i = 0;
  while (i < s.length) {
    const rest = s.slice(i);
    if (rest.startsWith("https://") || rest.startsWith("http://")) {
      i += Math.max(1, linkEnd(rest));
      continue;
    }
    if (rest[0] === "\\" && rest.length > 1 && "*_~`>#-\\|".includes(rest[1])) {
      push(out, s.slice(plainStart, i), style);
      push(out, rest[1], style);
      i += 2;
      plainStart = i;
      continue;
    }
    const marker = MARKERS.find((m) => rest.startsWith(m));
    if (!marker) {
      i += rest.codePointAt(0)! > 0xffff ? 2 : 1;
      continue;
    }
    const before = s[i - 1];
    const openEnd = i + marker.length;
    if (marker === "_" && isWord(before)) {
      i = openEnd;
      continue;
    }
    const close = findClose(s, openEnd, marker);
    if (close !== null) {
      const inner = s.slice(openEnd, close);
      const after = s[close + marker.length];
      const ok =
        inner.length > 0 &&
        !inner.startsWith(" ") &&
        !(marker === "_" && isWord(after)) &&
        (marker.length === 2 || marker === "`" || !inner.endsWith(" "));
      if (ok) {
        push(out, s.slice(plainStart, i), style);
        const st = { ...style };
        if (marker === "**") st.bold = true;
        else if (marker === "__") st.underline = true;
        else if (marker === "~~") st.strike = true;
        else if (marker === "*" || marker === "_") st.italic = true;
        else st.code = true;
        if (marker === "`") push(out, inner, st);
        else parseInline(inner, st, out);
        i = close + marker.length;
        plainStart = i;
        continue;
      }
    }
    i = openEnd;
  }
  push(out, s.slice(plainStart), style);
}

function findClose(s: string, from: number, marker: string): number | null {
  let j = from;
  while (j < s.length) {
    const rest = s.slice(j);
    if (marker !== "`" && rest[0] === "\\") {
      j += 2;
      continue;
    }
    if (marker === "`") {
      const k = rest.indexOf("`");
      return k < 0 ? null : j + k;
    }
    if (rest.startsWith(marker)) {
      const single = marker.length === 1;
      if (single && rest.slice(1).startsWith(marker)) {
        j += 2;
        continue;
      }
      if (!single && rest.slice(2).startsWith(marker[0])) {
        j += 1;
        continue;
      }
      return j;
    }
    if (rest[0] === "`") {
      const k = rest.slice(1).indexOf("`");
      if (k >= 0) {
        j += k + 2;
        continue;
      }
    }
    j++;
  }
  return null;
}

function heading(line: string): [number, string] | null {
  for (const [prefix, level] of [["### ", 3], ["## ", 2], ["# ", 1]] as const) {
    if (line.startsWith(prefix)) return [level, line.slice(prefix.length)];
  }
  return null;
}

export function blocks(s: string): Block[] {
  const out: Block[] = [];
  const lines = s.split("\n");
  let para: string[] = [];
  const flush = () => {
    if (para.length) {
      out.push({ kind: "p", segs: inline(para.join("\n")) });
      para = [];
    }
  };
  const quoteLine = (t: string) => (t.startsWith("> ") ? t.slice(2) : t === ">" ? "" : null);
  for (let n = 0; n < lines.length; n++) {
    const line = lines[n];
    const trimmed = line.trimStart();
    if (trimmed.startsWith("```")) {
      flush();
      const after = trimmed.slice(3);
      const end = after.indexOf("```");
      if (end >= 0) {
        out.push({ kind: "code", text: after.slice(0, end) });
        continue;
      }
      const code: string[] = [];
      if (after.trim() && after.trim().includes(" ")) code.push(after);
      for (n++; n < lines.length; n++) {
        const l = lines[n];
        const e = l.indexOf("```");
        if (e >= 0) {
          if (e > 0) code.push(l.slice(0, e));
          break;
        }
        code.push(l);
      }
      out.push({ kind: "code", text: code.join("\n") });
    } else if (quoteLine(trimmed) !== null) {
      flush();
      const quoted = [quoteLine(trimmed)!];
      while (n + 1 < lines.length && quoteLine(lines[n + 1].trimStart()) !== null) {
        quoted.push(quoteLine(lines[++n].trimStart())!);
      }
      out.push({ kind: "quote", blocks: blocks(quoted.join("\n")) });
    } else if (heading(trimmed)) {
      flush();
      const [level, rest] = heading(trimmed)!;
      out.push({ kind: "h", level, segs: inline(rest) });
    } else if (trimmed.startsWith("-# ")) {
      flush();
      out.push({ kind: "sub", segs: inline(trimmed.slice(3)) });
    } else if (trimmed.startsWith("- ") || trimmed.startsWith("* ")) {
      flush();
      const depth = Math.min(3, Math.floor((line.length - trimmed.length) / 2));
      out.push({ kind: "li", depth, segs: inline(trimmed.slice(2)) });
    } else {
      para.push(line);
    }
  }
  flush();
  return out;
}

/** A piece of message text: plain, a link, or a mention. */
export type Piece =
  | { kind: "text"; text: string }
  | { kind: "link"; url: string }
  | { kind: "user"; id: number }
  | { kind: "role"; id: number }
  | { kind: "everyone"; text: string };

export function pieces(content: string): Piece[] {
  const out: Piece[] = [];
  let textStart = 0;
  let i = 0;
  const pushText = (to: number) => {
    if (textStart < to) out.push({ kind: "text", text: content.slice(textStart, to) });
  };
  while (i < content.length) {
    const here = content.slice(i);
    let found: [Piece, number] | null = null;
    if (here.startsWith("https://") || here.startsWith("http://")) {
      const end = linkEnd(here);
      const scheme = here.startsWith("https://") ? 8 : 7;
      if (end > scheme) found = [{ kind: "link", url: here.slice(0, end) }, end];
    } else if (here.startsWith("<@")) {
      const m = /^<@(&?)(\d+)>/.exec(here);
      if (m) found = [m[1] ? { kind: "role", id: Number(m[2]) } : { kind: "user", id: Number(m[2]) }, m[0].length];
    } else if (here.startsWith("@everyone") || here.startsWith("@here")) {
      const len = here.startsWith("@everyone") ? 9 : 5;
      found = [{ kind: "everyone", text: here.slice(0, len) }, len];
    }
    if (found) {
      pushText(i);
      out.push(found[0]);
      i += found[1];
      textStart = i;
    } else {
      i++;
    }
  }
  pushText(content.length);
  return out;
}

export function mightHaveMarkdown(s: string): boolean {
  return /[*_~`>#-]/.test(s);
}
