## Which file do I need?

**To chat** (everyone):

| Your computer | Download |
|---|---|
| Windows 10 / 11 | `Celerity-Setup-@VERSION@.exe` |
| Mac (Intel or Apple Silicon, macOS 12.3+) | `Celerity-@VERSION@-macOS.dmg` |
| Ubuntu, Debian, Mint, Pop!_OS | `celerity_@VERSION@_amd64.deb` |
| Other Linux | `celerity-@VERSION@-linux-x86_64.tar.gz` |

**To host the server** (only the one PC that runs it):

| Server computer | Download |
|---|---|
| Windows | `Celerity-Server-Setup-@VERSION@.exe` |
| Ubuntu / Debian | `celerity-server_@VERSION@_amd64.deb` |
| Mac | `Celerity-Server-@VERSION@-macOS.zip` |
| Other Linux | `celerity-server-@VERSION@-linux-x86_64.tar.gz` |

## Installing

**Windows:** run `Celerity-Setup-@VERSION@.exe`. Windows may show *"Windows protected your PC"* because
the installer isn't signed with a paid certificate: click **More info → Run anyway**.

**Mac:** open the `.dmg` and drag **Celerity** into **Applications**. The first time, macOS will say it
can't check the app for malware (it isn't notarized by Apple):
- macOS 15 and newer: try to open it once, then go to **System Settings → Privacy & Security**, scroll
  down and click **Open Anyway**.
- macOS 14 and older: **right-click** Celerity in Applications and choose **Open**, then **Open** again.

macOS will also ask for microphone access the first time you join a voice channel, and camera
access the first time you turn on your camera: click **Allow**. To share your screen, allow
Celerity in **System Settings → Privacy & Security → Screen Recording**, then reopen Celerity.

**Ubuntu / Debian:** double-click the `.deb`, or run `sudo apt install ./celerity_@VERSION@_amd64.deb`.

**Other Linux:** unpack the `.tar.gz` and run `./install.sh` (or just `./celerity`).

## Joining

Open Celerity, click **Register**, and enter the **server address**, a username, a password and the
**invite code** from whoever runs the server. Use headphones in voice channels.

## Hosting

Run the server installer on a PC that stays on. If setup reports a problem, or friends can't
connect, see [server troubleshooting](https://github.com/bxpo/SynCord#server-troubleshooting).
Open <http://localhost:7878> on the server PC to check it's running. On Windows it starts automatically with the PC,
opens the firewall, and shows the invite code at the end (later: Start menu → **Celerity Server →
Server settings and invite code**). Register your own account first: the first account becomes
the server owner. See the [setup guide](https://github.com/bxpo/SynCord#hosting-on-a-friends-pc)
for letting friends connect from outside your home.
