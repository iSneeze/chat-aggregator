//! The window shown when the app can't start: a broken `config.toml`, the
//! port already taken, …
//!
//! Without it, the release build on Windows (which has no console) would
//! just not appear, and the reason would only be in the log file. The window
//! says what went wrong and what to do; it never changes anything itself
//! (e.g. a broken `config.toml` stays as it is for you to fix).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use chat_engine::Appearance;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Root, StyledExt, h_flex, v_flex};
use gpui_kit::*;

use crate::{appearance, logging};

/// Which step of starting up failed: decides what advice fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Reading `config.toml`.
    Config,
    /// Starting the overlay server (binding the port).
    Server,
    /// Anything else.
    Other,
}

/// A failed start: the step, and the error from it.
pub struct StartupError {
    pub stage: Stage,
    pub error: anyhow::Error,
}

impl StartupError {
    /// For `map_err`: `.map_err(StartupError::at(Stage::Config))`.
    pub fn at(stage: Stage) -> impl FnOnce(anyhow::Error) -> Self {
        move |error| Self { stage, error }
    }
}

/// What the window says.
#[derive(Debug, Clone)]
struct Problem {
    headline: String,
    advice: String,
    /// The error itself, for the curious and for bug reports.
    details: String,
    settings_dir: PathBuf,
}

fn describe(failed: &StartupError, settings_dir: &Path) -> Problem {
    let io_kind = failed
        .error
        .chain()
        .find_map(|e| e.downcast_ref::<std::io::Error>())
        .map(std::io::Error::kind);
    let (headline, advice) = match (failed.stage, io_kind) {
        (Stage::Config, _) => (
            "config.toml has a mistake",
            "Fix it in a text editor (the details below name the line), then start \
             chat-aggregator again. Nothing in the file was changed.",
        ),
        (Stage::Server, Some(ErrorKind::AddrInUse)) => (
            "The overlay's port is already in use",
            "Is chat-aggregator already running? If a different program uses this port, choose \
             another `port` under [server] in config.toml.",
        ),
        (Stage::Server, Some(ErrorKind::PermissionDenied)) => (
            "The overlay's port isn't allowed",
            "The system doesn't let chat-aggregator use this port (Windows reserves \
             some ports, e.g. for Hyper-V or WSL). Choose another `port` under \
             [server] in config.toml.",
        ),
        (Stage::Server, _) => (
            "The overlay server couldn't start",
            "Check the details below; changing the `port` under [server] in \
             config.toml may help.",
        ),
        (Stage::Other, _) => (
            "chat-aggregator couldn't start",
            "Check the details below. If it keeps happening, the log file helps \
             with a bug report.",
        ),
    };
    Problem {
        headline: headline.into(),
        advice: advice.into(),
        details: format!("{:#}", failed.error),
        settings_dir: settings_dir.to_path_buf(),
    }
}

/// Runs a GPUI app with just the error window, until it's closed.
pub fn show(failed: &StartupError, settings_dir: &Path) {
    let problem = describe(failed, settings_dir);
    application().with_assets(assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        let bounds = Bounds::centered(None, size(px(520.), px(380.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("chat-aggregator couldn't start".into()),
                ..Default::default()
            }),
            app_id: Some("chat-aggregator".into()),
            ..Default::default()
        };
        let opened = cx.open_window(options, |window, cx| {
            // The settings may be what's broken: the default look.
            appearance::apply(Appearance::default(), Some(window), cx);
            let view = cx.new(|_| StartupErrorView { problem });
            cx.new(|cx| Root::new(view, window, cx))
        });
        if let Err(e) = opened {
            // Not even a window: the log file is all that's left.
            tracing::error!("can't show the startup error either: {e:#}");
            cx.quit();
        }
    });
}

struct StartupErrorView {
    problem: Problem,
}

impl Render for StartupErrorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let problem = &self.problem;
        let log = problem.settings_dir.join(logging::FILE_NAME);
        let dir = problem.settings_dir.clone();
        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .child(problem.headline.clone()),
            )
            .child(div().child(problem.advice.clone()))
            .child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(cx.theme().muted)
                    .text_sm()
                    .child(problem.details.clone()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Also in the log file: {}", log.display())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("startup-open-folder")
                            .label("Open settings folder")
                            .on_click(move |_, _, cx| cx.open_with_system(&dir)),
                    )
                    .child(
                        Button::new("startup-quit")
                            .label("Quit")
                            .primary()
                            .on_click(|_, _, cx| cx.quit()),
                    ),
            )
    }
}

// Explicit imports, not `use super::*`: that would also bring in
// `gpui_kit::*`, whose `test` attribute replaces the standard `#[test]`.
#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::path::Path;

    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::{Problem, Stage, StartupError, StartupErrorView, describe};

    fn failed(stage: Stage, error: anyhow::Error) -> Problem {
        describe(&StartupError { stage, error }, Path::new("/settings"))
    }

    fn io(kind: ErrorKind) -> anyhow::Error {
        anyhow::Error::new(std::io::Error::from(kind)).context("can't listen on 127.0.0.1:7878")
    }

    #[test]
    fn a_taken_port_asks_whether_the_app_is_already_running() {
        let problem = failed(Stage::Server, io(ErrorKind::AddrInUse));
        assert!(problem.headline.contains("already in use"), "{problem:?}");
        assert!(
            problem
                .advice
                .contains("Is chat-aggregator already running?"),
            "{problem:?}"
        );
        assert!(problem.details.contains("127.0.0.1:7878"), "{problem:?}");
    }

    #[test]
    fn a_reserved_port_says_so() {
        let problem = failed(Stage::Server, io(ErrorKind::PermissionDenied));
        assert!(problem.advice.contains("reserves"), "{problem:?}");
    }

    #[test]
    fn a_broken_config_names_the_line() {
        let error = chat_engine::ConfigFile::parse("[server]\nprot = 7878\n").unwrap_err();
        let problem = failed(Stage::Config, error);
        assert!(problem.headline.contains("config.toml"), "{problem:?}");
        assert!(problem.details.contains("prot"), "{problem:?}");
        assert!(problem.details.contains("line 2"), "{problem:?}");
    }

    #[gpui_kit::test]
    fn the_window_offers_the_folder_and_quit(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let problem = failed(Stage::Server, io(ErrorKind::AddrInUse));
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|_| StartupErrorView { problem });
            Root::new(view, window, cx)
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("startup-open-folder").is_some());
            assert!(window.try_find("startup-quit").is_some());
        })
        .unwrap();
    }
}
