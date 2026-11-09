//! The channel list: categories (collapsible, draggable), channels in order, lock icons on
//! private channels, the server menu under the server's name, and the channel settings dialog.

use celerity_proto::{Category, Channel, ChannelKind, User, VoiceState, perms};
use iced::widget::{button, checkbox, column, container, mouse_area, pin, row, space, stack, text, text_input};
use iced::{Alignment, Border, Color, Element, Length, Padding};

use crate::app::{App, Message, Session};
use crate::app_channels::{ChannelEdit, ChannelsMsg, DragItem, DropTarget, EDIT_NAME_ID, EditTarget};
use crate::icons;
use crate::press::press_sensor;
use crate::style::{self, bold, card, label, semibold};
use crate::theme::with_alpha;

fn ch(m: ChannelsMsg) -> Message {
    Message::Channels(m)
}

impl App {
    /// The server's name; clicking it opens the server menu.
    pub(crate) fn sidebar_header<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let chevron = if self.server_menu { &icons::CLOSE } else { &icons::CHEVRON_DOWN };
        let name = row![
            text(s.community.clone()).size(fs * 1.05).font(bold()).color(t.header.0).width(Length::Fill),
            icons::icon(chevron, fs * 0.95, t.muted.0),
        ]
        .align_y(Alignment::Center);
        let header = button(container(name).padding([0, 16]).height(48).align_y(Alignment::Center))
            .width(Length::Fill)
            .padding(0)
            .on_press(ch(ChannelsMsg::ToggleServerMenu))
            .style(style::row_button_still(t, false));
        let header = crate::widgets::hover_fade(header, t.hover.0, style::panel_radius(t));
        let mut col = column![Element::from(header)];
        if self.server_menu {
            col = col.push(self.server_menu_view(s));
        }
        col.into()
    }

    fn server_menu_view<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let item =
            |glyph: &'static std::sync::LazyLock<iced::widget::svg::Handle>, label: &'static str, msg: Message| {
                button(
                    row![text(label).size(fs * 0.9).width(Length::Fill), icons::icon(glyph, fs, t.muted.0)]
                        .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([7, 10])
                .on_press(msg)
                .style(style::row_button(t, false))
            };
        let mut col = column![].spacing(2);
        if s.v07 && s.can(perms::CREATE_INVITES) {
            col =
                col.push(item(&icons::INVITE, "Invite People", Message::Invites(crate::app_invites::InviteMsg::Open)));
        }
        if let Some(tab) = self.first_server_tab(s) {
            col = col.push(item(&icons::GEAR, "Server Settings", ch(ChannelsMsg::ServerSettings(tab))));
        }
        if s.can(perms::MANAGE_CHANNELS) {
            col =
                col.push(item(&icons::PLUS, "Create Channel", ch(ChannelsMsg::NewChannelIn(None, ChannelKind::Text))));
            if s.v07 {
                col = col.push(item(&icons::PLUS, "Create Category", ch(ChannelsMsg::NewCategory)));
            }
        }
        col = col.push(item(
            &icons::CHEVRON_RIGHT,
            "Mark All as Read",
            Message::MarkServerRead(self.settings.server_position.min(self.parked.len())),
        ));
        container(
            container(col).padding(6).width(Length::Fill).style(card(style::darken(t.floating.0, 0.12), t.radius)),
        )
        .padding([4, 8])
        .into()
    }

    /// Channels without a category, then each category with its channels.
    pub(crate) fn channel_list<'a>(&'a self, s: &'a Session) -> Element<'a, Message> {
        let manage = s.can(perms::MANAGE_CHANNELS);
        let mut list = column![].spacing(0);
        let top = s.channels_in(None);
        let (top_text, top_voice): (Vec<&Channel>, Vec<&Channel>) =
            top.into_iter().partition(|c| c.kind == ChannelKind::Text);
        // Older servers (and servers without categories) look as they always have.
        let headings = s.categories.is_empty() || manage;
        if headings || !top_text.is_empty() {
            list = list.push(self.section_header("Text channels", ChannelKind::Text));
        }
        for c in top_text {
            list = list.push(self.text_channel_row(s, c));
        }
        if headings || !top_voice.is_empty() {
            list = list.push(self.section_header("Voice channels", ChannelKind::Voice));
        }
        for c in top_voice {
            list = list.push(self.voice_channel_rows(s, c));
        }
        for cat in s.sorted_categories() {
            list = list.push(self.category_header(s, cat));
            let collapsed = self.category_collapsed(cat.id);
            for c in s.channels_in(Some(cat.id)) {
                // A folded category still shows where you are.
                let here = s.current == Some(c.id) || self.voice_here().is_some_and(|v| v.channel_id == c.id);
                if collapsed && !here {
                    continue;
                }
                list = list.push(match c.kind {
                    ChannelKind::Voice => self.voice_channel_rows(s, c),
                    _ => self.text_channel_row(s, c),
                });
            }
        }
        // Somewhere to drop a category to make it the last one.
        let dragging_category = matches!(self.channel_drag.as_ref().map(|d| d.item), Some(DragItem::Category(_)));
        if dragging_category {
            let active = self.drop_target() == Some(DropTarget::AfterCategories);
            list = list.push(self.drop_line(active));
            list =
                list.push(self.drop_zone(space().height(48).width(Length::Fill).into(), DropTarget::AfterCategories));
        }
        list.into()
    }

    /// Where the thing being dragged would land, once it has moved.
    fn drop_target(&self) -> Option<DropTarget> {
        self.channel_drag.as_ref().filter(|d| d.moved()).and_then(|d| d.over)
    }

    /// A thin accent line where a dragged channel would go (keeps its space either way).
    fn drop_line<'a>(&self, active: bool) -> Element<'a, Message> {
        let color = if active { self.theme.accent.0 } else { Color::TRANSPARENT };
        container(space()).height(2).width(Length::Fill).style(style::flat(color, 1.0)).into()
    }

    /// `content` reports the pointer over it while something is being dragged.
    fn drop_zone<'a>(&self, content: Element<'a, Message>, target: DropTarget) -> Element<'a, Message> {
        let area = mouse_area(content);
        if self.channel_drag.is_some() {
            area.on_enter(ch(ChannelsMsg::DragOver(target))).on_exit(ch(ChannelsMsg::DragOut(target))).into()
        } else {
            area.into()
        }
    }

    /// "Text channels" / "Voice channels" above the channels outside every category.
    fn section_header<'a>(&'a self, title: &'static str, kind: ChannelKind) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let mut line = row![label(t, title, fs), space::horizontal()].align_y(Alignment::Center);
        if self.can_manage_channels() {
            line = line.push(
                button(text("+").size(fs * 1.2))
                    .padding([0, 6])
                    .on_press(ch(ChannelsMsg::NewChannelIn(None, kind)))
                    .style(style::link_button(t.muted.0, t.header.0)),
            );
        }
        let active = matches!(self.drop_target(), Some(DropTarget::EndOf(None)))
            && matches!(self.channel_drag.as_ref().map(|d| d.item), Some(DragItem::Channel(_)));
        let accent = t.accent.0;
        let body = container(line)
            .padding(Padding { left: 8.0, top: 16.0, right: 4.0, bottom: 4.0 })
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: active.then(|| with_alpha(accent, 0.15).into()),
                border: Border { radius: 6.0.into(), ..Default::default() },
                ..Default::default()
            });
        self.drop_zone(body.into(), DropTarget::EndOf(None))
    }

    fn category_header<'a>(&'a self, s: &'a Session, cat: &'a Category) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let manage = s.can(perms::MANAGE_CHANNELS);
        let collapsed = self.category_collapsed(cat.id);
        let chevron = if collapsed { &icons::CHEVRON_RIGHT } else { &icons::CHEVRON_DOWN };
        let mut line = row![
            icons::icon(chevron, fs * 0.7, t.muted.0),
            text(cat.name.to_uppercase()).size(fs * 0.75).font(bold()).color(t.muted.0),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        if !cat.allowed_roles.is_empty() {
            line = line.push(icons::icon(&icons::LOCK, fs * 0.7, t.muted.0));
        }
        line = line.push(space::horizontal());
        if manage {
            line = line.push(
                button(text("+").size(fs * 1.2))
                    .padding([0, 6])
                    .on_press(ch(ChannelsMsg::NewChannelIn(Some(cat.id), ChannelKind::Text)))
                    .style(style::link_button(t.muted.0, t.header.0)),
            );
        }
        let target = DropTarget::EndOf(Some(cat.id));
        let dragging = self.channel_drag.as_ref().map(|d| d.item);
        let over = self.drop_target() == Some(target);
        let highlight = over && matches!(dragging, Some(DragItem::Channel(_)));
        let accent = t.accent.0;
        let header = button(line)
            .width(Length::Fill)
            .padding(Padding { left: 2.0, top: 14.0, right: 4.0, bottom: 4.0 })
            .on_press(ch(ChannelsMsg::ToggleCategory(cat.id)))
            .style(move |_, _| button::Style {
                background: highlight.then(|| with_alpha(accent, 0.15).into()),
                border: Border { radius: 6.0.into(), ..Default::default() },
                ..Default::default()
            });
        let header: Element<'a, Message> = if manage {
            press_sensor(header, ch(ChannelsMsg::Press(DragItem::Category(cat.id))), ch(ChannelsMsg::DragRelease))
                .into()
        } else {
            header.into()
        };
        let id = cat.id;
        let header: Element<'a, Message> = if manage {
            crate::widgets::context_menu(header, move || {
                self.simple_menu(vec![
                    ("Edit Category".into(), ch(ChannelsMsg::Edit(EditTarget::Category(id)))),
                    ("Create Text Channel".into(), ch(ChannelsMsg::NewChannelIn(Some(id), ChannelKind::Text))),
                    ("Create Voice Channel".into(), ch(ChannelsMsg::NewChannelIn(Some(id), ChannelKind::Voice))),
                    ("Delete Category".into(), ch(ChannelsMsg::AskDeleteCategory(id))),
                ])
            })
            .into()
        } else {
            header
        };
        // Dragging a category: the line shows it would go above this one.
        let line_above = over && matches!(dragging, Some(DragItem::Category(_)));
        self.drop_zone(column![self.drop_line(line_above), header].into(), target)
    }

    fn text_channel_row<'a>(&'a self, s: &'a Session, c: &'a Channel) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let selected = s.current == Some(c.id);
        // Muted channels don't light up for new messages (only for pings).
        let muted = self.settings.is_muted(&self.settings.server, c.id);
        let unread = s.is_unread(c.id) && !selected && !muted;
        let mut name = text(c.name.clone()).size(fs);
        if unread {
            name = name.font(semibold()).color(t.header.0);
        } else if muted && !selected {
            name = name.color(with_alpha(t.muted.0, 0.6));
        }
        let mut line = row![text("#").size(fs * 1.25).color(t.muted.0), name].spacing(6).align_y(Alignment::Center);
        if s.is_private(c) {
            line = line.push(icons::icon(&icons::LOCK, fs * 0.75, t.muted.0));
        }
        line = line.push(space::horizontal());
        if s.mention_count(c.id) > 0 && !selected {
            line = line.push(crate::view::mention_badge(t, s.mention_count(c.id), fs));
        }
        let row = style::fade_row(
            t,
            button(line).width(Length::Fill).padding([6, 8]).on_press(Message::SelectChannel(c.id)),
            selected,
        );
        self.draggable_channel(s, c, self.with_channel_menu(row, c.id, false))
    }

    /// A row a manager can drag, with the drop line above it.
    fn draggable_channel<'a>(&'a self, s: &Session, c: &Channel, row: Element<'a, Message>) -> Element<'a, Message> {
        let row: Element<'a, Message> = if s.can(perms::MANAGE_CHANNELS) {
            press_sensor(row, ch(ChannelsMsg::Press(DragItem::Channel(c.id))), ch(ChannelsMsg::DragRelease)).into()
        } else {
            row
        };
        let target = DropTarget::BeforeChannel(c.id);
        let active = self.drop_target() == Some(target)
            && matches!(self.channel_drag.as_ref().map(|d| d.item), Some(DragItem::Channel(id)) if id != c.id);
        self.drop_zone(column![self.drop_line(active), row].into(), target)
    }

    /// A voice channel and the people in it.
    fn voice_channel_rows<'a>(&'a self, s: &'a Session, c: &'a Channel) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let connected = self.voice_here().is_some_and(|v| v.channel_id == c.id);
        let mut line =
            row![icons::icon(&icons::SPEAKER, fs * 1.2, t.muted.0), text(c.name.clone()).size(fs).width(Length::Fill)]
                .spacing(6)
                .align_y(Alignment::Center);
        if s.is_private(c) {
            line = line.push(icons::icon(&icons::LOCK, fs * 0.75, t.muted.0));
        }
        if let Some(length) = s.call_length(c.id) {
            line = line.push(text(crate::app::format_duration(length)).size(fs * 0.75).color(t.muted.0));
        }
        let drop_here = self.voice_drag.as_ref().is_some_and(|d| d.moved() && d.over == Some(c.id));
        let entry =
            button(line).width(Length::Fill).padding([6, 8]).style(style::row_button_still(t, connected || drop_here));
        let entry = entry.on_press(if connected { Message::OpenStage } else { Message::JoinVoice(c.id) });
        // Drop target for dragging people between channels.
        let entry = crate::widgets::hover_fade(entry, t.hover.0, t.radius * 0.8);
        let entry =
            mouse_area(entry).on_enter(Message::VoiceDragOver(Some(c.id))).on_exit(Message::VoiceDragOver(None));
        let mut col = column![self.draggable_channel(s, c, self.with_channel_menu(entry, c.id, false))];

        // Who's in this channel.
        let mut members: Vec<(&User, &VoiceState)> = s
            .voice
            .values()
            .filter(|v| v.channel_id == c.id)
            .filter_map(|v| Some((s.users.get(&v.user_id)?, v)))
            .collect();
        members.sort_by_key(|(u, _)| u.name().to_lowercase());
        for (u, v) in members {
            col = col.push(
                mouse_area(self.voice_member(u, v, s.me))
                    .on_enter(Message::VoiceDragOver(Some(c.id)))
                    .on_exit(Message::VoiceDragOver(None)),
            );
        }
        col.into()
    }

    /// The name that follows the pointer while dragging a channel or category.
    pub(crate) fn with_channel_drag<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        let (Some(s), Some(d)) = (&self.session, self.channel_drag.as_ref().filter(|d| d.moved())) else {
            return base;
        };
        let t = &self.theme;
        let fs = self.fs();
        let name = match d.item {
            DragItem::Channel(id) => s.channels.iter().find(|c| c.id == id).map(|c| match c.kind {
                ChannelKind::Text => format!("# {}", c.name),
                _ => c.name.clone(),
            }),
            DragItem::Category(id) => s.category(id).map(|c| c.name.to_uppercase()),
        };
        let Some(name) = name else { return base };
        let hint = if d.over.is_some() { "Drop to move it here" } else { "Drop it in the channel list" };
        let ghost = container(
            column![
                text(name).size(fs * 0.9).font(semibold()).color(t.header.0),
                text(hint).size(fs * 0.75).color(t.muted.0)
            ]
            .spacing(2),
        )
        .padding([6, 10])
        .style(card(t.floating.0, t.radius * 0.6));
        stack![base, pin(ghost).x(d.at.x + 12.0).y(d.at.y + 8.0)].into()
    }

    /// Channel or category settings: name, topic and who can see it.
    pub(crate) fn channel_settings_dialog<'a>(&'a self, s: &'a Session, e: &'a ChannelEdit) -> Element<'a, Message> {
        let t = &self.theme;
        let fs = self.fs();
        let channel = match e.target {
            EditTarget::Channel(id) => s.channels.iter().find(|c| c.id == id),
            _ => None,
        };
        let is_text = channel.is_some_and(|c| c.kind == ChannelKind::Text);
        let title = match e.target {
            EditTarget::Channel(_) => "Channel settings",
            EditTarget::Category(_) => "Category settings",
            EditTarget::NewCategory => "Create category",
        };
        let what = if matches!(e.target, EditTarget::Channel(_)) { "channel" } else { "category" };
        let preview = if is_text { celerity_proto::normalize_channel_name(&e.name) } else { e.name.trim().to_string() };
        let mut body = column![
            text(title).size(fs * 1.3).font(bold()).color(t.header.0),
            label(t, if what == "channel" { "Channel name" } else { "Category name" }, fs),
            text_input(if what == "channel" { "channel-name" } else { "New category" }, &e.name)
                .id(iced::widget::Id::new(EDIT_NAME_ID))
                .on_input(|v| ch(ChannelsMsg::EditName(v)))
                .on_submit(ch(ChannelsMsg::Save))
                .padding(10)
                .size(fs)
                .style(style::input(t, t.rail.0)),
        ]
        .spacing(10);
        if is_text && !preview.is_empty() && preview != e.name {
            body = body.push(text(format!("Will appear as #{preview}")).size(fs * 0.8).color(t.muted.0));
        }
        if is_text {
            body = body.push(label(t, "Topic", fs)).push(
                text_input("What's this channel about?", &e.topic)
                    .on_input(|v| ch(ChannelsMsg::EditTopic(v)))
                    .on_submit(ch(ChannelsMsg::Save))
                    .padding(10)
                    .size(fs)
                    .style(style::input(t, t.rail.0)),
            );
        }
        // Who can see it (a new category starts public; its roles can be set afterwards).
        if s.v07 && e.target != EditTarget::NewCategory {
            let explain = if what == "channel" {
                "Only people with one of the roles below (and admins) can see, read and join it."
            } else {
                "Only people with one of the roles below (and admins) see it and the channels in it that have no roles of their own."
            };
            body = body.push(style::divider(t.hover.0)).push(
                iced::widget::toggler(e.private)
                    .label(format!("Private {what}"))
                    .on_toggle(|on| ch(ChannelsMsg::EditPrivate(on)))
                    .text_size(fs),
            );
            body = body.push(text(explain).size(fs * 0.8).color(t.muted.0));
            if e.private {
                let mut roles = column![].spacing(6);
                let others: Vec<_> = s.roles.iter().filter(|r| r.id != celerity_proto::EVERYONE_ROLE).collect();
                if others.is_empty() {
                    roles = roles.push(
                        text("There are no roles yet. Make some in Server Settings > Roles.")
                            .size(fs * 0.85)
                            .color(t.muted.0),
                    );
                }
                for r in others {
                    let id = r.id;
                    let color = r.color.map(crate::theme::rgb).unwrap_or(t.text.0);
                    roles = roles.push(
                        checkbox(e.roles.contains(&id))
                            .label(r.name.clone())
                            .on_toggle(move |on| ch(ChannelsMsg::EditRole(id, on)))
                            .text_size(fs * 0.9)
                            .style(move |theme, status| {
                                let mut st = checkbox::primary(theme, status);
                                st.text_color = Some(color);
                                st
                            }),
                    );
                }
                body = body.push(
                    container(iced::widget::scrollable(roles).height(Length::Shrink).style(style::scroll(t)))
                        .max_height(180)
                        .padding(10)
                        .width(Length::Fill)
                        .style(card(t.rail.0, t.radius * 0.6)),
                );
                if e.roles.is_empty() {
                    body = body.push(
                        text("Pick at least one role, or only admins will see it.")
                            .size(fs * 0.8)
                            .color(crate::theme::rgb(0xF0B232)),
                    );
                }
            }
        }
        let mut buttons = row![].spacing(8).align_y(Alignment::Center);
        let danger = t.danger.0;
        match e.target {
            EditTarget::Channel(id) if s.channels.len() > 1 => {
                buttons = buttons.push(
                    button(text("Delete Channel").size(fs * 0.9))
                        .padding([8, 12])
                        .on_press(Message::AskDeleteChannel(id))
                        .style(style::link_button(danger, style::darken(danger, 0.2))),
                );
            }
            EditTarget::Category(id) => {
                buttons = buttons.push(
                    button(text("Delete Category").size(fs * 0.9))
                        .padding([8, 12])
                        .on_press(ch(ChannelsMsg::AskDeleteCategory(id)))
                        .style(style::link_button(danger, style::darken(danger, 0.2))),
                );
            }
            _ => {}
        }
        let save = if e.target == EditTarget::NewCategory { "Create Category" } else { "Save Changes" };
        buttons = buttons.push(space::horizontal()).push(
            button(text("Cancel").size(fs))
                .padding([8, 16])
                .on_press(Message::CloseOverlay)
                .style(style::link_button(t.text.0, t.header.0)),
        );
        buttons = buttons.push(
            button(text(save).size(fs))
                .padding([8, 16])
                .on_press_maybe((!e.name.trim().is_empty()).then_some(ch(ChannelsMsg::Save)))
                .style(style::solid_button(t.accent.0, t.radius * 0.6)),
        );
        container(body.push(buttons)).width(460).padding(24).style(card(t.chat.0, t.radius * 1.5)).into()
    }

    /// The first server settings page you may open, if any.
    pub(crate) fn first_server_tab(&self, s: &Session) -> Option<crate::app::SettingsTab> {
        use crate::app::SettingsTab;
        let admin = s.can(perms::ADMINISTRATOR);
        if s.v07 && admin {
            Some(SettingsTab::Server)
        } else if s.can(perms::MANAGE_ROLES) {
            Some(SettingsTab::Roles)
        } else if s.v07 && s.can(perms::MANAGE_EMOJI) {
            Some(SettingsTab::Emoji)
        } else if s.v07 && s.can(perms::MANAGE_SOUNDBOARD) {
            Some(SettingsTab::Soundboard)
        } else {
            None
        }
    }
}
