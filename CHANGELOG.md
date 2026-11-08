# Changelog

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
