//! Shared UI primitives, layout helpers, dialogs, debouncing, and typed
//! side-window handles used across `chat-app`'s windows.
//!
//! Centralizing these here eliminates sideways imports between windows (e.g.
//! `widgets_window` importing from `reactions_window` or `app_view`) and gives
//! every window consistent styling and behavior.

use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use chat_engine::{Health, OutputStatus};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme, Root, Selectable, Sizable, StyledExt, WindowExt, h_flex, v_flex,
};
use gpui_kit::*;

/// How long typing must pause before debounced edits apply (settings and
/// widgets windows).
pub(crate) const APPLY_AFTER: Duration = Duration::from_millis(800);

/// Coalesces rapid edits (typing, number stepping) so they apply once input
/// pauses for [`APPLY_AFTER`]. Dropping the stored `Task` cancels any previous
/// timer automatically.
#[derive(Default)]
pub(crate) struct Debouncer {
    pending: Option<Task<()>>,
    dirty: bool,
}

impl Debouncer {
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Marks the form dirty and (re)starts the debounce timer.
    pub(crate) fn schedule<T: 'static>(
        &mut self,
        cx: &mut Context<T>,
        on_fire: impl FnOnce(&mut T, &mut Context<T>) + 'static,
    ) {
        self.dirty = true;
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(APPLY_AFTER).await;
            let _ = this.update(cx, on_fire);
        }));
    }

    /// Cancels any pending timer and clears the dirty flag without firing.
    pub(crate) fn cancel(&mut self) {
        self.pending = None;
        self.dirty = false;
    }

    /// Cancels any pending timer and returns whether there were unapplied
    /// edits (clearing the dirty flag). Useful for `flush()` / `apply_now()`.
    pub(crate) fn take_dirty(&mut self) -> bool {
        self.pending = None;
        std::mem::take(&mut self.dirty)
    }
}

/// A typed handle to a singleton side window (Settings, Reactions, Widgets,
/// Test messages), storing both its `WindowHandle<Root>` and a `WeakEntity<V>`
/// to its inner view so callers never need to downcast `Root::view()`.
pub(crate) struct SideWindow<V> {
    window: WindowHandle<Root>,
    view: WeakEntity<V>,
}

impl<V: Render + 'static> SideWindow<V> {
    /// Brings the window in `slot` to the front if it's still open; otherwise
    /// opens a new one built by `build` and stores it in `slot`.
    pub(crate) fn open_or_focus(
        slot: &mut Option<Self>,
        title: &str,
        size: Size<Pixels>,
        build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
        window: &mut Window,
        cx: &mut App,
    ) {
        // `update` fails once the window is closed; then open a new one.
        if let Some(side) = slot.as_ref()
            && side
                .window
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size, cx))),
            titlebar: Some(TitlebarOptions {
                title: Some(format!("{title} – chat-aggregator").into()),
                ..Default::default()
            }),
            // Same Wayland app id as the main window: window managers group them.
            app_id: Some("chat-aggregator".into()),
            window_min_size: Some(gpui_kit::size(px(400.), px(320.))),
            ..Default::default()
        };
        let mut weak = None;
        match cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| build(window, cx));
            weak = Some(view.downgrade());
            cx.new(|cx| Root::new(view, window, cx))
        }) {
            Ok(handle) => {
                *slot = weak.map(|view| Self {
                    window: handle,
                    view,
                });
            }
            Err(e) => report(window, cx, format!("Couldn't open the window: {e:#}")),
        }
    }

    /// The live view entity if this side window is still open.
    pub(crate) fn view(&self) -> Option<Entity<V>> {
        self.view.upgrade()
    }

    #[cfg(test)]
    pub(crate) fn window(&self) -> WindowHandle<Root> {
        self.window
    }
}

/// The status light: a small coloured dot.
pub(crate) fn light(health: Health, cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let color = match health {
        Health::Ok => theme.success,
        Health::Warning => theme.warning,
        Health::Error => theme.danger,
        Health::Off => theme.muted_foreground,
    };
    div().flex_none().size(px(10.)).rounded_full().bg(color)
}

/// A bordered section card: a thin rounded frame so a window reads as a few
/// blocks rather than one long list.
pub(crate) fn card(cx: &App) -> Div {
    v_flex()
        .gap_2()
        .p_3()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
}

/// A choice of one among a few, as a segmented control: one tinted track
/// with the chosen item filled in. Built from buttons (not gpui-kit's
/// `TabBar`) because a tab bar scrolls sideways when too wide, while this
/// track wraps cleanly onto a second line.
pub(crate) fn segmented(cx: &App, choices: impl IntoIterator<Item = Button>) -> Div {
    h_flex()
        // As wide as its choices, not the whole row.
        .self_start()
        .flex_wrap()
        .gap(px(2.))
        .p(px(3.))
        .rounded(cx.theme().radius)
        .bg(cx.theme().tab_bar_segmented)
        .children(choices)
}

/// One item of a [`segmented`] choice (filled when chosen, flat ghost on the
/// track otherwise).
pub(crate) fn choice(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
) -> Button {
    let button = Button::new(id).label(label).small().selected(selected);
    if selected {
        button.primary()
    } else {
        button.ghost()
    }
}

/// A form row: the label in a 90px column, then the control.
pub(crate) fn labelled<E: IntoElement>(
    label: &'static str,
    control: E,
    cx: &App,
) -> impl IntoElement + use<E> {
    labelled_width(px(90.), label, control, cx)
}

/// A form row with a custom fixed label column width.
pub(crate) fn labelled_width<L: Into<SharedString>, E: IntoElement>(
    width: Pixels,
    label: L,
    control: E,
    cx: &App,
) -> impl IntoElement + use<L, E> {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(width)
                .flex_none()
                .text_color(cx.theme().muted_foreground)
                .child(label.into()),
        )
        .child(div().flex_1().min_w_0().child(control))
}

/// One program the reactions send to: its light, name, and connection summary.
pub(crate) fn render_output(output: &OutputStatus, cx: &App) -> impl IntoElement + use<> {
    h_flex()
        .gap_2()
        .items_center()
        .child(light(Health::from(&output.state), cx))
        .child(div().font_semibold().child(output.target.label()))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(cx.theme().muted_foreground)
                .child(output.summary()),
        )
}

/// Shows an error toast notification in `window`.
pub(crate) fn report(window: &mut Window, cx: &mut App, message: String) {
    window.push_notification(Notification::error(message), cx);
}

/// Creates `dir` if it doesn't exist yet and opens it in the system file
/// manager, reporting any error as a notification.
pub(crate) fn open_folder(dir: &Path, window: &mut Window, cx: &mut App) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        report(
            window,
            cx,
            format!("Couldn't create {}: {e}", dir.display()),
        );
        return;
    }
    cx.open_with_system(dir);
}

/// Asks a yes/no question; `on_ok` runs on yes, nothing on cancel.
pub(crate) fn confirm(
    window: &mut Window,
    cx: &mut App,
    title: String,
    description: impl Into<SharedString>,
    ok: &'static str,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    confirm_with_cancel(window, cx, title, description, ok, on_ok, |_, _| {});
}

/// Like [`confirm`], but also invokes `on_cancel` when dismissed without
/// confirming (useful when a window tracks whether a modal dialog is open).
pub(crate) fn confirm_with_cancel(
    window: &mut Window,
    cx: &mut App,
    title: String,
    description: impl Into<SharedString>,
    ok: &'static str,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&mut Window, &mut App) + 'static,
) {
    // The builder may run again on redraws (it's an `Fn`), so what it hands on
    // is shared via `Rc` / `SharedString` rather than moved.
    let on_ok = Rc::new(on_ok);
    let on_cancel = Rc::new(on_cancel);
    let description = description.into();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = on_ok.clone();
        let on_cancel_btn = on_cancel.clone();
        let on_close = on_cancel.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            .ok_text(ok)
            .show_cancel(true)
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            })
            .on_cancel(move |_, window, cx| {
                on_cancel_btn(window, cx);
                true
            })
            .on_close(move |_, window, cx| {
                on_close(window, cx);
            })
    });
}
