//! Desktop notifications for messages you'd want to know about while Celerity isn't in
//! front: direct messages and @mentions by default, or every message (see Settings >
//! Notifications), except in channels and servers you've muted.
//!
//! Clicking one opens that conversation (through the same `celerity://` link a second
//! launch would pass along).

/// Shown once per message, on its own thread (showing can block for a moment, and
/// waiting for the click blocks until the notification goes away).
pub fn show(title: String, body: String, link: String) {
    let _ = std::thread::Builder::new().name("celerity-notify".into()).spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.summary(&title).body(&body).appname("Celerity");
        #[cfg(windows)]
        {
            n.app_id(APP_ID);
            if let Some(icon) = icon_file() {
                n.image_path(&icon.to_string_lossy());
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            n.icon("celerity").action("default", "Open");
        }
        let handle = match n.show() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("couldn't show a notification: {e}");
                return;
            }
        };
        let open = move || crate::tray::open_link(link);
        #[cfg(windows)]
        let _ = handle.wait_for_response(|r: &notify_rust::NotificationResponse| {
            if r.is_default_action() {
                open();
            }
        });
        #[cfg(not(windows))]
        handle.wait_for_action(|action| {
            if action == "default" {
                open();
            }
        });
    });
}

/// What the notification says: who and where, and the message in one plain line.
pub fn text_of(content: &str, attachments: usize) -> String {
    let plain: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut body =
        if plain.chars().count() > 180 { format!("{}…", plain.chars().take(180).collect::<String>()) } else { plain };
    if body.is_empty() {
        body = match attachments {
            0 => String::new(),
            1 => "Sent a file".into(),
            n => format!("Sent {n} files"),
        };
    }
    body
}

/// Windows shows a notification under an app id it knows the name and icon of.
#[cfg(windows)]
pub const APP_ID: &str = "io.github.bxpo.Celerity";

#[cfg(windows)]
fn icon_file() -> Option<std::path::PathBuf> {
    let path = dirs::config_dir()?.join("celerity").join("notification-icon.png");
    if !path.exists() {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, include_bytes!("../../../packaging/icons/celerity-128.png")).ok()?;
    }
    Some(path)
}

/// Tell Windows who `APP_ID` is (name and icon in notifications), and that
/// `celerity://` links open this program. Per user, no admin rights needed.
#[cfg(windows)]
pub fn register() {
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
        RegSetValueExW,
    };
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn set(path: &str, name: Option<&str>, value: &str) {
        let mut key: HKEY = std::ptr::null_mut();
        let path = wide(path);
        // SAFETY: plain registry calls with valid, NUL-terminated wide strings.
        unsafe {
            if RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            ) != 0
            {
                return;
            }
            let name = name.map(wide);
            let value = wide(value);
            RegSetValueExW(
                key,
                name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            );
            RegCloseKey(key);
        }
    }
    let base = format!("Software\\Classes\\AppUserModelId\\{APP_ID}");
    set(&base, Some("DisplayName"), "Celerity");
    if let Some(icon) = icon_file() {
        set(&base, Some("IconUri"), &icon.to_string_lossy());
    }
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.to_string_lossy();
        set("Software\\Classes\\celerity", None, "URL:Celerity");
        set("Software\\Classes\\celerity", Some("URL Protocol"), "");
        set("Software\\Classes\\celerity\\DefaultIcon", None, &format!("\"{exe}\",0"));
        set("Software\\Classes\\celerity\\shell\\open\\command", None, &format!("\"{exe}\" \"%1\""));
    }
}

#[cfg(not(windows))]
pub fn register() {}

// ---- When to notify

use celerity_proto::Message as ChatMessage;

use crate::app::{App, Message};

impl App {
    /// A message arrived (on the open server or one in the background): pop up a
    /// notification if it's one you'd want to hear about.
    pub(crate) fn maybe_notify(&self, m: &ChatMessage) {
        let Some(s) = &self.session else { return };
        if !self.settings.desktop_notifications || self.dnd() || m.author_id == s.me || s.blocked.contains(&m.author_id)
        {
            return;
        }
        if self.settings.ignored_here(&self.settings.server).contains(&m.author_id) {
            return;
        }
        // You're looking at it.
        if self.focused && !self.background && s.current == Some(m.channel_id) {
            return;
        }
        let direct = s.is_conversation(m.channel_id);
        let mention = s.mentions_me(m);
        let muted = self.settings.is_muted(&self.settings.server, m.channel_id);
        let wanted = if direct { !muted } else { mention || (self.settings.notify_all && !muted) };
        if !wanted {
            return;
        }
        let author = s.users.get(&m.author_id).map(|u| u.name().to_string()).unwrap_or_else(|| "Someone".into());
        let title = match s.channels.iter().find(|c| c.id == m.channel_id) {
            Some(c) => format!("{author} (#{}, {})", c.name, s.community),
            None => author,
        };
        // Mentions read as names, not ids.
        let mut content = m.content.clone();
        for id in &m.mentions {
            if let Some(u) = s.users.get(id) {
                content = content.replace(&format!("<@{id}>"), &format!("@{}", u.name()));
            }
        }
        let link = format!("celerity://open/{}/{}", crate::net::server_key(&self.settings.server), m.channel_id);
        show(title, text_of(&content, m.attachments.len()), link);
    }

    /// A `celerity://` link: open the conversation it points to (on whichever server).
    pub(crate) fn open_link(&mut self, link: &str) -> iced::Task<Message> {
        let show = iced::window::latest().and_then(|id| {
            iced::Task::batch([iced::window::set_mode(id, iced::window::Mode::Windowed), iced::window::gain_focus(id)])
        });
        if link.starts_with("celerity://join/") {
            return iced::Task::batch([show, self.open_invite(link)]);
        }
        let Some(rest) = link.strip_prefix("celerity://open/") else { return show };
        // celerity://open/<server>/<channel>, with /<message> for a message link.
        let mut parts = rest.trim_end_matches('/').split('/');
        let (Some(server), Some(channel)) = (parts.next(), parts.next()) else { return show };
        let Ok(channel) = channel.parse::<i64>() else { return show };
        let message = parts.next().and_then(|m| m.parse::<i64>().ok());
        let switch = match self.rail().into_iter().find(|e| e.key == server) {
            Some(e) if !e.open => self.switch_server(e.position),
            _ => iced::Task::none(),
        };
        // A message in a thread opens its channel with the thread beside it.
        let thread = self.session.as_ref().and_then(|s| s.threads.get(&channel)).map(|t| t.parent_id);
        let known = self.session.as_ref().is_some_and(|s| {
            s.channels.iter().any(|c| c.id == channel) || s.is_conversation(channel) || thread.is_some()
        });
        let open = match (known, thread, message) {
            (false, _, _) => iced::Task::none(),
            (true, Some(parent), _) => iced::Task::batch([
                self.update(Message::SelectChannel(parent)),
                self.update(Message::Threads(crate::app_threads::ThreadMsg::Open(channel))),
            ]),
            (true, None, Some(m)) => self.update(Message::OpenSearchResult(channel, m)),
            (true, None, None) => self.update(Message::SelectChannel(channel)),
        };
        iced::Task::batch([show, switch, open])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn notification_text() {
        assert_eq!(super::text_of("hello\n\n  there", 0), "hello there");
        assert_eq!(super::text_of("", 2), "Sent 2 files");
        assert!(super::text_of(&"a".repeat(500), 0).ends_with('…'));
    }
}
