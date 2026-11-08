# SynCord

A lightweight, self-hosted chat app for you and your friends, written entirely in Rust.
It has text and voice channels and looks and feels like the chat apps you're used to, but it's a
native program instead of a web browser in disguise, so it's tiny, and every customization
feature is free.

![SynCord main window](docs/screenshot.png)

## Why

| | SynCord | Typical Electron chat app |
|---|---|---|
| Client memory (idle, one server) | **~29 MB** (low memory mode) | 300 MB to 1 GB+ |
| Client memory (in a voice call) | **~31 MB** | higher still |
| Client download | ~6 MB installer | 100 MB+ installer |
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
- Auto-reconnect if the connection drops (and rejoins your voice call); stays logged in between launches

### Voice chat

- **Voice channels** with everyone in them listed underneath, and a green ring around whoever is talking
- **Opus** audio at 64 kbps (the codec the big apps use), with error correction for lost packets
- **Noise suppression** (an RNNoise port) that removes fans, keyboard clatter and hum
- **Voice activity detection** with an adjustable sensitivity and a live level meter, so you
  only send audio while you're talking
- **Mute and deafen** buttons, shown to everyone in the channel
- **Per-person volume**: click someone in your call to turn them up or down
- Pick your microphone and speakers, input and output volume, and a **mic test** that plays you back
- Uses **UDP** for the lowest delay and switches automatically to the normal connection if UDP is
  blocked, so voice still works through strict networks and HTTPS proxies
- A jitter buffer smooths out network hiccups

| Profile settings | Themes | Voice settings |
|---|---|---|
| ![Profile settings](docs/profile-settings.png) | ![Themes](docs/themes.png) | ![Voice settings](docs/voice-settings.png) |

---

## Getting the apps

**Download the latest release:** <https://github.com/bxpo/SynCord/releases/latest>

Send that link to your friends. The release page explains which file each person needs:

| | Everyone (to chat) | The host PC (to run the server) |
|---|---|---|
| **Windows** | `SynCord-Setup-x.y.z.exe` | `SynCord-Server-Setup-x.y.z.exe` |
| **Mac** | `SynCord-x.y.z-macOS.dmg` (Intel and Apple Silicon) | `SynCord-Server-x.y.z-macOS.zip` |
| **Ubuntu / Debian** | `syncord_x.y.z_amd64.deb` | `syncord-server_x.y.z_amd64.deb` |
| **Other Linux** | `syncord-x.y.z-linux-x86_64.tar.gz` | `syncord-server-x.y.z-linux-x86_64.tar.gz` |

- The **Windows server installer** does the whole hosting setup: it starts the server with Windows,
  opens the firewall and shows the invite code. On Ubuntu/Debian the server `.deb` sets up a
  background service the same way.
- The apps aren't signed with paid certificates. **Windows** may say *"Windows protected your PC"*
  (click **More info → Run anyway**), and **macOS** needs you to allow the app the first time
  (macOS 15+: **System Settings → Privacy & Security → Open Anyway**; older: right-click → **Open**).
- **Phones and tablets** aren't supported yet. SynCord is a desktop app for now.

### Making a new release

Change `version` in `Cargo.toml`, push, then on GitHub open **Actions → Release → Run workflow**.
It builds every installer, tests the Windows ones on a real Windows machine, and publishes a
release named after the version. The packaging scripts are in [`packaging/`](packaging/).

**From source:** install Rust from <https://rustup.rs> and [CMake](https://cmake.org) (used to
build the Opus audio codec). On Linux, also install the ALSA headers (`libasound2-dev` on
Debian/Ubuntu, `alsa-lib-devel` on Fedora). Then:

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

1. In the router's settings, forward port **7878** for both **TCP** (chat) and **UDP** (voice)
   to the server PC's local IP address. If you can only forward TCP, voice still works over
   TCP, just with slightly more delay.
2. Allow the port through the firewall. On Windows, run these in an admin PowerShell:

   ```powershell
   New-NetFirewallRule -DisplayName "SynCord" -Direction Inbound -Protocol TCP -LocalPort 7878 -Action Allow
   ```

   ```powershell
   New-NetFirewallRule -DisplayName "SynCord voice" -Direction Inbound -Protocol UDP -LocalPort 7878 -Action Allow
   ```
3. Friends use your home's public IP address (search "what is my IP" on the server PC).

> **Heads up:** with plain port forwarding, traffic (including passwords) is not encrypted.
> Tailscale avoids this. If you'd rather use port forwarding, put a reverse proxy such as
> [Caddy](https://caddyserver.com) in front of SynCord with a domain name. Caddy gets an HTTPS
> certificate automatically, and friends then connect with `wss://your.domain`. Voice then
> travels through the same encrypted connection.

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

### Server troubleshooting

**Is it running?** On the server PC, open <http://localhost:7878> in a browser. It should say
*SynCord server is running.*

**Windows server installer:**
- If setup finds a problem it says what it was, and writes the details to
  `C:\ProgramData\SynCord Server\setup.log`.
- Start menu → **SynCord Server → Run server in a window (troubleshooting)** runs the server in a
  window so you can read any error. If it says the port is already in use, the server is already
  running in the background, which is good.
- The server's own log is `C:\ProgramData\SynCord Server\syncord-server.log`.
- **Firewall:** if another antivirus or firewall manages your PC's firewall instead of Windows (Norton,
  McAfee, Bitdefender and so on), allow `syncord-server.exe` in that program.
- **Starting it by hand:** open PowerShell **as administrator** and run:

  ```powershell
  & "C:\Program Files\SynCord Server\syncord-server.exe" "C:\ProgramData\SynCord Server\syncord-server.toml"
  ```

  It runs until you close that window.

**Linux service:** check `sudo systemctl status syncord-server`, and see the log with
`sudo journalctl -u syncord-server`.

### Voice tips

- **Use headphones.** There's no echo cancellation yet, so speakers can feed your friends' voices
  back into your mic.
- Open **Settings → Voice & Audio** and press **Let's Check** to hear yourself and set the
  sensitivity: the bar should turn green when you talk and stay yellow when you're quiet.
- The **Voice Connected** box shows **direct (UDP)** or **relayed (TCP)**. Relayed works fine; if
  you'd like the lower delay, open UDP port 7878 as described above.
- **macOS** asks for microphone permission the first time you join a call. If you start SynCord
  from Terminal, the permission is granted to Terminal.

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

1. Echo cancellation and push-to-talk (needs a global hotkey that works while the app is in
   the background)
2. Image and file uploads, link previews
3. Markdown formatting, emoji picker, replies, @mentions and desktop notifications
4. Custom server emoji, animated avatars, uploadable avatar and banner images
5. Screen sharing

## Project layout

```
crates/
  proto/   shared message types (the "language" client and server speak)
  server/  axum + tokio WebSocket server, SQLite storage, voice relay (UDP + WebSocket)
  client/  iced desktop app
    src/voice/  audio engine: cpal devices, noise suppression, voice gate,
                Opus, jitter buffer, mixer
```

Run the tests with:

```bash
cargo test --workspace
```

They include end-to-end tests that start a real server and drive it with two clients, including
one that sends real Opus audio both ways (over UDP in one direction and the WebSocket fallback
in the other) and checks it comes out the other end.
