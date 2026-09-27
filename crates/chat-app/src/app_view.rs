//! The main window: overall status, overlay URL, the list of sources with
//! their status lights, and a form to add sources.
//!
//! The view only draws and forwards clicks. Engine calls run as GPUI tasks
//! (`cx.spawn`) that await the engine's answer and then update the view;
//! the list of sources itself comes from the engine's status channel.

use std::time::Duration;

use chat_engine::{
    EngineHandle, Health, SourceConfig, SourceId, SourceState, SourceStatus, Status,
};
use chat_render::themes;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, IconName, Root, Selectable, Sizable, StyledExt, WindowExt, h_flex,
    scroll::ScrollableElement, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::appearance;
use chat_engine::{Appearance, Newest};

use crate::app_config::AppConfig;
use crate::manual_window::ManualWindow;
use crate::settings_window::SettingsWindow;
use crate::youtube_panel::YouTubePanel;

/// What the add form creates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NewSource {
    Twitch,
    YouTubeOwn,
    YouTubeVideo,
    Demo,
}

impl NewSource {
    const ALL: [NewSource; 4] = [
        NewSource::Twitch,
        NewSource::YouTubeOwn,
        NewSource::YouTubeVideo,
        NewSource::Demo,
    ];

    fn label(self) -> &'static str {
        match self {
            NewSource::Twitch => "Twitch",
            NewSource::YouTubeOwn => "YouTube (your broadcasts)",
            NewSource::YouTubeVideo => "YouTube (a video)",
            NewSource::Demo => "Demo",
        }
    }

    /// Placeholder for the text field, if this kind needs one.
    fn needs_text(self) -> Option<&'static str> {
        match self {
            NewSource::Twitch => Some("channel name"),
            NewSource::YouTubeVideo => Some("video id (the part after watch?v=)"),
            NewSource::YouTubeOwn | NewSource::Demo => None,
        }
    }

    fn config(self, text: &str) -> SourceConfig {
        let text = text.trim().to_string();
        match self {
            NewSource::Twitch => SourceConfig::Twitch { channel: text },
            NewSource::YouTubeOwn => SourceConfig::YouTube { video_id: None },
            NewSource::YouTubeVideo => SourceConfig::YouTube {
                video_id: Some(text),
            },
            NewSource::Demo => SourceConfig::Demo,
        }
    }
}

pub struct AppView {
    engine: EngineHandle,
    config: Entity<AppConfig>,
    youtube: Entity<YouTubePanel>,
    status: Status,
    new_source: NewSource,
    input: Entity<InputState>,
    form_error: Option<String>,
    /// Overlay theme folders found in the themes folder.
    themes: Vec<String>,
    theme_select: Entity<SelectState<Vec<SharedString>>>,
    /// The overlay's default chat direction.
    newest_select: Entity<SelectState<Vec<SharedString>>>,
    /// The "New theme" name field, while that form is open.
    new_theme: Option<Entity<InputState>>,
    theme_error: Option<String>,
    /// The test messages and settings windows, once opened (they may be
    /// closed since).
    manual_window: Option<WindowHandle<Root>>,
    settings_window: Option<WindowHandle<Root>>,
    // Subscriptions end when dropped; keeping them here ties them to the
    // view's lifetime.
    _subscriptions: Vec<Subscription>,
}

/// The built-in theme's entry in the theme picker. No theme folder can have
/// this name (`themes::validate_name` refuses it).
const DEFAULT_THEME: &str = "Default";

/// The chat direction dropdown's entries.
const NEWEST_CHOICES: [(Newest, &str); 2] = [
    (Newest::Bottom, "Newest at bottom"),
    (Newest::Top, "Newest at top"),
];

fn newest_label(newest: Newest) -> SharedString {
    NEWEST_CHOICES
        .iter()
        .find(|(n, _)| *n == newest)
        .map(|(_, label)| SharedString::from(*label))
        .unwrap_or_default()
}

impl AppView {
    pub fn new(
        engine: EngineHandle,
        config: Entity<AppConfig>,
        tokio: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("channel name"));
        let youtube =
            cx.new(|cx| YouTubePanel::new(engine.clone(), tokio, config.clone(), window, cx));
        let themes = themes::list(&config.read(cx).themes_dir());
        let current =
            SharedString::from(config.read(cx).theme().unwrap_or(DEFAULT_THEME).to_string());
        let theme_select = cx.new(|cx| {
            let mut select = SelectState::new(theme_items(&themes), None, window, cx);
            select.set_selected_value(&current, window, cx);
            select
        });
        let current_newest = newest_label(config.read(cx).server().newest);
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
            // Enter in the text field adds the source.
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_source(window, cx);
                }
            }),
            // Follow the system's light/dark setting when it changes, unless
            // a fixed look is chosen in the settings.
            cx.observe_window_appearance(window, |this, window, cx| {
                let appearance = this.config.read(cx).app().appearance;
                if appearance == Appearance::System {
                    appearance::apply(appearance, Some(window), cx);
                }
            }),
        ];

        let status = engine.status().borrow().clone();
        Self::watch_status(&engine, cx);
        Self::tick_while_retrying(cx);

        Self {
            engine,
            config,
            youtube,
            status,
            new_source: NewSource::Twitch,
            input,
            form_error: None,
            themes,
            theme_select,
            newest_select,
            new_theme: None,
            theme_error: None,
            manual_window: None,
            settings_window: None,
            _subscriptions: subscriptions,
        }
    }

    // ---- overlay themes ----

    fn rescan_themes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.config.read(cx);
        let themes = themes::list(&config.themes_dir());
        if themes == self.themes {
            return;
        }
        let current = SharedString::from(config.theme().unwrap_or(DEFAULT_THEME).to_string());
        self.themes = themes;
        let items = theme_items(&self.themes);
        self.theme_select.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            select.set_selected_value(&current, window, cx);
        });
        cx.notify();
    }

    /// Switches the overlay theme: overlays reload by themselves (the engine
    /// tells them to) and the choice is saved.
    fn select_theme(&mut self, name: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let theme = (name != DEFAULT_THEME).then(|| name.to_string());
        let themes_dir = self.config.read(cx).themes_dir();
        self.engine
            .set_theme_dir(theme.as_ref().map(|name| themes_dir.join(name)));
        let saved = self.config.update(cx, |config, _| config.set_theme(theme));
        if let Err(e) = saved {
            self.report_save_error(e, window, cx);
        }
        cx.notify();
    }

    /// The overlays' default chat direction: they reload to show it (those
    /// with `?newest=` in their URL keep their own), and it's saved.
    fn set_newest(&mut self, newest: Newest, window: &mut Window, cx: &mut Context<Self>) {
        self.engine.set_newest(newest);
        let saved = self
            .config
            .update(cx, |config, _| config.set_newest(newest));
        if let Err(e) = saved {
            self.report_save_error(e, window, cx);
        }
        cx.notify();
    }

    fn create_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.new_theme else {
            return;
        };
        let name = input.read(cx).value().trim().to_string();
        match themes::create_from_default(&self.config.read(cx).themes_dir(), &name) {
            Ok(dir) => {
                self.new_theme = None;
                self.theme_error = None;
                self.rescan_themes(window, cx);
                let name = SharedString::from(name);
                self.theme_select.update(cx, |select, cx| {
                    select.set_selected_value(&name, window, cx)
                });
                self.select_theme(name, window, cx);
                window.push_notification(
                    Notification::success(format!(
                        "Theme created in {}: edit overlay.css there, then reload the overlays.",
                        dir.display()
                    )),
                    cx,
                );
            }
            Err(e) => self.theme_error = Some(format!("{e:#}")),
        }
        cx.notify();
    }

    fn open_themes_folder(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.config.read(cx).themes_dir();
        // It only exists once there's a theme; create it so there's
        // something to open (and to put themes into).
        if let Err(e) = std::fs::create_dir_all(&dir) {
            report(
                window,
                cx,
                format!("Couldn't create {}: {e}", dir.display()),
            );
            return;
        }
        cx.open_with_system(&dir);
    }

    /// Redraws whenever the engine publishes a new status.
    fn watch_status(engine: &EngineHandle, cx: &mut Context<Self>) {
        let mut status = engine.status();
        // `this` is a *weak* handle: the task doesn't keep the view alive,
        // and `update` fails once the window is closed, which ends the loop.
        cx.spawn(async move |this, cx| {
            loop {
                // Clone the value out right away: the borrow holds a lock
                // and must not live across the `.await` below.
                let current = status.borrow_and_update().clone();
                let still_open = this.update(cx, |view, cx| {
                    view.status = current;
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

    fn add_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value();
        let config = self.new_source.config(&text);
        if let Err(problem) = config.validate() {
            self.form_error = Some(problem);
            cx.notify();
            return;
        }
        self.form_error = None;
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));

        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = engine.add_source(config.clone()).await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(id) => {
                    let saved = view.config.update(cx, |c, _| c.added(id, config));
                    if let Err(e) = saved {
                        view.report_save_error(e, window, cx);
                    }
                }
                Err(e) => report(window, cx, format!("Couldn't add the source: {e:#}")),
            });
        })
        .detach();
    }

    /// Switches a source on or off, and remembers that for the next start.
    fn set_running(&mut self, id: SourceId, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = if on {
                engine.start_source(id).await
            } else {
                engine.stop_source(id).await
            };
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(()) => {
                    let saved = view.config.update(cx, |c, _| c.set_enabled(id, on));
                    if let Err(e) = saved {
                        view.report_save_error(e, window, cx);
                    }
                }
                Err(e) => report(window, cx, format!("Couldn't switch the source: {e:#}")),
            });
        })
        .detach();
    }

    fn remove_source(&mut self, id: SourceId, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = engine.remove_source(id).await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(()) => {
                    let saved = view.config.update(cx, |c, _| c.removed(id));
                    if let Err(e) = saved {
                        view.report_save_error(e, window, cx);
                    }
                }
                Err(e) => report(window, cx, format!("Couldn't remove the source: {e:#}")),
            });
        })
        .detach();
    }

    /// Opens the test messages window (one test source is enough).
    fn open_manual_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        open_or_focus(
            &mut self.manual_window,
            "Test messages",
            size(px(520.), px(420.)),
            move |window, cx| {
                let view = cx.new(|cx| ManualWindow::new(engine, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            },
            window,
            cx,
        );
    }

    fn open_settings_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (engine, config) = (self.engine.clone(), self.config.clone());
        open_or_focus(
            &mut self.settings_window,
            "Settings",
            size(px(640.), px(480.)),
            move |window, cx| {
                let view = cx.new(|cx| SettingsWindow::new(engine, config, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            },
            window,
            cx,
        );
    }

    fn report_save_error(&self, e: anyhow::Error, window: &mut Window, cx: &mut App) {
        let path = self.config.read(cx).config_path().display().to_string();
        report(window, cx, format!("Couldn't save {path}: {e:#}"));
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
        let url = self.status.overlay_url.clone();
        let overlays = self.status.overlays_connected;
        let obs = match overlays {
            0 => "no overlay connected".to_string(),
            1 => "overlay connected".to_string(),
            n => format!("{n} overlays connected"),
        };

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(light(overall, cx))
                    .child(div().flex_1().text_lg().font_semibold().child(headline))
                    .child(
                        Button::new("open-settings")
                            .icon(IconName::Settings)
                            .small()
                            .ghost()
                            .tooltip("Settings")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings_window(window, cx)
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("Overlay"),
                    )
                    .child(div().font_family("monospace").child(url.clone()))
                    .child(
                        Button::new("copy-url")
                            .icon(IconName::Copy)
                            .label("Copy")
                            .small()
                            .ghost()
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                                window.push_notification(
                                    Notification::success("Overlay URL copied"),
                                    cx,
                                );
                            }),
                    )
                    .child(
                        div()
                            .text_color(if overlays > 0 {
                                cx.theme().success
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(obs),
                    ),
            )
    }

    fn render_themes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let missing = self
            .config
            .read(cx)
            .theme()
            .filter(|name| !self.themes.iter().any(|t| t == name))
            .map(str::to_string);

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
                Button::new("theme-reload")
                    .label("Reload overlays")
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.engine.reload_overlays();
                        window.push_notification(Notification::info("Overlays reloaded"), cx);
                    })),
            )
            .child(
                Button::new("theme-new")
                    .icon(IconName::Plus)
                    .label("New theme…")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.new_theme = Some(cx.new(|cx| {
                            InputState::new(window, cx).placeholder("name for the new theme")
                        }));
                        this.theme_error = None;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("theme-folder")
                    .icon(IconName::ExternalLink)
                    .label("Open themes folder")
                    .small()
                    .ghost()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_themes_folder(window, cx)),
                    ),
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
            .children(missing.map(|name| {
                div().text_sm().text_color(cx.theme().warning).child(format!(
                    "The theme folder \"{name}\" wasn't found; the overlay uses the built-in look."
                ))
            }))
            .children(self.new_theme.as_ref().map(|input| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(Input::new(input).id("theme-name")))
                    .child(
                        Button::new("theme-create")
                            .label("Create")
                            .primary()
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.create_theme(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("theme-cancel")
                            .label("Cancel")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_theme = None;
                                this.theme_error = None;
                                cx.notify();
                            })),
                    )
            }))
            .children(
                self.theme_error
                    .clone()
                    .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
            )
    }

    fn render_sources(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let list = v_flex().gap_1().children(
            self.status
                .sources
                .iter()
                .map(|source| self.render_source(source, cx)),
        );
        // The list takes whatever height is left (`flex_1`) and scrolls
        // within it. `min_h_0` lets it shrink below its content's height;
        // without it, flex items grow to fit their content and push the
        // rest of the window out of view instead of scrolling.
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(div().font_semibold().child("Sources"))
                    .child(
                        Button::new("open-manual")
                            .label("Test messages…")
                            .small()
                            .ghost()
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
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = source.id;
        let running = source.state != SourceState::Stopped;
        let key = id.get() as usize;

        h_flex()
            .gap_3()
            .items_center()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(light(source.health(), cx))
            .child(
                v_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(div().child(source.label()))
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
            // The test source belongs to its window: closing the window
            // removes it.
            .when(source.config != SourceConfig::Manual, |row| {
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

    fn render_add_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let kinds =
            h_flex()
                .gap_1()
                .flex_wrap()
                .children(NewSource::ALL.into_iter().enumerate().map(|(i, kind)| {
                    let selected = kind == self.new_source;
                    Button::new(("kind", i))
                        .label(kind.label())
                        .small()
                        .selected(selected)
                        // A clearly visible choice, not just a slight tint.
                        .when(selected, |button| button.primary())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.new_source = kind;
                            this.form_error = None;
                            if let Some(placeholder) = kind.needs_text() {
                                this.input.update(cx, |input, cx| {
                                    input.set_placeholder(placeholder, window, cx)
                                });
                            }
                            cx.notify();
                        }))
                }));

        let mut row = h_flex().gap_2().items_center();
        if self.new_source.needs_text().is_some() {
            row = row.child(
                div()
                    .flex_1()
                    .child(Input::new(&self.input).id("new-source-text")),
            );
        }
        row = row.child(
            Button::new("add")
                .icon(IconName::Plus)
                .label("Add")
                .primary()
                .on_click(cx.listener(|this, _, window, cx| this.add_source(window, cx))),
        );

        v_flex()
            .gap_2()
            .child(div().font_semibold().child("Add a source"))
            .child(kinds)
            .child(row)
            .children(
                self.form_error
                    .clone()
                    .map(|error| div().text_sm().text_color(cx.theme().danger).child(error)),
            )
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_4()
            .gap_5()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_header(cx))
            .child(self.render_themes(cx))
            .child(self.render_sources(cx))
            .child(self.youtube.clone())
            .child(self.render_add_form(cx))
    }
}

/// The theme picker's entries: the built-in theme first, then the folders.
fn theme_items(themes: &[String]) -> Vec<SharedString> {
    std::iter::once(SharedString::from(DEFAULT_THEME))
        .chain(themes.iter().cloned().map(SharedString::from))
        .collect()
}

/// Brings the window in `slot` to the front if it's still open; otherwise
/// opens a new one with `build` and remembers it there. For the side
/// windows: one of each is enough.
fn open_or_focus(
    slot: &mut Option<WindowHandle<Root>>,
    title: &str,
    size: Size<Pixels>,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<Root>,
    window: &mut Window,
    cx: &mut App,
) {
    // `update` fails once the window is closed; then open a new one.
    if let Some(handle) = *slot
        && handle
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
    match cx.open_window(options, build) {
        Ok(handle) => *slot = Some(handle),
        Err(e) => report(window, cx, format!("Couldn't open the window: {e:#}")),
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

pub(crate) fn report(window: &mut Window, cx: &mut App, message: String) {
    window.push_notification(Notification::error(message), cx);
}

/// UI tests: the real window, rendered headless, driven by simulated clicks
/// and typing. Imports are explicit on purpose: with gpui-kit's test support
/// on, `use gpui_kit::*` would also import a `test` macro that shadows Rust's
/// built-in `#[test]`.
#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use std::path::PathBuf;

    use chat_engine::{ConfigFile, Engine, EngineConfig, YouTubeSettings};
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowHandle};

    use super::AppView;
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
        cx.update(gpui_kit::init);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let engine = runtime
            .block_on(Engine::start(EngineConfig {
                bind: (Ipv4Addr::LOCALHOST, 0).into(),
                ..EngineConfig::default()
            }))
            .unwrap();
        let file = ConfigFile {
            youtube: YouTubeSettings::default(), // not set up
            ..ConfigFile::default()
        };
        let settings_dir =
            std::env::temp_dir().join(format!("chat-app-ui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&settings_dir);
        let config = AppConfig::new(file, settings_dir.join("config.toml"));
        let (handle, tokio) = (engine.handle(), runtime.handle().clone());

        // `add_window` builds the root element; the view is created inside
        // and handed out through `view` for the test to inspect.
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let config = cx.new(|_| config);
            let app = cx.new(|cx| AppView::new(handle, config, tokio, window, cx));
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
    fn the_text_field_is_only_there_when_needed(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            // Twitch (selected at start) needs a channel name...
            assert!(window.try_find("new-source-text").is_some());
            // ...Demo needs nothing...
            window.click(("kind", 3usize), cx);
            assert!(window.try_find("new-source-text").is_none());
            // ...and a YouTube video needs its id.
            window.click(("kind", 2usize), cx);
            assert!(window.try_find("new-source-text").is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn youtube_without_a_client_asks_for_one(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            assert!(window.try_find("youtube-client-id").is_some());
            assert!(window.try_find("youtube-connect").is_none());

            // An obviously wrong client id is refused; the form stays open.
            window.click("youtube-client-id", cx);
            window.input("not-a-client-id", cx);
            window.click("youtube-client-secret", cx);
            window.input("GOCSPX-x", cx);
            assert_eq!(
                window.find("youtube-client-id").value(),
                Some("not-a-client-id")
            );
            window.click("youtube-save", cx);
            assert!(window.try_find("youtube-client-id").is_some());
            assert_eq!(
                window.find("youtube-client-id").value(),
                Some("not-a-client-id"),
                "the typed id stays for correcting"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn new_theme_is_created_selected_and_saved(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("theme-new", cx);
            window.click("theme-name", cx);
            window.input("Cozy Night", cx);
            window.click("theme-create", cx);
        })
        .unwrap();

        let theme = ui.settings_dir.join("themes").join("Cozy Night");
        assert!(
            theme.join("overlay.css").exists(),
            "created from the defaults"
        );
        assert_eq!(
            ui.engine.handle().theme_dir(),
            Some(theme),
            "overlays switch to it"
        );
        let saved = ConfigFile::load(&ui.settings_dir.join("config.toml")).unwrap();
        assert_eq!(
            saved.server.theme.as_deref(),
            Some("Cozy Night"),
            "and it's saved"
        );
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
    fn switching_a_source_off_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx);
        let path = ui.settings_dir.join("config.toml");
        let saved_sources = || {
            ConfigFile::load(&path)
                .map(|file| file.sources)
                .unwrap_or_default()
        };
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click(("kind", 3usize), cx); // Demo
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

    #[gpui_kit::test]
    fn chat_direction_reaches_the_engine_and_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            ui.view.update(cx, |view, cx| {
                view.set_newest(chat_engine::Newest::Top, window, cx);
            });
        })
        .unwrap();
        assert_eq!(ui.engine.handle().newest(), chat_engine::Newest::Top);
        let saved = ConfigFile::load(&ui.settings_dir.join("config.toml")).unwrap();
        assert_eq!(saved.server.newest, chat_engine::Newest::Top);
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

    #[gpui_kit::test]
    fn test_messages_window_opens_only_once(cx: &mut TestAppContext) {
        let ui = open(cx);
        let click = |cx: &mut TestAppContext| {
            cx.update_window(ui.window.into(), |_, window, cx| {
                window.click("open-manual", cx);
            })
            .unwrap();
        };
        click(cx);
        assert_eq!(cx.update(|cx| cx.windows().len()), 2);
        click(cx);
        assert_eq!(
            cx.update(|cx| cx.windows().len()),
            2,
            "the open one comes to the front instead"
        );
    }

    #[gpui_kit::test]
    fn invalid_theme_name_is_refused(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("theme-new", cx);
            window.click("theme-name", cx);
            window.input("../escape", cx);
            window.click("theme-create", cx);
            // The form stays open with the typed name, to correct it.
            assert_eq!(window.find("theme-name").value(), Some("../escape"));
        })
        .unwrap();
        let error = ui.view.read_with(cx, |view, _| view.theme_error.clone());
        assert!(error.is_some_and(|e| e.contains("letters")));
        assert!(!ui.settings_dir.join("escape").exists());
    }
}
