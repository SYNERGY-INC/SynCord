// The web app: sign-in, the channel list, the chat, the message box, voice, and settings.
// Phones get one panel at a time (the channel list slides in); wide screens get both.

import { Conn } from "./conn";
import { ICONS, fill, h, icon } from "./dom";
import { fileUrl, upload } from "./files";
import * as notify from "./notify";
import { HISTORY_PAGE, MAX_ATTACHMENTS, MAX_MESSAGE_LEN, mentionsUser, perms } from "./proto";
import type { Attachment, Id, Message, ServerMsg, User } from "./proto";
import { Store } from "./store";
import type { Change } from "./store";
import { avatar, messageRow, messageText, newLine } from "./ui_messages";
import type { MessageActions } from "./ui_messages";
import { VoiceSession } from "./voice";

const QUICK_REACTIONS = ["👍", "❤️", "😂", "😮", "😢", "🎉"];
const MORE_EMOJI =
  "😀 😁 😅 🤣 😊 😍 😘 😎 🤔 🙄 😴 😭 😡 🤯 🥳 🥺 😬 🙃 😇 🤗 👀 💀 👏 🙏 💪 🤝 👋 ✌️ 👌 🤞 🔥 ✨ 💯 ✅ ❌ ⭐ 💔 💜 💙 💚 🧡 🖤 🎮 🎵 🍕 ☕ 🍻 🎂".split(" ");

interface PendingUpload {
  file: File;
  progress: number;
  done: Attachment | null;
  error: string | null;
}

export class App implements MessageActions {
  store = new Store();
  conn = new Conn();
  voice: VoiceSession | null = null;
  /** Voice channel we asked to join, before the server answers. */
  private joining: Id | null = null;
  private voiceChannel: Id | null = null;
  private speaking = false;
  /** Shown with the red NEW line: the last message read before opening this channel. */
  private newSince: { channel: Id; since: Id } | null = null;
  private replying: Id | null = null;
  private editing: Id | null = null;
  private uploads: PendingUpload[] = [];
  /** "@name" picked from suggestions, turned into a real mention when sent. */
  private picked = new Map<string, Id>();
  private lastTyping = 0;
  private lastMarked = new Map<Id, Id>();
  private authError = "";
  private signedIn = false;

  // Page parts.
  private root: HTMLElement;
  private sidebar = h("aside", { class: "sidebar" });
  private chat = h("main", { class: "chat" });
  private header = h("header", { class: "topbar" });
  private list = h("div", { class: "messages" });
  private typingEl = h("div", { class: "typing" });
  private composer = h("div", { class: "composer" });
  private input = h("textarea", { rows: 1, placeholder: "Message", enterKeyHint: "send", maxLength: MAX_MESSAGE_LEN }) as HTMLTextAreaElement;
  private suggestions = h("div", { class: "suggestions" });
  private sheetLayer = h("div", { class: "sheet-layer" });
  private toastEl = h("div", { class: "toast" });

  constructor(root: HTMLElement) {
    this.root = root;
    this.conn.hello = () => {
      const token = Conn.token;
      return token ? { op: "Resume", d: { token } } : null;
    };
    this.conn.onMessage = (m) => this.onServer(m);
    this.conn.onBinary = (d) => this.voice?.receive(d);
    this.conn.onState = (st) => {
      this.root.classList.toggle("offline", st !== "open" && this.signedIn);
      if (st === "closed" && this.voice) this.stopVoice("Voice disconnected (lost connection)");
    };
    this.store.subscribe((c) => this.redraw(c));
    this.list.addEventListener("scroll", () => this.onScroll(), { passive: true });
    // Tapping the dimmed chat beside the open channel list closes it.
    this.chat.addEventListener(
      "click",
      (e) => {
        if (this.root.classList.contains("drawer-open")) {
          e.stopPropagation();
          e.preventDefault();
          this.toggleDrawer(false);
        }
      },
      { capture: true },
    );
    this.wireInput();
    document.addEventListener("pointerdown", () => notify.unlockAudio(), { capture: true });
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden && this.signedIn) {
        this.conn.nudge();
        this.markRead();
      }
    });
    window.addEventListener("focus", () => this.markRead());
    // Call timers, speaking rings and the "is typing" line, updated in place so taps aren't lost.
    setInterval(() => this.tick(), 200);
    navigator.serviceWorker?.addEventListener("message", (e) => {
      const ch = (e.data as { channel?: Id })?.channel;
      if (typeof ch === "number") this.openChannel(ch);
    });
  }

  start(): void {
    if (Conn.token) {
      this.drawLoading();
      this.conn.connect();
    } else {
      this.drawLogin();
    }
  }

  // ---------- server messages ----------

  private onServer(msg: ServerMsg): void {
    switch (msg.op) {
      case "Ready": {
        const r = (msg as { d: { token: string } }).d;
        Conn.token = r.token;
        this.signedIn = true;
        break;
      }
      case "AuthFailed": {
        Conn.token = null;
        this.signedIn = false;
        this.authError = (msg.d as { reason: string }).reason;
        this.drawLogin();
        return;
      }
      case "Error":
        this.toast((msg.d as { message: string }).message);
        return;
      case "VoiceReady":
        void this.onVoiceReady(msg.d as { channel_id: Id; token: string; key?: string | null; ssrc: number });
        return;
      case "MessageCreated":
        this.onNewMessage(msg.d as Message);
        break;
      case "VoiceStateUpdate":
        this.onVoiceState(msg.d as { user_id: Id; state: { channel_id: Id; ssrc: number } | null });
        break;
    }
    const change = this.store.apply(msg);
    if (msg.op === "Ready") this.afterReady();
    if (change) this.store.emit(change);
  }

  private afterReady(): void {
    const s = this.store;
    this.drawShell();
    const fromLink = Number(new URLSearchParams(location.search).get("channel"));
    if (fromLink) history.replaceState(null, "", "/");
    const saved = fromLink || Number(localStorage.getItem("celerity.channel"));
    const first = s.channels.find((c) => c.kind === "Text")?.id ?? s.directs[0]?.id ?? null;
    const target = s.current ?? (saved && s.visible(saved) ? saved : first);
    this.current = null;
    if (target != null) this.openChannel(target, true);
    this.updateBadge();
  }

  private set current(id: Id | null) {
    this.store.current = id;
  }

  // ---------- notifications ----------

  private onNewMessage(m: Message): void {
    const s = this.store;
    if (m.author_id === s.me || s.blocked.has(m.author_id)) return;
    const looking = s.current === m.channel_id && !document.hidden && document.hasFocus();
    const direct = s.isDirect(m.channel_id) || !s.channels.some((c) => c.id === m.channel_id);
    const pinged = mentionsUser(m, s.me, s.meUser?.roles ?? []);
    if (pinged && !looking) s.mentionCounts.set(m.channel_id, (s.mentionCounts.get(m.channel_id) ?? 0) + 1);
    if (looking) return;
    const p = notify.prefs;
    if (pinged ? p.pingSound : direct ? p.dmSound : p.serverSound) notify.play(pinged ? "ping" : "message");
    if (p.notify && (pinged || direct || p.notifyAll) && (document.hidden || !document.hasFocus())) {
      const where = direct ? "" : ` in #${s.channelTitle(m.channel_id)}`;
      const body = m.content ? m.content.replace(/<@&?(\d+)>/g, (_, id) => `@${s.name(Number(id))}`).slice(0, 200) : "Sent a file";
      void notify.show(`${s.name(m.author_id)}${where}`, body, m.channel_id);
    }
  }

  private updateBadge(): void {
    const s = this.store;
    let count = 0;
    for (const n of s.mentionCounts.values()) count += n;
    for (const d of s.directs) if (s.isUnread(d.id) && !s.mentionCounts.get(d.id)) count += 1;
    notify.setBadge(count, s.community);
  }

  // ---------- drawing ----------

  private openDirectWith: Id | null = null;

  private redraw(c: Change): void {
    if (!this.signedIn) return;
    if (this.openDirectWith != null) {
      const d = this.store.directs.find((x) => x.user_id === this.openDirectWith);
      if (d) {
        this.openDirectWith = null;
        this.openChannel(d.id);
        return;
      }
    }
    if (c === "all") {
      this.drawShell();
      return;
    }
    if (c === "sidebar" || c === "messages" || c === "voice") this.drawSidebar();
    if (c === "messages") this.drawMessages();
    if (c === "voice") this.drawVoiceBar();
    if (c === "typing") this.drawTyping();
    this.updateBadge();
  }

  private drawLoading(): void {
    fill(this.root, h("div", { class: "center" }, h("div", { class: "spinner" }), h("p", { class: "muted" }, "Connecting...")));
  }

  private drawLogin(): void {
    this.conn.close();
    let mode: "login" | "register" = "login";
    const user = h("input", { autocomplete: "username", placeholder: "Username", autocapitalize: "none", required: true });
    const pass = h("input", { type: "password", autocomplete: "current-password", placeholder: "Password", required: true });
    const invite = h("input", { placeholder: "Invite code", autocapitalize: "none" });
    const err = h("p", { class: "error" }, this.authError);
    const submit = h("button", { class: "primary", type: "submit" }, "Sign in");
    const switcher = h("button", { type: "button", class: "link" }, "Need an account? Register");
    const inviteRow = h("label", { class: "field hidden" }, "Invite code", invite);
    switcher.onclick = () => {
      mode = mode === "login" ? "register" : "login";
      inviteRow.classList.toggle("hidden", mode === "login");
      submit.textContent = mode === "login" ? "Sign in" : "Create account";
      switcher.textContent = mode === "login" ? "Need an account? Register" : "Have an account? Sign in";
      pass.autocomplete = mode === "login" ? "current-password" : "new-password";
    };
    const form = h(
      "form",
      { class: "login-card" },
      h("img", { src: "/icon-192.png", class: "logo", alt: "" }),
      h("h1", null, "Welcome to Celerity"),
      h("p", { class: "muted" }, location.host),
      h("label", { class: "field" }, "Username", user),
      h("label", { class: "field" }, "Password", pass),
      inviteRow,
      err,
      submit,
      switcher,
    );
    form.onsubmit = (e) => {
      e.preventDefault();
      notify.unlockAudio();
      this.authError = "";
      err.textContent = "";
      submit.disabled = true;
      const first =
        mode === "login"
          ? ({ op: "Login", d: { username: user.value.trim(), password: pass.value } } as const)
          : ({ op: "Register", d: { username: user.value.trim(), password: pass.value, invite: invite.value.trim() } } as const);
      this.conn.hello = () => first;
      this.conn.onState = (st) => {
        if (st === "closed" && !this.signedIn) {
          submit.disabled = false;
          err.textContent = this.authError || "Can't reach the server.";
        }
      };
      const done = this.conn.onMessage;
      this.conn.onMessage = (m) => {
        if (m.op === "AuthFailed") {
          submit.disabled = false;
          err.textContent = (m.d as { reason: string }).reason;
          this.conn.close();
          return;
        }
        if (m.op === "Ready") {
          this.conn.onMessage = done;
          this.conn.hello = () => (Conn.token ? { op: "Resume", d: { token: Conn.token } } : null);
          this.conn.onState = (st) => {
            this.root.classList.toggle("offline", st !== "open" && this.signedIn);
            if (st === "closed" && this.voice) this.stopVoice("Voice disconnected (lost connection)");
          };
          void this.maybeAskNotifications();
        }
        done(m);
      };
      this.conn.connect();
    };
    fill(this.root, h("div", { class: "center login" }, form));
    user.focus();
  }

  private drawShell(): void {
    fill(this.chat, this.header, this.list, this.typingEl, this.composer);
    fill(this.root, h("div", { class: "shell" }, this.sidebar, this.chat), this.sheetLayer, this.toastEl);
    this.root.classList.remove("offline");
    this.drawSidebar();
    this.drawHeader();
    this.drawMessages(true);
    this.drawComposer();
    this.drawVoiceBar();
  }

  private toggleDrawer(open?: boolean): void {
    this.root.classList.toggle("drawer-open", open);
  }

  private drawSidebar(): void {
    const s = this.store;
    const textChannels = s.channels.filter((c) => c.kind === "Text");
    const voiceChannels = s.channels.filter((c) => c.kind === "Voice");
    const item = (id: Id, label: Node[], extra?: Node | null) => {
      const unread = s.isUnread(id);
      const pings = s.mentionCounts.get(id) ?? 0;
      return h(
        "button",
        { class: `item${s.current === id ? " active" : ""}${unread ? " unread" : ""}`, onclick: () => this.openChannel(id) },
        ...label,
        pings ? h("span", { class: "badge" }, String(pings)) : null,
        extra ?? null,
      );
    };
    const voiceList = voiceChannels.map((c) => {
      const members = [...s.voice.values()].filter((v) => v.channel_id === c.id);
      const length = s.callLength(c.id);
      return h(
        "div",
        { class: "voice-channel" },
        h(
          "button",
          { class: `item${this.voiceChannel === c.id ? " active" : ""}`, onclick: () => void this.joinVoice(c.id) },
          icon(ICONS.speaker, 18),
          h("span", { class: "grow" }, c.name),
          length != null ? h("span", { class: "muted small", dataset: { call: String(c.id) } }, duration(length)) : null,
        ),
        members.map((v) => {
          const u = s.users.get(v.user_id);
          const talking = v.user_id === s.me ? this.speaking : this.isTalking(v.ssrc);
          return h(
            "button",
            { class: `voice-member${talking ? " talking" : ""}`, dataset: { user: String(v.user_id), ssrc: String(v.ssrc) }, onclick: () => this.showUser(v.user_id) },
            avatar(u, 22),
            h("span", { class: "grow" }, s.name(v.user_id)),
            v.server_muted || v.muted ? icon(ICONS.micOff, 15) : null,
            v.server_deafened || v.deafened ? icon(ICONS.headphonesOff, 15) : null,
          );
        }),
      );
    });
    const dms = s.directs.map((d) => {
      const u = s.users.get(d.user_id);
      return item(d.id, [avatar(u, 28), h("span", { class: "grow" }, s.name(d.user_id))]);
    });
    const me = s.meUser;
    fill(
      this.sidebar,
      h("div", { class: "community" }, h("span", { class: "grow" }, s.community), h("button", { class: "icon-btn", title: "People", onclick: () => this.showPeople() }, icon(ICONS.people))),
      h(
        "nav",
        { class: "channels" },
        textChannels.length ? h("div", { class: "section" }, "Text channels") : null,
        textChannels.map((c) => item(c.id, [icon(ICONS.hash, 18), h("span", { class: "grow" }, c.name)])),
        voiceChannels.length ? h("div", { class: "section" }, "Voice channels") : null,
        voiceList,
        h("div", { class: "section" }, h("span", { class: "grow" }, "Direct messages"), h("button", { class: "icon-btn small", title: "New message", onclick: () => this.showPeople() }, icon(ICONS.plus, 16))),
        dms.length ? dms : h("p", { class: "muted small pad" }, "No conversations yet."),
      ),
      h("div", { class: "voice-slot" }),
      h(
        "div",
        { class: "me" },
        avatar(me, 32),
        h("span", { class: "grow name-col" }, h("strong", null, s.name(s.me)), h("span", { class: "muted small" }, me ? `@${me.username}` : "")),
        h("button", { class: "icon-btn", title: "Settings", onclick: () => this.showSettings() }, icon(ICONS.gear)),
      ),
    );
    this.drawVoiceBar();
  }

  private drawHeader(): void {
    const s = this.store;
    const cur = s.current;
    const ch = s.channels.find((c) => c.id === cur);
    const other = cur != null ? s.directWith(cur) : undefined;
    fill(
      this.header,
      h("button", { class: "icon-btn menu-btn", title: "Channels", onclick: () => this.toggleDrawer() }, icon(ICONS.menu)),
      other != null ? avatar(s.users.get(other), 26) : icon(ICONS.hash, 20),
      h("span", { class: "title" }, s.channelTitle(cur)),
      ch?.topic ? h("span", { class: "topic muted" }, ch.topic) : null,
      h("span", { class: "grow" }),
      h("span", { class: "header-voice" }),
    );
  }

  /** Redraw the message list; keeps you at the bottom if you were there. */
  private drawMessages(jumpToEnd = false): void {
    const s = this.store;
    const cur = s.current;
    const atBottom = jumpToEnd || this.list.scrollHeight - this.list.scrollTop - this.list.clientHeight < 80;
    const prevHeight = this.list.scrollHeight;
    const prevTop = this.list.scrollTop;
    if (cur == null) {
      fill(this.list, h("div", { class: "center muted" }, "Pick a channel"));
      return;
    }
    const c = s.chan(cur);
    if (!c.loaded) {
      fill(this.list, h("div", { class: "center" }, h("div", { class: "spinner" })));
      return;
    }
    const msgs = c.messages;
    const rows: Node[] = [];
    if (c.reachedStart) {
      rows.push(h("div", { class: "channel-start" }, h("h2", null, s.isDirect(cur) ? s.channelTitle(cur) : `Welcome to #${s.channelTitle(cur)}!`), h("p", { class: "muted" }, s.isDirect(cur) ? "This is the start of your conversation." : "This is the start of the channel.")));
    } else {
      rows.push(h("div", { class: "older muted small" }, c.loadingOlder ? "Loading..." : "Scroll up for older messages"));
    }
    const since = this.newSince?.channel === cur ? this.newSince.since : null;
    let newAt = since != null ? msgs.find((m) => m.id > since && m.author_id !== s.me)?.id : undefined;
    const roles = s.meUser?.roles ?? [];
    let prev: Message | undefined;
    for (const m of msgs) {
      const isNew = m.id === newAt;
      if (isNew) {
        rows.push(newLine());
        newAt = undefined;
      }
      const grouped =
        !!prev &&
        !isNew &&
        !m.reply &&
        (!m.kind || m.kind.type === "Normal") &&
        prev.author_id === m.author_id &&
        m.created_at - prev.created_at < 5 * 60_000;
      if (this.editing === m.id) {
        rows.push(this.editRow(m));
      } else {
        rows.push(messageRow(s, m, { grouped, pinged: m.author_id !== s.me && mentionsUser(m, s.me, roles), replying: this.replying === m.id }, this));
      }
      prev = m;
    }
    fill(this.list, ...rows);
    if (atBottom) {
      this.list.scrollTop = this.list.scrollHeight;
      // Pictures load later and grow the list: keep following the bottom while they do.
      requestAnimationFrame(() => (this.list.scrollTop = this.list.scrollHeight));
    } else {
      this.list.scrollTop = prevTop + (this.list.scrollHeight - prevHeight) * (c.loadingOlder ? 1 : 0);
    }
    if (this.pendingJump != null && this.list.querySelector(`[data-id="${this.pendingJump}"]`)) {
      const id = this.pendingJump;
      this.pendingJump = null;
      requestAnimationFrame(() => this.jumpTo(id));
    }
    this.markRead();
  }

  private editRow(m: Message): HTMLElement {
    const box = h("textarea", { class: "edit-box", value: m.content, rows: 3 }) as HTMLTextAreaElement;
    const save = () => {
      const content = box.value.trim();
      if (content && content !== m.content) this.conn.send({ op: "EditMessage", d: { message_id: m.id, content } });
      this.editing = null;
      this.drawMessages();
    };
    box.onkeydown = (e) => {
      if (e.key === "Enter" && !e.shiftKey && !isTouch()) (e.preventDefault(), save());
      if (e.key === "Escape") (this.editing = null), this.drawMessages();
    };
    setTimeout(() => (box.focus(), box.setSelectionRange(box.value.length, box.value.length)));
    return h(
      "div",
      { class: "msg editing" },
      box,
      h("div", { class: "row gap" }, h("button", { class: "primary small", onclick: save }, "Save"), h("button", { class: "link", onclick: () => ((this.editing = null), this.drawMessages()) }, "Cancel")),
    );
  }

  private drawTyping(): void {
    const s = this.store;
    const who = s.current != null ? s.typingIn(s.current) : [];
    const names = who.map((u) => s.name(u));
    const text =
      names.length === 0 ? "" : names.length === 1 ? `${names[0]} is typing...` : names.length <= 3 ? `${names.join(", ")} are typing...` : "Several people are typing...";
    if (this.typingEl.textContent !== text) this.typingEl.textContent = text;
  }

  private drawComposer(): void {
    const s = this.store;
    const cur = s.current;
    const inServer = cur != null && s.channels.some((c) => c.id === cur);
    const canSend = cur != null && (!inServer || s.can(perms.SEND_MESSAGES));
    const canAttach = !inServer || s.can(perms.ATTACH_FILES);
    this.input.disabled = !canSend;
    this.input.placeholder = canSend ? `Message ${s.isDirect(cur ?? 0) ? "@" : "#"}${s.channelTitle(cur)}` : "You can't send messages here";
    const fileInput = h("input", { type: "file", multiple: true, class: "hidden" }) as HTMLInputElement;
    fileInput.onchange = () => {
      for (const f of fileInput.files ?? []) this.addUpload(f);
      fileInput.value = "";
    };
    const reply = this.replying != null ? s.chan(cur ?? 0).messages.find((m) => m.id === this.replying) : undefined;
    fill(
      this.composer,
      this.suggestions,
      reply
        ? h("div", { class: "reply-bar" }, h("span", { class: "grow" }, "Replying to ", h("strong", null, s.name(reply.author_id))), h("button", { class: "icon-btn small", onclick: () => this.setReply(null) }, icon(ICONS.close, 16)))
        : null,
      this.uploads.length
        ? h(
            "div",
            { class: "uploads" },
            this.uploads.map((u, i) =>
              h(
                "div",
                { class: `upload${u.error ? " failed" : ""}` },
                h("span", { class: "grow ellipsis" }, u.file.name || "photo"),
                h("span", { class: "muted small" }, u.error ? u.error : u.done ? "Ready" : `${Math.round(u.progress * 100)}%`),
                h("button", { class: "icon-btn small", onclick: () => (this.uploads.splice(i, 1), this.drawComposer()) }, icon(ICONS.close, 14)),
              ),
            ),
          )
        : null,
      h(
        "div",
        { class: "input-row" },
        canAttach ? h("button", { class: "icon-btn round", title: "Add files or photos", disabled: !canSend, onclick: () => fileInput.click() }, icon(ICONS.plus)) : null,
        fileInput,
        this.input,
        h("button", { class: "icon-btn round send", title: "Send", disabled: !canSend, onclick: () => this.send() }, icon(ICONS.send)),
      ),
    );
    this.autosize();
  }

  private drawVoiceBar(): void {
    const slot = this.sidebar.querySelector(".voice-slot");
    const headerSlot = this.header.querySelector(".header-voice");
    if (!slot || !headerSlot) return;
    if (this.voiceChannel == null && this.joining == null) {
      slot.replaceChildren();
      headerSlot.replaceChildren();
      return;
    }
    const s = this.store;
    const id = this.voiceChannel ?? this.joining!;
    const name = s.channels.find((c) => c.id === id)?.name ?? s.channelTitle(id);
    const mine = this.voice;
    const since = this.voiceSince ? duration(Date.now() - this.voiceSince) : "";
    const btn = (paths: string, on: boolean, title: string, fn: () => void, danger = false) =>
      h("button", { class: `icon-btn round${on ? " on" : ""}${danger ? " danger" : ""}`, title, onclick: fn }, icon(paths));
    fill(
      slot,
      h(
        "div",
        { class: "voice-bar" },
        h("div", { class: "grow" }, h("div", { class: "ok" }, this.voiceChannel != null ? "Voice connected" : "Connecting..."), h("div", { class: "muted small ellipsis" }, `${name} · `, h("span", { dataset: { since: "" } }, since))),
        mine ? btn(mine.muted || mine.deafened ? ICONS.micOff : ICONS.mic, mine.muted || mine.deafened, "Mute", () => this.setVoice(!mine.muted, mine.deafened)) : null,
        mine ? btn(mine.deafened ? ICONS.headphonesOff : ICONS.headphones, mine.deafened, "Deafen", () => this.setVoice(mine.muted, !mine.deafened)) : null,
        btn(ICONS.hangup, false, "Leave voice", () => this.leaveVoice(), true),
      ),
    );
    // On phones the channel list is hidden, so show a small call pill in the header too.
    fill(headerSlot, h("button", { class: "call-pill", onclick: () => this.toggleDrawer(true) }, icon(ICONS.speaker, 14), h("span", { dataset: { since: "" } }, since || "...")));
  }

  private tick(): void {
    this.drawTyping();
    if (!this.signedIn) return;
    const s = this.store;
    const since = this.voiceSince && this.voiceChannel != null ? duration(Date.now() - this.voiceSince) : "...";
    for (const el of this.root.querySelectorAll<HTMLElement>("[data-since]")) if (el.textContent !== since) el.textContent = since;
    for (const el of this.root.querySelectorAll<HTMLElement>("[data-call]")) {
      const length = s.callLength(Number(el.dataset.call));
      const text = length != null ? duration(length) : "";
      if (el.textContent !== text) el.textContent = text;
    }
    for (const el of this.root.querySelectorAll<HTMLElement>(".voice-member")) {
      const talking = Number(el.dataset.user) === s.me ? this.speaking : this.isTalking(Number(el.dataset.ssrc));
      el.classList.toggle("talking", talking);
    }
  }

  // ---------- channels and messages ----------

  openChannel(id: Id, initial = false): void {
    const s = this.store;
    if (!s.visible(id)) return;
    if (s.current !== id || initial) {
      this.newSince = s.isUnread(id) ? { channel: id, since: s.read.get(id) ?? 0 } : null;
      this.replying = null;
      this.editing = null;
      this.input.value = "";
      this.picked.clear();
      this.uploads = [];
    }
    this.current = id;
    try { localStorage.setItem("celerity.channel", String(id)); } catch { /* ignore */ }
    s.mentionCounts.delete(id);
    const c = s.chan(id);
    if (!c.loaded) this.conn.send({ op: "FetchHistory", d: { channel_id: id, before: null } });
    this.toggleDrawer(false);
    this.drawSidebar();
    this.drawHeader();
    this.drawMessages(true);
    this.drawComposer();
    this.drawVoiceBar();
    this.drawTyping();
    this.updateBadge();
  }

  private onScroll(): void {
    const s = this.store;
    const cur = s.current;
    if (cur == null) return;
    const c = s.chan(cur);
    if (this.list.scrollTop < 200 && c.loaded && !c.reachedStart && !c.loadingOlder && c.messages.length) {
      c.loadingOlder = true;
      this.conn.send({ op: "FetchHistory", d: { channel_id: cur, before: c.messages[0].id } });
    }
    this.markRead();
  }

  /** Tell the server we've read the channel, when its newest message is on screen. */
  private markRead(): void {
    const s = this.store;
    const cur = s.current;
    if (cur == null || document.hidden || !this.signedIn) return;
    const msgs = s.chan(cur).messages;
    const last = msgs[msgs.length - 1];
    if (!last) return;
    const nearBottom = this.list.scrollHeight - this.list.scrollTop - this.list.clientHeight < 120;
    if (!nearBottom || (s.read.get(cur) ?? 0) >= last.id || this.lastMarked.get(cur) === last.id) return;
    this.lastMarked.set(cur, last.id);
    s.read.set(cur, last.id);
    s.mentionCounts.delete(cur);
    this.conn.send({ op: "MarkRead", d: { channel_id: cur, message_id: last.id } });
    this.drawSidebar();
    this.updateBadge();
  }

  private wireInput(): void {
    this.input.addEventListener("input", () => {
      this.autosize();
      this.suggest();
      const cur = this.store.current;
      if (cur != null && this.input.value && Date.now() - this.lastTyping > 5000) {
        this.lastTyping = Date.now();
        this.conn.send({ op: "Typing", d: { channel_id: cur } });
      }
    });
    this.input.addEventListener("keydown", (e) => {
      // Enter sends on computers; phones use the send button (Enter adds a line).
      if (e.key === "Enter" && !e.shiftKey && !isTouch()) {
        e.preventDefault();
        this.send();
      } else if (e.key === "Escape") {
        this.setReply(null);
      } else if (e.key === "ArrowUp" && !this.input.value) {
        const mine = [...this.store.chan(this.store.current ?? 0).messages].reverse().find((m) => m.author_id === this.store.me && m.content);
        if (mine) (this.editing = mine.id), this.drawMessages();
      }
    });
    this.input.addEventListener("paste", (e) => {
      for (const f of e.clipboardData?.files ?? []) this.addUpload(f);
    });
  }

  private autosize(): void {
    this.input.style.height = "auto";
    this.input.style.height = `${Math.min(this.input.scrollHeight, 160)}px`;
  }

  /** "@name" suggestions while typing a mention. */
  private suggest(): void {
    const caret = this.input.selectionStart ?? this.input.value.length;
    const before = this.input.value.slice(0, caret);
    const m = /(^|\s)@([\p{L}\p{N}_.-]{0,32})$/u.exec(before);
    if (!m) {
      this.suggestions.replaceChildren();
      return;
    }
    const q = m[2].toLowerCase();
    const s = this.store;
    const matches = [...s.users.values()]
      .filter((u) => u.id !== s.me && (u.username.toLowerCase().startsWith(q) || s.name(u.id).toLowerCase().startsWith(q)))
      .slice(0, 6);
    fill(
      this.suggestions,
      matches.map((u) =>
        h(
          "button",
          {
            class: "suggestion",
            onclick: () => {
              const name = s.name(u.id).replace(/\s+/g, "");
              this.picked.set(`@${name}`, u.id);
              this.input.value = `${before.slice(0, before.length - m[2].length - 1)}@${name} ${this.input.value.slice(caret)}`;
              this.suggestions.replaceChildren();
              this.input.focus();
            },
          },
          avatar(u, 22),
          s.name(u.id),
          h("span", { class: "muted small" }, `@${u.username}`),
        ),
      ),
    );
  }

  private addUpload(file: File): void {
    const cur = this.store.current;
    if (cur == null) return;
    if (this.uploads.length >= MAX_ATTACHMENTS) return this.toast(`Up to ${MAX_ATTACHMENTS} files per message`);
    const max = this.store.maxUploadMb;
    if (max && file.size > max * 1024 * 1024) return this.toast(`${file.name} is bigger than ${max} MB`);
    const u: PendingUpload = { file, progress: 0, done: null, error: null };
    this.uploads.push(u);
    this.drawComposer();
    upload(cur, file, (p) => {
      u.progress = p;
      this.drawComposer();
    }).then(
      (a) => ((u.done = a), this.drawComposer()),
      (e: Error) => ((u.error = e.message), this.drawComposer()),
    );
  }

  private send(): void {
    const s = this.store;
    const cur = s.current;
    if (cur == null) return;
    if (this.uploads.some((u) => !u.done && !u.error)) return this.toast("Wait for the files to finish uploading");
    let content = this.input.value.trim();
    for (const [name, id] of this.picked) content = content.split(name).join(`<@${id}>`);
    const attachments = this.uploads.filter((u) => u.done).map((u) => u.done!.id);
    if (!content && !attachments.length) return;
    const reply_to = this.replying ?? undefined;
    this.conn.send({ op: "SendMessage", d: { channel_id: cur, content, attachments, ...(reply_to ? { reply_to } : {}) } });
    this.input.value = "";
    this.picked.clear();
    this.uploads = [];
    this.replying = null;
    this.newSince = null;
    this.drawComposer();
    this.drawMessages(true);
    if (!isTouch()) this.input.focus();
  }

  private setReply(id: Id | null): void {
    this.replying = id;
    this.drawComposer();
    this.drawMessages();
    if (id != null) this.input.focus();
  }

  // ---------- MessageActions ----------

  openMenu(m: Message): void {
    const s = this.store;
    const mine = m.author_id === s.me;
    const inServer = s.channels.some((c) => c.id === m.channel_id);
    const canDelete = mine || (inServer && s.can(perms.MANAGE_MESSAGES));
    const close = () => this.closeSheet();
    const action = (label: string, fn: () => void, danger = false) =>
      h("button", { class: `sheet-item${danger ? " danger" : ""}`, onclick: () => (close(), fn()) }, label);
    this.sheet(
      h("div", { class: "quick-reactions" }, QUICK_REACTIONS.map((e) => h("button", { onclick: () => (close(), this.toggleReaction(m, e)) }, e)), h("button", { class: "more", onclick: () => this.emojiPicker(m) }, icon(ICONS.smile))),
      action("Reply", () => this.setReply(m.id)),
      m.content ? action("Copy text", () => void navigator.clipboard?.writeText(m.content).then(() => this.toast("Copied"))) : null,
      mine && m.content ? action("Edit message", () => ((this.editing = m.id), this.drawMessages())) : null,
      canDelete
        ? action("Delete message", () => {
            if (confirm("Delete this message?")) this.conn.send({ op: "DeleteMessage", d: { message_id: m.id } });
          }, true)
        : null,
    );
  }

  private emojiPicker(m: Message): void {
    const close = () => this.closeSheet();
    this.sheet(h("div", { class: "emoji-grid" }, [...QUICK_REACTIONS, ...MORE_EMOJI].map((e) => h("button", { onclick: () => (close(), this.toggleReaction(m, e)) }, e))));
  }

  toggleReaction(m: Message, emoji: string): void {
    const mine = m.reactions.find((r) => r.emoji === emoji)?.users.includes(this.store.me);
    this.conn.send({ op: mine ? "RemoveReaction" : "AddReaction", d: { message_id: m.id, emoji } });
  }

  showUser(id: Id): void {
    const s = this.store;
    const u = s.users.get(id);
    if (!u) return;
    const roles = (u.roles ?? []).map((r) => s.roles.get(r)).filter((r) => r);
    this.sheet(
      h(
        "div",
        { class: "profile" },
        h("div", { class: "banner", style: { background: `#${u.profile.banner_color.toString(16).padStart(6, "0")}` } }),
        avatar(u, 72),
        h("h2", null, s.name(id)),
        h("div", { class: "muted" }, `@${u.username}${u.profile.pronouns ? ` · ${u.profile.pronouns}` : ""}`),
        u.profile.status_text ? h("p", null, u.profile.status_text) : null,
        u.profile.bio ? messageText(s, u.profile.bio, this) : null,
        roles.length ? h("div", { class: "role-list" }, roles.map((r) => h("span", { class: "role", style: { borderColor: r!.color != null ? `#${r!.color.toString(16).padStart(6, "0")}` : "" } }, r!.name))) : null,
        id !== s.me ? h("button", { class: "primary", onclick: () => (this.closeSheet(), this.messageUser(id)) }, "Send message") : null,
      ),
    );
  }

  viewImage(a: Attachment): void {
    const img = h("img", { alt: a.filename });
    fileUrl(a).then((u) => (img.src = u));
    const viewer = h("div", { class: "viewer", onclick: () => viewer.remove() }, img);
    this.root.appendChild(viewer);
  }

  jumpTo(id: Id): void {
    const row = this.list.querySelector(`[data-id="${id}"]`);
    if (row) {
      row.scrollIntoView({ block: "center", behavior: "smooth" });
      row.classList.add("flash");
      setTimeout(() => row.classList.remove("flash"), 1500);
    } else if (this.store.current != null) {
      this.pendingJump = id;
      this.conn.send({ op: "FetchAround", d: { channel_id: this.store.current, message_id: id } });
    }
  }

  /** Message to scroll to once FetchAround answers. */
  private pendingJump: Id | null = null;

  private messageUser(id: Id): void {
    const s = this.store;
    const existing = s.directs.find((d) => d.user_id === id);
    if (existing) return this.openChannel(existing.id);
    this.conn.send({ op: "OpenDirect", d: { user_id: id } });
    this.openDirectWith = id;
  }

  private showPeople(): void {
    const s = this.store;
    const people = [...s.users.values()].sort((a, b) => Number(s.online.has(b.id)) - Number(s.online.has(a.id)) || s.name(a.id).localeCompare(s.name(b.id)));
    const row = (u: User) =>
      h("button", { class: "sheet-item person", onclick: () => this.showUser(u.id) }, avatar(u, 32), h("span", { class: "grow" }, s.name(u.id)), h("span", { class: `dot${s.online.has(u.id) ? " online" : ""}` }));
    this.sheet(h("h3", null, "People"), people.map(row));
  }

  // ---------- voice ----------

  private voiceSince = 0;

  private isTalking(ssrc: number): boolean {
    const t = this.voice?.lastHeard.get(ssrc);
    return t != null && performance.now() - t < 300;
  }

  async joinVoice(channel: Id): Promise<void> {
    if (this.voiceChannel === channel || this.joining === channel) return;
    if (!this.store.can(perms.CONNECT_VOICE)) return this.toast("You don't have permission to join voice");
    if (this.voice) this.leaveVoice();
    this.joining = channel;
    this.drawVoiceBar();
    try {
      this.voice = await VoiceSession.prepare({
        send: (p) => this.conn.sendBinary(p),
        heard: () => {},
        speaking: (on) => (this.speaking = on),
      });
    } catch (e) {
      this.joining = null;
      this.drawVoiceBar();
      return this.toast((e as Error).message);
    }
    if (this.joining !== channel) return;
    this.conn.send({ op: "JoinVoice", d: { channel_id: channel, encrypted: true } });
  }

  private async onVoiceReady(d: { channel_id: Id; token: string; key?: string | null }): Promise<void> {
    if (!this.voice || !d.key) {
      if (!d.key) this.toast("This server doesn't support encrypted voice");
      return this.leaveVoice();
    }
    try {
      await this.voice.start(d.token, d.key, OPUS_WASM_URL);
    } catch (e) {
      this.toast(`Couldn't start voice: ${(e as Error).message}`);
      return this.leaveVoice();
    }
    (window as unknown as { __celerityVoice?: VoiceSession }).__celerityVoice = this.voice;
    this.voiceChannel = d.channel_id;
    this.joining = null;
    this.voiceSince = Date.now();
    if (notify.prefs.voiceSounds) notify.play("join");
    this.drawSidebar();
  }

  private onVoiceState(d: { user_id: Id; state: { channel_id: Id; ssrc: number } | null }): void {
    const s = this.store;
    const mine = this.voiceChannel;
    if (d.user_id === s.me) {
      // Moved by someone, or kicked out of voice.
      if (d.state && mine != null && d.state.channel_id !== mine) this.voiceChannel = d.state.channel_id;
      if (!d.state && this.voice && this.joining == null) this.stopVoice();
      return;
    }
    if (mine == null) return;
    const before = s.voice.get(d.user_id);
    const wasHere = before?.channel_id === mine;
    const isHere = d.state?.channel_id === mine;
    if (wasHere !== isHere && notify.prefs.voiceSounds && !this.voice?.deafened) notify.play(isHere ? "join" : "leave");
    if (before && !isHere) this.voice?.forget(before.ssrc);
  }

  private setVoice(muted: boolean, deafened: boolean): void {
    if (!this.voice) return;
    this.voice.setMuted(muted, deafened);
    this.conn.send({ op: "SetVoiceState", d: { muted, deafened } });
    this.drawVoiceBar();
  }

  leaveVoice(): void {
    if (this.voiceChannel != null && notify.prefs.voiceSounds) notify.play("leave");
    this.conn.send({ op: "LeaveVoice" });
    this.stopVoice();
  }

  private stopVoice(message?: string): void {
    this.voice?.stop();
    this.voice = null;
    this.voiceChannel = null;
    this.joining = null;
    this.speaking = false;
    if (message) this.toast(message);
    this.drawSidebar();
  }

  // ---------- settings ----------

  private async maybeAskNotifications(): Promise<void> {
    // Only ask after a tap, and only once; phones refuse otherwise.
    if (notify.notificationsSupported() && Notification.permission === "default" && !localStorage.getItem("celerity.asked")) {
      localStorage.setItem("celerity.asked", "1");
      await notify.askPermission();
    }
  }

  private showSettings(): void {
    const p = notify.prefs;
    const toggle = (label: string, key: keyof notify.Prefs) => {
      const box = h("input", { type: "checkbox", checked: p[key] }) as HTMLInputElement;
      box.onchange = () => ((p[key] = box.checked), notify.savePrefs());
      return h("label", { class: "toggle" }, h("span", { class: "grow" }, label), box);
    };
    const perm = notify.notificationsSupported() ? Notification.permission : "unsupported";
    const permRow =
      perm === "granted"
        ? h("p", { class: "muted small" }, "Notifications are allowed on this device.")
        : perm === "unsupported"
          ? h("p", { class: "muted small" }, notify.isIos() && !notify.installed() ? "On iPhone, add Celerity to your Home Screen first (Share, then Add to Home Screen) to get notifications." : "This browser can't show notifications.")
          : h("button", { class: "primary small", onclick: async () => ((await notify.askPermission()), this.showSettings()) }, perm === "denied" ? "Notifications are blocked: allow them in the browser's site settings" : "Allow notifications");
    const install = notify.installed()
      ? null
      : h(
          "div",
          { class: "install" },
          h("strong", null, "Install Celerity"),
          h("p", { class: "muted small" }, notify.isIos() ? "In Safari: tap Share, then Add to Home Screen." : "Open the browser menu and choose Install app (or Add to Home screen)."),
          installPrompt ? h("button", { class: "primary small", onclick: () => void installPrompt!.prompt() }, "Install") : null,
        );
    this.sheet(
      h("h3", null, "Settings"),
      h("div", { class: "section" }, "Notifications"),
      permRow,
      toggle("Notify me about direct messages and mentions", "notify"),
      toggle("...and every message in server channels", "notifyAll"),
      h("div", { class: "section" }, "Sounds"),
      toggle("Sound when someone mentions me", "pingSound"),
      toggle("Sound for new direct messages", "dmSound"),
      toggle("Sound for every message in server channels", "serverSound"),
      toggle("Sound when people join or leave my voice channel", "voiceSounds"),
      h("button", { class: "link", onclick: () => notify.play("message") }, "Test sound"),
      install,
      h("p", { class: "muted small" }, `Server ${this.store.ready?.server_version || "?"} · ${location.host}${this.voice ? ` · voice codec: ${this.voice.codecName}` : ""}`),
      h("button", { class: "sheet-item danger", onclick: () => this.logout() }, "Log out"),
    );
  }

  private logout(): void {
    this.closeSheet();
    this.leaveVoice();
    this.conn.send({ op: "Logout" });
    this.conn.close();
    Conn.token = null;
    this.signedIn = false;
    this.store = new Store();
    this.store.subscribe((c) => this.redraw(c));
    this.drawLogin();
  }

  // ---------- sheets and toasts ----------

  private sheet(...content: (Node | Node[] | null)[]): void {
    const panel = h("div", { class: "sheet", onclick: (e: Event) => e.stopPropagation() }, h("div", { class: "grabber" }), ...content);
    fill(this.sheetLayer, h("div", { class: "sheet-backdrop", onclick: () => this.closeSheet() }, panel));
    this.sheetLayer.classList.add("open");
  }

  private closeSheet(): void {
    this.sheetLayer.classList.remove("open");
    this.sheetLayer.replaceChildren();
  }

  toast(text: string): void {
    this.toastEl.textContent = text;
    this.toastEl.classList.add("show");
    clearTimeout(this.toastTimer);
    this.toastTimer = window.setTimeout(() => this.toastEl.classList.remove("show"), 3500);
  }
  private toastTimer: number | undefined;
}

/** Where the WebAssembly Opus fallback is (set by the build). */
declare const OPUS_WASM_URL: string;

interface InstallPromptEvent extends Event {
  prompt(): Promise<void>;
}
let installPrompt: InstallPromptEvent | null = null;
window.addEventListener("beforeinstallprompt", (e) => {
  e.preventDefault();
  installPrompt = e as InstallPromptEvent;
});

function isTouch(): boolean {
  return matchMedia("(pointer: coarse)").matches;
}

function duration(ms: number): string {
  const secs = Math.floor(ms / 1000);
  const h = Math.floor(secs / 3600);
  const m = Math.floor(secs / 60) % 60;
  const s = String(secs % 60).padStart(2, "0");
  return h ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

export { HISTORY_PAGE };
