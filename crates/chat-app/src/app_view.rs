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
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, IconName, Selectable, Sizable, StyledExt, WindowExt, h_flex,
    scroll::ScrollableElement, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::sources::SourceList;
use crate::youtube_panel::{SettingsChanged, YouTubePanel};

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
    sources: SourceList,
    youtube: Entity<YouTubePanel>,
    status: Status,
    new_source: NewSource,
    input: Entity<InputState>,
    form_error: Option<String>,
    // Subscriptions end when dropped; keeping them here ties them to the
    // view's lifetime.
    _subscriptions: Vec<Subscription>,
}

impl AppView {
    pub fn new(
        engine: EngineHandle,
        sources: SourceList,
        tokio: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("channel name"));
        let youtube = cx.new(|cx| {
            YouTubePanel::new(
                engine.clone(),
                tokio,
                sources.youtube().clone(),
                sources.settings_dir(),
                window,
                cx,
            )
        });
        let subscriptions = vec![
            // The panel reports new credentials; this view owns the config
            // file, so it saves them.
            cx.subscribe_in(
                &youtube,
                window,
                |this, _, event: &SettingsChanged, window, cx| {
                    if let Err(e) = this.sources.set_youtube(event.0.clone()) {
                        this.report_save_error(e, window, cx);
                    }
                },
            ),
            // Enter in the text field adds the source.
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_source(window, cx);
                }
            }),
            // Follow the system's light/dark setting when it changes.
            cx.observe_window_appearance(window, |_, window, cx| {
                gpui_kit::component::Theme::sync_system_appearance(Some(window), cx);
            }),
        ];

        let status = engine.status().borrow().clone();
        Self::watch_status(&engine, cx);
        Self::tick_while_retrying(cx);

        Self {
            engine,
            sources,
            youtube,
            status,
            new_source: NewSource::Twitch,
            input,
            form_error: None,
            _subscriptions: subscriptions,
        }
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
                    if let Err(e) = view.sources.added(id, config) {
                        view.report_save_error(e, window, cx);
                    }
                }
                Err(e) => report(window, cx, format!("Couldn't add the source: {e:#}")),
            });
        })
        .detach();
    }

    fn set_running(&mut self, id: SourceId, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = if on {
                engine.start_source(id).await
            } else {
                engine.stop_source(id).await
            };
            if let Err(e) = result {
                let _ = this.update_in(cx, |_, window, cx| {
                    report(window, cx, format!("Couldn't switch the source: {e:#}"));
                });
            }
        })
        .detach();
    }

    fn remove_source(&mut self, id: SourceId, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = engine.remove_source(id).await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(()) => {
                    if let Err(e) = view.sources.removed(id) {
                        view.report_save_error(e, window, cx);
                    }
                }
                Err(e) => report(window, cx, format!("Couldn't remove the source: {e:#}")),
            });
        })
        .detach();
    }

    fn report_save_error(&self, e: anyhow::Error, window: &mut Window, cx: &mut App) {
        report(
            window,
            cx,
            format!(
                "Couldn't save {}: {e:#}",
                self.sources.config_path().display()
            ),
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
                    .child(div().text_lg().font_semibold().child(headline)),
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
            .child(div().font_semibold().child("Sources"))
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
            .child(
                Button::new(("remove", key))
                    .icon(IconName::Close)
                    .small()
                    .ghost()
                    .tooltip("Remove this source")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove_source(id, window, cx);
                    })),
            )
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
            .child(self.render_sources(cx))
            .child(self.youtube.clone())
            .child(self.render_add_form(cx))
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

    use chat_engine::{ConfigFile, Engine, EngineConfig, YouTubeSettings};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, TestAppContext, WindowHandle};

    use super::AppView;
    use crate::sources::SourceList;

    /// Opens the window with a real engine (on its own tokio runtime) and
    /// no sources. The runtime and engine are returned so they live as long
    /// as the test.
    fn open(cx: &mut TestAppContext) -> (WindowHandle<AppView>, tokio::runtime::Runtime, Engine) {
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
        let path = std::env::temp_dir()
            .join(format!("chat-app-ui-test-{}", std::process::id()))
            .join("config.toml");
        let sources = SourceList::new(file, path);
        let (handle, tokio) = (engine.handle(), runtime.handle().clone());
        let window = cx.add_window(|window, cx| AppView::new(handle, sources, tokio, window, cx));
        (window, runtime, engine)
    }

    #[gpui_kit::test]
    fn invalid_twitch_name_is_rejected_in_the_form(cx: &mut TestAppContext) {
        let (window, _runtime, _engine) = open(cx);
        cx.update_window(window.into(), |_, window, cx| {
            window.click("new-source-text", cx);
            window.input("not valid!", cx);
            window.click("add", cx);
        })
        .unwrap();

        let error = window
            .read_with(cx, |view, _| view.form_error.clone())
            .unwrap();
        let error = error.expect("the form shows an error");
        // Names the typed text: proves the typing reached the field (an
        // empty field would be rejected too, with a different message).
        assert!(error.contains("not valid!"), "{error}");
    }

    #[gpui_kit::test]
    fn the_text_field_is_only_there_when_needed(cx: &mut TestAppContext) {
        let (window, _runtime, _engine) = open(cx);
        cx.update_window(window.into(), |_, window, cx| {
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
        let (window, _runtime, _engine) = open(cx);
        cx.update_window(window.into(), |_, window, cx| {
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
}
