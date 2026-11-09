// Drawing messages: Markdown, mentions, pictures, files, link previews and reactions.

import { ICONS, h, icon, rgb } from "./dom";
import { download, fileUrl, humanSize, profileImageUrl, setSrc } from "./files";
import { blocks, inline, mightHaveMarkdown, pieces } from "./markdown";
import type { Block, Seg } from "./markdown";
import type { Attachment, Embed, Id, Message, User } from "./proto";
import type { Store } from "./store";

export interface MessageActions {
  openMenu(m: Message): void;
  toggleReaction(m: Message, emoji: string): void;
  showUser(id: Id): void;
  viewImage(a: Attachment): void;
  jumpTo(id: Id): void;
}

const LINK_COLOR = "#00a8fc";

/** A round avatar: the uploaded picture, or a letter on the person's colour. */
export function avatar(user: User | undefined, size: number): HTMLElement {
  const el = h("span", { class: "avatar", style: { width: `${size}px`, height: `${size}px`, fontSize: `${size * 0.45}px` } });
  if (!user) return el;
  el.style.background = rgb(user.profile.accent_color);
  const glyph = user.profile.avatar_glyph || (user.profile.display_name || user.username).trim().charAt(0).toUpperCase();
  el.textContent = glyph;
  if (user.profile.avatar) {
    const img = h("img", { alt: "", loading: "lazy" });
    setSrc(img, profileImageUrl(user.profile.avatar.id));
    img.onload = () => (el.textContent = "", el.appendChild(img));
  }
  return el;
}

function nameColor(s: Store, id: Id): string {
  const role = s.roleColor(id);
  return role != null ? rgb(role) : "";
}

export function timeLabel(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (d.toDateString() === now.toDateString()) return `Today at ${time}`;
  const y = new Date(now);
  y.setDate(now.getDate() - 1);
  if (d.toDateString() === y.toDateString()) return `Yesterday at ${time}`;
  return `${d.toLocaleDateString()} ${time}`;
}

/** Links, mentions and plain text inside one styled run. */
function runNodes(s: Store, seg: Seg, act: MessageActions): Node[] {
  const st = seg.style;
  const wrap = (n: Node): Node => {
    let out = n;
    if (st.code) out = h("code", null, out);
    if (st.bold) out = h("strong", null, out);
    if (st.italic) out = h("em", null, out);
    if (st.underline) out = h("u", null, out);
    if (st.strike) out = h("s", null, out);
    return out;
  };
  if (st.code) return [wrap(document.createTextNode(seg.text))];
  return pieces(seg.text).map((p) => {
    switch (p.kind) {
      case "text":
        return wrap(document.createTextNode(p.text));
      case "link":
        return wrap(h("a", { href: p.url, target: "_blank", rel: "noopener noreferrer", style: { color: LINK_COLOR } }, p.url));
      case "user": {
        const u = s.users.get(p.id);
        return h("span", { class: "pill", onclick: () => act.showUser(p.id) }, `@${u ? s.name(p.id) : "unknown-user"}`);
      }
      case "role": {
        const r = s.roles.get(p.id);
        const color = r?.color != null ? rgb(r.color) : "";
        return h("span", { class: "pill", style: color ? { color, background: `${color}2e` } : {} }, `@${r?.name ?? "deleted-role"}`);
      }
      case "everyone":
        return h("span", { class: "pill" }, p.text);
    }
  });
}

function blockNodes(s: Store, bs: Block[], act: MessageActions): Node[] {
  return bs.map((b) => {
    const segs = (segs: Seg[]) => segs.flatMap((seg) => runNodes(s, seg, act));
    switch (b.kind) {
      case "p":
        return h("div", { class: "md-p" }, segs(b.segs));
      case "h":
        return h(`h${b.level}` as "h1", { class: "md-h" }, segs(b.segs));
      case "sub":
        return h("div", { class: "md-sub" }, segs(b.segs));
      case "li":
        return h("div", { class: "md-li", style: { paddingLeft: `${6 + b.depth * 18}px` } }, h("span", { class: "bullet" }, b.depth ? "◦" : "•"), h("span", null, segs(b.segs)));
      case "quote":
        return h("blockquote", null, blockNodes(s, b.blocks, act));
      case "code":
        return h("pre", null, h("code", null, b.text));
    }
  });
}

/** A message's text with Markdown, links and mentions. */
export function messageText(s: Store, content: string, act: MessageActions): HTMLElement {
  const el = h("div", { class: "content" });
  if (!mightHaveMarkdown(content)) {
    el.append(...inline(content).flatMap((seg) => runNodes(s, seg, act)));
  } else {
    el.append(...blockNodes(s, blocks(content), act));
  }
  return el;
}

function attachmentView(a: Attachment, act: MessageActions): HTMLElement {
  if (a.video) {
    const v = h("video", { controls: true, preload: "none", playsInline: true, class: "media" });
    if (a.width && a.height) v.style.aspectRatio = `${a.width} / ${a.height}`;
    // Videos download only when played, so scrolling past them costs nothing.
    v.addEventListener("play", () => {
      if (!v.src) {
        v.pause();
        fileUrl(a).then((u) => ((v.src = u), void v.play()), () => v.classList.add("broken"));
      }
    }, { once: false });
    return h("div", { class: "attachment" }, v);
  }
  if (a.width && a.height) {
    const img = h("img", { class: "media", alt: a.filename, loading: "lazy", onclick: () => act.viewImage(a) });
    img.style.aspectRatio = `${a.width} / ${a.height}`;
    setSrc(img, fileUrl(a, !a.animated));
    return h("div", { class: "attachment" }, img);
  }
  return h(
    "button",
    { class: "file", onclick: () => void download(a) },
    icon(ICONS.file, 28),
    h("span", { class: "file-info" }, h("span", { class: "file-name" }, a.filename), h("span", { class: "muted" }, humanSize(a.size))),
  );
}

function embedView(e: Embed, act: MessageActions): HTMLElement | null {
  if (!e.title && !e.description && !e.image) return null;
  return h(
    "div",
    { class: "embed", style: { borderLeftColor: e.color != null ? rgb(e.color) : "" } },
    e.site_name ? h("div", { class: "muted small" }, e.site_name) : null,
    e.title ? h("a", { href: e.url, target: "_blank", rel: "noopener noreferrer", class: "embed-title" }, e.title) : null,
    e.description ? h("div", { class: "embed-desc" }, e.description.slice(0, 300)) : null,
    e.image ? attachmentView(e.image, act) : null,
  );
}

export interface RowOptions {
  grouped: boolean;
  pinged: boolean;
  replying: boolean;
}

export function messageRow(s: Store, m: Message, o: RowOptions, act: MessageActions): HTMLElement {
  const author = s.users.get(m.author_id);
  const row = h("div", { class: `msg${o.grouped ? " grouped" : ""}${o.pinged ? " pinged" : ""}${o.replying ? " replying" : ""}`, dataset: { id: String(m.id) } });

  if (m.kind && m.kind.type !== "Normal") {
    const text =
      m.kind.type === "Call"
        ? `${s.name(m.author_id)} started a call${m.kind.ended_at ? " (ended)" : ""}`
        : `${s.name(m.author_id)} pinned a message`;
    row.classList.add("system");
    row.append(h("div", { class: "system-text muted" }, text, " · ", timeLabel(m.created_at)));
    return row;
  }

  if (s.blocked.has(m.author_id)) {
    row.append(h("div", { class: "system-text muted" }, "Message from someone you've blocked"));
    return row;
  }

  if (m.reply) {
    const r = m.reply;
    const who = r.author_id != null ? s.name(r.author_id) : null;
    row.append(
      h(
        "div",
        { class: "reply-preview", onclick: () => act.jumpTo(r.message_id) },
        icon(ICONS.reply, 14),
        who
          ? [h("strong", { style: { color: nameColor(s, r.author_id!) } }, who), " ", h("span", { class: "muted" }, r.snippet.split("\n")[0].slice(0, 90) || "(attachment)")]
          : h("span", { class: "muted" }, "Original message was deleted"),
      ),
    );
  }

  const body = h("div", { class: "msg-body" });
  if (!o.grouped) {
    body.append(
      h(
        "div",
        { class: "meta" },
        h("button", { class: "name", style: { color: nameColor(s, m.author_id) }, onclick: () => act.showUser(m.author_id) }, s.name(m.author_id)),
        h("span", { class: "time" }, timeLabel(m.created_at)),
      ),
    );
  }
  if (m.content) {
    const text = messageText(s, m.content, act);
    if (m.edited_at) text.append(h("span", { class: "muted small" }, " (edited)"));
    body.append(text);
  }
  if (m.attachments.length) body.append(h("div", { class: "attachments" }, m.attachments.map((a) => attachmentView(a, act))));
  for (const e of m.embeds ?? []) {
    const ev = embedView(e, act);
    if (ev) body.append(ev);
  }
  if (m.reactions.length) {
    body.append(
      h(
        "div",
        { class: "reactions" },
        m.reactions.map((r) =>
          h(
            "button",
            { class: `reaction${r.users.includes(s.me) ? " mine" : ""}`, onclick: () => act.toggleReaction(m, r.emoji) },
            r.emoji,
            h("span", null, String(r.users.length)),
          ),
        ),
      ),
    );
  }

  const gutter = o.grouped
    ? h("div", { class: "gutter" }, h("span", { class: "hover-time" }, new Date(m.created_at).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })))
    : h("button", { class: "gutter", onclick: () => act.showUser(m.author_id) }, avatar(author, 40));
  row.append(h("div", { class: "msg-line" }, gutter, body));

  // Long-press (phones) or right-click opens the message menu.
  let timer: number | undefined;
  let start: { x: number; y: number } | null = null;
  row.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    act.openMenu(m);
  });
  row.addEventListener("touchstart", (e) => {
    const t = e.touches[0];
    start = { x: t.clientX, y: t.clientY };
    timer = window.setTimeout(() => {
      timer = undefined;
      navigator.vibrate?.(10);
      act.openMenu(m);
    }, 450);
  }, { passive: true });
  const cancel = () => (clearTimeout(timer), (timer = undefined));
  row.addEventListener("touchmove", (e) => {
    const t = e.touches[0];
    if (start && Math.hypot(t.clientX - start.x, t.clientY - start.y) > 10) cancel();
  }, { passive: true });
  row.addEventListener("touchend", cancel);
  return row;
}

export function newLine(): HTMLElement {
  return h("div", { class: "new-line" }, h("span", null, "NEW"));
}
