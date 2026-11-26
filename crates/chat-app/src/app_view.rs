//! The main window: overall status, overlay URL, the list of sources with
//! their status lights, the chat reactions and widgets at a glance, and a
//! form to add sources.
//!
//! The view only draws and forwards clicks. Engine calls run as GPUI tasks
//! (`cx.spawn`) that await the engine's answer and then update the view;
//! the list of sources itself comes from the engine's status channel.
//!
//! Buttons follow one rule in every window, so anything clickable looks
//! clickable:
//! - `.primary()` (filled): the one main action of a section (Add, Save).
//! - default (bordered): every other action with text.
//! - `.link()`: only what opens a web page; it looks like a link because it
//!   leaves the app.
//! - `.ghost()`: only icon-only buttons (settings, remove), with a tooltip,
//!   and the unchosen items of a [`segmented`] choice (its track marks them
//!   as choices). Anywhere else a ghost button with text has no border
//!   until hovered and reads as a label.

use std::time::Duration;

use chat_engine::{Appearance, EngineHandle, Health, SourceId, SourceState, SourceStatus, Status};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, IconName, Sizable, StyledExt, WindowExt, h_flex, scroll::ScrollableElement, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_config::{AppConfig, edit_settings};
use crate::appearance;
use crate::manual_window::ManualWindow;
use crate::reactions_window::ReactionsWindow;
use crate::settings_window::SettingsWindow;
use crate::themes_window::ThemesWindow;
use crate::ui::{SideWindow, card, light, render_output, report};
use crate::widgets_window::WidgetsWindow;
use crate::youtube_panel::YouTubePanel;
use add_source::NewSource;
use api_approval::ActiveDialog;
use profiles::ProfileForm;
use themes::{NEWEST_CHOICES, newest_label, theme_items, theme_label};

mod add_source;
mod api_approval;
mod profiles;
mod themes;

pub struct AppView {
    engine: EngineHandle,
    config: Entity<AppConfig>,
    youtube: Entity<YouTubePanel>,
    status: Status,
    new_source: NewSource,
    /// The YouTube channel picked for a new "your broadcasts" source (its
    /// id), when several are connected; see `new_youtube_account`.
    new_account: Option<String>,
    input: Entity<InputState>,
    /// The second field, for kinds that need a secret (`needs_token`).
    token: Entity<InputState>,
    form_error: Option<String>,
    /// Overlay theme folders found in the themes folder.
    themes: Vec<String>,
    /// Cached missing custom theme name (checked off the UI thread so
    /// `render_themes` never touches the filesystem).
    missing_theme: Option<String>,
    /// Monotonic counter so stale background theme scans are ignored.
    theme_scan_gen: u64,
    theme_select: Entity<SelectState<Vec<SharedString>>>,
    /// The overlay's default chat direction.
    newest_select: Entity<SelectState<Vec<SharedString>>>,
    /// The profile name field (new, copy, rename), while it's open.
    profile_form: Option<ProfileForm>,
    /// Singleton side windows (typed handles; no downcasting needed).
    manual_window: Option<SideWindow<ManualWindow>>,
    settings_window: Option<SideWindow<SettingsWindow>>,
    reactions_window: Option<SideWindow<ReactionsWindow>>,
    widgets_window: Option<SideWindow<WidgetsWindow>>,
    themes_window: Option<SideWindow<ThemesWindow>>,
    /// Modal dialog currently owned by the main window.
    active_dialog: ActiveDialog,
    /// Sites answered here whose question may still be in the status for a
    /// moment (the engine publishes the change a little later): not asked
    /// about again in the meantime.
    answered: Vec<String>,
    // Subscriptions end when dropped; keeping them here ties them to the
    // view's lifetime.
    _subscriptions: Vec<Subscription>,
}

impl AppView {
    pub fn new(
        engine: EngineHandle,
        config: Entity<AppConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("channel name"));
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Overlay Token (same page)")
                .masked(true)
        });
        let youtube = cx.new(|cx| YouTubePanel::new(engine.clone(), config.clone(), window, cx));
        let themes = Vec::new();
        let settings = config.read(cx).settings();
        let current = theme_label(settings.profile.overlay.theme.as_deref());
        let theme_select = cx.new(|cx| {
            let mut select = SelectState::new(theme_items(&themes), None, window, cx);
            select.set_selected_value(&current, window, cx);
            select
        });
        let current_newest = newest_label(settings.profile.overlay.newest);
        let newest_select = cx.new(|cx| {
            let items = NEWEST_CHOICES.iter().map(|(_, l)| (*l).into()).collect();
            let mut select = SelectState::new(items, None, window, cx);
            select.set_selected_value(&current_newest, window, cx);
            select
        });

        let subscriptions = vec![
            // A chat direction picked.
            cx.subscribe_in(
                &newest_select,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                    if let SelectEvent::Confirm(Some(label)) = event
                        && let Some((newest, _)) =
                            NEWEST_CHOICES.iter().find(|(_, l)| *l == label.as_ref())
                    {
                        this.set_newest(*newest, window, cx);
                    }
                },
            ),
            // A theme picked in the dropdown.
            cx.subscribe_in(
                &theme_select,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                    if let SelectEvent::Confirm(Some(name)) = event {
                        this.select_theme(name.clone(), window, cx);
                    }
                },
            ),
            // Back from editing themes in a file manager: pick up new
            // folders without restarting.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.rescan_themes(window, cx);
                }
            }),
            // Enter in the text field adds the source...
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_source(window, cx);
                }
            }),
            // ...and so does Enter in the token field.
            cx.subscribe_in(&token, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_source(window, cx);
                }
            }),
            // Follow the system's light/dark setting when it changes, unless
            // a fixed look is chosen in the settings.
            cx.observe_window_appearance(window, |this, window, cx| {
                let appearance = this.config.read(cx).settings().app.appearance;
                if appearance == Appearance::System {
                    appearance::apply(appearance, Some(window), cx);
                }
            }),
            // Settings changed (here or in another window, e.g. Themes):
            // keep the theme picker synced (rescanning if a new custom folder
            // was just created & activated) and redraw.
            cx.observe_in(&config, window, |this, config, window, cx| {
                let raw = config.read(cx).settings().profile.overlay.theme;
                let current = theme_label(raw.as_deref());
                let needs_rescan = raw.as_deref().is_some_and(|name| {
                    chat_render::Builtin::from_name(name).is_none()
                        && !this.themes.iter().any(|t| t == name)
                });
                if needs_rescan {
                    this.rescan_themes(window, cx);
                } else {
                    let shown = this.theme_select.read(cx).selected_value().cloned();
                    if shown.as_ref() != Some(&current) {
                        this.theme_select.update(cx, |select, cx| {
                            select.set_selected_value(&current, window, cx);
                        });
                    }
                }
                cx.notify();
            }),
        ];

        let status = engine.status().borrow().clone();
        Self::watch_status(&engine, window, cx);
        Self::tick_while_retrying(cx);

        let mut view = Self {
            engine,
            config,
            youtube,
            status,
            new_source: NewSource::Twitch,
            new_account: None,
            input,
            token,
            form_error: None,
            themes,
            missing_theme: None,
            theme_scan_gen: 0,
            theme_select,
            newest_select,
            profile_form: None,
            manual_window: None,
            settings_window: None,
            reactions_window: None,
            widgets_window: None,
            themes_window: None,
            active_dialog: ActiveDialog::None,
            answered: Vec::new(),
            _subscriptions: subscriptions,
        };
        // Populate theme folders and missing-theme warning off the UI thread.
        view.rescan_themes(window, cx);
        view
    }

    /// Redraws whenever the engine publishes a new status, asks about web
    /// sites waiting to use the JSON API, and says when saving the settings
    /// failed.
    fn watch_status(engine: &EngineHandle, window: &mut Window, cx: &mut Context<Self>) {
        let mut status = engine.status();
        // `this` is a *weak* handle: the task doesn't keep the view alive,
        // and `update_in` fails once the window is closed, which ends the
        // loop. (`spawn_in`/`update_in`, not `spawn`/`update`: opening the
        // question needs the window.)
        cx.spawn_in(window, async move |this, cx| {
            loop {
                // Clone the value out right away: the borrow holds a lock
                // and must not live across the `.await` below.
                let current = status.borrow_and_update().clone();
                let still_open = this.update_in(cx, |view, window, cx| {
                    // A new problem saving config.toml (whichever window's
                    // change it was): said once, not on every status.
                    if let Some(error) = &current.save_error
                        && view.status.save_error.as_ref() != Some(error)
                    {
                        report(window, cx, format!("Couldn't save the settings: {error}"));
                    }
                    let switched = current.profile_switches != view.status.profile_switches;
                    // Once: the first save converted a config.toml from
                    // before profiles.
                    if current.converted_to_profiles && !view.status.converted_to_profiles {
                        window.push_notification(
                            Notification::info(
                                "Settings now come in profiles (the menu at the top right): \
                                 what you had is the first one. The old settings file is \
                                 kept as config.old.toml.",
                            ),
                            cx,
                        );
                    }
                    view.status = current;
                    if switched {
                        view.profile_switched(window, cx);
                    }
                    view.ask_about_api_requests(window, cx);
                    cx.notify();
                });
                if still_open.is_err() || status.changed().await.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// "Retrying in 3 s" counts down in the text, but the engine's status
    /// doesn't change while waiting: redraw once a second while anything is
    /// retrying.
    fn tick_while_retrying(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let still_open = this.update(cx, |view, cx| {
                    let retrying = view
                        .status
                        .sources
                        .iter()
                        .any(|s| matches!(s.state, SourceState::Retrying { .. }));
                    if retrying {
                        cx.notify();
                    }
                });
                if still_open.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Switches a source on or off; the engine remembers that for the next
    /// start.
    fn set_running(&mut self, id: SourceId, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = if on {
                engine.start_source(id).await
            } else {
                engine.stop_source(id).await
            };
            if let Err(e) = result {
                let message = format!("Couldn't switch the source: {e:#}");
                let _ = this.update_in(cx, |_, window, cx| report(window, cx, message));
            }
        })
        .detach();
    }

    fn remove_source(&mut self, id: SourceId, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            if let Err(e) = engine.remove_source(id).await {
                let message = format!("Couldn't remove the source: {e:#}");
                let _ = this.update_in(cx, |_, window, cx| report(window, cx, message));
            }
        })
        .detach();
    }

    /// Opens the test messages window (one test source is enough).
    fn open_manual_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        SideWindow::open_or_focus(
            &mut self.manual_window,
            "Test messages",
            size(px(520.), px(510.)),
            move |window, cx| ManualWindow::new(engine, window, cx),
            window,
            cx,
        );
    }

    fn open_settings_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (engine, config) = (self.engine.clone(), self.config.clone());
        SideWindow::open_or_focus(
            &mut self.settings_window,
            "Settings",
            size(px(640.), px(480.)),
            move |window, cx| SettingsWindow::new(engine, config, window, cx),
            window,
            cx,
        );
    }

    pub(crate) fn open_reactions_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (engine, config) = (self.engine.clone(), self.config.clone());
        SideWindow::open_or_focus(
            &mut self.reactions_window,
            "Reactions",
            size(px(880.), px(600.)),
            move |window, cx| ReactionsWindow::new(engine, config, window, cx),
            window,
            cx,
        );
    }

    fn open_widgets_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (engine, config) = (self.engine.clone(), self.config.clone());
        SideWindow::open_or_focus(
            &mut self.widgets_window,
            "Widgets",
            size(px(880.), px(600.)),
            move |window, cx| WidgetsWindow::new(engine, config, window, cx),
            window,
            cx,
        );
    }

    pub(super) fn open_themes_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config_ref = self.config.read(cx);
        let dir = config_ref.themes_dir();
        let active_key = config_ref
            .settings()
            .profile
            .overlay
            .theme_source(Some(&dir))
            .key();
        // If already open, focus it and select the active theme using its own window.
        if let Some(side) = self.themes_window.as_ref()
            && side.focus_and_update(cx, |view, window, cx| {
                view.select_theme_key(active_key.clone(), window, cx);
            })
        {
            return;
        }
        let (engine, config) = (self.engine.clone(), self.config.clone());
        SideWindow::open_or_focus(
            &mut self.themes_window,
            "Themes",
            size(px(926.), px(660.)),
            move |window, cx| ThemesWindow::new(engine, config, active_key, window, cx),
            window,
            cx,
        );
    }

    /// Whether any open side window has unsaved form changes that a profile
    /// switch would discard.
    pub(super) fn has_unsaved_side_windows(&self, cx: &App) -> bool {
        self.reactions_window
            .as_ref()
            .and_then(SideWindow::view)
            .is_some_and(|view| view.read(cx).unsaved(cx))
    }

    /// Flushes debounced edits in open side windows before switching profiles
    /// so typed settings stay with the profile they were edited in.
    pub(super) fn flush_side_windows(&self, cx: &mut App) {
        if let Some(view) = self.settings_window.as_ref().and_then(SideWindow::view) {
            view.update(cx, |view, cx| view.flush(cx));
        }
        if let Some(view) = self.widgets_window.as_ref().and_then(SideWindow::view) {
            view.update(cx, |view, cx| view.flush(cx));
        }
        if let Some(view) = self.themes_window.as_ref().and_then(SideWindow::view) {
            view.update(cx, |view, cx| view.flush(cx));
        }
    }

    /// The pause switch: every rule off (or back on) at once, right away.
    fn set_reactions_on(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let handle = window.window_handle();
        edit_settings(
            &self.engine,
            handle,
            move |s| s.profile.reactions.paused = !on,
            cx,
        );
    }

    // ---- drawing ----

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let overall = self.status.overall();
        let headline = match overall {
            Health::Ok => "All good",
            Health::Warning => "Something is recovering",
            Health::Error => "Something needs your attention",
            Health::Off => "Nothing running",
        };

        let reactions = &self.config.read(cx).settings().profile.reactions;
        let paused = reactions.paused && !reactions.rules.is_empty();

        h_flex()
            .gap_2()
            .items_center()
            .child(light(overall, cx))
            .child(div().text_lg().font_semibold().child(headline))
            // Easy to forget after a break: said where it's always seen.
            .child(
                div()
                    .flex_1()
                    .text_color(cx.theme().muted_foreground)
                    .when(paused, |d| d.child("· reactions paused")),
            )
            .children(self.render_profile_menu(cx))
            .child(
                Button::new("open-settings")
                    .icon(IconName::Settings)
                    .small()
                    .ghost()
                    .tooltip("Settings")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_settings_window(window, cx)),
                    ),
            )
    }

    fn render_sources(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let list = v_flex().gap_1().children(
            self.status
                .sources
                .iter()
                .enumerate()
                .map(|(i, source)| self.render_source(source, i == 0, cx)),
        );
        // The list takes whatever height is left (`flex_1`) and scrolls
        // within it. `min_h_0` lets it shrink below its content's height;
        // without it, flex items grow to fit their content and push the
        // rest of the window out of view instead of scrolling.
        card(cx)
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(div().font_semibold().child("Sources"))
                    .child(
                        Button::new("open-manual")
                            .label("Test messages…")
                            .small()
                            .tooltip("Send chat messages by hand")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_manual_window(window, cx)
                            })),
                    ),
            )
            .child(if self.status.sources.is_empty() {
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("No sources yet: add one below.")
                    .into_any_element()
            } else {
                div()
                    .id("source-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(list)
                    .into_any_element()
            })
    }

    // `+ use<>`: since edition 2024, a returned `impl Trait` is assumed to
    // borrow everything in scope (here also `cx`), which a caller in a loop
    // couldn't allow. The row doesn't borrow anything (its data is copied),
    // and `use<>` says exactly that: capture nothing.
    fn render_source(
        &self,
        source: &SourceStatus,
        first: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = source.id;
        let running = source.state != SourceState::Stopped;
        let key = id.get() as usize;

        h_flex()
            .gap_3()
            .items_center()
            .py_1()
            // Lines only *between* rows: one under the last row would sit
            // just above the card's own frame and look doubled.
            .when(!first, |row| {
                row.border_t_1().border_color(cx.theme().border)
            })
            .child(light(source.health(), cx))
            .child(
                v_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(div().child(source.label.clone()))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(source.summary()),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{} msgs", source.messages)),
            )
            .child(
                Switch::new(("running", key))
                    .checked(running)
                    .on_click(cx.listener(move |this, on: &bool, window, cx| {
                        this.set_running(id, *on, window, cx);
                    })),
            )
            // The test source (the one without settings) belongs to its
            // window: closing the window removes it.
            .when(source.config.is_some(), |row| {
                row.child(
                    Button::new(("remove", key))
                        .icon(IconName::Close)
                        .small()
                        .ghost()
                        .tooltip("Remove this source")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_source(id, window, cx);
                        })),
                )
            })
    }

    /// The chat reactions at a glance: on or paused, how their programs are
    /// connected, how much fired. Editing happens in the Reactions window.
    /// A `Div` (not just `impl IntoElement`): the row it shares with the
    /// widgets sizes it.
    fn render_reactions(&self, cx: &mut Context<Self>) -> Div {
        let settings = self.config.read(cx).settings().profile.reactions;
        let status = &self.status.reactions;
        let muted = cx.theme().muted_foreground;
        let edit = |label: &'static str| {
            Button::new("open-reactions")
                .label(label)
                .small()
                .on_click(cx.listener(|this, _, window, cx| this.open_reactions_window(window, cx)))
        };
        let title = div().flex_1().font_semibold().child("Reactions");
        if settings.rules.is_empty() {
            return card(cx)
                .child(h_flex().items_center().child(title).child(edit("Set up…")))
                .child(
                    div()
                        .text_color(muted)
                        .child("Chat can trigger things in Warudo and OBS: a filter, a sound, an item thrown at you."),
                );
        }

        let fired: u64 = status.rules.iter().map(|r| r.fired).sum();
        let problems = status.rules.iter().filter(|r| r.problem.is_some()).count();
        let rules = settings.rules.len();
        let mut counts = format!(
            "{rules} {} · {fired} fired",
            if rules == 1 { "rule" } else { "rules" }
        );
        if problems > 0 {
            counts += &format!(" · {problems} with a problem");
        }
        let outputs = (!settings.paused).then(|| {
            status
                .outputs
                .iter()
                .map(|output| render_output(output, cx))
        });

        card(cx)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(title)
                    .child(
                        Switch::new("reactions-on")
                            .checked(!settings.paused)
                            .tooltip("Switch every reaction off or on at once")
                            .on_click(cx.listener(|this, on: &bool, window, cx| {
                                this.set_reactions_on(*on, window, cx);
                            })),
                    )
                    .child(edit("Edit reactions…")),
            )
            // While reactions are switched off, hide output errors behind a
            // quiet grey light so disconnected programs don't look alarming.
            .when(settings.paused, |card| {
                card.child(
                    h_flex()
                        .id("reactions-disabled")
                        .gap_2()
                        .items_center()
                        .child(light(Health::Off, cx))
                        .child(div().text_color(muted).child("Reactions disabled"))
                        .test_support(),
                )
            })
            // No output while every rule is off: nothing to connect to.
            .when(!settings.paused && status.outputs.is_empty(), |card| {
                card.child(div().text_color(muted).child("Every rule is switched off."))
            })
            .children(outputs.into_iter().flatten())
            .child(div().text_sm().text_color(muted).child(counts))
    }

    /// The widgets at a glance: how many are showing chat in OBS right now.
    /// Picking their settings happens in the Widgets window.
    fn render_widgets(&self, cx: &mut Context<Self>) -> Div {
        let connected = self.status.widgets_connected;
        card(cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().font_semibold().child("Widgets"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(if connected > 0 {
                                cx.theme().success
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(format!("{connected} connected")),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .text_color(cx.theme().muted_foreground)
                    .child("Chat shown other ways, like emoji rising up the screen."),
            )
            .child(
                Button::new("open-widgets")
                    .label("Open widgets…")
                    .small()
                    .self_start()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_widgets_window(window, cx)),
                    ),
            )
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_4()
            // Smaller than before the cards: their frames and padding
            // already set the sections apart.
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_header(cx))
            .children(self.render_profile_form(cx))
            .child(self.render_overlay(cx))
            .child(self.render_sources(cx))
            .child(
                h_flex()
                    .gap_3()
                    .items_stretch()
                    // `min_w_0`: long lines shorten (…) rather than push
                    // the widgets out of the window.
                    .child(self.render_reactions(cx).flex_1().min_w_0())
                    // As wide as the default window allows: the reactions
                    // keep the room their top row (title, switch, "Edit
                    // reactions…") needs.
                    .child(self.render_widgets(cx).w(px(220.)).flex_none()),
            )
            .child(card(cx).child(self.youtube.clone()))
            .child(self.render_add_form(cx))
    }
}

/// UI tests: the real window, rendered headless, driven by simulated clicks
/// and typing. Imports are explicit on purpose: with gpui-kit's test support
/// on, `use gpui_kit::*` would also import a `test` macro that shadows Rust's
/// built-in `#[test]`.
#[cfg(test)]
mod tests {

    use std::path::PathBuf;

    use chat_engine::{ConfigFile, Engine, EngineConfig};
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowHandle};

    use chat_engine::SourceConfig;

    use super::AppView;
    use super::add_source::NewSource;
    use super::api_approval::ActiveDialog;
    use super::profiles::ProfileAction;
    use crate::app_config::AppConfig;

    /// The window as the app builds it (the view inside a `Root`, which
    /// notifications need), a real engine on its own tokio runtime, and a
    /// fresh settings folder.
    struct Ui {
        window: WindowHandle<Root>,
        view: Entity<AppView>,
        engine: Engine,
        settings_dir: PathBuf,
        _runtime: tokio::runtime::Runtime,
    }

    impl Drop for Ui {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.settings_dir);
        }
    }

    fn open(cx: &mut TestAppContext) -> Ui {
        open_with(cx, EngineConfig::default())
    }

    /// Picks `kind` in the add form, found by its place in the list (so
    /// reordering the list doesn't break the tests).
    fn pick(window: &mut gpui_kit::Window, kind: NewSource, cx: &mut gpui_kit::App) {
        let index = NewSource::ALL.iter().position(|k| *k == kind).unwrap();
        window.click(("kind", index), cx);
    }

    /// `open` with engine settings of the test's choosing (always on a free
    /// local port, saving into the test's settings folder, with the themes
    /// folder there too).
    fn open_with(cx: &mut TestAppContext, mut engine: EngineConfig) -> Ui {
        cx.update(gpui_kit::init);
        // The engine runs on real tokio threads, and its answers and new
        // settings wake GPUI tasks from there at any moment: the test
        // executor must accept that (see `manual_window::tests::run_until`).
        cx.executor().allow_parking();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        static NEXT_DIR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let settings_dir =
            std::env::temp_dir().join(format!("chat-app-ui-test-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&settings_dir);
        engine.settings.server.port = 0; // the OS picks a free one
        engine.themes_dir = Some(ConfigFile::themes_dir(&settings_dir));
        engine.widgets_dir = Some(ConfigFile::widgets_dir(&settings_dir));
        engine.fonts_dir = Some(ConfigFile::fonts_dir(&settings_dir));
        engine.save_to = Some(settings_dir.join("config.toml"));
        let engine = runtime.block_on(Engine::start(engine)).unwrap();
        let handle = engine.handle();
        let dir = settings_dir.clone();

        // `add_window` builds the root element; the view is created inside
        // and handed out through `view` for the test to inspect.
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let config = AppConfig::new(&handle, dir, cx);
            let app = cx.new(|cx| AppView::new(handle, config, window, cx));
            view = Some(app.clone());
            Root::new(app, window, cx)
        });
        Ui {
            window,
            view: view.expect("the window was built"),
            engine,
            settings_dir,
            _runtime: runtime,
        }
    }

    #[gpui_kit::test]
    fn invalid_twitch_name_is_rejected_in_the_form(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("new-source-text", cx);
            window.input("not valid!", cx);
            window.click("add", cx);
        })
        .unwrap();

        let error = ui.view.read_with(cx, |view, _| view.form_error.clone());
        let error = error.expect("the form shows an error");
        // Names the typed text: proves the typing reached the field (an
        // empty field would be rejected too, with a different message).
        assert!(error.contains("not valid!"), "{error}");
    }

    #[gpui_kit::test]
    fn builtin_themes_are_picked_by_label_and_saved_by_name(cx: &mut TestAppContext) {
        let ui = open(cx);
        let items = super::theme_items(&["studio".into()]);
        assert_eq!(
            items,
            [
                "Default", "Minimal", "Bubbles", "Cozy", "Retro", "Neon", "Ornate", "Aero",
                "Comic", "studio",
            ],
            "built-in themes first"
        );

        let pick = |cx: &mut TestAppContext, label: &'static str| {
            cx.update_window(ui.window.into(), |_, window, cx| {
                ui.view
                    .update(cx, |view, cx| view.select_theme(label.into(), window, cx));
            })
            .unwrap();
        };
        // `None` while there's no file yet.
        let saved = || {
            ConfigFile::load(&ui.settings_dir.join("config.toml"), None)
                .ok()
                .map(|file| file.profile.overlay.theme)
        };
        let engines = || {
            ui.engine
                .handle()
                .settings()
                .borrow()
                .profile
                .overlay
                .theme
                .clone()
        };

        pick(cx, "Minimal");
        run_until(cx, || saved() == Some(Some("minimal".into())));
        assert_eq!(engines().as_deref(), Some("minimal"));
        assert_eq!(super::theme_label(Some("minimal")), "Minimal");

        pick(cx, "Default");
        run_until(cx, || saved() == Some(None));
        assert_eq!(engines(), None, "the default is saved as no theme");
    }

    /// Lets GPUI tasks run until `done`; the engine answers from tokio's
    /// threads in real time (see `manual_window::tests::run_until`).
    fn run_until(cx: &mut TestAppContext, mut done: impl FnMut() -> bool) {
        cx.executor().allow_parking();
        for _ in 0..500 {
            cx.run_until_parked();
            if done() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out");
    }

    #[gpui_kit::test]
    fn reactions_are_paused_from_the_main_window(cx: &mut TestAppContext) {
        let mut config = EngineConfig::default();
        // Port 1, where nothing listens: avoids touching a real local Warudo.
        config.settings.profile.reactions.warudo.url = "ws://127.0.0.1:1".into();
        config
            .settings
            .profile
            .reactions
            .rules
            .push(chat_engine::Rule {
                name: "bonk".into(),
                enabled: true,
                trigger: chat_engine::Trigger::Command {
                    command: "!bonk".into(),
                },
                guards: chat_engine::Guards::default(),
                action: chat_engine::Action::Warudo {
                    action: "throw_item".into(),
                },
            });
        let ui = open_with(cx, config);
        let paused = || {
            ui.engine
                .handle()
                .settings()
                .borrow()
                .profile
                .reactions
                .paused
        };
        // While enabled, the output status is shown (and not the disabled note).
        run_until_window(cx, &ui, |window| {
            window.try_find("reaction-output-warudo").is_some()
                && window.try_find("reactions-disabled").is_none()
        });
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("reactions-on", cx);
        })
        .unwrap();
        run_until(cx, paused);
        // Once disabled, output errors are replaced by the quiet disabled row.
        run_until_window(cx, &ui, |window| {
            window.try_find("reactions-disabled").is_some()
                && window.try_find("reaction-output-warudo").is_none()
        });
        let path = ui.settings_dir.join("config.toml");
        run_until(cx, || {
            ConfigFile::load(&path, None).is_ok_and(|file| file.profile.reactions.paused)
        });
        // Switching back on restores the output row.
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("reactions-on", cx);
        })
        .unwrap();
        run_until(cx, || !paused());
        run_until_window(cx, &ui, |window| {
            window.try_find("reaction-output-warudo").is_some()
                && window.try_find("reactions-disabled").is_none()
        });
    }

    /// Engine settings with a YouTube client and these channels connected
    /// (none of them with a stored login), its secrets in a folder of the
    /// test's own: checking them asks nothing of Google.
    fn with_youtube_channels(channels: &[(&str, &str)]) -> EngineConfig {
        let mut config = EngineConfig::default();
        let youtube = &mut config.settings.youtube;
        youtube.client_id = Some("test-client.apps.googleusercontent.com".into());
        youtube.client_secret = Some("test-secret".into());
        for (id, name) in channels {
            youtube.connect(chat_engine::YouTubeChannel {
                channel_id: (*id).into(),
                name: (*name).into(),
            });
        }
        static NEXT_SECRETS_DIR: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT_SECRETS_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        config.secrets = Some(chat_engine::SecretStore::Dir(std::env::temp_dir().join(
            format!("chat-app-ui-test-secrets-{}-{id}", std::process::id()),
        )));
        config
    }

    /// With several YouTube channels connected, a new "your broadcasts"
    /// source reads the one picked; with one, that one. Either way the
    /// source names it.
    #[gpui_kit::test]
    fn a_new_youtube_source_reads_the_channel_picked(cx: &mut TestAppContext) {
        let ui = open_with(
            cx,
            with_youtube_channels(&[
                ("UC_main", "your_channel"),
                ("UC_second", "your_second_channel"),
            ]),
        );
        let path = ui.settings_dir.join("config.toml");
        let saved_sources = || {
            ConfigFile::load(&path, None)
                .map(|file| file.profile.sources)
                .unwrap_or_default()
        };
        cx.update_window(ui.window.into(), |_, window, cx| {
            assert!(
                window.try_find(("youtube-account", 0usize)).is_none(),
                "Twitch"
            );
            pick(window, NewSource::YouTubeOwn, cx);
            window.click(("youtube-account", 1usize), cx);
            window.click("add", cx);
        })
        .unwrap();
        run_until(cx, || saved_sources().len() == 1);
        assert_eq!(
            saved_sources()[0].config,
            SourceConfig::YouTube {
                video_id: None,
                account: Some("UC_second".into()),
            }
        );
        drop(ui);

        let ui = open_with(cx, with_youtube_channels(&[("UC_main", "your_channel")]));
        let path = ui.settings_dir.join("config.toml");
        cx.update_window(ui.window.into(), |_, window, cx| {
            pick(window, NewSource::YouTubeOwn, cx);
            assert!(
                window.try_find(("youtube-account", 0usize)).is_none(),
                "nothing to choose from"
            );
            window.click("add", cx);
        })
        .unwrap();
        let saved_sources = || {
            ConfigFile::load(&path, None)
                .map(|file| file.profile.sources)
                .unwrap_or_default()
        };
        run_until(cx, || saved_sources().len() == 1);
        assert_eq!(
            saved_sources()[0].config,
            SourceConfig::YouTube {
                video_id: None,
                account: Some("UC_main".into()),
            }
        );
    }

    /// Several channels get a row each in the YouTube section; "Log out"
    /// takes one off the list.
    #[gpui_kit::test]
    fn youtube_channels_are_listed_and_logged_out_one_by_one(cx: &mut TestAppContext) {
        let ui = open_with(
            cx,
            with_youtube_channels(&[
                ("UC_main", "your_channel"),
                ("UC_second", "your_second_channel"),
            ]),
        );
        // No login stored for either: both say so, and offer a new one.
        run_until_window(cx, &ui, |window| {
            window.try_find(("youtube-relogin", 1usize)).is_some()
        });
        cx.update_window(ui.window.into(), |_, window, cx| {
            assert!(window.try_find(("youtube-relogin", 0usize)).is_some());
            assert!(window.try_find("youtube-add").is_some());
            window.click(("youtube-logout", 1usize), cx);
        })
        .unwrap();
        let accounts = || {
            ui.engine
                .handle()
                .settings()
                .borrow()
                .youtube
                .accounts
                .clone()
        };
        run_until(cx, || accounts().len() == 1);
        assert_eq!(accounts()[0].channel_id, "UC_main");
        // One channel left: back in the first row, no list.
        run_until_window(cx, &ui, |window| {
            window.try_find(("youtube-logout", 1usize)).is_none()
                && window.try_find(("youtube-logout", 0usize)).is_some()
        });
    }

    #[gpui_kit::test]
    fn switching_a_source_off_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx);
        let path = ui.settings_dir.join("config.toml");
        let saved_sources = || {
            ConfigFile::load(&path, None)
                .map(|file| file.profile.sources)
                .unwrap_or_default()
        };
        cx.update_window(ui.window.into(), |_, window, cx| {
            pick(window, NewSource::Demo, cx);
            window.click("add", cx);
        })
        .unwrap();
        run_until(cx, || saved_sources().len() == 1);
        assert!(saved_sources()[0].enabled);

        let id = ui.engine.handle().status().borrow().sources[0].id;
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click(("running", id.get() as usize), cx);
        })
        .unwrap();
        run_until(cx, || !saved_sources()[0].enabled);
    }

    /// The engine saves; when that fails, the main window says so (here: a
    /// folder is where config.toml should go).
    #[gpui_kit::test]
    fn a_failed_save_is_shown(cx: &mut TestAppContext) {
        let ui = open(cx);
        std::fs::create_dir_all(ui.settings_dir.join("config.toml")).unwrap();
        cx.update_window(ui.window.into(), |_, window, cx| {
            ui.view.update(cx, |view, cx| {
                view.set_newest(chat_engine::Newest::Top, window, cx);
            });
        })
        .unwrap();
        run_until_window(cx, &ui, |window| window.try_find("notification").is_some());
        let error = ui.engine.handle().status().borrow().save_error.clone();
        assert!(error.is_some_and(|e| e.contains("config.toml")));
    }

    #[gpui_kit::test]
    fn settings_window_opens_only_once(cx: &mut TestAppContext) {
        let ui = open(cx);
        for _ in 0..2 {
            cx.update_window(ui.window.into(), |_, window, cx| {
                window.click("open-settings", cx);
            })
            .unwrap();
        }
        assert_eq!(cx.update(|cx| cx.windows().len()), 2);
    }

    // ---- web sites asking for the JSON API ----

    const GAME: &str = "https://game.example";

    /// An engine like the app's: the API on, asking about web sites.
    fn open_asking(cx: &mut TestAppContext) -> Ui {
        let mut config = EngineConfig {
            ask_api_origins: true,
            ..EngineConfig::default()
        };
        config.settings.server.api = true;
        open_with(cx, config)
    }

    /// A page on `GAME` opening the JSON API, as raw HTTP (the app's tests
    /// have no HTTP client). Resolves to the first line of the answer, e.g.
    /// `HTTP/1.1 101 Switching Protocols`.
    fn connect_as_game(ui: &Ui) -> tokio::task::JoinHandle<String> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let addr = ui.engine.addr();
        ui._runtime.spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request = format!(
                "GET /api/v1/ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\n\
                 Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nOrigin: {GAME}\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut answer = vec![0; 1024];
            let n = stream.read(&mut answer).await.unwrap();
            let answer = String::from_utf8_lossy(&answer[..n]);
            answer.lines().next().unwrap_or_default().to_string()
        })
    }

    /// Lets GPUI and the engine run until `done` holds for the window (like
    /// `run_until`, but `done` may look at the window).
    fn run_until_window(
        cx: &mut TestAppContext,
        ui: &Ui,
        mut done: impl FnMut(&mut gpui_kit::Window) -> bool,
    ) {
        cx.executor().allow_parking();
        for _ in 0..500 {
            cx.run_until_parked();
            let reached = cx
                .update_window(ui.window.into(), |_, window, cx| {
                    window.render_frame(cx);
                    done(window)
                })
                .unwrap();
            if reached {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out");
    }

    /// Clicks a button of the open question. The dialog ignores clicks
    /// while it's still fading in (on the wall clock), so wait that out:
    /// in real use, that also stops an "Allow" clicked by accident the
    /// moment the question pops up.
    fn click_in_dialog(cx: &mut TestAppContext, ui: &Ui, button: &'static str) {
        let fade_in = *gpui_kit::component::dialog::ANIMATION_DURATION;
        std::thread::sleep(fade_in + std::time::Duration::from_millis(50));
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.within("dialog").click(button, cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_web_site_is_asked_about_and_allow_lets_it_in(cx: &mut TestAppContext) {
        let ui = open_asking(cx);
        let connecting = connect_as_game(&ui);
        run_until_window(cx, &ui, |window| window.try_find("dialog").is_some());
        assert_eq!(
            ui.view
                .read_with(cx, |view, _| match &view.active_dialog {
                    ActiveDialog::ApiApproval(origin) => Some(origin.clone()),
                    _ => None,
                })
                .as_deref(),
            Some(GAME)
        );

        click_in_dialog(cx, &ui, "ok"); // "Allow"
        run_until_window(cx, &ui, |window| window.try_find("dialog").is_none());
        let answer = ui._runtime.block_on(connecting).unwrap();
        assert!(
            answer.contains(" 101 "),
            "the waiting connection got in: {answer}"
        );
        // And the site is saved.
        run_until(cx, || {
            ConfigFile::load(&ui.settings_dir.join("config.toml"), None)
                .is_ok_and(|file| file.config.server.api_origins == [GAME])
        });
    }

    #[gpui_kit::test]
    fn the_question_goes_away_when_the_api_is_switched_off(cx: &mut TestAppContext) {
        let ui = open_asking(cx);
        let connecting = connect_as_game(&ui);
        run_until_window(cx, &ui, |window| window.try_find("dialog").is_some());
        ui._runtime
            .block_on(ui.engine.handle().edit_settings(|s| s.server.api = false))
            .unwrap();
        run_until_window(cx, &ui, |window| window.try_find("dialog").is_none());
        let answer = ui._runtime.block_on(connecting).unwrap();
        assert!(answer.contains(" 403 "), "{answer}");
    }

    // ---- profiles ----

    /// Runs `change` on the main window's view, with its window.
    fn in_view(
        cx: &mut TestAppContext,
        ui: &Ui,
        change: impl FnOnce(&mut AppView, &mut gpui_kit::Window, &mut gpui_kit::Context<AppView>),
    ) {
        cx.update_window(ui.window.into(), |_, window, cx| {
            ui.view.update(cx, |view, cx| change(view, window, cx));
        })
        .unwrap();
    }

    /// Selects all of the focused field's text, so typing replaces it.
    fn select_all(window: &mut gpui_kit::Window, cx: &mut gpui_kit::App) {
        let keys = if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        };
        window.press(keys, cx);
    }

    fn profile(ui: &Ui) -> String {
        ui.engine.handle().status().borrow().profile.clone()
    }

    #[gpui_kit::test]
    fn a_new_profile_is_made_and_switched_to(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("profile-menu").is_some());
        })
        .unwrap();
        // What the menu's "New profile…" does.
        in_view(cx, &ui, |view, window, cx| {
            view.open_profile_form(ProfileAction::New { copy: false }, window, cx);
        });
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("profile-name", cx);
            window.input("a/b", cx);
            window.click("profile-submit", cx);
        })
        .unwrap();
        let error = ui.view.read_with(cx, |view, _| {
            view.profile_form.as_ref().and_then(|f| f.error.clone())
        });
        assert!(
            error.is_some_and(|e| e.contains('/')),
            "checked in the form"
        );

        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("profile-name", cx);
            select_all(window, cx);
            window.input(" Indie ", cx);
            window.click("profile-submit", cx);
        })
        .unwrap();
        // Nothing running: no question, switched to at once.
        run_until(cx, || profile(&ui) == "Indie");
        let status = ui.engine.handle().status().borrow().clone();
        assert_eq!(status.profiles, ["Indie", "Main"]);
        assert!(ui.view.read_with(cx, |view, _| view.profile_form.is_none()));

        // And renamed, the active one.
        in_view(cx, &ui, |view, window, cx| {
            view.open_profile_form(ProfileAction::Rename("Indie".into()), window, cx);
        });
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("profile-name", cx);
            select_all(window, cx);
            window.input("Solo", cx);
            window.click("profile-submit", cx);
        })
        .unwrap();
        run_until(cx, || profile(&ui) == "Solo");
    }

    #[gpui_kit::test]
    fn switching_asks_first_while_chat_comes_in(cx: &mut TestAppContext) {
        let ui = open_with(
            cx,
            EngineConfig {
                sources: vec![SourceConfig::Demo.into()],
                ..EngineConfig::default()
            },
        );
        let handle = ui.engine.handle();
        run_until(cx, || {
            let status = handle.status().borrow().clone();
            status
                .sources
                .iter()
                .any(|s| matches!(s.state, chat_engine::SourceState::Running(_)))
        });
        ui._runtime
            .block_on(handle.create_profile("Indie", false))
            .unwrap();
        // Indie's own look.
        let mut indie = chat_engine::ProfileFile::default();
        indie.overlay.newest = chat_engine::Newest::Top;
        indie
            .save(&ConfigFile::profile_path(&ui.settings_dir, "Indie"))
            .unwrap();

        in_view(cx, &ui, |view, window, cx| {
            view.ask_switch("Indie".into(), window, cx);
        });
        run_until_window(cx, &ui, |window| window.try_find("dialog").is_some());
        assert_eq!(profile(&ui), "Main", "not before the answer");
        click_in_dialog(cx, &ui, "ok"); // "Switch"
        run_until(cx, || profile(&ui) == "Indie");
        // The main window learns of it from the status, a moment later.
        let top = super::newest_label(chat_engine::Newest::Top);
        let shown = |cx: &mut TestAppContext| {
            ui.view.read_with(cx, |view, cx| {
                view.newest_select.read(cx).selected_value().cloned()
            })
        };
        for _ in 0..500 {
            cx.run_until_parked();
            if shown(cx).as_ref() == Some(&top) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(shown(cx), Some(top), "the pickers show Indie's look");
        let status = handle.status().borrow().clone();
        assert!(status.sources.is_empty(), "Indie has none");
    }

    #[gpui_kit::test]
    fn typed_settings_stay_with_the_profile_they_were_typed_in(cx: &mut TestAppContext) {
        let ui = open(cx);
        let handle = ui.engine.handle();
        ui._runtime
            .block_on(handle.create_profile("Indie", false))
            .unwrap();
        ui._runtime
            .block_on(handle.edit_settings(|s| {
                s.profile.overlay.hide = vec![chat_engine::Hide::Twitch];
            }))
            .unwrap();
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("open-settings", cx);
        })
        .unwrap();
        let (settings_window, settings) = ui.view.read_with(cx, |view, _| {
            let side = view.settings_window.as_ref().unwrap();
            (side.window(), side.view().unwrap())
        });
        let type_currency = |cx: &mut TestAppContext, text: &'static str| {
            cx.update_window(settings_window.into(), |_, window, cx| {
                settings.update(cx, |view, cx| view.type_tip_currency(text, window, cx));
            })
            .unwrap();
        };
        let load = |name: &str| {
            chat_engine::ProfileFile::load(&ConfigFile::profiles_dir(&ui.settings_dir), name)
                .unwrap()
        };

        // Typed in Main, and switched within the typing pause.
        type_currency(cx, "EUR");
        in_view(cx, &ui, |view, window, cx| {
            view.ask_switch("Indie".into(), window, cx);
        });
        run_until(cx, || profile(&ui) == "Indie");
        assert_eq!(
            load("Main").tips.currency(),
            Some("EUR"),
            "where it was typed"
        );
        assert_eq!(load("Indie").tips.currency(), None);

        // Typed in Indie: nothing of Main's comes along.
        type_currency(cx, "USD");
        cx.executor().advance_clock(crate::ui::APPLY_AFTER * 2);
        run_until(cx, || load("Indie").tips.currency() == Some("USD"));
        assert!(load("Indie").overlay.hide.is_empty(), "Main's stay Main's");
        assert_eq!(load("Main").tips.currency(), Some("EUR"));
    }
}
