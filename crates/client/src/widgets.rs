//! Small custom widgets.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector, keyboard, mouse};

/// Right-click `base` to open the menu `menu` builds, at the mouse pointer.
/// The menu closes when something in it is clicked, on a click elsewhere, or with Escape.
pub fn context_menu<'a, Message: 'a>(
    base: impl Into<Element<'a, Message>>,
    menu: impl Fn() -> Element<'a, Message> + 'a,
) -> ContextMenu<'a, Message> {
    ContextMenu { base: base.into(), menu: Box::new(menu), built: None }
}

pub struct ContextMenu<'a, Message> {
    base: Element<'a, Message>,
    menu: Box<dyn Fn() -> Element<'a, Message> + 'a>,
    /// The menu, made only while it's open (so closed menus cost nothing).
    built: Option<Element<'a, Message>>,
}

#[derive(Default)]
struct State {
    /// Where it was opened, in the base's coordinates.
    open_at: Option<Point>,
    menu: Option<Tree>,
    /// When it opened (for the pop-in).
    opened: Option<std::time::Instant>,
}

impl<'a, Message: 'a> Widget<Message, Theme, Renderer> for ContextMenu<'a, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.base)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.base));
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(State::default())
    }

    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State>()
    }

    fn size(&self) -> Size<Length> {
        self.base.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.base.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.base.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) = event
            && let Some(at) = cursor.position_over(layout.bounds())
        {
            let state = tree.state.downcast_mut::<State>();
            state.open_at = Some(at);
            state.menu = None;
            state.opened = Some(std::time::Instant::now());
            shell.capture_event();
            shell.invalidate_layout();
            shell.request_redraw();
            return;
        }
        self.base.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.base.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.base.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.base.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let (state, children) = (&mut tree.state, &mut tree.children);
        let state = state.downcast_mut::<State>();
        let Some(at) = state.open_at else {
            self.built = None;
            return self.base.as_widget_mut().overlay(&mut children[0], layout, renderer, viewport, translation);
        };
        let menu = self.built.insert((self.menu)());
        match &mut state.menu {
            Some(t) => t.diff(&*menu),
            None => state.menu = Some(Tree::new(&*menu)),
        }
        Some(overlay::Element::new(Box::new(MenuOverlay { position: at + translation, menu, state })))
    }
}

impl<'a, Message: 'a> From<ContextMenu<'a, Message>> for Element<'a, Message> {
    fn from(c: ContextMenu<'a, Message>) -> Self {
        Element::new(c)
    }
}

struct MenuOverlay<'a, 'b, Message> {
    position: Point,
    menu: &'b mut Element<'a, Message>,
    state: &'b mut State,
}

impl<Message> MenuOverlay<'_, '_, Message> {
    fn close(&mut self, shell: &mut Shell<'_, Message>) {
        self.state.open_at = None;
        self.state.menu = None;
        shell.invalidate_layout();
        shell.request_redraw();
    }
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for MenuOverlay<'_, '_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let tree = self.state.menu.as_mut().expect("menu tree exists while open");
        let node = self.menu.as_widget_mut().layout(tree, renderer, &layout::Limits::new(Size::ZERO, bounds));
        let size = node.size();
        // Open down and to the right of the pointer, but stay on screen.
        let x = if self.position.x + size.width > bounds.width {
            (self.position.x - size.width).max(0.0)
        } else {
            self.position.x
        };
        // Near the bottom (like the message box), open upwards instead.
        let y = if self.position.y + size.height > bounds.height {
            (self.position.y - size.height).max(0.0)
        } else {
            self.position.y
        };
        node.move_to(Point::new(x, y))
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        use iced::advanced::Renderer as _;
        let Some(tree) = &self.state.menu else { return };
        let elapsed = self.state.opened.map_or(POP_MS, |t| t.elapsed().as_secs_f32() * 1000.0);
        let scale = pop_scale(elapsed);
        if (scale - 1.0).abs() < 0.001 {
            return self.menu.as_widget().draw(tree, renderer, theme, style, layout, cursor, &layout.bounds());
        }
        // Grow out of the corner it opened from.
        let p = layout.bounds().position();
        let t = iced::Transformation::translate(p.x, p.y)
            * iced::Transformation::scale(scale)
            * iced::Transformation::translate(-p.x, -p.y);
        renderer.with_transformation(t, |r| {
            self.menu.as_widget().draw(tree, r, theme, style, layout, cursor, &layout.bounds());
        });
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        let inside = cursor.is_over(layout.bounds());
        if let Event::Window(iced::window::Event::RedrawRequested(_)) = event
            && self.state.opened.is_some_and(|t| t.elapsed().as_secs_f32() * 1000.0 < POP_MS)
        {
            shell.request_redraw();
        }
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) => {
                self.close(shell);
                shell.capture_event();
                return;
            }
            Event::Mouse(mouse::Event::ButtonPressed(_)) if !inside => {
                // A click elsewhere just closes the menu.
                self.close(shell);
                shell.capture_event();
                return;
            }
            _ => {}
        }
        if let Some(tree) = self.state.menu.as_mut() {
            let bounds = layout.bounds();
            self.menu.as_widget_mut().update(tree, event, layout, cursor, renderer, clipboard, shell, &bounds);
        }
        if inside && matches!(event, Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))) {
            self.close(shell);
        }
        if inside && matches!(event, Event::Mouse(_)) {
            shell.capture_event();
        }
    }

    fn mouse_interaction(&self, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &Renderer) -> mouse::Interaction {
        match &self.state.menu {
            Some(tree) => self.menu.as_widget().mouse_interaction(tree, layout, cursor, &layout.bounds(), renderer),
            None => mouse::Interaction::None,
        }
    }
}

/// Draw `content` scaled about its middle (layout and clicks are unchanged).
/// Used for the little "pop" when dialogs and menus open.
pub fn pop<'a, Message: 'a>(content: impl Into<Element<'a, Message>>, scale: f32) -> Pop<'a, Message> {
    Pop { content: content.into(), scale, offset: Vector::ZERO }
}

/// `content` drawn moved by `offset` (for sliding things into place). Like [`pop`],
/// only the drawing moves, so it's free when the offset is zero.
pub fn slide<'a, Message: 'a>(content: impl Into<Element<'a, Message>>, offset: Vector) -> Pop<'a, Message> {
    Pop { content: content.into(), scale: 1.0, offset }
}

pub struct Pop<'a, Message> {
    content: Element<'a, Message>,
    scale: f32,
    offset: Vector,
}

/// Quick ease-out: fast at first, gently settling.
pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// How far something sliding `distance` px into place over `duration_ms` still has to
/// go after `elapsed_ms` (0 once it's there).
pub fn slide_left(elapsed_ms: f32, duration_ms: f32, distance: f32) -> f32 {
    distance * (1.0 - ease_out(elapsed_ms / duration_ms))
}

/// How long slides take.
pub const SLIDE_MS: f32 = 220.0;

/// How big something that opened `elapsed_ms` ago is drawn: a quick grow with a
/// slight overshoot, settling at full size after [`POP_MS`].
pub fn pop_scale(elapsed_ms: f32) -> f32 {
    let t = (elapsed_ms / POP_MS).clamp(0.0, 1.0);
    // "Ease out back": overshoots a little, then settles.
    let (c1, u) = (2.2_f32, t - 1.0);
    let ease = 1.0 + (c1 + 1.0) * u * u * u + c1 * u * u;
    0.9 + 0.1 * ease
}

/// Length of the pop animation, in milliseconds.
pub const POP_MS: f32 = 260.0;

impl<'a, Message: 'a> Widget<Message, Theme, Renderer> for Pop<'a, Message> {
    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn state(&self) -> widget::tree::State {
        self.content.as_widget().state()
    }

    fn tag(&self) -> widget::tree::Tag {
        self.content.as_widget().tag()
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        if (self.scale - 1.0).abs() < 0.001 && self.offset.x.abs() < 0.1 && self.offset.y.abs() < 0.1 {
            return self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
        }
        let c = layout.bounds().center();
        let t = iced::Transformation::translate(c.x + self.offset.x, c.y + self.offset.y)
            * iced::Transformation::scale(self.scale)
            * iced::Transformation::translate(-c.x, -c.y);
        renderer.with_transformation(t, |r| {
            self.content.as_widget().draw(tree, r, theme, style, layout, cursor, viewport);
        });
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a, Message: 'a> From<Pop<'a, Message>> for Element<'a, Message> {
    fn from(p: Pop<'a, Message>) -> Self {
        Element::new(p)
    }
}

#[cfg(test)]
mod tests {
    use super::pop_scale;

    #[test]
    fn pop_grows_overshoots_and_settles() {
        assert!((pop_scale(0.0) - 0.9).abs() < 1e-4);
        let peak = (0..=260).map(|ms| pop_scale(ms as f32)).fold(0.0, f32::max);
        assert!(peak > 1.0 && peak < 1.04, "a small bounce past full size: {peak}");
        assert!((pop_scale(260.0) - 1.0).abs() < 1e-4);
        assert!((pop_scale(9999.0) - 1.0).abs() < 1e-4);
    }
}

// ---- Hover fade

/// A background that fades in when the pointer is over `content` and out when it
/// leaves (instead of switching instantly). Redraws only while fading.
pub fn hover_fade<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    color: iced::Color,
    radius: f32,
) -> HoverFade<'a, Message> {
    HoverFade { content: content.into(), color, radius }
}

pub struct HoverFade<'a, Message> {
    content: Element<'a, Message>,
    color: iced::Color,
    radius: f32,
}

/// Fade length in milliseconds.
const FADE_MS: f32 = 140.0;

#[derive(Default)]
struct FadeState {
    hovered: bool,
    /// When `hovered` last changed, and how faded-in it was then.
    since: Option<(std::time::Instant, f32)>,
}

impl FadeState {
    fn level(&self) -> f32 {
        let target = if self.hovered { 1.0 } else { 0.0 };
        let Some((at, from)) = self.since else { return target };
        let t = ease_out(at.elapsed().as_secs_f32() * 1000.0 / FADE_MS);
        from + (target - from) * t
    }

    fn fading(&self) -> bool {
        self.since.is_some_and(|(at, _)| at.elapsed().as_secs_f32() * 1000.0 < FADE_MS)
    }
}

impl<'a, Message: 'a> Widget<Message, Theme, Renderer> for HoverFade<'a, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(FadeState::default())
    }

    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<FadeState>()
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<FadeState>();
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) => {
                let over = cursor.is_over(layout.bounds());
                if over != state.hovered {
                    state.since = Some((std::time::Instant::now(), state.level()));
                    state.hovered = over;
                    shell.request_redraw();
                }
            }
            Event::Window(iced::window::Event::RedrawRequested(_)) if state.fading() => shell.request_redraw(),
            _ => {}
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        let level = tree.state.downcast_ref::<FadeState>().level();
        if level > 0.01 {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: iced::Border { radius: self.radius.into(), ..Default::default() },
                    ..Default::default()
                },
                iced::Color { a: self.color.a * level, ..self.color },
            );
        }
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

impl<'a, Message: 'a> From<HoverFade<'a, Message>> for Element<'a, Message> {
    fn from(h: HoverFade<'a, Message>) -> Self {
        Element::new(h)
    }
}

// ---- On-screen sensor

/// Tells the app (with `on_change(true/false)`) when `content` scrolls into or out of
/// view. It only speaks up when that changes, so it costs nothing while still.
pub fn on_screen<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    on_change: impl Fn(bool) -> Message + 'a,
) -> OnScreen<'a, Message> {
    OnScreen { content: content.into(), on_change: Box::new(on_change) }
}

pub struct OnScreen<'a, Message> {
    content: Element<'a, Message>,
    on_change: Box<dyn Fn(bool) -> Message + 'a>,
}

#[derive(Default)]
struct SeenState {
    seen: Option<bool>,
}

impl<'a, Message: 'a> Widget<Message, Theme, Renderer> for OnScreen<'a, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(SeenState::default())
    }

    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<SeenState>()
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Window(iced::window::Event::RedrawRequested(_)) = event {
            let seen = layout.bounds().intersects(viewport);
            let state = tree.state.downcast_mut::<SeenState>();
            if state.seen != Some(seen) {
                state.seen = Some(seen);
                shell.publish((self.on_change)(seen));
            }
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

impl<'a, Message: 'a> From<OnScreen<'a, Message>> for Element<'a, Message> {
    fn from(o: OnScreen<'a, Message>) -> Self {
        Element::new(o)
    }
}

// ---- Pictures that animate themselves

/// Milliseconds since GIFs started playing (they all share one clock).
pub fn anim_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// A decoded picture `width` x `height`, filling the box (`cover`) or fitting inside it.
/// A GIF plays by itself while on screen (with `playing`): it asks for a redraw just
/// when its next frame is due, so the rest of the window isn't rebuilt or redrawn.
pub fn picture<'a, Message: 'a>(
    pic: std::sync::Arc<crate::media::Picture>,
    width: f32,
    height: f32,
    cover: bool,
    radius: f32,
    playing: bool,
) -> Element<'a, Message> {
    Element::new(AnimatedPicture { pic, width, height, cover, radius, playing })
}

struct AnimatedPicture {
    pic: std::sync::Arc<crate::media::Picture>,
    width: f32,
    height: f32,
    cover: bool,
    radius: f32,
    playing: bool,
}

impl AnimatedPicture {
    fn elapsed(&self) -> u64 {
        if self.playing { anim_ms() } else { 0 }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for AnimatedPicture {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.width), Length::Fixed(self.height))
    }

    fn layout(&mut self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        layout::Node::new(limits.resolve(self.width, self.height, Size::new(self.width, self.height)))
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Window(iced::window::Event::RedrawRequested(now)) = event
            && self.playing
            && self.pic.is_animated()
            && layout.bounds().intersects(viewport)
        {
            let wait = self.pic.until_next_frame(anim_ms()).min(1000);
            shell.request_redraw_at(iced::window::RedrawRequest::At(*now + std::time::Duration::from_millis(wait)));
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        use iced::advanced::image::Renderer as _;
        let bounds = layout.bounds();
        let (iw, ih) = self.pic.size();
        let (iw, ih) = (iw.max(1) as f32, ih.max(1) as f32);
        let scale = if self.cover {
            (bounds.width / iw).max(bounds.height / ih)
        } else {
            (bounds.width / iw).min(bounds.height / ih)
        };
        let (w, h) = (iw * scale, ih * scale);
        let at = Rectangle {
            x: bounds.x + (bounds.width - w) / 2.0,
            y: bounds.y + (bounds.height - h) / 2.0,
            width: w,
            height: h,
        };
        let mut image = iced::advanced::image::Image::new(self.pic.frame(self.elapsed()).clone());
        image.border_radius = self.radius.into();
        renderer.draw_image(image, at, bounds);
    }
}

// ---- Hover that follows the mouse, not the scrolling

/// Like `mouse_area`'s on_enter/on_exit, but only the mouse moving changes it: content
/// scrolling under a still pointer doesn't, so scrolling past hundreds of messages
/// doesn't rebuild the window for each one.
pub fn hover_sensor<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    on_change: impl Fn(bool) -> Message + 'a,
) -> HoverSensor<'a, Message> {
    HoverSensor { content: content.into(), on_change: Box::new(on_change) }
}

pub struct HoverSensor<'a, Message> {
    content: Element<'a, Message>,
    on_change: Box<dyn Fn(bool) -> Message + 'a>,
}

impl<'a, Message: 'a> Widget<Message, Theme, Renderer> for HoverSensor<'a, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(SeenState::default())
    }

    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<SeenState>()
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let over = match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => Some(cursor.is_over(layout.bounds())),
            Event::Mouse(mouse::Event::CursorLeft) => Some(false),
            _ => None,
        };
        let state = tree.state.downcast_mut::<SeenState>();
        if let Some(over) = over
            && state.seen.unwrap_or(false) != over
        {
            state.seen = Some(over);
            shell.publish((self.on_change)(over));
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

impl<'a, Message: 'a> From<HoverSensor<'a, Message>> for Element<'a, Message> {
    fn from(h: HoverSensor<'a, Message>) -> Self {
        Element::new(h)
    }
}
