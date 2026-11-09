# Celerity web app (phones)

The server now serves a web version of Celerity. Open the server's address in a phone browser, sign in, and add it to the Home Screen. It then opens full screen like a normal app, with its own icon.

Example: `https://sc.hepbep.online`

## What it can do (first version)

- **Sign in / register** with the same accounts as the desktop app (invite code needed to register).
- **Text channels and DMs**: read and send messages, reply, react, edit and delete your own messages (and others' with Manage Messages), copy text, @mention people (type `@` to pick someone), typing indicator, unread dots, mention badges, the red NEW line, and older messages load as you scroll up.
- **Markdown**, same rules as the desktop app (bold, italics, code, quotes, lists, headings, links).
- **Pictures and files**: tap + to send photos or files from the phone (with upload progress), tap a picture to view it full screen, play videos, download files.
- **Voice**: join a voice channel, mute, deafen, leave. Shows who is in each channel, who is talking (green ring), and call timers. Join/leave sounds.
- **Notifications**: a pop-up for DMs and mentions while the app is in the background, a number on the app icon, and sounds. All of it can be switched on or off in Settings (the gear at the bottom of the channel list).
- **Profiles**: tap a name or avatar to see the profile and start a DM.

Not in this version: video and screen sharing, server settings and role management, editing your own profile, the theme editor, pinned messages and search. Use the desktop app for those.

## How to install it on a phone

**iPhone / iPad (Safari):** open the address, tap Share, then "Add to Home Screen". Open Celerity from the Home Screen icon and sign in again (the Home Screen app keeps its own login), then tap the gear at the bottom of the channel list and "Allow notifications". On iPhone, notifications only work after it has been added to the Home Screen; this is an Apple rule.

**Android (Chrome):** open the address, then the menu (three dots), then "Install app" or "Add to Home screen". There is also an Install button in the app's Settings.

## Requirements

- **It must be opened over https.** Browsers only allow the microphone, notifications and installing on secure addresses. The Cloudflare Tunnel address (`https://sc.hepbep.online`) already does this. Opening `http://192.168.0.11:7878` on the home network will show the app and chat will work, but voice and notifications won't.
- Nothing extra to set up on the server. It is built into the server program, so updating the Docker image is enough. Anyone running a server of version 0.6.0 or newer with this change gets the web app at their server's address.
- Cloudflare: WebSockets must be allowed (they are on by default).

## Good to know

- **Notifications only arrive while the app is running in the background.** If the phone has fully closed the app (after a long time, or after swiping it away), there are no notifications until it is opened again. True "closed app" push notifications would need Web Push, which needs extra server work; that is a possible next step.
- **Voice in the background:** iPhones pause web apps shortly after you switch away or lock the screen, so voice can stop. Android keeps it going longer. Keep the app open during calls for now.
- **Voice quality:** the same Opus audio as the desktop app (mono, 48 kHz). Phones use their built-in echo cancellation and noise suppression. Voice goes over the same secure connection as chat (browsers can't use the UDP voice port), encrypted the same way as the desktop app. Desktop and phone users can talk to each other in the same channel.
- **Sign out** is at the bottom of Settings. The login is remembered on the device until then.
- If something looks stuck, pull the app away and reopen it; it reconnects by itself after the phone wakes up.

## For developers

Code is in `web/`. Plain TypeScript, no framework, bundled with esbuild.

```
cd web
npm install
npm run check   # type check
npm test        # tests (Markdown, and voice encryption checked against the Rust code)
npm run build   # writes web/dist
```

`web/dist` is committed, and `crates/server/build.rs` embeds it into the server binary, so building the server and the Docker image doesn't need Node. **After changing anything in `web/`, run `npm run build` and commit `web/dist` too.**

Files:

| File | What it does |
| --- | --- |
| `src/main.ts` | Starts the app, registers the service worker |
| `src/app.ts` | All the screens: sign in, channel list, chat, message box, menus, voice bar, settings |
| `src/ui_messages.ts` | Draws a message (Markdown, mentions, pictures, files, link previews, reactions, replies) |
| `src/store.ts` | Keeps users, channels, messages, unread state and voice state up to date from server events |
| `src/conn.ts` | The WebSocket connection to `/ws`: sign in, reconnect, keep-alive |
| `src/proto.ts` | Message types matching `crates/proto` |
| `src/markdown.ts` | Port of the desktop app's Markdown rules |
| `src/files.ts` | Uploads, downloads and pictures (`/api/upload`, `/api/files`) |
| `src/voice.ts` | Microphone, speaker, voice activity, ChaCha20-Poly1305 encryption, voice packets over the WebSocket |
| `src/opus.ts` | Opus: the browser's own (WebCodecs) where available, otherwise a WebAssembly libopus (iPhones) |
| `src/notify.ts` | Sounds, notifications, the app badge, settings |
| `src/sw.ts` | Service worker: install support, offline start, tapping a notification opens the channel |
| `public/` | Page, styles, icons, manifest |
| `build.mjs` | The build |

Server side: `crates/server/src/web.rs` serves the files (at `/`, with long caching for `/assets/*`), and `build.rs` embeds them.

Voice details: the browser joins with `JoinVoice { encrypted: true }` and then sends and receives the normal voice packets as binary WebSocket messages instead of UDP. The server already supported this path; nothing changed in the voice protocol. Setting `localStorage["celerity.codec"] = "wasm"` forces the WebAssembly codec, to test what iPhones use on a computer.

Tested: Chrome at phone size against a local server (sign in, chat, replies, mentions, uploads, menus, settings), and a two-person call with one side on the WebAssembly codec, audio arriving both ways. Not yet tested on a real iPhone or Android phone.
