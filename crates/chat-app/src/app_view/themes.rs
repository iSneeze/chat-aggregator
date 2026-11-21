//! The Overlay card in the main window: URL copy, connection count, theme
//! picker, link to the Themes customization window, chat direction picker, and
//! background theme folder scanning (so render frames never touch the
//! filesystem).

use chat_engine::Newest;
use chat_render::{Builtin, themes};
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::Select;
use gpui_kit::component::{ActiveTheme, IconName, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use gpui_kit::*;

use super::AppView;
use crate::app_config::edit_settings;
use crate::ui::{card, open_folder};

/// The chat direction dropdown's entries.
pub(super) const NEWEST_CHOICES: [(Newest, &str); 2] = [
    (Newest::Bottom, "Newest at bottom"),
    (Newest::Top, "Newest at top"),
];

pub(super) fn newest_label(newest: Newest) -> SharedString {
    NEWEST_CHOICES
        .iter()
        .find(|(n, _)| *n == newest)
        .map(|(_, label)| SharedString::from(*label))
        .unwrap_or_default()
}

/// The theme picker's entry for the theme saved in `config.toml`
/// (`None` = the default built-in theme): built-in themes by their label,
/// folders by their name. No folder can be called like a built-in theme
/// (`themes::validate_name` refuses it), so the labels are unambiguous.
pub(super) fn theme_label(saved: Option<&str>) -> SharedString {
    match saved {
        None => Builtin::Default.label().into(),
        Some(name) => Builtin::from_name(name)
            .map_or_else(|| name.to_string(), |b| b.label().to_string())
            .into(),
    }
}

/// The picker's entries: the built-in themes, then the theme folders.
pub(super) fn theme_items(themes: &[String]) -> Vec<SharedString> {
    Builtin::ALL
        .into_iter()
        .map(|b| SharedString::from(b.label()))
        .chain(themes.iter().cloned().map(SharedString::from))
        .collect()
}

impl AppView {
    /// Scans the themes folder and checks whether the configured custom theme
    /// folder exists, both off the UI thread.
    pub(super) fn rescan_themes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme_scan_gen += 1;
        let scan_gen = self.theme_scan_gen;
        let config = self.config.read(cx);
        let themes_dir = config.themes_dir();
        let overlay = config.settings().profile.overlay;
        let folder = overlay
            .theme_source(Some(&themes_dir))
            .folder()
            .map(std::path::Path::to_path_buf);
        let missing_name = overlay.theme.clone().unwrap_or_default();

        let scanning = cx.background_executor().spawn(async move {
            let themes = themes::list(&themes_dir);
            let missing = folder
                .is_some_and(|dir| !dir.is_dir())
                .then_some(missing_name);
            (themes, missing)
        });
        cx.spawn_in(window, async move |this, cx| {
            let (themes, missing) = scanning.await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.theme_scan_gen != scan_gen {
                    return;
                }
                let mut changed = false;
                if themes != view.themes {
                    let current = theme_label(
                        view.config
                            .read(cx)
                            .settings()
                            .profile
                            .overlay
                            .theme
                            .as_deref(),
                    );
                    view.themes = themes;
                    let items = theme_items(&view.themes);
                    view.theme_select.update(cx, |select, cx| {
                        select.set_items(items, window, cx);
                        select.set_selected_value(&current, window, cx);
                    });
                    changed = true;
                }
                if view.missing_theme != missing {
                    view.missing_theme = missing;
                    changed = true;
                }
                if changed {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Switches the overlay theme: overlays reload by themselves (the engine
    /// tells them to) and the choice is saved.
    pub(super) fn select_theme(
        &mut self,
        label: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // What config.toml gets: nothing for the default, a built-in
        // theme's name, or the folder's name.
        let theme = match Builtin::ALL
            .into_iter()
            .find(|b| b.label() == label.as_ref())
        {
            Some(Builtin::Default) => None,
            Some(builtin) => Some(builtin.name().to_string()),
            None => Some(label.to_string()),
        };
        // A theme picked from the dropdown (built-in or scanned folder) is
        // known to exist; a subsequent rescan re-checks if deleted externally.
        self.missing_theme = None;
        edit_settings(
            &self.engine,
            window.window_handle(),
            move |settings| settings.profile.overlay.theme = theme,
            cx,
        );
        cx.notify();
    }

    /// The overlays' default chat direction: they reload to show it (those
    /// with `?newest=` in their URL keep their own), and it's saved.
    pub(super) fn set_newest(
        &mut self,
        newest: Newest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        edit_settings(
            &self.engine,
            window.window_handle(),
            move |settings| settings.profile.overlay.newest = newest,
            cx,
        );
        cx.notify();
    }

    /// The overlay's card: its address, how many are connected, and the
    /// theme it shows.
    pub(super) fn render_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let url = self.status.overlay_url.clone();
        let overlays = self.status.overlays_connected;
        let obs = match overlays {
            0 => "no overlay connected".to_string(),
            1 => "overlay connected".to_string(),
            n => format!("{n} overlays connected"),
        };

        // The title shares its row with the address (no "URL" label: the
        // address says what it is), saving a row for the sources list.
        let heading = h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(div().font_semibold().child("Overlay"))
            .child(div().font_family("monospace").child(url.clone()))
            .child(
                Button::new("copy-url")
                    .icon(IconName::Copy)
                    .label("Copy")
                    .small()
                    .on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                        window.push_notification(Notification::success("Overlay URL copied"), cx);
                    }),
            )
            // Pushes the status to the row's other end (or onto the next
            // line, in a narrow window).
            .child(div().flex_1())
            .child(
                div()
                    .text_sm()
                    .text_color(if overlays > 0 {
                        cx.theme().success
                    } else {
                        muted
                    })
                    .child(obs),
            );

        card(cx).child(heading).child(self.render_themes(cx))
    }

    fn render_themes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let row = h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(div().text_color(muted).child("Theme"))
            .child(
                div()
                    .w(px(180.))
                    .child(Select::new(&self.theme_select).small()),
            )
            .child(
                Button::new("theme-customize")
                    .label("Customize…")
                    .small()
                    .tooltip("Customize colors, typography, motion, and elements")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_themes_window(window, cx)),
                    ),
            )
            .child(
                Button::new("theme-reload")
                    .label("Reload overlays")
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.engine.reload_overlays();
                        window.push_notification(Notification::info("Overlays reloaded"), cx);
                    })),
            )
            .child(
                Button::new("theme-folder")
                    .icon(IconName::ExternalLink)
                    .label("Open themes folder")
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| {
                        let dir = this.config.read(cx).themes_dir();
                        open_folder(&dir, window, cx);
                    })),
            )
            // Pushes the direction to the row's other end (or onto the next
            // line, in a narrow window).
            .child(div().flex_1())
            .child(
                div()
                    .w(px(170.))
                    .child(Select::new(&self.newest_select).small()),
            );

        v_flex()
            .gap_2()
            .child(row)
            .children(self.missing_theme.clone().map(|name| {
                div().text_sm().text_color(cx.theme().warning).child(format!(
                    "The theme folder \"{name}\" wasn't found; the overlay uses the built-in look."
                ))
            }))
    }
}
