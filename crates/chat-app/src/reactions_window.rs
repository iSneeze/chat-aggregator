//! The Reactions window: the rules in a list on the left, the selected
//! rule's form on the right, and the two programs' connection settings
//! below.
//!
//! A rule's switch and its Test button in the list act at once, like a
//! source's switch. The form only applies with Save: a half-typed rule
//! would otherwise fire with a half-typed action. Leaving a form with
//! unsaved changes (picking another rule, a new one, closing the window)
//! asks first.

use std::collections::HashMap;

use chat_core::tiers::TipSettings;
use chat_engine::{EngineHandle, OutputState, ReactionStatus, Rule, Target};
use gpui_kit::component::button::Button;
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, IndexPath, Sizable, StyledExt, h_flex,
    scroll::ScrollableElement, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_config::{AppConfig, edit_settings};
use crate::ui::{card, confirm};
use form::{Form, Inputs, summary, tier_items};
use outputs::OutputInputs;

mod form;
mod outputs;

/// How the last test of a rule went: shown in its row until the next one.
#[derive(Clone, Debug, PartialEq)]
enum Tested {
    Running,
    Sent,
    Failed(String),
}

pub struct ReactionsWindow {
    engine: EngineHandle,
    config: Entity<AppConfig>,
    /// The profile switch whose rules and connections the window shows
    /// (`AppConfig::switches`).
    loaded_for: u64,
    status: ReactionStatus,
    /// The saved rule the form shows, by its name as saved; `None`: a new
    /// rule.
    editing: Option<String>,
    inputs: Inputs,
    /// The paid trigger's tier list: "Any amount", then tier 1 to 7, with
    /// what each means on every platform (so it follows the tip settings).
    tier_select: Entity<SelectState<Vec<SharedString>>>,
    /// The tip settings the list was made with.
    tier_labels_for: TipSettings,
    /// The form's current choices (trigger, role, platforms, action).
    picked: Form,
    /// The form as loaded or last saved: anything else is unsaved.
    loaded: Form,
    form_error: Option<String>,
    tests: HashMap<String, Tested>,
    outputs: OutputInputs,
    warudo_error: Option<String>,
    obs_error: Option<String>,
    /// Whether an OBS password is stored; `None` until the keyring answered.
    password_stored: Option<bool>,
    _subscriptions: Vec<Subscription>,
}

impl ReactionsWindow {
    pub fn new(
        engine: EngineHandle,
        config: Entity<AppConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let all_settings = config.read(cx).settings();
        let settings = all_settings.profile.reactions;
        let tips = all_settings.profile.tips;
        let inputs = Inputs::new(window, cx);
        let tier_select =
            cx.new(|cx| SelectState::new(tier_items(&tips), Some(IndexPath::new(0)), window, cx));
        let outputs = OutputInputs::new(&settings, window, cx);

        let subscriptions = vec![
            // Settings changed (a switch here, the pause in the main
            // window, new tip amounts in Settings): redraw. Another profile
            // brings its own rules and connections.
            cx.observe_in(&config, window, |this, config, window, cx| {
                let switches = config.read(cx).switches();
                if switches != this.loaded_for {
                    this.loaded_for = switches;
                    this.load_profile(window, cx);
                }
                this.relabel_tiers(window, cx);
                cx.notify();
            }),
            // A tier picked: index 0 is "Any amount", index n is tier n.
            cx.subscribe_in(
                &tier_select,
                window,
                |this, select, _: &SelectEvent<Vec<SharedString>>, _, cx| {
                    let index = select.read(cx).selected_index(cx).map_or(0, |i| i.row);
                    this.picked.min_tier = (index > 0).then_some(index as u32);
                    cx.notify();
                },
            ),
        ];
        Self::watch_status(&engine, window, cx);
        Self::ask_before_closing(window, cx);

        let loaded_for = config.read(cx).switches();
        let mut view = Self {
            engine,
            config,
            loaded_for,
            status: ReactionStatus::default(),
            editing: None,
            inputs,
            tier_select,
            tier_labels_for: tips,
            picked: Form::blank(),
            loaded: Form::blank(),
            form_error: None,
            tests: HashMap::new(),
            outputs,
            warudo_error: None,
            obs_error: None,
            password_stored: None,
            _subscriptions: subscriptions,
        };
        let first = settings.rules.first().map(|rule| rule.name.clone());
        view.load(first, window, cx);
        view.check_password(window, cx);
        view
    }

    fn watch_status(engine: &EngineHandle, window: &mut Window, cx: &mut Context<Self>) {
        let mut status = engine.status();
        // As in the main window: a weak handle, so the loop ends with the
        // window.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let current = status.borrow_and_update().reactions.clone();
                let open = this.update(cx, |view, cx| {
                    if view.status != current {
                        view.status = current;
                        cx.notify();
                    }
                });
                if open.is_err() || status.changed().await.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Closing the window with unsaved changes asks first.
    fn ask_before_closing(window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let Some(view) = view.upgrade() else {
                return true;
            };
            if !view.read(cx).unsaved(cx) {
                return true;
            }
            confirm(
                window,
                cx,
                "Close without saving?".into(),
                "Your changes to this window's settings would be lost.",
                "Discard",
                |window, _| window.remove_window(),
            );
            false
        });
    }

    // ---- profile / state helpers ----

    fn rules(&self, cx: &App) -> Vec<Rule> {
        self.config.read(cx).settings().profile.reactions.rules
    }

    fn form(&self, cx: &App) -> Form {
        self.inputs.read(&self.picked, cx)
    }

    /// Unsaved changes anywhere in the window: the rule form, the
    /// connection settings, a typed password not yet stored. (The main
    /// window asks before a profile switch would drop them.)
    pub(crate) fn unsaved(&self, cx: &App) -> bool {
        self.form(cx) != self.loaded || self.outputs_unsaved(cx)
    }

    /// Another profile is active: its first rule in the form, its
    /// connections in their fields. Unsaved changes are dropped: the main
    /// window asked before switching.
    fn load_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.config.read(cx).settings().profile.reactions;
        let first = settings.rules.first().map(|rule| rule.name.clone());
        self.load(first, window, cx);
        self.outputs.fill(&settings, window, cx);
        self.tests.clear();
        self.warudo_error = None;
        self.obs_error = None;
    }

    // ---- the list ----

    /// The list's switch: applies at once, like a source's.
    fn set_enabled(&mut self, name: String, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let handle = window.window_handle();
        edit_settings(
            &self.engine,
            handle,
            move |s| {
                if let Some(rule) = s
                    .profile
                    .reactions
                    .rules
                    .iter_mut()
                    .find(|r| r.name == name)
                {
                    rule.enabled = on;
                }
            },
            cx,
        );
    }

    /// Fires the saved rule now (see `EngineHandle::test_reaction`).
    fn test(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.tests.insert(name.clone(), Tested::Running);
        cx.notify();
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = engine.test_reaction(&name).await;
            let _ = this.update(cx, |view, cx| {
                let tested = match result {
                    Ok(()) => Tested::Sent,
                    Err(e) => Tested::Failed(format!("{e:#}")),
                };
                view.tests.insert(name, tested);
                cx.notify();
            });
        })
        .detach();
    }

    fn connected(&self, target: Target) -> bool {
        self.status
            .outputs
            .iter()
            .any(|o| o.target == target && o.state == OutputState::Connected)
    }

    // ---- drawing ----

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rules = self.rules(cx);
        let rows = rules
            .iter()
            .enumerate()
            .map(|(i, rule)| self.render_rule(i, rule, cx).into_any_element())
            .collect::<Vec<_>>();
        card(cx)
            .w(px(340.))
            .flex_none()
            .min_h_0()
            .child(
                h_flex()
                    .items_center()
                    .child(div().flex_1().font_semibold().child("Rules"))
                    .child(
                        Button::new("rule-new")
                            .label("New rule")
                            .icon(IconName::Plus)
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_rule(None, window, cx)),
                            ),
                    ),
            )
            .child(if rules.is_empty() {
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("No rules yet.")
                    .into_any_element()
            } else {
                div()
                    .id("rule-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(v_flex().gap_1().children(rows))
                    .into_any_element()
            })
    }

    fn render_rule(&self, i: usize, rule: &Rule, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, success) = (theme.muted_foreground, theme.danger, theme.success);
        let selected = self.editing.as_ref() == Some(&rule.name);
        let status = self.status.rules.iter().find(|r| r.name == rule.name);
        let fired = status.map_or(0, |s| s.fired);
        // A test's answer is the freshest news; otherwise OBS's last refusal.
        let note = match self.tests.get(&rule.name) {
            Some(Tested::Running) => Some(("testing…".to_string(), muted)),
            Some(Tested::Sent) => Some(("test sent".to_string(), success)),
            Some(Tested::Failed(why)) => Some((why.clone(), danger)),
            None => status
                .and_then(|s| s.problem.clone())
                .map(|problem| (problem, danger)),
        };
        let target = rule.action.target();
        let connected = self.connected(target);
        let (name, open, test, switch) = (
            rule.name.clone(),
            rule.name.clone(),
            rule.name.clone(),
            rule.name.clone(),
        );

        h_flex()
            .gap_2()
            .items_center()
            .p_1()
            .rounded(theme.radius)
            .when(selected, |row| row.bg(theme.accent))
            .child(
                Switch::new(("rule-on", i))
                    .checked(rule.enabled)
                    .tooltip("On or off, right away")
                    .on_click(cx.listener(move |this, on: &bool, window, cx| {
                        this.set_enabled(switch.clone(), *on, window, cx);
                    })),
            )
            .child(
                v_flex()
                    .id(("rule", i))
                    .flex_1()
                    .overflow_hidden()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_rule(Some(open.clone()), window, cx);
                    }))
                    .child(div().font_semibold().text_ellipsis().child(name))
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .text_ellipsis()
                            .child(summary(rule)),
                    )
                    .children(note.map(|(text, color)| {
                        div().text_sm().text_color(color).line_clamp(2).child(text)
                    }))
                    // Findable by its id in the UI tests (does nothing
                    // otherwise). Last: it wraps the element.
                    .test_support(),
            )
            .child(div().text_sm().text_color(muted).child(format!("{fired}×")))
            .child(
                Button::new(("rule-test", i))
                    .label("Test")
                    .small()
                    .disabled(!connected)
                    .tooltip(if connected {
                        "Do it now, as saved: no trigger, no guards".to_string()
                    } else {
                        format!("{} isn't connected", target.label())
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.test(test.clone(), window, cx);
                    })),
            )
    }
}

impl Render for ReactionsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                v_flex()
                    .child(div().text_lg().font_semibold().child("Reactions"))
                    .child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                        "Chat makes things happen in Warudo and OBS. Chat only picks a rule: \
                         nothing a chatter writes is passed on.",
                    )),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .items_stretch()
                    .child(self.render_list(cx))
                    .child(self.render_form(cx)),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(self.render_output(Target::Warudo, cx))
                    .child(self.render_output(Target::Obs, cx)),
            )
    }
}

/// UI tests: the real window, headless, driven by simulated clicks and
/// typing (see `app_view::tests` for how these work).
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use chat_core::tiers::Amount;
    use chat_engine::{
        Action, Compare, ConfigFile, Crowd, Engine, EngineConfig, Guards, Rule, SecretStore,
        Settings, Trigger,
    };
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowHandle};

    use gpui_kit::component::input::InputState;

    use super::form::{Inputs, summary};
    use super::{ReactionsWindow, Tested};
    use crate::app_config::AppConfig;

    struct Ui {
        window: WindowHandle<Root>,
        view: Entity<ReactionsWindow>,
        engine: Engine,
        dir: PathBuf,
        _runtime: tokio::runtime::Runtime,
    }

    impl Drop for Ui {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rule(name: &str) -> Rule {
        Rule {
            name: name.into(),
            enabled: true,
            trigger: Trigger::Command {
                command: format!("!{name}"),
            },
            guards: Guards::default(),
            action: Action::Warudo {
                action: name.into(),
            },
        }
    }

    fn open(cx: &mut TestAppContext, rules: &[&str]) -> Ui {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("chat-app-reactions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        config.settings.profile.reactions.rules = rules.iter().map(|name| rule(name)).collect();
        // Port 1, where nothing listens: Warudo's usual port may have the
        // real Warudo behind it on a developer's machine, and then a test
        // press would really reach it.
        config.settings.profile.reactions.warudo.url = "ws://127.0.0.1:1".into();
        config.save_to = Some(dir.join("config.toml"));
        // A folder, not the real keyring.
        config.secrets = Some(SecretStore::Dir(dir.join("secrets")));
        let engine = runtime.block_on(Engine::start(config)).unwrap();
        let handle = engine.handle();
        let settings_dir = dir.clone();
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let config = AppConfig::new(&handle, settings_dir, cx);
            let reactions = cx.new(|cx| ReactionsWindow::new(handle, config, window, cx));
            view = Some(reactions.clone());
            Root::new(reactions, window, cx)
        });
        Ui {
            window,
            view: view.expect("the window was built"),
            engine,
            dir,
            _runtime: runtime,
        }
    }

    fn settings(ui: &Ui) -> Settings {
        ui.engine.handle().settings().borrow().clone()
    }

    fn names(ui: &Ui) -> Vec<String> {
        settings(ui)
            .profile
            .reactions
            .rules
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    fn run_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        for _ in 0..500 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    fn act(
        cx: &mut TestAppContext,
        ui: &Ui,
        f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App),
    ) {
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            f(window, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn type_into(cx: &mut TestAppContext, ui: &Ui, field: &'static str, text: &str) {
        act(cx, ui, |window, cx| {
            window.click(field, cx);
            window.input(text, cx);
        });
    }

    /// Replaces a field's text (typing would add to it). `field` picks
    /// the field: a plain function pointer, as it borrows nothing.
    fn set_text(
        cx: &mut TestAppContext,
        ui: &Ui,
        field: fn(&Inputs) -> &Entity<InputState>,
        text: &str,
    ) {
        let view = ui.view.clone();
        let text = text.to_string();
        cx.update_window(ui.window.into(), |_, window, cx| {
            let input = field(&view.read(cx).inputs).clone();
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        })
        .unwrap();
    }

    fn set_name(cx: &mut TestAppContext, ui: &Ui, name: &str) {
        set_text(cx, ui, |inputs| &inputs.name, name);
    }

    fn dialog_open(cx: &mut TestAppContext, ui: &Ui) -> bool {
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find("dialog").is_some()
        })
        .unwrap()
    }

    /// As in `app_view::tests`: a dialog takes clicks once it has faded in.
    fn click_in_dialog(cx: &mut TestAppContext, ui: &Ui, button: &'static str) {
        let fade_in = *gpui_kit::component::dialog::ANIMATION_DURATION;
        std::thread::sleep(fade_in + Duration::from_millis(50));
        act(cx, ui, |window, cx| {
            window.within("dialog").click(button, cx)
        });
    }

    fn editing(cx: &mut TestAppContext, ui: &Ui) -> Option<String> {
        ui.view.read_with(cx, |view, _| view.editing.clone())
    }

    #[gpui_kit::test]
    fn a_new_rule_is_saved_as_typed(cx: &mut TestAppContext) {
        let ui = open(cx, &[]);
        assert_eq!(editing(cx, &ui), None, "no rules: a new one");
        type_into(cx, &ui, "rule-name", "chipmunk");
        act(cx, &ui, |window, cx| {
            window.click(("trigger", 5usize), cx); // Gift
            window.click(("role", 3usize), cx); // Mods
            window.click(("platform", 1usize), cx); // YouTube off
            window.click(("action", 1usize), cx); // OBS filter
        });
        type_into(cx, &ui, "rule-min-count", "0"); // "10"
        type_into(cx, &ui, "rule-source", "Mic");
        type_into(cx, &ui, "rule-filter", "Chipmunk");
        type_into(cx, &ui, "rule-seconds", "30");
        act(cx, &ui, |window, cx| window.click("rule-save", cx));

        run_until(cx, |_| !names(&ui).is_empty());
        let saved = &settings(&ui).profile.reactions.rules[0];
        assert_eq!(saved.name, "chipmunk");
        assert!(saved.enabled);
        assert_eq!(saved.trigger, Trigger::Gift { min_count: 10 });
        assert_eq!(saved.guards.min_role, chat_engine::Role::Moderator);
        assert_eq!(
            saved.guards.platforms,
            [
                chat_core::ChatPlatform::Twitch,
                chat_core::ChatPlatform::Picarto,
                chat_core::ChatPlatform::Rplay,
                chat_core::ChatPlatform::StreamElements,
                chat_core::ChatPlatform::Streamlabs
            ]
        );
        assert_eq!(
            saved.action,
            Action::ObsFilter {
                source: "Mic".into(),
                filter: "Chipmunk".into(),
                seconds: Some(30),
            }
        );
        run_until(cx, |cx| editing(cx, &ui).as_deref() == Some("chipmunk"));
        // Saved: nothing unsaved any more.
        assert!(!ui.view.read_with(cx, |view, cx| view.unsaved(cx)));
        // And in the file.
        let file = ConfigFile::load(&ui.dir.join("config.toml"), None).unwrap();
        assert_eq!(file.profile.reactions.rules.len(), 1);
    }

    #[gpui_kit::test]
    fn a_paid_rule_by_amount_or_by_tier(cx: &mut TestAppContext) {
        let ui = open(cx, &[]);
        type_into(cx, &ui, "rule-name", "nice");
        act(cx, &ui, |window, cx| window.click(("trigger", 3usize), cx)); // Paid
        act(cx, &ui, |window, cx| window.click(("paid-by", 1usize), cx)); // Amount
        act(cx, &ui, |window, cx| window.click(("compare", 1usize), cx)); // Exactly
        type_into(cx, &ui, "rule-amount", "69");
        type_into(cx, &ui, "rule-warudo-action", "nice");
        act(cx, &ui, |window, cx| window.click("rule-save", cx));
        run_until(cx, |_| !names(&ui).is_empty());
        let saved = settings(&ui).profile.reactions.rules[0].clone();
        assert_eq!(
            saved.trigger,
            Trigger::PaidAmount {
                amount: Amount::whole(69),
                compare: Compare::Exactly,
            }
        );
        assert_eq!(summary(&saved), "paid, exactly 69 → Warudo nice");

        // By tier: what picking "Tier 5 or more" in the list does.
        act(cx, &ui, |window, cx| window.click(("paid-by", 0usize), cx));
        ui.view.update(cx, |view, cx| {
            view.picked.min_tier = Some(5);
            cx.notify();
        });
        act(cx, &ui, |window, cx| window.click("rule-save", cx));
        run_until(cx, |_| {
            settings(&ui).profile.reactions.rules[0].trigger == Trigger::Paid { min_tier: Some(5) }
        });
        // Opened again, the list shows it.
        let view = ui.view.clone();
        cx.update_window(ui.window.into(), |_, window, cx| {
            view.update(cx, |view, cx| view.load(Some("nice".into()), window, cx));
        })
        .unwrap();
        let shown = ui.view.read_with(cx, |view, cx| {
            view.tier_select.read(cx).selected_index(cx).map(|i| i.row)
        });
        assert_eq!(shown, Some(5));
    }

    #[gpui_kit::test]
    fn the_tier_list_follows_the_tip_settings(cx: &mut TestAppContext) {
        let ui = open(cx, &[]);
        let label = |cx: &mut TestAppContext| {
            ui.view.read_with(cx, |view, cx| {
                view.tier_select.read(cx).selected_value().cloned()
            })
        };
        ui.view.update(cx, |view, cx| {
            view.picked.min_tier = Some(4);
            cx.notify();
        });
        let view = ui.view.clone();
        cx.update_window(ui.window.into(), |_, window, cx| {
            view.update(cx, |view, cx| view.show_tier(Some(4), window, cx));
        })
        .unwrap();
        assert!(label(cx).is_some_and(|l| l.contains("tips 10+")));
        let edit = ui
            .engine
            .handle()
            .edit_settings(|s| s.profile.tips.currency = "EUR".into());
        ui._runtime.block_on(edit).unwrap();
        run_until(cx, |cx| label(cx).is_some_and(|l| l.contains("€10.00+")));
        // Still tier 4, under its new label.
        let picked = ui.view.read_with(cx, |view, _| view.picked.min_tier);
        assert_eq!(picked, Some(4));
    }

    #[gpui_kit::test]
    fn a_form_problem_is_shown_and_nothing_is_saved(cx: &mut TestAppContext) {
        let ui = open(cx, &["bonk"]);
        act(cx, &ui, |window, cx| window.click("rule-new", cx));
        type_into(cx, &ui, "rule-name", "wave");
        // No command typed.
        act(cx, &ui, |window, cx| window.click("rule-save", cx));
        let error = ui.view.read_with(cx, |view, _| view.form_error.clone());
        assert!(error.is_some_and(|e| e.contains("command")));

        // A name that's taken.
        set_name(cx, &ui, "bonk");
        type_into(cx, &ui, "rule-command", "!wave");
        type_into(cx, &ui, "rule-warudo-action", "wave");
        act(cx, &ui, |window, cx| window.click("rule-save", cx));
        let error = ui.view.read_with(cx, |view, _| view.form_error.clone());
        assert!(error.is_some_and(|e| e.contains("already")));
        assert_eq!(names(&ui), ["bonk"]);
    }

    #[gpui_kit::test]
    fn a_crowd_takes_both_numbers_and_no_per_chatter_cooldown(cx: &mut TestAppContext) {
        let ui = open(cx, &[]);
        let error = |cx: &mut TestAppContext| ui.view.read_with(cx, |v, _| v.form_error.clone());
        type_into(cx, &ui, "rule-name", "respects");
        type_into(cx, &ui, "rule-command", "F");
        type_into(cx, &ui, "rule-warudo-action", "candles");
        // Half a crowd isn't saved as no crowd.
        type_into(cx, &ui, "rule-crowd", "10");
        act(cx, &ui, |window, cx| window.click("rule-save", cx));
        assert!(error(cx).is_some_and(|e| e.contains("seconds")));
        // Refused together (decided 2026-11-13), in config.toml's words.
        type_into(cx, &ui, "rule-crowd-secs", "30");
        type_into(cx, &ui, "rule-user-cooldown", "5"); // "05"
        act(cx, &ui, |window, cx| window.click("rule-save", cx));
        assert!(error(cx).is_some_and(|e| e.contains("per-chatter")));
        assert!(names(&ui).is_empty());

        set_text(cx, &ui, |inputs| &inputs.user_cooldown, "0");
        act(cx, &ui, |window, cx| window.click("rule-save", cx));
        run_until(cx, |_| !names(&ui).is_empty());
        let saved = settings(&ui).profile.reactions.rules[0].clone();
        assert_eq!(
            saved.guards.crowd,
            Some(Crowd {
                chatters: 10,
                within_secs: 30,
            })
        );
        assert_eq!(summary(&saved), "F, 10 chatters in 30 s → Warudo candles");
        // Loaded back into the form, it reads the same: nothing unsaved.
        run_until(cx, |cx| editing(cx, &ui).as_deref() == Some("respects"));
        assert!(!ui.view.read_with(cx, |view, cx| view.unsaved(cx)));
    }

    #[gpui_kit::test]
    fn a_renamed_rule_keeps_its_place(cx: &mut TestAppContext) {
        let ui = open(cx, &["a", "b", "c"]);
        assert_eq!(editing(cx, &ui).as_deref(), Some("a"), "the first is shown");
        act(cx, &ui, |window, cx| window.click(("rule", 1usize), cx));
        assert_eq!(editing(cx, &ui).as_deref(), Some("b"));
        set_name(cx, &ui, "b2");
        act(cx, &ui, |window, cx| window.click("rule-save", cx));
        run_until(cx, |_| names(&ui) == ["a", "b2", "c"]);
        run_until(cx, |cx| editing(cx, &ui).as_deref() == Some("b2"));
    }

    #[gpui_kit::test]
    fn the_list_switch_applies_at_once(cx: &mut TestAppContext) {
        let ui = open(cx, &["a", "b"]);
        act(cx, &ui, |window, cx| window.click(("rule-on", 1usize), cx));
        run_until(cx, |_| !settings(&ui).profile.reactions.rules[1].enabled);
        assert!(settings(&ui).profile.reactions.rules[0].enabled);
    }

    #[gpui_kit::test]
    fn delete_asks_then_removes(cx: &mut TestAppContext) {
        let ui = open(cx, &["a", "b"]);
        act(cx, &ui, |window, cx| window.click("rule-delete", cx));
        assert!(dialog_open(cx, &ui));
        assert_eq!(names(&ui), ["a", "b"], "nothing yet");
        click_in_dialog(cx, &ui, "ok");
        run_until(cx, |_| names(&ui) == ["b"]);
        run_until(cx, |cx| editing(cx, &ui).as_deref() == Some("b"));
    }

    #[gpui_kit::test]
    fn unsaved_changes_are_not_dropped_without_asking(cx: &mut TestAppContext) {
        let ui = open(cx, &["a", "b"]);
        type_into(cx, &ui, "rule-name", "x"); // "ax"
        act(cx, &ui, |window, cx| window.click(("rule", 1usize), cx));
        assert!(dialog_open(cx, &ui), "asked");
        assert_eq!(editing(cx, &ui).as_deref(), Some("a"), "still there");

        click_in_dialog(cx, &ui, "cancel");
        run_until(cx, |cx| !dialog_open(cx, &ui));
        assert_eq!(editing(cx, &ui).as_deref(), Some("a"));
        let name = ui
            .view
            .read_with(cx, |view, cx| view.inputs.name.read(cx).value().to_string());
        assert_eq!(name, "ax", "the change is kept");

        act(cx, &ui, |window, cx| window.click(("rule", 1usize), cx));
        click_in_dialog(cx, &ui, "ok"); // discard
        run_until(cx, |cx| editing(cx, &ui).as_deref() == Some("b"));
        assert_eq!(names(&ui), ["a", "b"], "nothing saved");

        // Without changes: no question.
        act(cx, &ui, |window, cx| window.click(("rule", 0usize), cx));
        assert!(!dialog_open(cx, &ui));
        assert_eq!(editing(cx, &ui).as_deref(), Some("a"));
    }

    #[gpui_kit::test]
    fn another_profile_brings_its_own_rules_and_connections(cx: &mut TestAppContext) {
        let ui = open(cx, &["a", "b"]);
        let handle = ui.engine.handle();
        ui._runtime
            .block_on(handle.create_profile("Indie", false))
            .unwrap();
        let mut indie = chat_engine::ProfileFile::default();
        indie.reactions.rules = vec![rule("wave")];
        indie.reactions.warudo.url = "ws://127.0.0.1:2".into();
        indie
            .save(&ConfigFile::profile_path(&ui.dir, "Indie"))
            .unwrap();
        type_into(cx, &ui, "rule-name", "x"); // "ax": the main window asked

        ui._runtime
            .block_on(handle.switch_profile("Indie"))
            .unwrap();
        run_until(cx, |cx| editing(cx, &ui).as_deref() == Some("wave"));
        let (name, warudo, unsaved) = ui.view.read_with(cx, |view, cx| {
            let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
            (
                text(&view.inputs.name),
                text(&view.outputs.warudo_url),
                view.unsaved(cx),
            )
        });
        assert_eq!(name, "wave");
        assert_eq!(warudo, "ws://127.0.0.1:2");
        assert!(!unsaved, "nothing of Main's left over");

        // Renamed: the same profile, so what's typed stays.
        type_into(cx, &ui, "rule-name", "s"); // "waves"
        ui._runtime
            .block_on(handle.rename_profile("Indie", "Solo"))
            .unwrap();
        run_until(cx, |cx| {
            ui.view
                .read_with(cx, |view, cx| view.config.read(cx).profile() == "Solo")
        });
        // The window hears of the rename a moment later (a GPUI task woken
        // from the engine's thread): give it that moment, then look.
        let name = |cx: &mut TestAppContext| {
            ui.view
                .read_with(cx, |view, cx| view.inputs.name.read(cx).value().to_string())
        };
        for _ in 0..30 {
            cx.run_until_parked();
            if name(cx) != "waves" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(name(cx), "waves");
    }

    #[gpui_kit::test]
    fn a_test_needs_its_program_connected(cx: &mut TestAppContext) {
        let ui = open(cx, &["a"]);
        // Nothing listens at Warudo's address in a test: not connected, so
        // Test is off, and the engine would say why anyway.
        act(cx, &ui, |window, cx| {
            window.click(("rule-test", 0usize), cx)
        });
        assert_eq!(
            ui.view
                .read_with(cx, |view, _| view.tests.get("a").cloned()),
            None
        );
        let view = ui.view.clone();
        cx.update_window(ui.window.into(), |_, window, cx| {
            view.update(cx, |view, cx| view.test("a".into(), window, cx));
        })
        .unwrap();
        run_until(cx, |cx| {
            matches!(
                ui.view
                    .read_with(cx, |view, _| view.tests.get("a").cloned()),
                Some(Tested::Failed(_))
            )
        });
    }

    #[gpui_kit::test]
    fn the_obs_password_goes_to_the_secret_store(cx: &mut TestAppContext) {
        let ui = open(cx, &[]);
        let stored =
            |cx: &mut TestAppContext| ui.view.read_with(cx, |view, _| view.password_stored);
        run_until(cx, |cx| stored(cx) == Some(false));
        type_into(cx, &ui, "obs-password", "hunter2");
        act(cx, &ui, |window, cx| window.click("obs-password-store", cx));
        run_until(cx, |cx| stored(cx) == Some(true));
        let store = SecretStore::Dir(ui.dir.join("secrets"));
        let saved = ui
            ._runtime
            .block_on(store.load(chat_engine::OBS_PASSWORD))
            .unwrap();
        assert_eq!(saved.as_deref(), Some("hunter2"));
        let field = ui.view.read_with(cx, |view, cx| {
            view.outputs.obs_password.read(cx).value().to_string()
        });
        assert_eq!(field, "", "not kept in the window");
        // Never in the settings file.
        let file = std::fs::read_to_string(ui.dir.join("config.toml")).unwrap_or_default();
        assert!(!file.contains("hunter2"));

        act(cx, &ui, |window, cx| {
            window.click("obs-password-remove", cx)
        });
        run_until(cx, |cx| stored(cx) == Some(false));
    }

    #[gpui_kit::test]
    fn connection_settings_are_checked_and_saved(cx: &mut TestAppContext) {
        let ui = open(cx, &[]);
        type_into(cx, &ui, "warudo-queue", "0"); // "00": still zero, fine
        let view = ui.view.clone();
        cx.update_window(ui.window.into(), |_, window, cx| {
            let url = view.read(cx).outputs.warudo_url.clone();
            url.update(cx, |input, cx| {
                input.set_value("http://192.168.1.20:19190", window, cx)
            });
        })
        .unwrap();
        act(cx, &ui, |window, cx| window.click("warudo-save", cx));
        let error = ui.view.read_with(cx, |view, _| view.warudo_error.clone());
        assert!(error.is_some_and(|e| e.contains("ws://")));

        cx.update_window(ui.window.into(), |_, window, cx| {
            let url = view.read(cx).outputs.warudo_url.clone();
            url.update(cx, |input, cx| {
                input.set_value("ws://192.168.1.20:19190", window, cx)
            });
        })
        .unwrap();
        type_into(cx, &ui, "warudo-queue", "30"); // "0030"
        act(cx, &ui, |window, cx| window.click("warudo-save", cx));
        run_until(cx, |_| {
            settings(&ui).profile.reactions.warudo.queue_secs == 30
        });
        assert_eq!(
            settings(&ui).profile.reactions.warudo.url,
            "ws://192.168.1.20:19190"
        );
        assert!(!ui.view.read_with(cx, |view, cx| view.unsaved(cx)));
    }
}
