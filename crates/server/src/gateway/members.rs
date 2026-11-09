//! Requests added in 0.7: invite links, kicking and banning, and end-to-end encryption
//! (published keys, encrypted messages and call key exchange). The server never reads
//! anything encrypted; it only checks sizes and who may receive it.

use super::*;
use celerity_proto::e2ee::{MAX_CALL_SIGNAL_LEN, b64_decode, is_key32};
use celerity_proto::invites::{MAX_INVITE_AGE_SECS, MAX_INVITE_USES};
use celerity_proto::{EncryptedPayload, IdentityKeys};

impl Connection {
    pub(super) async fn handle_members(&mut self, me: UserId, msg: ClientMsg) -> Result<Flow, rusqlite::Error> {
        let db = self.state.db.clone();
        let may_invite = move |c: &rusqlite::Connection| db::access(c, me).map(|a| a.can(perms::CREATE_INVITES));
        match msg {
            // ---- Invites
            ClientMsg::CreateInvite { max_age_secs, max_uses } => {
                if max_age_secs.is_some_and(|s| s == 0 || s > MAX_INVITE_AGE_SECS) {
                    return Ok(self.error("Invites can last at most a year").await);
                }
                if max_uses.is_some_and(|n| n == 0 || n > MAX_INVITE_USES) {
                    return Ok(self.error(format!("Invites can be used at most {MAX_INVITE_USES} times")).await);
                }
                let token = random_token(10);
                let expires_at = max_age_secs.map(|s| db::now_ms() + s as i64 * 1000);
                let made = db
                    .call(move |c| {
                        if !may_invite(c)? {
                            return Ok(None);
                        }
                        db::create_invite(c, &token, me, expires_at, max_uses).map(Some)
                    })
                    .await?;
                match made {
                    Some(invite) => {
                        info!("user {me} made an invite link");
                        self.send(&ServerMsg::InviteCreated(invite)).await;
                    }
                    None => return Ok(self.error("You need the Create invites permission").await),
                }
            }
            ClientMsg::ListInvites => {
                let invites =
                    db.call(move |c| if may_invite(c)? { db::usable_invites(c).map(Some) } else { Ok(None) }).await?;
                match invites {
                    Some(invites) => {
                        self.send(&ServerMsg::Invites { invites }).await;
                    }
                    None => return Ok(self.error("You need the Create invites permission").await),
                }
            }
            ClientMsg::RevokeInvite { token } => {
                let t = token.clone();
                let revoked = db
                    .call(move |c| if may_invite(c)? { db::revoke_invite(c, &t).map(Some) } else { Ok(None) })
                    .await?;
                match revoked {
                    Some(_) => {
                        self.send(&ServerMsg::InviteRevoked { token }).await;
                    }
                    None => return Ok(self.error("You need the Create invites permission").await),
                }
            }

            // ---- Kicking and banning
            ClientMsg::KickMember { user_id } | ClientMsg::BanMember { user_id } if user_id == me => {
                return Ok(self.error("You can't remove yourself").await);
            }
            ClientMsg::KickMember { user_id } => {
                if let Err(e) = crate::moderation::remove(&self.state, me, user_id, false).await {
                    return Ok(self.error(e).await);
                }
            }
            ClientMsg::BanMember { user_id } => {
                if let Err(e) = crate::moderation::remove(&self.state, me, user_id, true).await {
                    return Ok(self.error(e).await);
                }
            }
            ClientMsg::UnbanMember { user_id } => {
                if let Err(e) = crate::moderation::unban(&self.state, me, user_id).await {
                    return Ok(self.error(e).await);
                }
            }

            // ---- End-to-end encryption
            ClientMsg::PublishIdentityKeys { agreement_key, signing_key, signature } => {
                let signature_ok = signature.is_empty() || (signature.len() <= 172 && b64_decode(&signature).is_some());
                if !is_key32(&agreement_key) || !is_key32(&signing_key) || !signature_ok {
                    return Ok(self.error("Those keys aren't valid (32 bytes each, in base64)").await);
                }
                let keys =
                    IdentityKeys { user_id: me, agreement_key, signing_key, signature, updated_at: db::now_ms() };
                let k = keys.clone();
                db.call(move |c| db::set_identity_keys(c, &k)).await?;
                self.broadcast(&ServerMsg::IdentityKeysUpdated(keys));
            }
            ClientMsg::FetchIdentityKeys { user_ids } => {
                let ids: Vec<UserId> = user_ids.into_iter().take(1000).collect();
                let keys = db.call(move |c| db::identity_keys_of(c, &ids)).await?;
                self.send(&ServerMsg::IdentityKeys { keys }).await;
            }
            ClientMsg::SendEncrypted { channel_id, payload, attachments, reply_to } => {
                if !payload.is_well_formed() {
                    return Ok(self.error("That encrypted message is malformed").await);
                }
                if attachments.len() > MAX_ATTACHMENTS {
                    return Ok(self.error(format!("You can attach at most {MAX_ATTACHMENTS} files")).await);
                }
                let extras = db::MessageExtras { encrypted: Some(payload), ..Default::default() };
                return self.post(me, channel_id, String::new(), attachments, reply_to, extras).await;
            }
            ClientMsg::EditEncrypted { message_id, payload } => {
                if !payload.is_well_formed() {
                    return Ok(self.error("That encrypted message is malformed").await);
                }
                let updated = db
                    .call(move |c| {
                        let Some(m) = db::get_message(c, message_id)? else { return Ok(None) };
                        if m.author_id != me
                            || m.encrypted.is_none()
                            || db::accessible_kind(c, me, m.channel_id)?.is_none()
                        {
                            return Ok(None);
                        }
                        let p: &EncryptedPayload = &payload;
                        db::edit_encrypted(c, message_id, p)?;
                        db::get_message(c, message_id)
                    })
                    .await?;
                match updated {
                    Some(m) => {
                        let channel = m.channel_id;
                        self.broadcast_in(channel, &ServerMsg::MessageUpdated(m)).await?;
                    }
                    None => return Ok(self.error("You can only edit your own messages").await),
                }
            }
            ClientMsg::CallSignal { to, data } => {
                if data.len() > MAX_CALL_SIGNAL_LEN {
                    return Ok(self.error("That's too much to send at once").await);
                }
                if !self.signals.allow() {
                    return Ok(Flow::Continue);
                }
                let Some(channel_id) = self.state.voice.channel_of(me) else {
                    return Ok(self.error("You're not in a call").await);
                };
                if !is_call(&db, channel_id).await? {
                    return Ok(self.error("Key exchange is only for direct message and group calls").await);
                }
                let others: Vec<UserId> =
                    self.state.voice.members_of(channel_id).into_iter().filter(|u| *u != me).collect();
                let recipients = match to {
                    Some(u) if others.contains(&u) => vec![u],
                    Some(_) => return Ok(self.error("They're not in this call").await),
                    None => others,
                };
                if !recipients.is_empty() {
                    self.broadcast_target(
                        &ServerMsg::CallSignal { channel_id, from: me, data },
                        Target::only(recipients).modern(true),
                    );
                }
            }
            _ => unreachable!("handle_members only gets invite, moderation and encryption requests"),
        }
        Ok(Flow::Continue)
    }
}
