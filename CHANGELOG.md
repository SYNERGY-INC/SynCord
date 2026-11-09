# Changelog

Celerity was called SynCord before 0.7.0.

## v0.7.0

**SynCord is now Celerity.** Your settings, servers, messages and invite codes come along: the app
and server update to Celerity by themselves.

**Celerity on your phone**: open your server's address in the phone's browser and add it to the
Home Screen (thanks, hepbep!). Text channels, DMs, pictures and files, voice and notifications.

**Streaming at the highest quality, with sound**

- **Share a single window** instead of your whole screen (the picker lists Screens and Windows).
- **Share your screen's sound**, in stereo at music quality: on Windows and Mac just the shared
  window's app, or everything your computer plays except the call. Only people watching hear it,
  each with their own volume slider on the video.
- **Quality up to 1440p at 60 fps, or your screen's own resolution (up to 4K)**. 1440p60 is the
  default. Cameras open at 1080p (Settings > Voice & Video > Camera quality).
- **Graphics-card encoding** (NVIDIA, AMD and Intel on Windows; Apple's encoder on Mac), checked
  before use and falling back to the built-in encoder, so your CPU stays free for the game.
- **No more flickering** while watching a stream, and what you watch is drawn by your graphics card
  at full resolution.

**Voice**

- **Push to talk** with any key or mouse side button, which works while a game is in front.
- **Echo cancellation**: people no longer hear themselves through your speakers.
- **Soundboard**: play short sounds into the call (servers add them in Server Settings).
- **Connection meter**: the bars show your ping and how much audio is getting lost.
- **"You're muted"** when you talk while muted; click to unmute.
- Per-person volume goes up to 200%, and muting, deafening and going live make a sound.

**Talk more ways**

- **Be signed in to several servers at once**: each has its own icon in the left rail, with unread
  dots and pings; the + adds one. A call keeps going while you look at another server.
- **Threads**: start one from any message; it opens beside the channel.
- **Group messages** with 3 to 10 people, with group calls.
- **Custom emoji and stickers** for your server, in the picker and `:name:` autocomplete.
- **Polls**: the + next to the message box > Create Poll.
- **Categories**: collapse them, drag channels and categories to reorder, and make channels private
  to some roles (with a lock).
- **End-to-end encrypted direct messages and calls**: only the people in the conversation can read
  or hear them, not even the server (a lock in the header shows it). Files aren't covered yet.

**Feels instant**

- Messages appear the moment you press Enter. If one can't be sent, you can retry or delete it.
- Channels you're likely to open next are loaded in the background.
- Short connection drops are invisible: the screen stays as it was and quietly catches up.

**Notifications**

- **Desktop notifications** for direct messages and @mentions (or every message) while Celerity
  isn't in front, from all your servers. Click one to open the conversation.
- **Mute** a channel, conversation or server from its right-click menu.
- Opening a busy channel tells you how many new messages there are, with a jump to the first one.

**For server owners**

- **Invite links**: Invite People makes a link that expires when you want, for a set number of
  uses. Opening it in Celerity fills in the address and code; the web link has a download button.
- **Kick and ban** from a member's right-click menu.
- **An admin page** in the browser (`http://your-server:7878/admin`) with who's online, storage,
  invites, members and backups.
- **Daily backups** of the database (the last 7 are kept).
- **Docker auto-update** (optional): see the README.

**Polish**

- A new logo, new sounds, and a short welcome the first time you sign in (theme, picture, mic check).
- Rich previews for YouTube (with a play button), X/Twitter posts and Spotify.
- The emoji you use most are one click away when you hover a message; *Copy Message Link* in its menu.

## v0.6.0

**New: a fresh look, cameras and screen sharing, statuses, search, pinned messages, replies and markdown.**

- **A new look**: the channel list, chat and member list float as rounded panels over a darker
  backdrop, with soft shadows. Icon buttons in the channel header (pins, members, delete), a
  pill-shaped search and message box, a glow on the selected channel, rounder hover highlights,
  and a refreshed login screen, settings and Direct Messages home. Works with every theme.

- **Turn on your camera or share your screen** in any call (voice channels and DM calls): the
  Camera and Screen buttons are under *Voice Connected*. Pick a screen and choose sharp text
  (1080p, 15 fps) or smooth motion (720p, 30 fps). Cameras show for everyone in the call; a shared
  screen shows a red **LIVE** badge and people click **Watch Stream** to see it. Click any video to
  make it big. Video is encrypted like voice. Pick your camera in Voice & Audio settings.
- **Call view**: click the voice channel you're in to see everyone's video; *Back to chat* returns.
- **Statuses**: click your name at the bottom left for Online, Idle, Do Not Disturb or Invisible,
  and set what you're doing ("Playing ...", "Watching ...", "Listening to ..."). SynCord goes idle
  after 10 minutes away and can show the game you're playing (games from Steam, Epic, GOG, Riot
  and Xbox). Do Not Disturb silences sounds and pings.
- **Search messages**: the search box at the top (or **Ctrl+F**). Narrow it with `from:alice`,
  `in:general` and `has:link`, `has:file`, `has:image` or `has:video`. Click a result to jump to it.
- **Calls leave a note** in the DM: who started it, how long it lasted, or that it was missed.
- A **speaker on a server's icon** shows when people are in its voice channels.
- **The X button keeps SynCord running in the tray** (Windows and Mac), so you still get messages
  and calls. Turn it off in Appearance → *Closing the window*. Opening SynCord again brings back
  the running one instead of starting a second copy.
- **Pinned messages**: right-click a message → *Pin Message*. The pin button at the top lists a
  channel's pins. Anyone can pin in DMs; in server channels it takes *Manage messages*.
- **Links ask before opening**, showing exactly which website they go to. Tick *Trust ... links*
  to stop asking for that site (Appearance lists trusted sites, with *Remove*).
- **Message box**: Ctrl+Z / Ctrl+Y undo and redo, Ctrl+Backspace deletes a word, and right-click
  for Cut, Copy, Paste and Select All.
- **Escape** jumps back to the newest message. Click beside a picture to close it.
- **Roles**: Server settings → Roles now shows each role's members, with *Add* and *Remove*.
- Smoother: hover highlights fade, new messages slide in, side panels slide open.
- **Much smoother scrolling**: SynCord now draws with your graphics card by default (Low memory
  mode, in Appearance, still draws on the CPU if you'd rather save RAM). Scrolling no longer
  rebuilds the window for every message that passes under the mouse.
- **GIFs**: several on screen at once all load and play (they used to push each other out of
  memory), at about 30 fps, and only the ones you can see use any CPU or memory.
- Fixed: profile pictures sometimes turned back into letters until SynCord restarted.
- Fixed: after switching microphone or speakers during a call, others stopped hearing you.
- The Mac app now needs macOS 12.3 or newer (for screen sharing).

From hepbep (thanks!):

- **Reply** to messages (hover a message, or right-click → *Reply*).
- **Markdown**: `**bold**`, `*italic*`, `__underline__`, `~~strike~~`, `` `code` ``, code blocks,
  `> quotes`, headings, lists and `-# small text`.
- **Call timers** on voice channels and under *Voice Connected*.
- **Move people between voice channels** (drag them) and **server mute / deafen** (right-click),
  with new *Move members* and *Mute members* permissions. They're off for @everyone; give them
  to a role. Nobody can move or mute the owner or someone ranked above them, and they never
  apply in private calls.
- **Choose the font** in Appearance (any installed font, or a .ttf/.otf file).
- A **colour picker** for profile and role colours, an **update button** next to settings, a red
  **NEW** line above unread messages, and an option to flash the taskbar for every message.

## v0.5.0

**New: roles, @mentions, right-click menus, profile pictures and banners, link previews, and encryption.**

- **Encrypted connections.** Messages, files, logins and voice are now encrypted between the app
  and the server. The first time you connect, SynCord remembers the server's identity and refuses
  impostors after that. Nothing to set up.
- **Roles.** Server settings → Roles: give people a colour, a place in the member list, and
  permissions (manage channels, delete messages, ping @everyone, and more). Right-click someone to
  give them a role.
- **@mentions.** Type `@` to pick someone (or a role, or @everyone). Pings highlight the message,
  show a red badge on the channel and play a sound.
- **Shift+Enter** starts a new line, and Up in an empty box edits your last message.
- **Right-click anyone** for Profile, Message, Start a Call, Roles, Ignore, Block and Copy ID.
- **Calls in direct messages**, with a ringing popup for the person you call.
- **Profile pictures and banners**: upload a picture or GIF and crop it, Discord-style. GIFs stay
  animated. Plus **pronouns** on your profile.
- **Links** are clickable and get a preview card (title, description, picture).
- **Keyboard shortcuts** like Discord's: Ctrl+K to jump anywhere, Alt+Up/Down between channels,
  Ctrl+Shift+M to mute and more. Ctrl+/ lists them all.
- **Twemoji**, the same emoji Discord uses, on every computer, including the newest emoji.
- Remove GIFs from your favourites with the star in the GIF picker.
- Long file names no longer run out of their box.
- **Videos play in the chat** (MP4/MOV from phones and screen recorders): press play, then
  pause, seek and change the volume right there.
- Menus and dialogs pop open with a little bounce.

From hepbep (thanks!):

- **Right-click a message** (or its ⋯ button) to react, copy its text, select part of it, edit or
  delete it. Double-click a message to select its text.
- **Sounds**: a chime for new messages (separate switches for DMs and server channels), chimes
  when people join or leave your voice channel, a volume slider and a Test button
  (Voice & Audio → Sounds).
- **Theme editor** in Appearance: edit your theme as CSS or TOML with a live preview. `.css` theme
  files work too.

## v0.4.0

**New: paste screenshots, send files up to 1 GB, and search for GIFs.**

- Take a screenshot (Win+Shift+S, Print Screen, or Cmd+Ctrl+Shift+4 on a Mac) and press Ctrl+V
  in SynCord: it uploads straight away and shows a preview, then press Enter to send.
- Copy files in File Explorer or Finder and paste them into SynCord to send them.
- Click a picture and press *Copy* (or Ctrl+C) to put it on your clipboard.
- Files can now be up to 1 GB. Uploads and downloads show a progress bar, and big files are sent
  and saved in pieces, so they don't use lots of memory.
- GIF search in the GIF tab of the emoji picker, with trending GIFs when you haven't typed
  anything. Point at a GIF to see it move, click to send. The server owner needs to add a free
  GIPHY or KLIPY key to the server settings once (see the README); until then the tab shows how.

- The server can now run in Docker or CasaOS: each release publishes a server image
  (`ghcr.io/bxpo/syncord-server`). See the README. Thanks to hepbep for adding it!

Server owners: the upload limit goes up from 25 MB to 1 GB by itself (unless you'd changed it).

## v0.3.1

- Clearer messages when your server is older than the app: instead of a technical error, SynCord
  explains that the server needs updating (for direct messages, reactions and file uploads).

## v0.3.0

**New: automatic updates.**

- SynCord now updates itself. When a new version is out, a popup counts down and installs it,
  then SynCord reopens. *Update now* skips the wait; *Later* installs it next time. It never
  interrupts a voice call.
- The Windows server updates itself too, at a moment when nobody is connected.
- Updates are signed, and anything that doesn't match the signature is refused.
- Settings → Appearance → Updates shows your version and the server's, with a *Check for updates* button.

This is the last version you need to install by hand.

## v0.2.0

**New: pictures, GIFs, emoji, reactions and direct messages.**

- Send pictures and files: the **+** button, drag and drop, or paste a picture with Ctrl+V / Cmd+V.
  Click a picture to see it full size or save it.
- GIFs play in the chat. Star the ones you like and re-send them from the GIF tab of the emoji picker.
  (Don't want them always moving? Settings → Appearance → *Play GIFs automatically*.)
- Emoji picker, `:shortcode:` suggestions as you type, and big emoji-only messages.
- Reactions on messages: hover and click the smiley, or click a reaction to add yours.
- Direct messages: the new **Home** button at the top-left, or **Message** on anyone's profile.
- Unread markers are now remembered between launches.

The **server** needs updating too (run the new server installer; your messages and settings are kept).
Older apps can still connect, but won't see the new features.

## v0.1.1

- Windows server installer: much more robust setup, clear messages if something can't be set up,
  a setup log, and a *Run server in a window* shortcut for troubleshooting.
- The server keeps a log file next to its settings and explains common problems (like the port
  already being in use).

## v0.1.0

- First release: text channels, voice channels, themes, profile customization, installers.
