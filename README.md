# SynCord

A lightweight, self-hosted chat app for you and your friends, written entirely in Rust.
It looks and feels like the chat apps you're used to, but it's a native program instead of a
web browser in disguise, so it's tiny, and every customization feature is free.

![SynCord main window](docs/screenshot.png)

## Why

| | SynCord | Typical Electron chat app |
|---|---|---|
| Client memory (idle, one server) | **~22 MB** (low memory mode) | 300 MB to 1 GB+ |
| Client download | ~16 MB, single file | 100 MB+ installer |
| Server memory | under 50 MB | n/a (someone else's cloud) |
| Themes, profile colours, avatar rings | free | paid |
| Who owns your messages | you (one SQLite file) | them |

Numbers were measured on Linux with the release build. In low memory mode (the default)
the client draws on the CPU, which avoids the GPU driver's memory overhead. You can turn it
off in **Settings → Appearance** for GPU rendering.

## Features

- **Discord-style layout**: server bar, channel list, chat, member list, user panel
- Real-time text chat with message grouping, edit/delete, typing indicators, unread channels
- Online/offline presence, clickable profile cards
- **Profile customization**: display name, custom status, bio, name/avatar colour,
  banner colour, avatar symbol, and decorative avatar rings (thin, thick, double) in any colour
- **Themes**: 7 built-in (Classic Dark, Light, Midnight, Ocean, Forest, Sakura, Ember), plus
  your own `.toml` theme files with every colour editable
- Text size slider and compact mode
- Invite-code sign-up so strangers can't join; the first account becomes the server owner
- Owner can create and delete channels and remove any message
- Auto-reconnect if the connection drops; stays logged in between launches

**Coming next:** voice channels (see [Roadmap](#roadmap)).

| Profile settings | Themes |
|---|---|
| ![Profile settings](docs/profile-settings.png) | ![Themes](docs/themes.png) |

---

## Getting the apps

There are two programs:

- `syncord-server` runs on the one PC that hosts everything (your friend's PC).
- `syncord` is the app everyone opens to chat.

**Easiest:** open this repo's **Actions** tab on GitHub, click the latest successful
**Build** run, and download `syncord-windows`, `syncord-macos` or `syncord-linux` from the
**Artifacts** section. Each zip contains both programs.

**From source:** install Rust from <https://rustup.rs>, then:

```bash
cargo build --release
```

The programs end up in `target/release/`.

---

## Hosting on a friend's PC

### 1. Start the server

Put `syncord-server` (`syncord-server.exe` on Windows) in its own folder, for example
`C:\SynCord`, and run it once. It creates two files next to itself:

- `syncord-server.toml`: the settings
- `syncord.db`: the database that holds all accounts, channels and messages

The console prints the **invite code** your friends need to sign up. Open
`syncord-server.toml` to change it or to rename the server:

```toml
bind = "0.0.0.0:7878"          # listen on port 7878 on every network card
community_name = "The Hangout" # shown at the top of the channel list
invite_code = "pick-something" # needed to create an account; "" lets anyone sign up
database = "syncord.db"
```

Restart the server after editing. To check it's running, open <http://localhost:7878> on that
PC. It should say *SynCord server is running.*

### 2. Let your friends reach it

Pick **one** of these.

#### Option A: Tailscale (recommended)

[Tailscale](https://tailscale.com) is free for personal use. It puts everyone's PCs on a
private, encrypted network, so there's no router setup and nothing exposed to the internet.

1. Install Tailscale on the server PC and on each friend's PC, signed in to the same tailnet.
   Or install it on the server PC only and [share that machine](https://tailscale.com/kb/1084/sharing)
   with your friends' accounts.
2. Find the server PC's Tailscale address (it looks like `100.x.y.z`) in the Tailscale app.
3. Everyone types that address into SynCord's **Server address** box.

#### Option B: Port forwarding

1. In the router's settings, forward **TCP port 7878** to the server PC's local IP address.
2. Allow the port through the firewall. On Windows, run this in an admin PowerShell:

   ```powershell
   New-NetFirewallRule -DisplayName "SynCord" -Direction Inbound -Protocol TCP -LocalPort 7878 -Action Allow
   ```
3. Friends use your home's public IP address (search "what is my IP" on the server PC).

> **Heads up:** with plain port forwarding, traffic (including passwords) is not encrypted.
> Tailscale avoids this. If you'd rather use port forwarding, put a reverse proxy such as
> [Caddy](https://caddyserver.com) in front of SynCord with a domain name. Caddy gets an HTTPS
> certificate automatically, and friends then connect with `wss://your.domain`.

On the same Wi-Fi as the server you can always use its local IP (e.g. `192.168.1.20`).

### 3. Keep it running

**Windows:** open *Task Scheduler* and choose **Create Task**:
- *Triggers*: **At startup**
- *Actions*: **Start a program**, pointing to `C:\SynCord\syncord-server.exe`, with
  *Start in* set to `C:\SynCord`
- *General*: tick **Run whether user is logged on or not**

**Linux:** create `/etc/systemd/system/syncord.service`:

```ini
[Unit]
Description=SynCord chat server
After=network-online.target

[Service]
WorkingDirectory=/opt/syncord
ExecStart=/opt/syncord/syncord-server
Restart=always
User=syncord

[Install]
WantedBy=multi-user.target
```

Then run:

```bash
sudo systemctl enable --now syncord
```

### 4. Create accounts

Open `syncord`, click **Register**, enter the server address, a username, a password and the
invite code. **The first account created becomes the server owner**, so the host should
register first. Only the owner can create and delete channels.

### Backups

Everything lives in `syncord.db`. Stop the server and copy `syncord.db` (plus any
`syncord.db-wal` / `syncord.db-shm` files next to it) somewhere safe.

---

## Customization

### Profile

**Settings → My Profile**: display name, status, bio, avatar symbol (any 1–2 characters),
name colour, banner colour, avatar ring style and ring colour. A live preview shows exactly
what your friends will see. Changes reach everyone the moment you save.

### Themes

**Settings → Appearance** has the built-in themes. To make your own:

1. Open the themes folder:
   - Windows: `%APPDATA%\syncord\themes`
   - macOS: `~/Library/Application Support/syncord/themes`
   - Linux: `~/.config/syncord/themes`
2. Copy `example.toml`, give it a new `name`, and change any colours.
3. Press **Reload themes**.

```toml
name = "My Theme"
rail = "#1E1F22"        # far-left server bar
sidebar = "#2B2D31"     # channel list and member list
user_panel = "#232428"  # your name/avatar at the bottom-left
chat = "#313338"        # main chat area
floating = "#383A40"    # message box, popups, cards
hover = "#35373C"       # hovered rows
selected = "#404249"    # selected channel
text = "#DBDEE1"        # message text
muted = "#949BA4"       # timestamps, hints, channel names
header = "#F2F3F5"      # headings and names
accent = "#EB459E"      # buttons and highlights
online = "#23A55A"
danger = "#F23F43"
font_size = 15.0
radius = 8.0            # corner roundness
```

Themes are plain files, so you can send them to friends.

---

## Roadmap

1. **Voice channels**: Opus audio (the same codec the big apps use), noise suppression
   with `nnnoiseless`, push-to-talk or voice activation. The server relays audio between the
   people in a channel.
2. Image and file uploads, link previews
3. Markdown formatting, emoji picker, replies, @mentions and desktop notifications
4. Custom server emoji, animated avatars, uploadable avatar and banner images
5. Screen sharing

## Project layout

```
crates/
  proto/   shared message types (the "language" client and server speak)
  server/  axum + tokio WebSocket server, SQLite storage
  client/  iced desktop app
```

Run the tests with:

```bash
cargo test --workspace
```

They include an end-to-end test that starts a real server and drives it with two clients.
