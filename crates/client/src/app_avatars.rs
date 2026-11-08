//! Profile pictures and banners: loading them, and drawing avatars that use them.

use std::sync::Arc;

use iced::widget::{container, image, space, stack};
use iced::{Border, Color, Element, Length, Task};
use syncord_proto::{AvatarRing, User};

use crate::app::{App, Message};
use crate::media::{self, Picture};
use crate::style;
use crate::theme::rgb;

/// Avatars are decoded this big (they're shown at most 80 px; 2x for sharp screens).
const AVATAR_DECODE: u32 = 160;
/// Banners are shown 320 px wide on the profile card.
const BANNER_DECODE: u32 = 340;

impl App {
    /// Start loading everyone's profile pictures (still images; small).
    pub(crate) fn request_avatars(&mut self) -> Task<Message> {
        let Some(s) = &self.session else { return Task::none() };
        let ids: Vec<String> =
            s.users.values().filter_map(|u| u.profile.avatar.as_ref().map(|a| a.id.clone())).collect();
        Task::batch(ids.into_iter().map(|id| self.load_profile_picture(id, false, false)))
    }

    /// Animated avatar and banner for an open profile card.
    pub(crate) fn request_profile_extras(&mut self, user: &User) -> Task<Message> {
        let mut tasks = Vec::new();
        if let Some(a) = user.profile.avatar.as_ref().filter(|a| a.animated) {
            tasks.push(self.load_profile_picture(a.id.clone(), true, false));
        }
        if let Some(b) = &user.profile.banner {
            tasks.push(self.load_profile_picture(b.id.clone(), b.animated, true));
        }
        Task::batch(tasks)
    }

    /// Animated pictures are only kept while a profile card shows them.
    pub(crate) fn drop_profile_extras(&mut self, user: &User) {
        if let Some(a) = &user.profile.avatar {
            self.images.remove(&media::avatar_key(&a.id, true));
        }
        if let Some(b) = &user.profile.banner {
            self.images.remove(&media::banner_key(&b.id));
        }
    }

    fn load_profile_picture(&mut self, id: String, animate: bool, banner: bool) -> Task<Message> {
        let key = if banner { media::banner_key(&id) } else { media::avatar_key(&id, animate) };
        if !self.images.start(&key) {
            return Task::none();
        }
        let Some((http, base, token)) = self.http_ctx() else { return Task::none() };
        let side = if banner { BANNER_DECODE } else { AVATAR_DECODE };
        Task::perform(media::load_profile_image(http, base, token, id, side, animate, !banner), move |r| {
            Message::PictureLoaded(key.clone(), r)
        })
    }

    fn avatar_picture(&self, u: &User, animated: bool) -> Option<&Arc<Picture>> {
        let a = u.profile.avatar.as_ref()?;
        animated
            .then(|| self.images.picture(&media::avatar_key(&a.id, true)))
            .flatten()
            .or_else(|| self.images.picture(&media::avatar_key(&a.id, false)))
    }

    /// Someone's avatar: their picture if they uploaded one, else the coloured letter.
    pub(crate) fn avatar<'a>(&self, u: &User, size: f32) -> Element<'a, Message> {
        self.avatar_inner(u, size, false)
    }

    pub(crate) fn avatar_inner<'a>(&self, u: &User, size: f32, animated: bool) -> Element<'a, Message> {
        let Some(pic) = self.avatar_picture(u, animated) else { return style::avatar(u.name(), &u.profile, size) };
        let frame = pic.frame(if animated { self.animation_ms(None) } else { 0 }).clone();
        let ring = rgb(u.profile.ring_color);
        let round = |inner: f32| image(frame.clone()).width(inner).height(inner).border_radius(inner / 2.0);
        let with_ring = |w: f32| -> Element<'a, Message> {
            container(round(size - 2.0 * w))
                .center(size)
                .style(move |_| container::Style {
                    border: Border { width: w, color: ring, radius: (size / 2.0).into() },
                    ..Default::default()
                })
                .into()
        };
        match u.profile.avatar_ring {
            AvatarRing::None => round(size).into(),
            AvatarRing::Thin => with_ring((size * 0.05).max(1.5)),
            AvatarRing::Thick => with_ring((size * 0.1).max(2.5)),
            AvatarRing::Double => {
                let w = (size * 0.045).max(1.5);
                container(container(round(size - 4.0 * w)).center(size - 2.0 * w).style(move |_| container::Style {
                    border: Border { width: w, color: ring, radius: (size / 2.0).into() },
                    ..Default::default()
                }))
                .center(size)
                .style(move |_| container::Style {
                    border: Border { width: w, color: ring, radius: (size / 2.0).into() },
                    ..Default::default()
                })
                .into()
            }
        }
    }

    /// Avatar with the online dot.
    pub(crate) fn avatar_status<'a>(&self, u: &User, size: f32, online: bool, cutout: Color) -> Element<'a, Message> {
        let t = &self.theme;
        let dot_size = (size * 0.38).round();
        let color = if online { t.online.0 } else { t.muted.0 };
        let dot = container(space()).width(dot_size).height(dot_size).style(move |_| container::Style {
            background: Some(color.into()),
            border: Border { radius: (dot_size / 2.0).into(), width: dot_size * 0.22, color: cutout },
            ..Default::default()
        });
        stack![self.avatar_inner(u, size, false), container(dot).align_right(size).align_bottom(size)].into()
    }

    /// The banner across the top of a profile card: picture/GIF if uploaded, else the colour.
    pub(crate) fn banner<'a>(&self, u: &User, height: f32, radius: f32) -> Element<'a, Message> {
        let banner_color = rgb(u.profile.banner_color);
        let pic = u.profile.banner.as_ref().and_then(|b| self.images.picture(&media::banner_key(&b.id)));
        match pic {
            Some(p) => {
                let frame = p.frame(self.animation_ms(None)).clone();
                container(image(frame).width(Length::Fill).height(height).content_fit(iced::ContentFit::Fill))
                    .width(Length::Fill)
                    .height(height)
                    .clip(true)
                    .style(move |_| container::Style {
                        border: Border { radius: iced::border::top(radius), ..Default::default() },
                        ..Default::default()
                    })
                    .into()
            }
            None => container(space())
                .width(Length::Fill)
                .height(height)
                .style(move |_| container::Style {
                    background: Some(banner_color.into()),
                    border: Border { radius: iced::border::top(radius), ..Default::default() },
                    ..Default::default()
                })
                .into(),
        }
    }
}
