//! Threads, group direct messages, polls and channel categories (added in 0.7).

use serde::{Deserialize, Serialize};

use crate::{ChannelId, MessageId, RoleId, UserId};

pub type CategoryId = i64;

/// A heading in the channel list that text and voice channels can sit under.
/// Limited to roles (see [`Category::allowed_roles`]), it hides the channels in it too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    pub id: CategoryId,
    pub name: String,
    pub position: i64,
    /// Empty: everyone sees it. Otherwise only people with one of these roles (and admins).
    /// Channels in it that have no roles of their own follow this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_roles: Vec<RoleId>,
}

/// Longest category name, in characters.
pub const MAX_CATEGORY_NAME: usize = 32;

/// Where a channel goes in [`crate::ClientMsg::ReorderChannels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPosition {
    pub id: ChannelId,
    pub position: i64,
    /// `None` puts it at the top level, outside every category.
    #[serde(default)]
    pub category_id: Option<CategoryId>,
}

/// Where a category goes in [`crate::ClientMsg::ReorderChannels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryPosition {
    pub id: CategoryId,
    pub position: i64,
}

/// A side conversation in a text channel, started from a message or on its own.
/// Its messages use the normal message requests with the thread's id as the channel id.
/// Whoever can see the parent channel can see and post in its threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ChannelId,
    /// The text channel it belongs to.
    pub parent_id: ChannelId,
    /// The message it was started from (it may have been deleted since).
    #[serde(default)]
    pub parent_message_id: Option<MessageId>,
    pub name: String,
    /// Who started it (they may rename and archive it).
    pub owner_id: UserId,
    /// Archived threads are hidden from the channel's list of active threads; posting
    /// in one brings it back.
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub message_count: u32,
    #[serde(default)]
    pub last_message_id: Option<MessageId>,
    /// Unix ms of the newest message (or of when it was started).
    #[serde(default)]
    pub last_activity: i64,
    #[serde(default)]
    pub created_at: i64,
}

impl Thread {
    pub fn summary(&self) -> ThreadSummary {
        ThreadSummary {
            id: self.id,
            name: self.name.clone(),
            message_count: self.message_count,
            archived: self.archived,
            last_activity: self.last_activity,
        }
    }
}

/// What a message that started a thread shows about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: ChannelId,
    pub name: String,
    #[serde(default)]
    pub message_count: u32,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub last_activity: i64,
}

/// Longest thread name, in characters.
pub const MAX_THREAD_NAME: usize = 100;

/// A direct-message conversation between three to ten people.
/// Messages, reactions, pins, read markers and calls work like in a direct message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupChannel {
    pub id: ChannelId,
    /// Empty: the app names it after its members.
    #[serde(default)]
    pub name: String,
    /// Who made it (or took over when they left): they may add, remove and rename.
    pub owner_id: UserId,
    /// Everyone in it, including you.
    pub members: Vec<UserId>,
    #[serde(default)]
    pub last_message_id: Option<MessageId>,
}

/// Fewest people a new group can have (two people is a direct message).
pub const MIN_GROUP_SIZE: usize = 3;
/// Most people a group can have.
pub const MAX_GROUP_SIZE: usize = 10;
/// Longest group name, in characters.
pub const MAX_GROUP_NAME: usize = 64;

/// A question with answers people vote on. Carried by [`crate::Message::poll`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Poll {
    pub question: String,
    pub options: Vec<PollOption>,
    /// People may pick more than one answer.
    #[serde(default)]
    pub multiple: bool,
    /// When voting ends (Unix ms), if it does.
    #[serde(default)]
    pub ends_at: Option<i64>,
    /// Voting is over: the results are final.
    #[serde(default)]
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollOption {
    /// Its place in the list, starting at 0 (what [`crate::ClientMsg::Vote`] refers to).
    pub id: u32,
    pub text: String,
    /// Who picked it.
    #[serde(default)]
    pub votes: Vec<UserId>,
}

pub const MIN_POLL_OPTIONS: usize = 2;
pub const MAX_POLL_OPTIONS: usize = 10;
/// Longest question and answer, in characters.
pub const MAX_POLL_QUESTION: usize = 300;
pub const MAX_POLL_OPTION: usize = 55;
/// Longest a poll can run.
pub const MAX_POLL_DURATION_SECS: u64 = 32 * 24 * 3600;

impl Poll {
    /// Total number of people who voted.
    pub fn voters(&self) -> usize {
        let mut all: Vec<UserId> = self.options.iter().flat_map(|o| o.votes.iter().copied()).collect();
        all.sort_unstable();
        all.dedup();
        all.len()
    }
}
