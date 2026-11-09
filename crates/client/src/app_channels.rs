//! Channel categories, ordering channels by drag and drop, and private channels (0.7).
//!
//! Dragging works like moving people between voice channels (see voice_moderation.rs): a
//! press on a channel or category header starts it, rows under the pointer say where it
//! would land, and letting go sends the new order in one `ReorderChannels`.

use celerity_proto::{
    Category, CategoryId, CategoryPosition, Channel, ChannelId, ChannelKind, ChannelPosition, ClientMsg, RoleId,
    ServerMsg, perms,
};
use iced::{Point, Task};

use crate::app::{App, Message, Overlay, Session};
use crate::features::{ConfirmAction, Dialog};

/// How far the pointer must move before a press counts as a drag.
const DRAG_THRESHOLD: f32 = 5.0;
/// Id of the name box in the channel and category settings.
pub const EDIT_NAME_ID: &str = "channel-edit-name";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragItem {
    Channel(ChannelId),
    Category(CategoryId),
}

/// Where something being dragged would land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTarget {
    /// Just above this channel, in its category.
    BeforeChannel(ChannelId),
    /// At the end of a category (`None`: the channels above every category). A category
    /// dragged onto a header goes just above that category.
    EndOf(Option<CategoryId>),
    /// Below every category (for dragging categories).
    AfterCategories,
}

pub struct ChannelDrag {
    pub item: DragItem,
    pub start: Point,
    pub at: Point,
    pub over: Option<DropTarget>,
}

impl ChannelDrag {
    pub fn moved(&self) -> bool {
        self.start.distance(self.at) > DRAG_THRESHOLD
    }
}

#[derive(Debug, Clone)]
pub enum ChannelsMsg {
    ToggleCategory(CategoryId),
    Press(DragItem),
    DragMove(Point),
    DragOver(DropTarget),
    DragOut(DropTarget),
    DragRelease,
    ToggleServerMenu,
    /// Create a channel of this kind in a category (`None`: at the top).
    NewChannelIn(Option<CategoryId>, ChannelKind),
    NewCategory,
    Edit(EditTarget),
    EditName(String),
    EditTopic(String),
    EditPrivate(bool),
    EditRole(RoleId, bool),
    Save,
    AskDeleteCategory(CategoryId),
    /// "Server Settings" in the server menu.
    ServerSettings(crate::app::SettingsTab),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    Channel(ChannelId),
    Category(CategoryId),
    NewCategory,
}

/// The channel (or category) settings dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelEdit {
    pub target: EditTarget,
    pub name: String,
    pub topic: String,
    /// Only people with one of `roles` (and admins) see it.
    pub private: bool,
    pub roles: Vec<RoleId>,
}

impl Session {
    pub fn category(&self, id: CategoryId) -> Option<&Category> {
        self.categories.iter().find(|c| c.id == id)
    }

    /// Categories in the order the list shows them.
    pub fn sorted_categories(&self) -> Vec<&Category> {
        let mut cats: Vec<&Category> = self.categories.iter().collect();
        cats.sort_by_key(|c| (c.position, c.id));
        cats
    }

    /// The category a channel is listed under (channels in a category we can't see sit at the top).
    pub fn listed_category(&self, c: &Channel) -> Option<CategoryId> {
        c.category_id.filter(|id| self.category(*id).is_some())
    }

    /// Channels listed under a category (`None`: the top), text before voice, in order.
    pub fn channels_in(&self, category: Option<CategoryId>) -> Vec<&Channel> {
        let mut list: Vec<&Channel> = self
            .channels
            .iter()
            .filter(|c| matches!(c.kind, ChannelKind::Text | ChannelKind::Voice) && self.listed_category(c) == category)
            .collect();
        list.sort_by_key(|c| (c.kind == ChannelKind::Voice, c.position, c.id));
        list
    }

    /// Only some roles can see it (by its own list or its category's).
    pub fn is_private(&self, c: &Channel) -> bool {
        !c.allowed_roles.is_empty()
            || c.category_id.and_then(|id| self.category(id)).is_some_and(|cat| !cat.allowed_roles.is_empty())
    }
}

/// The positions to send after dropping channel `moving` on `target`, renumbering the
/// categories it leaves and joins. `channels` is every channel as (id, category, position);
/// channels whose category isn't in `categories` are listed at the top.
pub fn plan_channel_move(
    channels: &[(ChannelId, Option<CategoryId>, i64)],
    categories: &[CategoryId],
    moving: ChannelId,
    target: DropTarget,
) -> Vec<ChannelPosition> {
    let listed = |c: Option<CategoryId>| c.filter(|id| categories.contains(id));
    let Some(&(_, from_raw, _)) = channels.iter().find(|c| c.0 == moving) else { return Vec::new() };
    let from = listed(from_raw);
    let to = match target {
        DropTarget::BeforeChannel(id) if id == moving => return Vec::new(),
        DropTarget::BeforeChannel(id) => match channels.iter().find(|c| c.0 == id) {
            Some(c) => listed(c.1),
            None => return Vec::new(),
        },
        DropTarget::EndOf(c) => listed(c),
        DropTarget::AfterCategories => return Vec::new(),
    };
    let ordered = |cat: Option<CategoryId>| {
        let mut list: Vec<&(ChannelId, Option<CategoryId>, i64)> =
            channels.iter().filter(|c| listed(c.1) == cat && c.0 != moving).collect();
        list.sort_by_key(|c| (c.2, c.0));
        list
    };
    let mut dest = ordered(to);
    let at = match target {
        DropTarget::BeforeChannel(id) => dest.iter().position(|c| c.0 == id).unwrap_or(dest.len()),
        _ => dest.len(),
    };
    let me = channels.iter().find(|c| c.0 == moving).unwrap();
    dest.insert(at, me);
    let mut out = Vec::new();
    for (i, &&(id, cat, pos)) in dest.iter().enumerate() {
        if cat != to || pos != i as i64 {
            out.push(ChannelPosition { id, position: i as i64, category_id: to });
        }
    }
    if from != to {
        for (i, &&(id, cat, pos)) in ordered(from).iter().enumerate() {
            if cat != from || pos != i as i64 {
                out.push(ChannelPosition { id, position: i as i64, category_id: from });
            }
        }
    }
    out
}

/// The positions to send after dropping category `moving` just above `before` (`None`: last).
pub fn plan_category_move(
    categories: &[(CategoryId, i64)],
    moving: CategoryId,
    before: Option<CategoryId>,
) -> Vec<CategoryPosition> {
    if before == Some(moving) || !categories.iter().any(|c| c.0 == moving) {
        return Vec::new();
    }
    let mut list: Vec<&(CategoryId, i64)> = categories.iter().filter(|c| c.0 != moving).collect();
    list.sort_by_key(|c| (c.1, c.0));
    let at = before.and_then(|b| list.iter().position(|c| c.0 == b)).unwrap_or(list.len());
    list.insert(at, categories.iter().find(|c| c.0 == moving).unwrap());
    list.iter()
        .enumerate()
        .filter(|(i, c)| c.1 != *i as i64)
        .map(|(i, c)| CategoryPosition { id: c.0, position: i as i64 })
        .collect()
}

impl App {
    pub(crate) fn can_manage_channels(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.can(perms::MANAGE_CHANNELS))
    }

    pub(crate) fn category_collapsed(&self, id: CategoryId) -> bool {
        let key = crate::net::server_key(&self.settings.server);
        self.settings.collapsed_categories.get(&key).is_some_and(|c| c.contains(&id))
    }

    pub(crate) fn update_channels(&mut self, msg: ChannelsMsg) -> Task<Message> {
        match msg {
            ChannelsMsg::ToggleCategory(id) => {
                let key = crate::net::server_key(&self.settings.server);
                let list = self.settings.collapsed_categories.entry(key).or_default();
                if list.contains(&id) {
                    list.retain(|c| *c != id);
                } else {
                    list.push(id);
                }
                self.settings.save();
            }
            ChannelsMsg::Press(item) => {
                if self.can_manage_channels() {
                    let at = crate::app_messages::last_cursor();
                    self.channel_drag = Some(ChannelDrag { item, start: at, at, over: None });
                }
            }
            ChannelsMsg::DragMove(at) => {
                if let Some(d) = &mut self.channel_drag {
                    d.at = at;
                }
            }
            ChannelsMsg::DragOver(target) => {
                if let Some(d) = &mut self.channel_drag {
                    d.over = Some(target);
                }
            }
            ChannelsMsg::DragOut(target) => {
                if let Some(d) = self.channel_drag.as_mut().filter(|d| d.over == Some(target)) {
                    d.over = None;
                }
            }
            ChannelsMsg::DragRelease => {
                if let Some(d) = self.channel_drag.take().filter(|d| d.moved())
                    && let Some(target) = d.over
                {
                    self.drop_dragged(d.item, target);
                }
            }
            ChannelsMsg::ToggleServerMenu => self.server_menu = !self.server_menu,
            ChannelsMsg::ServerSettings(tab) => {
                self.server_menu = false;
                let open = self.update(Message::OpenSettings);
                return Task::batch([open, self.update(Message::SettingsTab(tab))]);
            }
            ChannelsMsg::NewChannelIn(category, kind) => {
                self.server_menu = false;
                let open = self.update(Message::OpenNewChannel(kind));
                self.new_channel_category = category;
                return open;
            }
            ChannelsMsg::NewCategory => {
                self.server_menu = false;
                self.overlay = Some(Overlay::Dialog(Dialog::Channel(ChannelEdit {
                    target: EditTarget::NewCategory,
                    name: String::new(),
                    topic: String::new(),
                    private: false,
                    roles: Vec::new(),
                })));
                return iced::widget::operation::focus(iced::widget::Id::new(EDIT_NAME_ID));
            }
            ChannelsMsg::Edit(target) => {
                let Some(s) = &self.session else { return Task::none() };
                let edit = match target {
                    EditTarget::Channel(id) => s.channels.iter().find(|c| c.id == id).map(|c| ChannelEdit {
                        target,
                        name: c.name.clone(),
                        topic: c.topic.clone(),
                        private: !c.allowed_roles.is_empty(),
                        roles: c.allowed_roles.clone(),
                    }),
                    EditTarget::Category(id) => s.category(id).map(|c| ChannelEdit {
                        target,
                        name: c.name.clone(),
                        topic: String::new(),
                        private: !c.allowed_roles.is_empty(),
                        roles: c.allowed_roles.clone(),
                    }),
                    EditTarget::NewCategory => None,
                };
                if let Some(edit) = edit {
                    self.close_overlay();
                    self.overlay = Some(Overlay::Dialog(Dialog::Channel(edit)));
                }
            }
            ChannelsMsg::EditName(v) => self.edit_channel_draft(|e| e.name = v),
            ChannelsMsg::EditTopic(v) => self.edit_channel_draft(|e| e.topic = v),
            ChannelsMsg::EditPrivate(on) => self.edit_channel_draft(|e| e.private = on),
            ChannelsMsg::EditRole(role, on) => self.edit_channel_draft(|e| {
                e.roles.retain(|r| *r != role);
                if on {
                    e.roles.push(role);
                }
            }),
            ChannelsMsg::Save => self.save_channel_edit(),
            ChannelsMsg::AskDeleteCategory(id) => {
                let name = self.session.as_ref().and_then(|s| s.category(id)).map(|c| c.name.clone());
                if let Some(name) = name {
                    self.ask(
                        "Delete category",
                        format!("Delete the {name} category? Its channels stay, and move to the top of the list."),
                        "Delete Category",
                        ConfirmAction::DeleteCategory(id),
                    );
                }
            }
        }
        Task::none()
    }

    fn edit_channel_draft(&mut self, f: impl FnOnce(&mut ChannelEdit)) {
        if let Some(Overlay::Dialog(Dialog::Channel(e))) = &mut self.overlay {
            f(e);
        }
    }

    fn save_channel_edit(&mut self) {
        let Some(Overlay::Dialog(Dialog::Channel(e))) = self.overlay.clone() else { return };
        let Some(s) = &self.session else { return };
        let roles = if e.private { e.roles.clone() } else { Vec::new() };
        let name = e.name.trim().to_string();
        let msg = match e.target {
            EditTarget::NewCategory if !name.is_empty() => Some(ClientMsg::CreateCategory { name }),
            EditTarget::NewCategory => None,
            EditTarget::Category(id) => s.category(id).map(|c| ClientMsg::UpdateCategory {
                category_id: id,
                name: (name != c.name && !name.is_empty()).then_some(name),
                allowed_roles: (roles != c.allowed_roles).then_some(roles),
            }),
            EditTarget::Channel(id) => s.channels.iter().find(|c| c.id == id).map(|c| ClientMsg::UpdateChannel {
                channel_id: id,
                name: (name != c.name && !name.is_empty()).then_some(name),
                topic: (e.topic.trim() != c.topic).then(|| e.topic.trim().to_string()),
                allowed_roles: (roles != c.allowed_roles).then_some(roles),
            }),
        };
        if let Some(msg) = msg {
            self.send(msg);
            self.close_overlay();
        }
    }

    /// Something was let go over `target`: work out the new order, show it and send it.
    fn drop_dragged(&mut self, item: DragItem, target: DropTarget) {
        let Some(s) = &mut self.session else { return };
        if !s.can(perms::MANAGE_CHANNELS) {
            return;
        }
        match item {
            DragItem::Channel(id) => {
                let all: Vec<_> = s.channels.iter().map(|c| (c.id, c.category_id, c.position)).collect();
                let cats: Vec<CategoryId> = s.categories.iter().map(|c| c.id).collect();
                let plan = plan_channel_move(&all, &cats, id, target);
                if plan.is_empty() {
                    return;
                }
                for p in &plan {
                    if let Some(c) = s.channels.iter_mut().find(|c| c.id == p.id) {
                        c.position = p.position;
                        c.category_id = p.category_id;
                    }
                }
                s.channels.sort_by_key(|c| (c.position, c.id));
                self.send(ClientMsg::ReorderChannels { categories: Vec::new(), channels: plan });
            }
            DragItem::Category(id) => {
                let before = match target {
                    DropTarget::EndOf(Some(c)) => Some(c),
                    DropTarget::AfterCategories => None,
                    // Over a channel: just above the category it's in.
                    DropTarget::BeforeChannel(ch) => {
                        let channel = s.channels.iter().find(|c| c.id == ch);
                        match channel.and_then(|c| s.listed_category(c)) {
                            Some(c) => Some(c),
                            None => s.sorted_categories().first().map(|c| c.id),
                        }
                    }
                    DropTarget::EndOf(None) => s.sorted_categories().first().map(|c| c.id),
                };
                let all: Vec<_> = s.categories.iter().map(|c| (c.id, c.position)).collect();
                let plan = plan_category_move(&all, id, before);
                if plan.is_empty() {
                    return;
                }
                for p in &plan {
                    if let Some(c) = s.categories.iter_mut().find(|c| c.id == p.id) {
                        c.position = p.position;
                    }
                }
                self.send(ClientMsg::ReorderChannels { categories: plan, channels: Vec::new() });
            }
        }
    }

    pub(crate) fn on_channel_event(&mut self, msg: ServerMsg) -> Task<Message> {
        let Some(s) = &mut self.session else { return Task::none() };
        match msg {
            ServerMsg::ChannelUpdated(c) => {
                match s.channels.iter_mut().find(|x| x.id == c.id) {
                    Some(slot) => *slot = c,
                    None => s.channels.push(c),
                }
                s.channels.sort_by_key(|c| (c.position, c.id));
            }
            ServerMsg::CategoryUpdated(c) => match s.categories.iter_mut().find(|x| x.id == c.id) {
                Some(slot) => *slot = c,
                None => s.categories.push(c),
            },
            ServerMsg::CategoryDeleted { category_id } => {
                s.categories.retain(|c| c.id != category_id);
                for c in s.channels.iter_mut().filter(|c| c.category_id == Some(category_id)) {
                    c.category_id = None;
                }
            }
            _ => {}
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moves(plan: &[ChannelPosition]) -> Vec<(ChannelId, i64, Option<CategoryId>)> {
        let mut v: Vec<_> = plan.iter().map(|p| (p.id, p.position, p.category_id)).collect();
        v.sort();
        v
    }

    #[test]
    fn moving_channels_within_and_between_categories() {
        // Top: 1, 2, 3. Category 10: 4, 5.
        let chans = [(1, None, 0), (2, None, 1), (3, None, 2), (4, Some(10), 0), (5, Some(10), 1)];
        let cats = [10];
        // 3 above 1: the top is renumbered.
        let plan = plan_channel_move(&chans, &cats, 3, DropTarget::BeforeChannel(1));
        assert_eq!(moves(&plan), vec![(1, 1, None), (2, 2, None), (3, 0, None)]);
        // 1 into the category, above 5: both categories are renumbered, nothing else changes.
        let plan = plan_channel_move(&chans, &cats, 1, DropTarget::BeforeChannel(5));
        assert_eq!(moves(&plan), vec![(1, 1, Some(10)), (2, 0, None), (3, 1, None), (5, 2, Some(10))]);
        // Dropped on the category's header: last in it.
        let plan = plan_channel_move(&chans, &cats, 2, DropTarget::EndOf(Some(10)));
        assert_eq!(moves(&plan), vec![(2, 2, Some(10)), (3, 1, None)]);
        // Where it already is: nothing to send.
        assert!(plan_channel_move(&chans, &cats, 2, DropTarget::BeforeChannel(3)).is_empty());
        assert!(plan_channel_move(&chans, &cats, 2, DropTarget::BeforeChannel(2)).is_empty());
        assert!(plan_channel_move(&chans, &cats, 99, DropTarget::EndOf(None)).is_empty());
    }

    #[test]
    fn channels_of_unknown_categories_count_as_top_level() {
        // Channel 2 says it's in category 7, which we can't see.
        let chans = [(1, None, 0), (2, Some(7), 5)];
        let plan = plan_channel_move(&chans, &[], 1, DropTarget::EndOf(None));
        assert_eq!(moves(&plan), vec![(1, 1, None), (2, 0, None)]);
    }

    #[test]
    fn moving_categories() {
        let cats = [(10, 0), (11, 1), (12, 2)];
        let plan = plan_category_move(&cats, 12, Some(10));
        let mut got: Vec<_> = plan.iter().map(|p| (p.id, p.position)).collect();
        got.sort();
        assert_eq!(got, vec![(10, 1), (11, 2), (12, 0)]);
        let plan = plan_category_move(&cats, 10, None);
        let mut got: Vec<_> = plan.iter().map(|p| (p.id, p.position)).collect();
        got.sort();
        assert_eq!(got, vec![(10, 2), (11, 0), (12, 1)]);
        assert!(plan_category_move(&cats, 11, Some(12)).is_empty());
        assert!(plan_category_move(&cats, 11, Some(11)).is_empty());
    }
}
