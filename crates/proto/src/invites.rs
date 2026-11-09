//! Invite links (added in 0.7): tokens people with Create invites make, which work
//! instead of the server's invite code when registering.
//!
//! A link looks like `celerity://join/<host:port>/<token>`. The server also serves a small
//! web page at `/invite/<token>` with an "Open in Celerity" button, for sharing in places
//! where `celerity://` links aren't clickable.

use serde::{Deserialize, Serialize};

use crate::UserId;

/// The web page for an invite: `GET /invite/<token>`.
pub const INVITE_PAGE_PATH: &str = "/invite";
/// Where the invite page sends people who don't have the app yet.
pub const DOWNLOAD_URL: &str = "https://github.com/bxpo/SynCord/releases/latest";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    /// Put this in the invite box when registering (or open [`invite_link`]).
    pub token: String,
    pub created_by: UserId,
    /// Unix ms.
    pub created_at: i64,
    /// Stops working after this (Unix ms).
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Stops working after this many accounts were made with it.
    #[serde(default)]
    pub max_uses: Option<u32>,
    #[serde(default)]
    pub uses: u32,
}

impl Invite {
    /// Whether it can still be used at `now` (Unix ms).
    pub fn usable(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|t| now < t) && self.max_uses.is_none_or(|m| self.uses < m)
    }
}

/// `celerity://join/<address>/<token>`, where `address` is `host:port` as typed into the app.
pub fn invite_link(address: &str, token: &str) -> String {
    format!("celerity://join/{}/{token}", address.trim().trim_end_matches('/'))
}

/// The server address and token of an invite link.
pub fn parse_invite_link(link: &str) -> Option<(String, String)> {
    let rest = link.trim().strip_prefix("celerity://join/")?;
    let (address, token) = rest.trim_end_matches('/').rsplit_once('/')?;
    let token_ok = !token.is_empty() && token.len() <= 64 && token.bytes().all(|b| b.is_ascii_alphanumeric());
    (!address.is_empty() && token_ok).then(|| (address.to_string(), token.to_string()))
}

/// Longest an invite can last, and most uses it can have.
pub const MAX_INVITE_AGE_SECS: u64 = 365 * 24 * 3600;
pub const MAX_INVITE_USES: u32 = 1000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        let link = invite_link("chat.example.com:7878", "AbC123");
        assert_eq!(link, "celerity://join/chat.example.com:7878/AbC123");
        assert_eq!(parse_invite_link(&link), Some(("chat.example.com:7878".into(), "AbC123".into())));
        assert_eq!(parse_invite_link("celerity://join/[::1]:7878/x9/"), Some(("[::1]:7878".into(), "x9".into())));
        assert_eq!(parse_invite_link("https://example.com/x"), None);
        assert_eq!(parse_invite_link("celerity://join/host/bad token"), None);
        let i = Invite {
            token: "t".into(),
            created_by: 1,
            created_at: 0,
            expires_at: Some(10),
            max_uses: Some(2),
            uses: 1,
        };
        assert!(i.usable(5));
        assert!(!i.usable(10));
        assert!(!Invite { uses: 2, ..i }.usable(5));
    }
}
