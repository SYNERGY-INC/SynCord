//! Requests added in 0.7 for custom emoji, stickers and the soundboard. Uploads go over
//! HTTP (see `crate::expressions`); renaming, deleting, sending and playing go here.

use super::*;
use crate::db::PictureKind;
use celerity_proto::expressions::valid_expression_name;

/// Sounds one person may play within [`SOUND_WINDOW`].
const SOUNDS_PER_WINDOW: usize = 3;
const SOUND_WINDOW: Duration = Duration::from_secs(10);

impl Connection {
    /// Whether `me` may play another sound now (and count it if so).
    fn sound_allowed(&self, me: UserId) -> bool {
        let mut recent = self.state.soundboard.lock().unwrap();
        let mine = recent.entry(me).or_default();
        let now = Instant::now();
        while mine.front().is_some_and(|t| now.duration_since(*t) > SOUND_WINDOW) {
            mine.pop_front();
        }
        if mine.len() >= SOUNDS_PER_WINDOW {
            return false;
        }
        mine.push_back(now);
        true
    }

    async fn rename_picture(
        &mut self,
        me: UserId,
        kind: PictureKind,
        id: i64,
        name: String,
    ) -> Result<Flow, rusqlite::Error> {
        let name = name.trim().to_string();
        if !valid_expression_name(&name) {
            return Ok(self.error("Names are 2-32 letters, numbers and underscores").await);
        }
        let done = self
            .state
            .db
            .call(move |c| {
                if !db::access(c, me)?.can(perms::MANAGE_EMOJI) {
                    return Ok(Err("You need the Manage emoji permission"));
                }
                if db::picture_name_taken(c, kind, &name, id)? {
                    return Ok(Err("That name is already taken"));
                }
                if !db::rename_picture(c, kind, id, &name)? {
                    return Ok(Err("That no longer exists"));
                }
                Ok(Ok(db::get_picture(c, kind, id)?))
            })
            .await?;
        match done {
            Ok(Some(p)) => self.broadcast(&match kind {
                PictureKind::Emoji => ServerMsg::EmojiUpdated(p.emoji()),
                PictureKind::Sticker => ServerMsg::StickerUpdated(p.sticker()),
            }),
            Ok(None) => {}
            Err(e) => return Ok(self.error(e).await),
        }
        Ok(Flow::Continue)
    }

    async fn delete_picture(&mut self, me: UserId, kind: PictureKind, id: i64) -> Result<Flow, rusqlite::Error> {
        let done = self
            .state
            .db
            .call(move |c| {
                if !db::access(c, me)?.can(perms::MANAGE_EMOJI) {
                    return Ok(Err("You need the Manage emoji permission"));
                }
                Ok(Ok(db::delete_picture(c, kind, id)?))
            })
            .await?;
        match done {
            Ok(Some(file)) => {
                crate::http::remove_files(&self.state.config.uploads, &[file]).await;
                self.broadcast(&match kind {
                    PictureKind::Emoji => ServerMsg::EmojiDeleted { emoji_id: id },
                    PictureKind::Sticker => ServerMsg::StickerDeleted { sticker_id: id },
                });
            }
            Ok(None) => {}
            Err(e) => return Ok(self.error(e).await),
        }
        Ok(Flow::Continue)
    }

    pub(super) async fn handle_sounds(&mut self, me: UserId, msg: ClientMsg) -> Result<Flow, rusqlite::Error> {
        let db = self.state.db.clone();
        match msg {
            ClientMsg::RenameEmoji { emoji_id, name } => {
                return self.rename_picture(me, PictureKind::Emoji, emoji_id, name).await;
            }
            ClientMsg::DeleteEmoji { emoji_id } => return self.delete_picture(me, PictureKind::Emoji, emoji_id).await,
            ClientMsg::RenameSticker { sticker_id, name } => {
                return self.rename_picture(me, PictureKind::Sticker, sticker_id, name).await;
            }
            ClientMsg::DeleteSticker { sticker_id } => {
                return self.delete_picture(me, PictureKind::Sticker, sticker_id).await;
            }
            ClientMsg::SendSticker { channel_id, sticker_id, reply_to } => {
                let Some(sticker) = db.call(move |c| db::get_sticker(c, sticker_id)).await? else {
                    return Ok(self.error("That sticker no longer exists").await);
                };
                let extras = db::MessageExtras { sticker: Some(sticker), ..Default::default() };
                return self.post(me, channel_id, String::new(), Vec::new(), reply_to, extras).await;
            }
            ClientMsg::UpdateSound { sound_id, name, emoji } => {
                let name = crate::expressions::clean_sound_name(&name);
                if name.is_empty() {
                    return Ok(self.error("Give the sound a name").await);
                }
                let emoji = crate::expressions::clean_sound_emoji(emoji);
                let done = db
                    .call(move |c| {
                        if !db::access(c, me)?.can(perms::MANAGE_SOUNDBOARD) {
                            return Ok(Err("You need the Manage soundboard permission"));
                        }
                        if !db::update_sound(c, sound_id, &name, emoji.as_deref())? {
                            return Ok(Err("That sound no longer exists"));
                        }
                        Ok(Ok(db::get_sound(c, sound_id)?.map(|s| s.sound)))
                    })
                    .await?;
                match done {
                    Ok(Some(sound)) => self.broadcast(&ServerMsg::SoundUpdated(sound)),
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::DeleteSound { sound_id } => {
                let done = db
                    .call(move |c| {
                        if !db::access(c, me)?.can(perms::MANAGE_SOUNDBOARD) {
                            return Ok(Err("You need the Manage soundboard permission"));
                        }
                        Ok(Ok(db::delete_sound(c, sound_id)?))
                    })
                    .await?;
                match done {
                    Ok(Some(file)) => {
                        crate::http::remove_files(&self.state.config.uploads, &[file]).await;
                        self.broadcast(&ServerMsg::SoundDeleted { sound_id });
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.error(e).await),
                }
            }
            ClientMsg::PlaySound { sound_id } => {
                let Some(state) = self.state.voice.states().into_iter().find(|v| v.user_id == me) else {
                    return Ok(self.error("Join a voice channel to play sounds").await);
                };
                if state.muted || state.server_muted {
                    return Ok(self.error("Unmute yourself to play sounds").await);
                }
                let channel_id = state.channel_id;
                let refusal = db
                    .call(move |c| {
                        if db::get_sound(c, sound_id)?.is_none() {
                            return Ok(Some("That sound no longer exists"));
                        }
                        let call = db::channel_kind(c, channel_id)?.is_some_and(is_conversation);
                        Ok((!call && !db::access(c, me)?.can(perms::USE_SOUNDBOARD))
                            .then_some("You don't have permission to use the soundboard"))
                    })
                    .await?;
                if let Some(why) = refusal {
                    return Ok(self.error(why).await);
                }
                if !self.sound_allowed(me) {
                    return Ok(self.error("Wait a moment before playing another sound").await);
                }
                let listeners = self.state.voice.members_of(channel_id);
                self.broadcast_target(
                    &ServerMsg::SoundPlayed { channel_id, user_id: me, sound_id },
                    Target::only(listeners).modern(true),
                );
            }
            _ => unreachable!("handle_sounds only gets emoji, sticker and soundboard requests"),
        }
        Ok(Flow::Continue)
    }
}
