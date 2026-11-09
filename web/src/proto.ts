// The messages the server and apps exchange over the WebSocket (JSON, `{op, d}`),
// mirroring crates/proto/src/lib.rs. Only the parts the web app uses are spelled out.

export type Id = number;

export const EVERYONE_ROLE = 1;
export const HISTORY_PAGE = 50;
export const MAX_MESSAGE_LEN = 4000;
export const MAX_ATTACHMENTS = 10;
export const UPLOAD_PATH = "/api/upload";
export const FILES_PATH = "/api/files";
export const PROFILE_IMAGE_PATH = "/api/profile-image";

export const perms = {
  ADMINISTRATOR: 1 << 0,
  MANAGE_CHANNELS: 1 << 1,
  MANAGE_ROLES: 1 << 2,
  MANAGE_MESSAGES: 1 << 3,
  MENTION_EVERYONE: 1 << 4,
  SEND_MESSAGES: 1 << 5,
  ATTACH_FILES: 1 << 6,
  CONNECT_VOICE: 1 << 7,
  EMBED_LINKS: 1 << 8,
  ADD_REACTIONS: 1 << 9,
  MOVE_MEMBERS: 1 << 10,
  MUTE_MEMBERS: 1 << 11,
} as const;

export interface ProfileImage { id: string; animated?: boolean }
export interface Profile {
  display_name: string;
  status_text: string;
  bio: string;
  accent_color: number;
  banner_color: number;
  avatar_glyph: string;
  pronouns: string;
  avatar: ProfileImage | null;
  banner: ProfileImage | null;
}
export interface User { id: Id; username: string; profile: Profile; is_owner: boolean; roles: Id[] }
export interface Role { id: Id; name: string; color: number | null; permissions: number; position: number; hoist?: boolean }
export type ChannelKind = "Text" | "Voice" | "Direct";
export interface Channel { id: Id; name: string; topic: string; position: number; kind: ChannelKind; last_message_id: Id | null }
export interface DirectChannel { id: Id; user_id: Id; last_message_id: Id | null }
export interface ReadState { channel_id: Id; last_read: Id }
export interface VideoInfo { duration_ms: number; has_audio: boolean }
export interface Attachment {
  id: string;
  filename: string;
  size: number;
  mime: string;
  width?: number | null;
  height?: number | null;
  animated?: boolean;
  video?: VideoInfo | null;
}
export interface Embed {
  url: string;
  site_name?: string | null;
  title?: string | null;
  description?: string | null;
  color?: number | null;
  image?: Attachment | null;
  media?: boolean;
}
export interface Reaction { emoji: string; users: Id[] }
export interface ReplyRef { message_id: Id; author_id: Id | null; snippet: string }
export type MessageKind =
  | { type: "Normal" }
  | { type: "Call"; participants: Id[]; ended_at: number | null }
  | { type: "Pinned"; message_id: Id };
export interface Message {
  id: Id;
  channel_id: Id;
  author_id: Id;
  content: string;
  created_at: number;
  edited_at: number | null;
  attachments: Attachment[];
  reactions: Reaction[];
  embeds: Embed[];
  mentions: Id[];
  reply?: ReplyRef;
  pinned?: boolean;
  kind?: MessageKind;
}
export interface VoiceState {
  user_id: Id;
  channel_id: Id;
  muted: boolean;
  deafened: boolean;
  ssrc: number;
  server_muted?: boolean;
  server_deafened?: boolean;
  joined_at?: number;
}
export type Status = "Online" | "Idle" | "DoNotDisturb" | "Invisible";
export interface PresenceInfo { user_id: Id; status: Status; activity?: { kind: string; name: string } | null }
export interface MentionCount { channel_id: Id; count: number }

export interface Ready {
  token: string;
  me: Id;
  community_name: string;
  users: User[];
  online: Id[];
  channels: Channel[];
  voice: VoiceState[];
  directs: DirectChannel[];
  read_states: ReadState[];
  server_version: string;
  max_upload_mb: number;
  roles: Role[];
  blocked: Id[];
  mentions: MentionCount[];
  server_time: number;
  presences: PresenceInfo[];
}

export type ClientMsg =
  | { op: "Register"; d: { username: string; password: string; invite: string } }
  | { op: "Login"; d: { username: string; password: string } }
  | { op: "Resume"; d: { token: string } }
  | { op: "Logout" }
  | { op: "SendMessage"; d: { channel_id: Id; content: string; attachments: string[]; reply_to?: Id } }
  | { op: "EditMessage"; d: { message_id: Id; content: string } }
  | { op: "DeleteMessage"; d: { message_id: Id } }
  | { op: "FetchHistory"; d: { channel_id: Id; before: Id | null } }
  | { op: "FetchAround"; d: { channel_id: Id; message_id: Id } }
  | { op: "Typing"; d: { channel_id: Id } }
  | { op: "AddReaction"; d: { message_id: Id; emoji: string } }
  | { op: "RemoveReaction"; d: { message_id: Id; emoji: string } }
  | { op: "OpenDirect"; d: { user_id: Id } }
  | { op: "MarkRead"; d: { channel_id: Id; message_id: Id } }
  | { op: "JoinVoice"; d: { channel_id: Id; encrypted: boolean } }
  | { op: "LeaveVoice" }
  | { op: "SetVoiceState"; d: { muted: boolean; deafened: boolean } }
  | { op: "Ping" };

export type ServerMsg =
  | { op: "Ready"; d: Ready }
  | { op: "AuthFailed"; d: { reason: string } }
  | { op: "MessageCreated"; d: Message }
  | { op: "MessageUpdated"; d: Message }
  | { op: "MessageDeleted"; d: { channel_id: Id; message_id: Id } }
  | { op: "History"; d: { channel_id: Id; messages: Message[]; reached_start: boolean } }
  | { op: "Around"; d: { channel_id: Id; message_id: Id; messages: Message[]; reached_start: boolean } }
  | { op: "Typing"; d: { channel_id: Id; user_id: Id } }
  | { op: "UserJoined"; d: User }
  | { op: "UserUpdated"; d: User }
  | { op: "Presence"; d: { user_id: Id; online: boolean; status?: Status } }
  | { op: "ChannelCreated"; d: Channel }
  | { op: "ChannelDeleted"; d: { channel_id: Id } }
  | { op: "DirectOpened"; d: DirectChannel }
  | { op: "ReactionUpdate"; d: { channel_id: Id; message_id: Id; emoji: string; user_id: Id; added: boolean } }
  | { op: "VoiceReady"; d: { channel_id: Id; ssrc: number; token: string; udp_port: number; key?: string | null } }
  | { op: "VoiceStateUpdate"; d: { user_id: Id; state: VoiceState | null } }
  | { op: "Error"; d: { message: string } }
  | { op: "RoleUpdated"; d: Role }
  | { op: "RoleDeleted"; d: { role_id: Id } }
  | { op: "BlockUpdated"; d: { user_id: Id; blocked: boolean } }
  | { op: "Pong" }
  | { op: string; d?: unknown };

/** Mentions in message text: users `<@id>`, roles `<@&id>`, and @everyone / @here. */
export function mentionsUser(m: Message, me: Id, myRoles: Id[]): boolean {
  if (m.mentions?.includes(me)) return true;
  if (m.content.includes(`<@${me}>`)) return true;
  if (m.content.includes("@everyone") || m.content.includes("@here")) return true;
  return myRoles.some((r) => m.content.includes(`<@&${r}>`));
}
