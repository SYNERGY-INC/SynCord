//! Requests added in 0.5: roles, blocking and profile pictures.

use super::*;
use celerity_proto::{MAX_PINS, MessageKind};

impl Connection {
    /// Handle a 0.5 request; `None` if `msg` isn't one of them.
    pub(super) async fn handle_extra(&mut self, me: UserId, msg: ClientMsg) -> Result<Flow, rusqlite::Error> {
        let db = self.state.db.clone();
        match msg {
            ClientMsg::CreateRole { name } => {
                let name =
                    Role { id: 0, name, color: None, permissions: 0, position: 0, hoist: false }.sanitized().name;
                let created = db
                    .call(move |c| {
                        if !db::access(c, me)?.can(perms::MANAGE_ROLES) {
                            return Ok(Err("You need the Manage roles permission"));
                        }
                        if db::role_count(c)? >= MAX_ROLES {
                            return Ok(Err("This server has as many roles as it can have"));
                        }
                        db::create_role(c, &name)?;
                        Ok(Ok(db::all_roles(c)?))
                    })
                    .await?;
                match created {
                    // Positions shifted, so everyone gets every role again.
                    Ok(roles) => roles.into_iter().for_each(|r| self.broadcast(&ServerMsg::RoleUpdated(r))),
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::UpdateRole(role) => {
                let role = role.sanitized();
                let updated = db
                    .call(move |c| {
                        let access = db::access(c, me)?;
                        let Some(current) = db::get_role(c, role.id)? else { return Ok(Err("That role no longer exists")) };
                        let editable = if role.id == EVERYONE_ROLE {
                            access.can(perms::MANAGE_ROLES)
                        } else {
                            access.can_manage_role(&current)
                        };
                        if !editable {
                            return Ok(Err("You can only change roles below your own highest role"));
                        }
                        // Nobody can hand out permissions they don't have themselves.
                        let changed = role.permissions ^ current.permissions;
                        if !access.is_owner && changed & !access.permissions != 0 {
                            return Ok(Err("You can't give a role permissions you don't have"));
                        }
                        let mut next = role.clone();
                        if role.id == EVERYONE_ROLE {
                            next.name = current.name.clone();
                            next.color = None;
                            next.hoist = false;
                            next.position = 0;
                        } else {
                            let ceiling = if access.is_owner { i64::MAX } else { access.top - 1 };
                            next.position = role.position.clamp(1, ceiling.max(1));
                            if next.position != current.position {
                                // Insert at the new place: everything from there up moves up one.
                                c.execute(
                                    "UPDATE roles SET position = position + 1 WHERE position >= ?1 AND id NOT IN (?2, ?3)",
                                    rusqlite::params![next.position, next.id, EVERYONE_ROLE],
                                )?;
                            }
                        }
                        db::update_role(c, &next)?;
                        Ok(Ok(db::renumber_roles(c)?))
                    })
                    .await?;
                match updated {
                    Ok(roles) => roles.into_iter().for_each(|r| self.broadcast(&ServerMsg::RoleUpdated(r))),
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::DeleteRole { role_id } => {
                let deleted = db
                    .call(move |c| {
                        let access = db::access(c, me)?;
                        match db::get_role(c, role_id)? {
                            Some(r) if access.can_manage_role(&r) => {
                                db::delete_role(c, role_id)?;
                                Ok(Ok(db::renumber_roles(c)?))
                            }
                            Some(_) => Ok(Err("You can only delete roles below your own highest role")),
                            None => Ok(Err("That role no longer exists")),
                        }
                    })
                    .await?;
                match deleted {
                    Ok(roles) => {
                        self.broadcast(&ServerMsg::RoleDeleted { role_id });
                        roles.into_iter().for_each(|r| self.broadcast(&ServerMsg::RoleUpdated(r)));
                    }
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::SetUserRoles { user_id, roles } => {
                let updated = db
                    .call(move |c| {
                        let access = db::access(c, me)?;
                        if !access.can(perms::MANAGE_ROLES) {
                            return Ok(Err("You need the Manage roles permission"));
                        }
                        let Some(target) = db::get_user(c, user_id)? else {
                            return Ok(Err("That person doesn't exist"));
                        };
                        if target.is_owner && !access.is_owner {
                            return Ok(Err("Nobody can change the server owner's roles"));
                        }
                        let all = db::all_roles(c)?;
                        // Roles you can't manage stay exactly as they were.
                        let mut next: Vec<RoleId> = Vec::new();
                        for r in all.iter().filter(|r| r.id != EVERYONE_ROLE) {
                            let has = target.roles.contains(&r.id);
                            let wants = roles.contains(&r.id);
                            if if access.can_manage_role(r) { wants } else { has } {
                                next.push(r.id);
                            }
                        }
                        db::set_user_roles(c, user_id, &next)?;
                        Ok(Ok(db::get_user(c, user_id)?))
                    })
                    .await?;
                match updated {
                    Ok(Some(user)) => self.broadcast(&ServerMsg::UserUpdated(user)),
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::SetBlocked { user_id, blocked } => {
                if user_id == me {
                    return Ok(self.error("You can't block yourself").await);
                }
                let exists = db
                    .call(move |c| {
                        if db::get_user(c, user_id)?.is_none() {
                            return Ok(false);
                        }
                        db::set_blocked(c, me, user_id, blocked)?;
                        Ok(true)
                    })
                    .await?;
                if exists {
                    // Your other devices hear about it too.
                    self.broadcast_to(&ServerMsg::BlockUpdated { user_id, blocked }, Some(Arc::from([me])));
                }
            }
            ClientMsg::ClearProfileImage { banner } => {
                let cleared = db
                    .call(move |c| {
                        let Some(mut user) = db::get_user(c, me)? else { return Ok(None) };
                        let old = if banner { user.profile.banner.take() } else { user.profile.avatar.take() };
                        db::update_profile(c, me, &user.profile)?;
                        if let Some(old) = &old {
                            db::delete_profile_image(c, &old.id)?;
                        }
                        Ok(Some((user, old)))
                    })
                    .await?;
                if let Some((user, old)) = cleared {
                    if let Some(old) = old {
                        crate::http::remove_files(&self.state.config.uploads, &[old.id]).await;
                    }
                    self.broadcast(&ServerMsg::UserUpdated(user));
                }
            }
            ClientMsg::SetPinned { message_id, pinned } => {
                let done = db
                    .call(move |c| {
                        let Some(m) = db::get_message(c, message_id)? else {
                            return Ok(Err("That message no longer exists"));
                        };
                        // Server channels need Manage messages; in a DM either person can pin.
                        let allowed = match db::accessible_kind(c, me, m.channel_id)? {
                            Some(ChannelKind::Direct) => true,
                            Some(ChannelKind::Text) => db::access(c, me)?.can(perms::MANAGE_MESSAGES),
                            _ => return Ok(Err("That message no longer exists")),
                        };
                        if !allowed {
                            return Ok(Err("You need the Manage messages permission to pin messages"));
                        }
                        if !m.kind.is_normal() || m.pinned == pinned {
                            return Ok(Ok(None));
                        }
                        if pinned && db::pin_count(c, m.channel_id)? >= MAX_PINS {
                            return Ok(Err("This channel has as many pins as it can have. Unpin one first."));
                        }
                        db::set_pinned(c, message_id, pinned)?;
                        let updated = db::get_message(c, message_id)?;
                        // Pinning leaves a note in the chat, like Discord.
                        let note = if pinned {
                            let name = db::get_user(c, me)?.map(|u| u.name().to_string()).unwrap_or_default();
                            let text = format!("📌 {name} pinned a message to this channel.");
                            match db::insert_message(c, m.channel_id, me, &text, &[], None)? {
                                Ok(n) => {
                                    db::set_kind(c, n.id, &MessageKind::Pinned { message_id })?;
                                    db::mark_read(c, me, m.channel_id, n.id)?;
                                    db::get_message(c, n.id)?
                                }
                                Err(_) => None,
                            }
                        } else {
                            None
                        };
                        Ok(Ok(updated.map(|u| (u, note))))
                    })
                    .await?;
                match done {
                    Ok(Some((updated, note))) => {
                        let to = super::audience(&db, me, updated.channel_id).await?.and_then(|(_, to)| to);
                        self.broadcast_to(&ServerMsg::MessageUpdated(updated), to.clone());
                        if let Some(note) = note {
                            self.broadcast_to(&ServerMsg::MessageCreated(note), to);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::FetchPins { channel_id } => {
                if super::audience(&db, me, channel_id).await?.is_none() {
                    return Ok(self.error("You can't see that channel").await);
                }
                let messages = db.call(move |c| db::pins(c, channel_id)).await?;
                self.send(&ServerMsg::Pins { channel_id, messages }).await;
            }
            ClientMsg::Search(query) => {
                let q = query.clone();
                let (results, more) = db.call(move |c| db::search(c, me, &q)).await?;
                self.send(&ServerMsg::SearchResults { query, results, more }).await;
            }
            ClientMsg::FetchAround { channel_id, message_id } => {
                if super::audience(&db, me, channel_id).await?.is_none() {
                    return Ok(self.error("You can't see that channel").await);
                }
                let (messages, reached_start) = db.call(move |c| db::around(c, channel_id, message_id)).await?;
                self.send(&ServerMsg::Around { channel_id, message_id, messages, reached_start }).await;
            }
            ClientMsg::SetPresence { status, activity } => {
                let activity = activity.and_then(celerity_proto::Activity::sanitized);
                let info = celerity_proto::PresenceInfo { user_id: me, status, activity };
                let changed = self.state.presence.lock().unwrap().insert(me, info.clone()) != Some(info);
                if changed {
                    // Going invisible looks like going offline; coming back looks like coming online.
                    self.broadcast(&self.state.presence_msg(me));
                }
            }
            _ => unreachable!("handle_extra only gets 0.5 requests"),
        }
        Ok(Flow::Continue)
    }
}
