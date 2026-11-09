// Everything the web app knows about the server, updated from server messages.

import { EVERYONE_ROLE, HISTORY_PAGE, perms } from "./proto";
import type { Channel, DirectChannel, Id, Message, Ready, Role, ServerMsg, User, VoiceState } from "./proto";

/** What changed, so the UI only redraws what it has to. */
export type Change = "all" | "sidebar" | "messages" | "typing" | "voice";

export interface ChannelState {
  messages: Message[];
  loaded: boolean;
  reachedStart: boolean;
  loadingOlder: boolean;
}

export class Store {
  ready: Ready | null = null;
  me: Id = 0;
  community = "Celerity";
  users = new Map<Id, User>();
  roles = new Map<Id, Role>();
  channels: Channel[] = [];
  directs: DirectChannel[] = [];
  read = new Map<Id, Id>();
  latest = new Map<Id, Id>();
  mentionCounts = new Map<Id, number>();
  online = new Set<Id>();
  voice = new Map<Id, VoiceState>();
  blocked = new Set<Id>();
  chans = new Map<Id, ChannelState>();
  typing = new Map<Id, Map<Id, number>>();
  current: Id | null = null;
  maxUploadMb = 0;
  /** Server clock minus this device's clock (ms). */
  clockOffset = 0;

  private listeners = new Set<(c: Change) => void>();

  subscribe(fn: (c: Change) => void): void {
    this.listeners.add(fn);
  }

  emit(c: Change): void {
    for (const fn of this.listeners) fn(c);
  }

  get meUser(): User | undefined {
    return this.users.get(this.me);
  }

  chan(id: Id): ChannelState {
    let c = this.chans.get(id);
    if (!c) {
      c = { messages: [], loaded: false, reachedStart: false, loadingOlder: false };
      this.chans.set(id, c);
    }
    return c;
  }

  name(id: Id | null | undefined): string {
    const u = id == null ? undefined : this.users.get(id);
    if (!u) return "Unknown";
    return u.profile.display_name.trim() || u.username;
  }

  /** Colour of someone's highest coloured role, if any. */
  roleColor(id: Id): number | null {
    const u = this.users.get(id);
    if (!u) return null;
    let best: Role | null = null;
    for (const rid of u.roles ?? []) {
      const r = this.roles.get(rid);
      if (r && r.color != null && (!best || r.position > best.position)) best = r;
    }
    return best?.color ?? null;
  }

  /** My permissions in server channels. */
  get permissions(): number {
    const me = this.meUser;
    if (!me) return 0;
    if (me.is_owner) return ~0;
    let p = this.roles.get(EVERYONE_ROLE)?.permissions ?? 0;
    for (const rid of me.roles ?? []) p |= this.roles.get(rid)?.permissions ?? 0;
    return p & perms.ADMINISTRATOR ? ~0 : p;
  }

  can(perm: number): boolean {
    return (this.permissions & perm) !== 0;
  }

  isDirect(channel: Id): boolean {
    return this.directs.some((d) => d.id === channel);
  }

  directWith(channel: Id): Id | undefined {
    return this.directs.find((d) => d.id === channel)?.user_id;
  }

  channelTitle(id: Id | null): string {
    if (id == null) return "";
    const d = this.directWith(id);
    if (d != null) return this.name(d);
    const c = this.channels.find((c) => c.id === id);
    return c ? c.name : "";
  }

  isUnread(channel: Id): boolean {
    const latest = this.latest.get(channel);
    return latest != null && latest > (this.read.get(channel) ?? 0);
  }

  /** How long a voice channel's call has been going, in ms. */
  callLength(channel: Id): number | null {
    let start = Infinity;
    for (const v of this.voice.values()) if (v.channel_id === channel && v.joined_at) start = Math.min(start, v.joined_at);
    return start === Infinity ? null : Math.max(0, Date.now() + this.clockOffset - start);
  }

  typingIn(channel: Id): Id[] {
    const now = Date.now();
    const t = this.typing.get(channel);
    if (!t) return [];
    for (const [u, until] of t) if (until < now) t.delete(u);
    return [...t.keys()].filter((u) => u !== this.me);
  }

  /** Apply a message from the server. Returns what changed. */
  apply(msg: ServerMsg): Change | null {
    switch (msg.op) {
      case "Ready": {
        const r = msg.d as Ready;
        this.ready = r;
        this.me = r.me;
        this.community = r.community_name || "Celerity";
        this.users = new Map(r.users.map((u) => [u.id, u]));
        this.roles = new Map((r.roles ?? []).map((x) => [x.id, x]));
        this.channels = [...r.channels].sort((a, b) => a.position - b.position || a.id - b.id);
        this.directs = [...(r.directs ?? [])].sort((a, b) => (b.last_message_id ?? 0) - (a.last_message_id ?? 0));
        this.read = new Map((r.read_states ?? []).map((s) => [s.channel_id, s.last_read]));
        this.latest = new Map();
        for (const c of this.channels) if (c.last_message_id != null) this.latest.set(c.id, c.last_message_id);
        for (const d of this.directs) if (d.last_message_id != null) this.latest.set(d.id, d.last_message_id);
        this.mentionCounts = new Map((r.mentions ?? []).map((m) => [m.channel_id, m.count]));
        this.online = new Set(r.online);
        this.voice = new Map((r.voice ?? []).map((v) => [v.user_id, v]));
        this.blocked = new Set(r.blocked ?? []);
        this.maxUploadMb = r.max_upload_mb ?? 0;
        this.clockOffset = r.server_time ? r.server_time - Date.now() : 0;
        // Messages may have changed while we were away: load them again.
        this.chans.clear();
        if (this.current != null && !this.visible(this.current)) this.current = null;
        return "all";
      }
      case "MessageCreated": {
        const m = msg.d as Message;
        this.typing.get(m.channel_id)?.delete(m.author_id);
        this.latest.set(m.channel_id, Math.max(this.latest.get(m.channel_id) ?? 0, m.id));
        if (m.author_id === this.me) this.read.set(m.channel_id, m.id);
        const i = this.directs.findIndex((d) => d.id === m.channel_id);
        if (i > 0) this.directs.unshift(...this.directs.splice(i, 1));
        const c = this.chans.get(m.channel_id);
        if (c?.loaded && !c.messages.some((x) => x.id === m.id)) c.messages.push(m);
        return "messages";
      }
      case "MessageUpdated": {
        const m = msg.d as Message;
        const list = this.chans.get(m.channel_id)?.messages;
        const i = list?.findIndex((x) => x.id === m.id) ?? -1;
        if (list && i >= 0) list[i] = m;
        return "messages";
      }
      case "MessageDeleted": {
        const d = msg.d as { channel_id: Id; message_id: Id };
        const c = this.chans.get(d.channel_id);
        if (c) c.messages = c.messages.filter((x) => x.id !== d.message_id);
        return "messages";
      }
      case "History": {
        const d = msg.d as { channel_id: Id; messages: Message[]; reached_start: boolean };
        const c = this.chan(d.channel_id);
        const have = new Set(c.messages.map((m) => m.id));
        const older = d.messages.filter((m) => !have.has(m.id));
        c.messages = [...older, ...c.messages].sort((a, b) => a.id - b.id);
        c.loaded = true;
        c.loadingOlder = false;
        c.reachedStart = d.reached_start || d.messages.length < HISTORY_PAGE;
        return "messages";
      }
      case "Around": {
        const d = msg.d as { channel_id: Id; messages: Message[]; reached_start: boolean };
        const c = this.chan(d.channel_id);
        c.messages = [...d.messages].sort((a, b) => a.id - b.id);
        c.loaded = true;
        c.reachedStart = d.reached_start;
        return "messages";
      }
      case "Typing": {
        const d = msg.d as { channel_id: Id; user_id: Id };
        let t = this.typing.get(d.channel_id);
        if (!t) this.typing.set(d.channel_id, (t = new Map()));
        t.set(d.user_id, Date.now() + 8000);
        return "typing";
      }
      case "UserJoined":
      case "UserUpdated": {
        const u = msg.d as User;
        this.users.set(u.id, u);
        return "all";
      }
      case "Presence": {
        const d = msg.d as { user_id: Id; online: boolean };
        if (d.online) this.online.add(d.user_id);
        else this.online.delete(d.user_id);
        return "sidebar";
      }
      case "ChannelCreated": {
        const c = msg.d as Channel;
        this.channels = [...this.channels.filter((x) => x.id !== c.id), c].sort((a, b) => a.position - b.position || a.id - b.id);
        return "sidebar";
      }
      case "ChannelDeleted": {
        const id = (msg.d as { channel_id: Id }).channel_id;
        this.channels = this.channels.filter((c) => c.id !== id);
        if (this.current === id) this.current = null;
        return "all";
      }
      case "DirectOpened": {
        const d = msg.d as DirectChannel;
        if (!this.directs.some((x) => x.id === d.id)) this.directs.unshift(d);
        return "sidebar";
      }
      case "ReactionUpdate": {
        const d = msg.d as { channel_id: Id; message_id: Id; emoji: string; user_id: Id; added: boolean };
        const m = this.chans.get(d.channel_id)?.messages.find((x) => x.id === d.message_id);
        if (!m) return null;
        let r = m.reactions.find((x) => x.emoji === d.emoji);
        if (d.added) {
          if (!r) m.reactions.push((r = { emoji: d.emoji, users: [] }));
          if (!r.users.includes(d.user_id)) r.users.push(d.user_id);
        } else if (r) {
          r.users = r.users.filter((u) => u !== d.user_id);
          if (!r.users.length) m.reactions = m.reactions.filter((x) => x !== r);
        }
        return "messages";
      }
      case "VoiceStateUpdate": {
        const d = msg.d as { user_id: Id; state: VoiceState | null };
        if (d.state) this.voice.set(d.user_id, d.state);
        else this.voice.delete(d.user_id);
        return "voice";
      }
      case "RoleUpdated": {
        const r = msg.d as Role;
        this.roles.set(r.id, r);
        return "all";
      }
      case "RoleDeleted": {
        this.roles.delete((msg.d as { role_id: Id }).role_id);
        return "all";
      }
      case "BlockUpdated": {
        const d = msg.d as { user_id: Id; blocked: boolean };
        if (d.blocked) this.blocked.add(d.user_id);
        else this.blocked.delete(d.user_id);
        return "messages";
      }
      default:
        return null;
    }
  }

  visible(channel: Id): boolean {
    return this.channels.some((c) => c.id === channel) || this.isDirect(channel);
  }
}
