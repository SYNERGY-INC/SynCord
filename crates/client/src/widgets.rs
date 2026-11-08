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
        let y = (self.position.y).min((bounds.height - size.height).max(0.0));
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
            Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Escape), .. }) => {
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
    Pop { content: content.into(), scale }
}

pub struct Pop<'a, Message> {
    content: Element<'a, Message>,
    scale: f32,
}

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
        if (self.scale - 1.0).abs() < 0.001 {
            return self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
        }
        let c = layout.bounds().center();
        let t = iced::Transformation::translate(c.x, c.y)
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
