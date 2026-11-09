//! Requests added in 0.7: categories and channel order, private channels, threads,
//! group direct messages and polls.

use super::*;
use celerity_proto::social::{
    MAX_CATEGORY_NAME, MAX_GROUP_NAME, MAX_GROUP_SIZE, MAX_POLL_DURATION_SECS, MAX_POLL_OPTION, MAX_POLL_OPTIONS,
    MAX_POLL_QUESTION, MAX_THREAD_NAME, MIN_GROUP_SIZE, MIN_POLL_OPTIONS,
};
use celerity_proto::{GroupChannel, Poll, PollOption};

/// Trim, drop control characters and cut to `max` characters.
fn clean_text(s: &str, max: usize) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Everyone in `before` (`None`: all members) who isn't in `after`.
fn lost_sight(before: Option<Vec<UserId>>, after: &Option<Vec<UserId>>, all: Vec<UserId>) -> Vec<UserId> {
    let before = before.unwrap_or(all);
    match after {
        None => Vec::new(),
        Some(after) => before.into_iter().filter(|u| !after.contains(u)).collect(),
    }
}

/// Announce the final results of polls whose time is up.
pub async fn close_due_polls(state: &Arc<AppState>) -> Result<(), rusqlite::Error> {
    let closed = state
        .db
        .call(|c| {
            let mut out = Vec::new();
            for id in db::due_polls(c, db::now_ms())? {
                db::close_poll(c, id)?;
                if let Some(m) = db::get_message(c, id)?
                    && let Some(poll) = m.poll
                {
                    out.push((m.channel_id, id, poll));
                }
            }
            Ok(out)
        })
        .await?;
    for (channel_id, message_id, poll) in closed {
        let to = state.channel_target(channel_id).await?;
        state.broadcast_target(&ServerMsg::PollUpdated { channel_id, message_id, poll }, to);
    }
    Ok(())
}

impl Connection {
    /// The group, if `me` is in it (and whether they own it).
    async fn my_group(
        &mut self,
        me: UserId,
        channel_id: ChannelId,
    ) -> Result<Option<(GroupChannel, bool)>, rusqlite::Error> {
        let group = self.state.db.call(move |c| db::get_group(c, channel_id)).await?;
        Ok(group.filter(|g| g.members.contains(&me)).map(|g| {
            let owner = g.owner_id == me;
            (g, owner)
        }))
    }

    fn announce_group(&self, group: &GroupChannel) {
        self.broadcast_target(&ServerMsg::GroupUpdated(group.clone()), Target::only(group.members.clone()));
    }

    /// Someone left a group (or was removed): they're told, and taken out of its call.
    async fn left_group(&mut self, group: ChannelId, user: UserId, members: &[UserId]) {
        self.broadcast_to(&ServerMsg::GroupRemoved { channel_id: group }, Some(Arc::from([user])));
        if self.state.voice.channel_of(user) == Some(group) {
            self.state.voice.remove_user(user);
            let mut to = members.to_vec();
            to.push(user);
            self.broadcast_target(
                &ServerMsg::VoiceStateUpdate { user_id: user, state: None },
                Target::only(to).modern(true),
            );
            crate::calls::left(&self.state, group).await;
        }
    }

    /// The message a thread was started from changed what it shows about the thread.
    async fn parent_message_changed(&mut self, message: Option<MessageId>) -> Result<(), rusqlite::Error> {
        let Some(id) = message else { return Ok(()) };
        if let Some(m) = self.state.db.call(move |c| db::get_message(c, id)).await? {
            let channel = m.channel_id;
            self.broadcast_in(channel, &ServerMsg::MessageUpdated(m)).await?;
        }
        Ok(())
    }

    /// Handle a 0.7 social request.
    pub(super) async fn handle_social(&mut self, me: UserId, msg: ClientMsg) -> Result<Flow, rusqlite::Error> {
        let db = self.state.db.clone();
        let manage_channels = move |c: &rusqlite::Connection| db::access(c, me).map(|a| a.can(perms::MANAGE_CHANNELS));
        match msg {
            // ---- Categories and channels
            ClientMsg::CreateCategory { name } => {
                let name = clean_text(&name, MAX_CATEGORY_NAME);
                if name.is_empty() {
                    return Ok(self.error("Give the category a name").await);
                }
                let made = db
                    .call(move |c| if manage_channels(c)? { db::create_category(c, &name).map(Some) } else { Ok(None) })
                    .await?;
                match made {
                    Some(cat) => self.broadcast(&ServerMsg::CategoryUpdated(cat)),
                    None => return Ok(self.error("You need the Manage channels permission").await),
                }
            }
            ClientMsg::UpdateCategory { category_id, name, allowed_roles } => {
                let name = name.map(|n| clean_text(&n, MAX_CATEGORY_NAME));
                if name.as_ref().is_some_and(String::is_empty) {
                    return Ok(self.error("Give the category a name").await);
                }
                let before = db.call(db::visibility_map).await?;
                let done = db
                    .call(move |c| {
                        if !manage_channels(c)? {
                            return Ok(Err("You need the Manage channels permission"));
                        }
                        if db::get_category(c, category_id)?.is_none() {
                            return Ok(Err("That category no longer exists"));
                        }
                        let saw = db::category_audience(c, category_id)?;
                        if let Some(name) = &name {
                            db::rename_category(c, category_id, name)?;
                        }
                        if let Some(roles) = &allowed_roles {
                            db::set_category_roles(c, category_id, &existing_roles(&db::all_roles(c)?, roles))?;
                        }
                        let sees = db::category_audience(c, category_id)?;
                        let all = db::all_users(c)?.into_iter().map(|u| u.id).collect();
                        let lost = lost_sight(saw, &sees, all);
                        Ok(Ok((db::get_category(c, category_id)?, sees, lost)))
                    })
                    .await?;
                match done {
                    Ok((Some(cat), sees, lost)) => {
                        if !lost.is_empty() {
                            self.broadcast_to(&ServerMsg::CategoryDeleted { category_id }, Some(Arc::from(lost)));
                        }
                        self.broadcast_to(&ServerMsg::CategoryUpdated(cat), sees.map(Arc::from));
                        announce_visibility(&self.state, before).await?;
                    }
                    Ok((None, ..)) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::DeleteCategory { category_id } => {
                let before = db.call(db::visibility_map).await?;
                let done = db
                    .call(move |c| {
                        if !manage_channels(c)? {
                            return Ok(Err("You need the Manage channels permission"));
                        }
                        let moved = db::delete_category(c, category_id)?;
                        let mut channels = Vec::new();
                        for id in moved {
                            channels.extend(db::get_channel(c, id)?);
                        }
                        Ok(Ok(channels))
                    })
                    .await?;
                match done {
                    Ok(channels) => {
                        self.broadcast(&ServerMsg::CategoryDeleted { category_id });
                        for ch in channels {
                            let id = ch.id;
                            self.broadcast_in(id, &ServerMsg::ChannelUpdated(ch)).await?;
                        }
                        announce_visibility(&self.state, before).await?;
                    }
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::UpdateChannel { channel_id, name, topic, allowed_roles } => {
                let before = db.call(db::visibility_map).await?;
                let done = db
                    .call(move |c| {
                        if !manage_channels(c)? {
                            return Ok(Err("You need the Manage channels permission"));
                        }
                        let Some(ch) = db::get_channel(c, channel_id)? else {
                            return Ok(Err("That channel no longer exists"));
                        };
                        let name = match name {
                            Some(n) if ch.kind == ChannelKind::Voice => clean_voice_channel_name(&n),
                            Some(n) => normalize_channel_name(&n),
                            None => ch.name.clone(),
                        };
                        if name.is_empty() {
                            return Ok(Err("Channel names need at least one letter or number"));
                        }
                        let topic = topic.map(|t| t.trim().chars().take(120).collect()).unwrap_or(ch.topic.clone());
                        db::update_channel(c, channel_id, &name, &topic)?;
                        if let Some(roles) = &allowed_roles {
                            db::set_channel_roles(c, channel_id, &existing_roles(&db::all_roles(c)?, roles))?;
                        }
                        Ok(Ok(db::get_channel(c, channel_id)?))
                    })
                    .await?;
                match done {
                    Ok(Some(ch)) => {
                        self.broadcast_in(channel_id, &ServerMsg::ChannelUpdated(ch)).await?;
                        announce_visibility(&self.state, before).await?;
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::ReorderChannels { categories, channels } => {
                if categories.len() > 500 || channels.len() > 500 {
                    return Ok(self.error("That's too many channels to move at once").await);
                }
                let before = db.call(db::visibility_map).await?;
                let (cats, chans) = (categories.clone(), channels.clone());
                let done = db
                    .call(move |c| {
                        if !manage_channels(c)? {
                            return Ok(Err("You need the Manage channels permission"));
                        }
                        if !db::reorder(c, &cats, &chans)? {
                            return Ok(Err("Something changed while you were moving things. Try again."));
                        }
                        let mut updated_cats = Vec::new();
                        for cat in &cats {
                            if let Some(x) = db::get_category(c, cat.id)? {
                                updated_cats.push((db::category_audience(c, x.id)?, x));
                            }
                        }
                        let mut updated = Vec::new();
                        for ch in &chans {
                            updated.extend(db::get_channel(c, ch.id)?);
                        }
                        Ok(Ok((updated_cats, updated)))
                    })
                    .await?;
                match done {
                    Ok((cats, chans)) => {
                        for (to, cat) in cats {
                            self.broadcast_to(&ServerMsg::CategoryUpdated(cat), to.map(Arc::from));
                        }
                        for ch in chans {
                            let id = ch.id;
                            self.broadcast_in(id, &ServerMsg::ChannelUpdated(ch)).await?;
                        }
                        announce_visibility(&self.state, before).await?;
                    }
                    Err(e) => return Ok(self.error(e).await),
                }
            }

            // ---- Threads
            ClientMsg::CreateThread { channel_id, message_id, name } => {
                let name = clean_text(&name, MAX_THREAD_NAME);
                let made = db
                    .call(move |c| {
                        if db::accessible_kind(c, me, channel_id)? != Some(ChannelKind::Text) {
                            return Ok(Err("Threads can only be started in text channels"));
                        }
                        if !db::access(c, me)?.can(perms::SEND_MESSAGES) {
                            return Ok(Err("You don't have permission to send messages here."));
                        }
                        let mut fallback = "New thread".to_string();
                        if let Some(mid) = message_id {
                            let Some(m) = db::get_message(c, mid)?.filter(|m| m.channel_id == channel_id) else {
                                return Ok(Err("That message no longer exists"));
                            };
                            if db::thread_for_message(c, mid)?.is_some() {
                                return Ok(Err("That message already has a thread"));
                            }
                            let snippet = clean_text(&m.content, 40);
                            if !snippet.is_empty() && m.encrypted.is_none() {
                                fallback = snippet;
                            }
                        }
                        let name = if name.is_empty() { fallback } else { name };
                        Ok(Ok(db::create_thread(c, channel_id, message_id, &name, me)?))
                    })
                    .await?;
                match made {
                    Ok(thread) => {
                        let (id, parent_message) = (thread.id, thread.parent_message_id);
                        self.broadcast_in(id, &ServerMsg::ThreadCreated(thread)).await?;
                        self.parent_message_changed(parent_message).await?;
                    }
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::ListThreads { channel_id } => {
                let threads = db
                    .call(move |c| match db::accessible_kind(c, me, channel_id)? {
                        Some(ChannelKind::Text) => db::threads_in(c, channel_id).map(Some),
                        _ => Ok(None),
                    })
                    .await?;
                match threads {
                    Some(threads) => {
                        self.send(&ServerMsg::Threads { channel_id, threads }).await;
                    }
                    None => return Ok(self.error("You can't see that channel").await),
                }
            }
            ClientMsg::UpdateThread { thread_id, name, archived } => {
                let name = name.map(|n| clean_text(&n, MAX_THREAD_NAME));
                if name.as_ref().is_some_and(String::is_empty) {
                    return Ok(self.error("Give the thread a name").await);
                }
                let done = db
                    .call(move |c| {
                        let Some(t) = db::get_thread(c, thread_id)? else {
                            return Ok(Err("That thread no longer exists"));
                        };
                        if db::accessible_kind(c, me, thread_id)?.is_none() {
                            return Ok(Err("That thread no longer exists"));
                        }
                        if t.owner_id != me && !db::access(c, me)?.can(perms::MANAGE_MESSAGES) {
                            return Ok(Err("Only whoever started the thread (or Manage messages) can change it"));
                        }
                        db::set_thread(
                            c,
                            thread_id,
                            name.as_deref().unwrap_or(&t.name),
                            archived.unwrap_or(t.archived),
                        )?;
                        Ok(Ok(db::get_thread(c, thread_id)?))
                    })
                    .await?;
                match done {
                    Ok(Some(t)) => {
                        let parent_message = t.parent_message_id;
                        self.broadcast_in(thread_id, &ServerMsg::ThreadUpdated(t)).await?;
                        self.parent_message_changed(parent_message).await?;
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::DeleteThread { thread_id } => {
                let to = self.state.channel_target(thread_id).await?;
                let done = db
                    .call(move |c| {
                        let Some(t) = db::get_thread(c, thread_id)? else {
                            return Ok(Err("That thread no longer exists"));
                        };
                        let access = db::access(c, me)?;
                        if db::accessible_kind(c, me, thread_id)?.is_none() {
                            return Ok(Err("That thread no longer exists"));
                        }
                        if !access.can(perms::MANAGE_MESSAGES) && !access.can(perms::MANAGE_CHANNELS) {
                            return Ok(Err("You need the Manage messages permission to delete threads"));
                        }
                        let files = db::delete_channel(c, thread_id)?.unwrap_or_default();
                        Ok(Ok((t, files)))
                    })
                    .await?;
                match done {
                    Ok((t, files)) => {
                        crate::http::remove_files(&self.state.config.uploads, &files).await;
                        self.broadcast_target(&ServerMsg::ThreadDeleted { thread_id, parent_id: t.parent_id }, to);
                        self.parent_message_changed(t.parent_message_id).await?;
                    }
                    Err(e) => return Ok(self.error(e).await),
                }
            }

            // ---- Groups
            ClientMsg::CreateGroup { members, name } => {
                let mut others: Vec<UserId> = members.into_iter().filter(|u| *u != me).collect();
                others.sort_unstable();
                others.dedup();
                let total = others.len() + 1;
                if !(MIN_GROUP_SIZE..=MAX_GROUP_SIZE).contains(&total) {
                    return Ok(self
                        .error(format!("Groups have {MIN_GROUP_SIZE} to {MAX_GROUP_SIZE} people, including you"))
                        .await);
                }
                let name = clean_text(&name, MAX_GROUP_NAME);
                let made = db
                    .call(move |c| {
                        for u in &others {
                            match db::get_user(c, *u)? {
                                Some(user) if !user.removed => {}
                                _ => return Ok(Err("Some of those people aren't on this server")),
                            }
                            if db::blocked_between(c, me, *u)? {
                                return Ok(Err("You can't add some of those people right now"));
                            }
                        }
                        let id = db::create_group(c, me, &others, &name)?;
                        Ok(Ok(db::get_group(c, id)?))
                    })
                    .await?;
                match made {
                    Ok(Some(group)) => self.announce_group(&group),
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::AddGroupMember { channel_id, user_id } => {
                let Some((group, owner)) = self.my_group(me, channel_id).await? else {
                    return Ok(self.error("That group no longer exists").await);
                };
                if !owner {
                    return Ok(self.error("Only the group's owner can add people").await);
                }
                if group.members.contains(&user_id) {
                    return Ok(Flow::Continue);
                }
                if group.members.len() >= MAX_GROUP_SIZE {
                    return Ok(self.error(format!("Groups can have at most {MAX_GROUP_SIZE} people")).await);
                }
                let added = db
                    .call(move |c| {
                        match db::get_user(c, user_id)? {
                            Some(u) if !u.removed => {}
                            _ => return Ok(Err("That person isn't on this server")),
                        }
                        if db::blocked_between(c, me, user_id)? {
                            return Ok(Err("You can't add this person right now"));
                        }
                        db::add_group_member(c, channel_id, user_id)?;
                        Ok(Ok(db::get_group(c, channel_id)?))
                    })
                    .await?;
                match added {
                    Ok(Some(group)) => self.announce_group(&group),
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::RemoveGroupMember { channel_id, user_id } => {
                let Some((group, owner)) = self.my_group(me, channel_id).await? else {
                    return Ok(self.error("That group no longer exists").await);
                };
                if user_id == me {
                    return Ok(self.error("Use Leave group to leave it yourself").await);
                }
                if !owner {
                    return Ok(self.error("Only the group's owner can remove people").await);
                }
                if !group.members.contains(&user_id) {
                    return Ok(Flow::Continue);
                }
                let files = db.call(move |c| db::remove_group_member(c, channel_id, user_id)).await?;
                crate::http::remove_files(&self.state.config.uploads, &files).await;
                if let Some((group, _)) = self.my_group(me, channel_id).await? {
                    self.announce_group(&group);
                    self.left_group(channel_id, user_id, &group.members).await;
                }
            }
            ClientMsg::RenameGroup { channel_id, name } => {
                let Some((_, owner)) = self.my_group(me, channel_id).await? else {
                    return Ok(self.error("That group no longer exists").await);
                };
                if !owner {
                    return Ok(self.error("Only the group's owner can rename it").await);
                }
                let name = clean_text(&name, MAX_GROUP_NAME);
                let group = db
                    .call(move |c| {
                        db::rename_group(c, channel_id, &name)?;
                        db::get_group(c, channel_id)
                    })
                    .await?;
                if let Some(group) = group {
                    self.announce_group(&group);
                }
            }
            ClientMsg::LeaveGroup { channel_id } => {
                if self.my_group(me, channel_id).await?.is_none() {
                    return Ok(self.error("You're not in that group").await);
                }
                let (files, group) = db
                    .call(move |c| {
                        let files = db::remove_group_member(c, channel_id, me)?;
                        Ok((files, db::get_group(c, channel_id)?))
                    })
                    .await?;
                crate::http::remove_files(&self.state.config.uploads, &files).await;
                let rest = group.as_ref().map(|g| g.members.clone()).unwrap_or_default();
                if let Some(group) = &group {
                    self.announce_group(group);
                }
                self.left_group(channel_id, me, &rest).await;
            }

            // ---- Polls
            ClientMsg::SendPoll { channel_id, question, options, multiple, duration_secs } => {
                let question = clean_text(&question, MAX_POLL_QUESTION + 1);
                let options: Vec<String> = options.iter().map(|o| clean_text(o, MAX_POLL_OPTION + 1)).collect();
                if question.is_empty() || question.chars().count() > MAX_POLL_QUESTION {
                    return Ok(self.error(format!("Questions are 1 to {MAX_POLL_QUESTION} characters")).await);
                }
                if !(MIN_POLL_OPTIONS..=MAX_POLL_OPTIONS).contains(&options.len())
                    || options.iter().any(|o| o.is_empty() || o.chars().count() > MAX_POLL_OPTION)
                {
                    return Ok(self
                        .error(format!(
                            "Polls have {MIN_POLL_OPTIONS} to {MAX_POLL_OPTIONS} answers of up to {MAX_POLL_OPTION} characters"
                        ))
                        .await);
                }
                if duration_secs.is_some_and(|d| d == 0 || d > MAX_POLL_DURATION_SECS) {
                    return Ok(self.error("Polls can run for at most 32 days").await);
                }
                // Older apps see the poll written out.
                let mut content = format!("📊 {question}");
                for o in &options {
                    content.push_str("\n• ");
                    content.push_str(o);
                }
                let poll = Poll {
                    question,
                    options: options
                        .into_iter()
                        .enumerate()
                        .map(|(i, text)| PollOption { id: i as u32, text, votes: Vec::new() })
                        .collect(),
                    multiple,
                    ends_at: duration_secs.map(|d| db::now_ms() + d as i64 * 1000),
                    closed: false,
                };
                let extras = db::MessageExtras { poll: Some(poll), ..Default::default() };
                return self.post(me, channel_id, content, Vec::new(), None, extras).await;
            }
            ClientMsg::Vote { message_id, option, voted } => {
                let done = db
                    .call(move |c| {
                        let Some(m) = db::get_message(c, message_id)? else {
                            return Ok(Err("That poll no longer exists"));
                        };
                        if db::accessible_kind(c, me, m.channel_id)?.is_none() {
                            return Ok(Err("That poll no longer exists"));
                        }
                        let Some(poll) = m.poll else { return Ok(Err("That message isn't a poll")) };
                        if poll.closed {
                            return Ok(Err("This poll has ended"));
                        }
                        if !poll.options.iter().any(|o| o.id == option) {
                            return Ok(Err("That answer isn't in the poll"));
                        }
                        if !db::vote(c, message_id, option, me, voted, poll.multiple)? {
                            return Ok(Ok(None));
                        }
                        Ok(Ok(db::get_poll(c, message_id)?.map(|p| (m.channel_id, p))))
                    })
                    .await?;
                match done {
                    Ok(Some((channel_id, poll))) => {
                        self.broadcast_in(channel_id, &ServerMsg::PollUpdated { channel_id, message_id, poll }).await?;
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::EndPoll { message_id } => {
                let done = db
                    .call(move |c| {
                        let Some(m) = db::get_message(c, message_id)? else {
                            return Ok(Err("That poll no longer exists"));
                        };
                        let Some(kind) = db::accessible_kind(c, me, m.channel_id)? else {
                            return Ok(Err("That poll no longer exists"));
                        };
                        let Some(poll) = &m.poll else { return Ok(Err("That message isn't a poll")) };
                        let moderator = is_server_text(kind) && db::access(c, me)?.can(perms::MANAGE_MESSAGES);
                        if m.author_id != me && !moderator {
                            return Ok(Err("Only whoever made the poll can end it"));
                        }
                        if poll.closed {
                            return Ok(Ok(None));
                        }
                        db::close_poll(c, message_id)?;
                        Ok(Ok(db::get_poll(c, message_id)?.map(|p| (m.channel_id, p))))
                    })
                    .await?;
                match done {
                    Ok(Some((channel_id, poll))) => {
                        self.broadcast_in(channel_id, &ServerMsg::PollUpdated { channel_id, message_id, poll }).await?;
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            _ => unreachable!("handle_social only gets 0.7 social requests"),
        }
        Ok(Flow::Continue)
    }
}
