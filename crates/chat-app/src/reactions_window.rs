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

use chat_core::ChatPlatform;
use chat_core::tiers::{Amount, TipSettings};
use chat_engine::{
    Action, Compare, EngineHandle, Guards, Health, MAX_QUEUE_SECS, ObsSettings, OutputState,
    ReactionStatus, Role, Rule, Target, Trigger, WarudoSettings,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, IndexPath, Selectable, Sizable, StyledExt, WindowExt,
    h_flex, scroll::ScrollableElement, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_config::{AppConfig, edit_settings};
use crate::app_view::{card, light, report, segmented};
use crate::manual_window::labelled;
use crate::paid;

/// What fires a rule, as picked in the form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TriggerKind {
    Command,
    Keyword,
    Emote,
    Paid,
    Membership,
    Gift,
}

impl TriggerKind {
    const ALL: [TriggerKind; 6] = [
        TriggerKind::Command,
        TriggerKind::Keyword,
        TriggerKind::Emote,
        TriggerKind::Paid,
        TriggerKind::Membership,
        TriggerKind::Gift,
    ];

    fn label(self) -> &'static str {
        match self {
            TriggerKind::Command => "Command",
            TriggerKind::Keyword => "Keyword",
            TriggerKind::Emote => "Emote",
            TriggerKind::Paid => "Paid",
            TriggerKind::Membership => "Membership",
            TriggerKind::Gift => "Gift",
        }
    }
}

/// What a paid trigger looks at, as picked in the form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaidBy {
    /// The tier (`Trigger::Paid`).
    Tier,
    /// The amount as a number (`Trigger::PaidAmount`).
    Amount,
}

/// What a rule does, as picked in the form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionKind {
    Warudo,
    ObsFilter,
    ObsSource,
    ObsMedia,
}

impl ActionKind {
    const ALL: [ActionKind; 4] = [
        ActionKind::Warudo,
        ActionKind::ObsFilter,
        ActionKind::ObsSource,
        ActionKind::ObsMedia,
    ];

    fn label(self) -> &'static str {
        match self {
            ActionKind::Warudo => "Warudo",
            ActionKind::ObsFilter => "OBS filter",
            ActionKind::ObsSource => "OBS source",
            ActionKind::ObsMedia => "OBS media",
        }
    }
}

/// The role guard's choices, least to most.
const ROLES: [(Role, &str); 5] = [
    (Role::Everyone, "Everyone"),
    (Role::Subscriber, "Subs/members"),
    (Role::Vip, "VIPs"),
    (Role::Moderator, "Mods"),
    (Role::Broadcaster, "Only me"),
];

/// The platforms a rule can be limited to (those the app reads chat from).
const PLATFORMS: [(ChatPlatform, &str); 5] = [
    (ChatPlatform::Twitch, "Twitch"),
    (ChatPlatform::YouTube, "YouTube"),
    (ChatPlatform::Picarto, "Picarto"),
    (ChatPlatform::StreamElements, "StreamElements"),
    (ChatPlatform::Streamlabs, "Streamlabs"),
];

/// Everything the form holds, as typed and picked. Compared with what was
/// loaded to tell unsaved changes, and turned into a [`Rule`] on Save.
#[derive(Clone, Debug, PartialEq)]
struct Form {
    name: String,
    trigger: TriggerKind,
    command: String,
    keyword: String,
    emote: String,
    paid_by: PaidBy,
    /// Picked from the tier list; `None`: any amount.
    min_tier: Option<u32>,
    amount: String,
    compare: Compare,
    min_count: String,
    role: Role,
    /// The ticked ones; all ticked is the same as no platform guard.
    platforms: Vec<ChatPlatform>,
    cooldown: String,
    user_cooldown: String,
    action: ActionKind,
    warudo_action: String,
    source: String,
    filter: String,
    scene: String,
    seconds: String,
}

impl Form {
    fn blank() -> Self {
        Self {
            name: String::new(),
            trigger: TriggerKind::Command,
            command: String::new(),
            keyword: String::new(),
            emote: String::new(),
            paid_by: PaidBy::Tier,
            min_tier: None,
            amount: String::new(),
            compare: Compare::AtLeast,
            min_count: "1".into(),
            role: Role::Everyone,
            platforms: all_platforms(),
            cooldown: "0".into(),
            user_cooldown: "0".into(),
            action: ActionKind::Warudo,
            warudo_action: String::new(),
            source: String::new(),
            filter: String::new(),
            scene: String::new(),
            seconds: String::new(),
        }
    }

    fn of(rule: &Rule) -> Self {
        // Starts blank: fields the rule doesn't use keep their defaults,
        // ready if the streamer switches to that kind.
        let mut form = Self::blank();
        form.name = rule.name.clone();
        let optional = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
        form.trigger = match &rule.trigger {
            Trigger::Command { command } => {
                form.command = command.clone();
                TriggerKind::Command
            }
            Trigger::Keyword { keyword } => {
                form.keyword = keyword.clone();
                TriggerKind::Keyword
            }
            Trigger::Emote { emote } => {
                form.emote = emote.clone();
                TriggerKind::Emote
            }
            Trigger::Paid { min_tier } => {
                form.min_tier = *min_tier;
                TriggerKind::Paid
            }
            Trigger::PaidAmount { amount, compare } => {
                form.paid_by = PaidBy::Amount;
                form.amount = amount.to_string();
                form.compare = *compare;
                TriggerKind::Paid
            }
            Trigger::Membership => TriggerKind::Membership,
            Trigger::Gift { min_count } => {
                form.min_count = min_count.to_string();
                TriggerKind::Gift
            }
        };
        let guards = &rule.guards;
        form.role = guards.min_role;
        if !guards.platforms.is_empty() {
            form.platforms = all_platforms()
                .into_iter()
                .filter(|p| guards.platforms.contains(p))
                .collect();
        }
        form.cooldown = guards.cooldown_secs.to_string();
        form.user_cooldown = guards.user_cooldown_secs.to_string();
        form.action = match &rule.action {
            Action::Warudo { action } => {
                form.warudo_action = action.clone();
                ActionKind::Warudo
            }
            Action::ObsFilter {
                source,
                filter,
                seconds,
            } => {
                form.source = source.clone();
                form.filter = filter.clone();
                form.seconds = optional(*seconds);
                ActionKind::ObsFilter
            }
            Action::ObsSource {
                scene,
                source,
                seconds,
            } => {
                form.scene = scene.clone();
                form.source = source.clone();
                form.seconds = optional(*seconds);
                ActionKind::ObsSource
            }
            Action::ObsMedia { source } => {
                form.source = source.clone();
                ActionKind::ObsMedia
            }
        };
        form
    }

    /// The rule the form describes, or what's wrong with it in words for
    /// the streamer. `enabled` comes from the list's switch, not the form.
    fn rule(&self, enabled: bool) -> Result<Rule, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Give the rule a name.".into());
        }
        let text = |value: &str, what: &str| {
            let value = value.trim();
            if value.is_empty() {
                Err(format!("Enter {what}."))
            } else {
                Ok(value.to_string())
            }
        };
        let trigger = match self.trigger {
            TriggerKind::Command => Trigger::Command {
                command: text(&self.command, "the command, e.g. !bonk")?,
            },
            TriggerKind::Keyword => Trigger::Keyword {
                keyword: text(&self.keyword, "the keyword")?,
            },
            TriggerKind::Emote => Trigger::Emote {
                emote: text(&self.emote, "the emote's code")?,
            },
            TriggerKind::Paid => match self.paid_by {
                PaidBy::Tier => Trigger::Paid {
                    min_tier: self.min_tier,
                },
                PaidBy::Amount => Trigger::PaidAmount {
                    amount: text(&self.amount, "the amount, e.g. 69")?
                        .parse::<Amount>()
                        .map_err(|e| capitalized(&format!("the amount: {e}")))?,
                    compare: self.compare,
                },
            },
            TriggerKind::Membership => Trigger::Membership,
            TriggerKind::Gift => Trigger::Gift {
                min_count: number(&self.min_count, "The number of gifts")? as usize,
            },
        };
        if self.platforms.is_empty() {
            return Err("Tick at least one platform.".into());
        }
        let platforms = if self.platforms.len() == PLATFORMS.len() {
            Vec::new() // all of them: no guard, so platforms added later count too
        } else {
            self.platforms.clone()
        };
        let guards = Guards {
            min_role: self.role,
            platforms,
            cooldown_secs: number(&self.cooldown, "The cooldown")?,
            user_cooldown_secs: number(&self.user_cooldown, "The per-chatter cooldown")?,
        };
        let seconds = optional_number(&self.seconds, "The seconds")?;
        let action = match self.action {
            ActionKind::Warudo => Action::Warudo {
                action: text(&self.warudo_action, "the action's name, as in Warudo")?,
            },
            ActionKind::ObsFilter => Action::ObsFilter {
                source: text(&self.source, "the source, as named in OBS")?,
                filter: text(&self.filter, "the filter, as named in OBS")?,
                seconds,
            },
            ActionKind::ObsSource => Action::ObsSource {
                scene: text(&self.scene, "the scene, as named in OBS")?,
                source: text(&self.source, "the source, as named in OBS")?,
                seconds,
            },
            ActionKind::ObsMedia => Action::ObsMedia {
                source: text(&self.source, "the media source, as named in OBS")?,
            },
        };
        let rule = Rule {
            name: name.to_string(),
            enabled,
            trigger,
            guards,
            action,
        };
        // What `config.toml` would refuse, said the same way.
        rule.check().map_err(capitalized)?;
        Ok(rule)
    }
}

fn all_platforms() -> Vec<ChatPlatform> {
    PLATFORMS.iter().map(|(p, _)| *p).collect()
}

/// A whole number; empty is 0.
fn number(value: &str, what: &str) -> Result<u32, String> {
    Ok(optional_number(value, what)?.unwrap_or(0))
}

/// A whole number, or `None` when empty.
fn optional_number(value: &str, what: &str) -> Result<Option<u32>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| format!("{what} must be a whole number."))
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// The form's text fields. Each is a GPUI entity: the text field keeps its
/// own state (text, cursor, selection), and the view reads it when needed.
struct Inputs {
    name: Entity<InputState>,
    command: Entity<InputState>,
    keyword: Entity<InputState>,
    emote: Entity<InputState>,
    amount: Entity<InputState>,
    min_count: Entity<InputState>,
    cooldown: Entity<InputState>,
    user_cooldown: Entity<InputState>,
    warudo_action: Entity<InputState>,
    source: Entity<InputState>,
    filter: Entity<InputState>,
    scene: Entity<InputState>,
    seconds: Entity<InputState>,
}

impl Inputs {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let mut input = |placeholder: &'static str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            name: input("e.g. bonk"),
            command: input("e.g. !bonk"),
            keyword: input("a word or phrase, e.g. hype"),
            emote: input("e.g. Kappa or :_hypeWave:"),
            amount: input("e.g. 69 or 4.20"),
            min_count: input("1"),
            cooldown: input("0"),
            user_cooldown: input("0"),
            warudo_action: input("the Action of an On WebSocket Action node"),
            source: input("as named in OBS"),
            filter: input("as named in OBS"),
            scene: input("as named in OBS"),
            seconds: input("empty: stays on"),
        }
    }

    /// The form as it is now: the texts from here, the choices from
    /// `picked` (which holds the current ones; its texts are ignored).
    fn read(&self, picked: &Form, cx: &App) -> Form {
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        Form {
            name: text(&self.name),
            command: text(&self.command),
            keyword: text(&self.keyword),
            emote: text(&self.emote),
            amount: text(&self.amount),
            min_count: text(&self.min_count),
            cooldown: text(&self.cooldown),
            user_cooldown: text(&self.user_cooldown),
            warudo_action: text(&self.warudo_action),
            source: text(&self.source),
            filter: text(&self.filter),
            scene: text(&self.scene),
            seconds: text(&self.seconds),
            ..picked.clone()
        }
    }

    fn fill(&self, form: &Form, window: &mut Window, cx: &mut App) {
        let mut set = |input: &Entity<InputState>, value: &str| {
            let value = value.to_string();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        };
        set(&self.name, &form.name);
        set(&self.command, &form.command);
        set(&self.keyword, &form.keyword);
        set(&self.emote, &form.emote);
        set(&self.amount, &form.amount);
        set(&self.min_count, &form.min_count);
        set(&self.cooldown, &form.cooldown);
        set(&self.user_cooldown, &form.user_cooldown);
        set(&self.warudo_action, &form.warudo_action);
        set(&self.source, &form.source);
        set(&self.filter, &form.filter);
        set(&self.scene, &form.scene);
        set(&self.seconds, &form.seconds);
    }
}

/// The connection settings' fields, for both programs.
struct OutputInputs {
    warudo_url: Entity<InputState>,
    warudo_queue: Entity<InputState>,
    obs_url: Entity<InputState>,
    obs_queue: Entity<InputState>,
    obs_password: Entity<InputState>,
}

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
        let settings = all_settings.reactions;
        let tips = all_settings.tips;
        let inputs = Inputs::new(window, cx);
        let tier_select =
            cx.new(|cx| SelectState::new(tier_items(&tips), Some(IndexPath::new(0)), window, cx));
        let mut field = |value: String, placeholder: &'static str, masked: bool| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
                    .default_value(value)
            })
        };
        let outputs = OutputInputs {
            warudo_url: field(settings.warudo.url.clone(), "ws://127.0.0.1:19190", false),
            warudo_queue: field(settings.warudo.queue_secs.to_string(), "0", false),
            obs_url: field(settings.obs.url.clone(), "ws://127.0.0.1:4455", false),
            obs_queue: field(settings.obs.queue_secs.to_string(), "0", false),
            obs_password: field(String::new(), "from OBS: Show Connect Info", true),
        };

        let subscriptions = vec![
            // Settings changed (a switch here, the pause in the main
            // window, new tip amounts in Settings): redraw.
            cx.observe_in(&config, window, |this, _, window, cx| {
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

        let mut view = Self {
            engine,
            config,
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
                    view.status = current;
                    cx.notify();
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

    // ---- the rule form ----

    fn rules(&self, cx: &App) -> Vec<Rule> {
        self.config.read(cx).settings().reactions.rules
    }

    fn form(&self, cx: &App) -> Form {
        self.inputs.read(&self.picked, cx)
    }

    /// Unsaved changes anywhere in the window: the rule form, the
    /// connection settings, a typed password not yet stored.
    fn unsaved(&self, cx: &App) -> bool {
        self.form(cx) != self.loaded || self.outputs_unsaved(cx)
    }

    fn outputs_unsaved(&self, cx: &App) -> bool {
        let settings = self.config.read(cx).settings().reactions;
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        // As a number: "030" is what's saved as 30.
        let queue = |input: &Entity<InputState>| number(&text(input), "").ok();
        let o = &self.outputs;
        text(&o.warudo_url) != settings.warudo.url
            || queue(&o.warudo_queue) != Some(settings.warudo.queue_secs)
            || text(&o.obs_url) != settings.obs.url
            || queue(&o.obs_queue) != Some(settings.obs.queue_secs)
            || !text(&o.obs_password).is_empty()
    }

    /// Shows the saved rule `name` in the form (`None`: a new, blank one),
    /// asking first if that would discard unsaved changes to the form.
    fn open_rule(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if name.is_some() && name == self.editing {
            return;
        }
        if self.form(cx) == self.loaded {
            self.load(name, window, cx);
            return;
        }
        let what = match &self.editing {
            Some(rule) => format!("Discard your changes to \"{rule}\"?"),
            None => "Discard the new rule?".into(),
        };
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            what,
            "They aren't saved yet.",
            "Discard",
            move |window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.load(name.clone(), window, cx));
                }
            },
        );
    }

    fn load(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let rule = name
            .as_ref()
            .and_then(|name| self.rules(cx).into_iter().find(|r| r.name == *name));
        let form = rule.as_ref().map_or_else(Form::blank, Form::of);
        self.inputs.fill(&form, window, cx);
        self.show_tier(form.min_tier, window, cx);
        self.editing = rule.map(|r| r.name);
        self.picked = form.clone();
        self.loaded = form;
        self.form_error = None;
        cx.notify();
    }

    /// Selects `tier` in the tier list (`None`: "Any amount").
    fn show_tier(&self, tier: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let index = IndexPath::new(tier.map_or(0, |t| t as usize));
        self.tier_select.update(cx, |select, cx| {
            select.set_selected_index(Some(index), window, cx);
        });
    }

    /// New tip settings: the tier list says the new amounts, and keeps the
    /// picked tier (its labels changed, so the list would drop it).
    fn relabel_tiers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tips = self.config.read(cx).settings().tips;
        if tips == self.tier_labels_for {
            return;
        }
        self.tier_select.update(cx, |select, cx| {
            select.set_items(tier_items(&tips), window, cx);
        });
        self.tier_labels_for = tips;
        self.show_tier(self.picked.min_tier, window, cx);
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rules = self.rules(cx);
        let saved = self
            .editing
            .as_ref()
            .and_then(|name| rules.iter().find(|r| r.name == *name));
        // A new rule starts switched on; an edited one keeps its switch.
        let enabled = saved.is_none_or(|r| r.enabled);
        let form = self.form(cx);
        let rule = match form.rule(enabled) {
            Ok(rule) => rule,
            Err(problem) => {
                self.form_error = Some(problem);
                cx.notify();
                return;
            }
        };
        let taken = rules
            .iter()
            .any(|r| r.name == rule.name && Some(&r.name) != self.editing.as_ref());
        if taken {
            self.form_error = Some(format!("There's already a rule called \"{}\".", rule.name));
            cx.notify();
            return;
        }

        // Replaced where it was (found by its saved name, so a rename keeps
        // its place), or added at the end.
        let original = self.editing.clone();
        let name = rule.name.clone();
        let saving = self.engine.edit_settings(move |s| {
            let rules = &mut s.reactions.rules;
            match original.and_then(|old| rules.iter().position(|r| r.name == old)) {
                Some(i) => rules[i] = rule,
                None => rules.push(rule),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = saving.await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(()) => {
                        // A rename: its last test was the old name's.
                        if let Some(old) = &view.editing
                            && *old != name
                        {
                            view.tests.remove(old);
                        }
                        view.editing = Some(name);
                        view.loaded = form;
                        view.form_error = None;
                    }
                    // The engine checks too: what it refuses isn't saved.
                    Err(e) => view.form_error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.editing.clone() else {
            return;
        };
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            format!("Delete \"{name}\"?"),
            "The rule is removed from the settings.",
            "Delete",
            move |window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.remove(name.clone(), window, cx));
                }
            },
        );
    }

    fn remove(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let gone = name.clone();
        let removing = self
            .engine
            .edit_settings(move |s| s.reactions.rules.retain(|r| r.name != gone));
        cx.spawn_in(window, async move |this, cx| {
            let result = removing.await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(()) => {
                    view.tests.remove(&name);
                    let first = view.rules(cx).first().map(|r| r.name.clone());
                    view.load(first, window, cx);
                }
                Err(e) => report(window, cx, format!("Couldn't delete the rule: {e:#}")),
            });
        })
        .detach();
    }

    // ---- the list ----

    /// The list's switch: applies at once, like a source's.
    fn set_enabled(&mut self, name: String, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let handle = window.window_handle();
        edit_settings(
            &self.engine,
            handle,
            move |s| {
                if let Some(rule) = s.reactions.rules.iter_mut().find(|r| r.name == name) {
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

    // ---- the programs ----

    fn save_output(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        let o = &self.outputs;
        let (url, queue) = match target {
            Target::Warudo => (&o.warudo_url, &o.warudo_queue),
            Target::Obs => (&o.obs_url, &o.obs_queue),
        };
        let url = url.read(cx).value().trim().to_string();
        let queue = queue.read(cx).value().to_string();
        let checked = match (url.starts_with("ws://"), number(&queue, "The waiting time")) {
            (false, _) => Err("The address starts with ws://".to_string()),
            (_, Err(e)) => Err(e),
            (_, Ok(secs)) if secs > MAX_QUEUE_SECS => {
                Err(format!("Waiting can be at most {MAX_QUEUE_SECS} seconds."))
            }
            (_, Ok(secs)) => Ok(secs),
        };
        let error = match target {
            Target::Warudo => &mut self.warudo_error,
            Target::Obs => &mut self.obs_error,
        };
        let queue_secs = match checked {
            Ok(secs) => secs,
            Err(problem) => {
                *error = Some(problem);
                cx.notify();
                return;
            }
        };
        *error = None;
        cx.notify();
        let handle = window.window_handle();
        edit_settings(
            &self.engine,
            handle,
            move |s| match target {
                Target::Warudo => s.reactions.warudo = WarudoSettings { url, queue_secs },
                Target::Obs => s.reactions.obs = ObsSettings { url, queue_secs },
            },
            cx,
        );
    }

    fn check_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let stored = engine.obs_password_stored().await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = &stored {
                    tracing::warn!("couldn't read the OBS password's state: {e:#}");
                }
                view.password_stored = stored.ok();
                cx.notify();
            });
        })
        .detach();
    }

    /// Stores the typed password (`store`), or removes the stored one.
    fn set_password(&mut self, store: bool, window: &mut Window, cx: &mut Context<Self>) {
        let password = store.then(|| self.outputs.obs_password.read(cx).value().to_string());
        if password.as_deref() == Some("") {
            self.obs_error = Some("Type the password first.".into());
            cx.notify();
            return;
        }
        self.obs_error = None;
        let engine = self.engine.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = engine.set_obs_password(password).await;
            let _ = this.update_in(cx, |view, window, cx| {
                match result {
                    Ok(()) => {
                        view.password_stored = Some(store);
                        // Out of the field once it's stored: the window
                        // keeps no copy.
                        view.outputs
                            .obs_password
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    Err(e) => view.obs_error = Some(format!("Couldn't store it: {e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
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

    /// The paid trigger: by tier (picked from the list) or by amount (at
    /// least or exactly).
    fn paid_fields(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let p = &self.picked;
        let by = segmented(
            cx,
            [(PaidBy::Tier, "Tier"), (PaidBy::Amount, "Amount")]
                .into_iter()
                .enumerate()
                .map(|(n, (by, label))| {
                    choice(("paid-by", n), label, by == p.paid_by).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.picked.paid_by = by;
                            cx.notify();
                        },
                    ))
                }),
        );
        let detail = match p.paid_by {
            PaidBy::Tier => labelled(
                "At least",
                Select::new(&self.tier_select).id("rule-tier").small(),
                cx,
            )
            .into_any_element(),
            PaidBy::Amount => {
                let compare = segmented(
                    cx,
                    [
                        (Compare::AtLeast, "At least"),
                        (Compare::Exactly, "Exactly"),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(n, (compare, label))| {
                        choice(("compare", n), label, compare == p.compare).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.picked.compare = compare;
                                cx.notify();
                            },
                        ))
                    }),
                );
                // What the number is compared with, so "69" isn't a
                // surprise on another platform.
                let hint = match self.config.read(cx).settings().tips.currency() {
                    Some(code) => {
                        format!("Tips and Super Chats in {code}, Twitch in bits. No conversion.")
                    }
                    None => "Tips as sent, Twitch in bits. Super Chats count once a tip \
                             currency is set (⚙ Settings → Paid messages)."
                        .into(),
                };
                v_flex()
                    .gap_2()
                    .child(labelled(
                        "",
                        h_flex().gap_2().items_center().child(compare).child(
                            div()
                                .w(px(110.))
                                .child(input(&self.inputs.amount, "rule-amount")),
                        ),
                        cx,
                    ))
                    .child(labelled(
                        "",
                        div().text_sm().text_color(muted).child(hint),
                        cx,
                    ))
                    .into_any_element()
            }
        };
        v_flex()
            .gap_2()
            .child(labelled("Paid", by, cx))
            .child(detail)
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let p = &self.picked;
        let i = &self.inputs;
        let title = match &self.editing {
            Some(name) => format!("Edit \"{name}\""),
            None => "New rule".into(),
        };

        let triggers = segmented(
            cx,
            TriggerKind::ALL.into_iter().enumerate().map(|(n, kind)| {
                choice(("trigger", n), kind.label(), kind == p.trigger).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.picked.trigger = kind;
                        cx.notify();
                    },
                ))
            }),
        );
        let trigger_field = match p.trigger {
            TriggerKind::Command => {
                labelled("Command", input(&i.command, "rule-command"), cx).into_any_element()
            }
            TriggerKind::Keyword => {
                labelled("Keyword", input(&i.keyword, "rule-keyword"), cx).into_any_element()
            }
            TriggerKind::Emote => {
                labelled("Emote", input(&i.emote, "rule-emote"), cx).into_any_element()
            }
            TriggerKind::Paid => self.paid_fields(cx).into_any_element(),
            TriggerKind::Membership => labelled(
                "",
                div()
                    .text_color(muted)
                    .child("New subscribers and members, resubs, milestones."),
                cx,
            )
            .into_any_element(),
            TriggerKind::Gift => {
                labelled("At least", input(&i.min_count, "rule-min-count"), cx).into_any_element()
            }
        };

        let roles = segmented(
            cx,
            ROLES.into_iter().enumerate().map(|(n, (role, label))| {
                choice(("role", n), label, role == p.role).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.picked.role = role;
                        cx.notify();
                    },
                ))
            }),
        );
        let platforms = h_flex()
            .gap_3()
            .children(
                PLATFORMS
                    .into_iter()
                    .enumerate()
                    .map(|(n, (platform, label))| {
                        Checkbox::new(("platform", n))
                            .label(label)
                            .checked(p.platforms.contains(&platform))
                            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                                let ticked = &mut this.picked.platforms;
                                ticked.retain(|p| *p != platform);
                                if *on {
                                    ticked.push(platform);
                                    // In the table's order, so ticking off and on
                                    // again isn't an unsaved change.
                                    ticked.sort_by_key(|p| {
                                        PLATFORMS.iter().position(|(q, _)| q == p)
                                    });
                                }
                                cx.notify();
                            }))
                    }),
            );
        let cooldowns = h_flex()
            .gap_2()
            .items_center()
            .child(div().w(px(64.)).child(input(&i.cooldown, "rule-cooldown")))
            .child(div().text_color(muted).child("s for everyone,"))
            .child(
                div()
                    .w(px(64.))
                    .child(input(&i.user_cooldown, "rule-user-cooldown")),
            )
            .child(div().text_color(muted).child("s per chatter"));

        let actions = segmented(
            cx,
            ActionKind::ALL.into_iter().enumerate().map(|(n, kind)| {
                choice(("action", n), kind.label(), kind == p.action).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.picked.action = kind;
                        cx.notify();
                    },
                ))
            }),
        );
        let seconds =
            || labelled("Seconds", input(&i.seconds, "rule-seconds"), cx).into_any_element();
        let action_fields: Vec<AnyElement> = match p.action {
            ActionKind::Warudo => vec![
                labelled("Action", input(&i.warudo_action, "rule-warudo-action"), cx)
                    .into_any_element(),
            ],
            ActionKind::ObsFilter => vec![
                labelled("Source", input(&i.source, "rule-source"), cx).into_any_element(),
                labelled("Filter", input(&i.filter, "rule-filter"), cx).into_any_element(),
                seconds(),
            ],
            ActionKind::ObsSource => vec![
                labelled("Scene", input(&i.scene, "rule-scene"), cx).into_any_element(),
                labelled("Source", input(&i.source, "rule-source"), cx).into_any_element(),
                seconds(),
            ],
            ActionKind::ObsMedia => {
                vec![labelled("Source", input(&i.source, "rule-source"), cx).into_any_element()]
            }
        };

        card(cx).flex_1().min_h_0().child(
            div()
                .id("rule-form")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().font_semibold().child(title))
                        .child(labelled("Name", input(&i.name, "rule-name"), cx))
                        .child(labelled("When", triggers, cx))
                        .child(trigger_field)
                        .child(labelled("Who", roles, cx))
                        .child(labelled("Where", platforms, cx))
                        .child(labelled("Cooldown", cooldowns, cx))
                        .child(labelled("Then", actions, cx))
                        .children(action_fields)
                        .children(
                            self.form_error
                                .clone()
                                .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Button::new("rule-save").label("Save").primary().on_click(
                                    cx.listener(|this, _, window, cx| this.save(window, cx)),
                                ))
                                .when(self.editing.is_some(), |row| {
                                    row.child(Button::new("rule-delete").label("Delete").on_click(
                                        cx.listener(|this, _, window, cx| this.delete(window, cx)),
                                    ))
                                }),
                        ),
                ),
        )
    }

    fn render_output(&self, target: Target, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let o = &self.outputs;
        let (url, queue, error, prefix) = match target {
            Target::Warudo => (&o.warudo_url, &o.warudo_queue, &self.warudo_error, "warudo"),
            Target::Obs => (&o.obs_url, &o.obs_queue, &self.obs_error, "obs"),
        };
        let output = self.status.outputs.iter().find(|o| o.target == target);
        let (health, summary) = match output {
            Some(output) => (Health::from(&output.state), output.summary()),
            None => (
                Health::Off,
                "not connected: no switched-on rule uses it".into(),
            ),
        };

        card(cx)
            .flex_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(light(health, cx))
                    .child(div().font_semibold().child(target.label()))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_sm()
                            .text_color(muted)
                            .child(summary),
                    ),
            )
            .child(labelled(
                "Address",
                Input::new(url).id(format!("{prefix}-url")),
                cx,
            ))
            .child(labelled(
                "Wait",
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(64.))
                            .child(Input::new(queue).id(format!("{prefix}-queue"))),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("s while it's away (0: drop)"),
                    ),
                cx,
            ))
            .when(target == Target::Obs, |card| {
                let stored = match self.password_stored {
                    Some(true) => "a password is stored",
                    Some(false) => "no password stored",
                    None => "…",
                };
                card.child(labelled(
                    "Password",
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&o.obs_password).id("obs-password")),
                        )
                        .child(
                            Button::new("obs-password-store")
                                .label("Store")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_password(true, window, cx)
                                })),
                        )
                        .child(
                            Button::new("obs-password-remove")
                                .label("Remove")
                                .small()
                                .disabled(self.password_stored != Some(true))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_password(false, window, cx)
                                })),
                        ),
                    cx,
                ))
                .child(div().text_sm().text_color(muted).child(format!(
                    "In your system's keyring, not the settings file: {stored}."
                )))
            })
            .children(
                error
                    .clone()
                    .map(|e| div().text_sm().text_color(danger).child(e)),
            )
            .child(
                Button::new(format!("{prefix}-save"))
                    .label("Save")
                    .primary()
                    .small()
                    .self_start()
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.save_output(target, window, cx)
                        }),
                    ),
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

/// The paid trigger's tier list: "Any amount", then every tier.
fn tier_items(tips: &TipSettings) -> Vec<SharedString> {
    std::iter::once("Any amount".to_string())
        .chain(paid::all().map(|tier| paid::tier_label(tier, tips)))
        .map(SharedString::from)
        .collect()
}

/// A text field with an id the tests can find it by.
fn input(state: &Entity<InputState>, id: &'static str) -> Input {
    Input::new(state).id(id)
}

/// One item of a [`segmented`] choice (filled when chosen, as in the main
/// window).
fn choice(id: impl Into<ElementId>, label: &'static str, selected: bool) -> Button {
    let button = Button::new(id).label(label).small().selected(selected);
    if selected {
        button.primary()
    } else {
        button.ghost()
    }
}

/// A rule in one line, for the list: "!bonk → Warudo throw_item".
fn summary(rule: &Rule) -> String {
    let when = match &rule.trigger {
        Trigger::Command { command } => command.clone(),
        Trigger::Keyword { keyword } => format!("\"{keyword}\""),
        Trigger::Emote { emote } => emote.clone(),
        Trigger::Paid { min_tier: None } => "paid".into(),
        Trigger::Paid {
            min_tier: Some(tier),
        } => format!("paid, tier {tier}+"),
        Trigger::PaidAmount {
            amount,
            compare: Compare::AtLeast,
        } => format!("paid, {amount}+"),
        Trigger::PaidAmount {
            amount,
            compare: Compare::Exactly,
        } => format!("paid, exactly {amount}"),
        Trigger::Membership => "membership".into(),
        Trigger::Gift { min_count: 1 } => "gift".into(),
        Trigger::Gift { min_count } => format!("{min_count}+ gifts"),
    };
    let secs = |seconds: &Option<u32>| seconds.map(|s| format!(" for {s} s")).unwrap_or_default();
    let then = match &rule.action {
        Action::Warudo { action } => format!("Warudo {action}"),
        Action::ObsFilter {
            source,
            filter,
            seconds,
        } => format!("{filter} on {source}{}", secs(seconds)),
        Action::ObsSource {
            source, seconds, ..
        } => format!("show {source}{}", secs(seconds)),
        Action::ObsMedia { source } => format!("play {source}"),
    };
    format!("{when} → {then}")
}

/// Asks a yes/no question; `on_ok` runs on yes, nothing on cancel.
fn confirm(
    window: &mut Window,
    cx: &mut App,
    title: String,
    description: &'static str,
    ok: &'static str,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    // The builder may run again on redraws (it's an `Fn`), so what it
    // hands on is shared, not moved: an `Rc`.
    let on_ok = std::rc::Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = on_ok.clone();
        alert
            .title(title.clone())
            .description(description)
            .ok_text(ok)
            .show_cancel(true)
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            })
    });
}

/// UI tests: the real window, headless, driven by simulated clicks and
/// typing (see `app_view::tests` for how these work).
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use chat_core::tiers::Amount;
    use chat_engine::{
        Action, Compare, ConfigFile, Engine, EngineConfig, Guards, Rule, SecretStore, Settings,
        Trigger,
    };
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowHandle};

    use super::{ReactionsWindow, Tested, summary};
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
        config.settings.reactions.rules = rules.iter().map(|name| rule(name)).collect();
        // Port 1, where nothing listens: Warudo's usual port may have the
        // real Warudo behind it on a developer's machine, and then a test
        // press would really reach it.
        config.settings.reactions.warudo.url = "ws://127.0.0.1:1".into();
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

    /// Replaces a field's text (typing would add to it).
    fn set_name(cx: &mut TestAppContext, ui: &Ui, name: &str) {
        let view = ui.view.clone();
        let name = name.to_string();
        cx.update_window(ui.window.into(), |_, window, cx| {
            let input = view.read(cx).inputs.name.clone();
            input.update(cx, |input, cx| input.set_value(name, window, cx));
        })
        .unwrap();
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
        let saved = &settings(&ui).reactions.rules[0];
        assert_eq!(saved.name, "chipmunk");
        assert!(saved.enabled);
        assert_eq!(saved.trigger, Trigger::Gift { min_count: 10 });
        assert_eq!(saved.guards.min_role, chat_engine::Role::Moderator);
        assert_eq!(
            saved.guards.platforms,
            [
                chat_core::ChatPlatform::Twitch,
                chat_core::ChatPlatform::Picarto,
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
        let file = ConfigFile::load(&ui.dir.join("config.toml")).unwrap();
        assert_eq!(file.reactions.rules.len(), 1);
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
        let saved = settings(&ui).reactions.rules[0].clone();
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
            settings(&ui).reactions.rules[0].trigger == Trigger::Paid { min_tier: Some(5) }
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
            .edit_settings(|s| s.tips.currency = "EUR".into());
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
        run_until(cx, |_| !settings(&ui).reactions.rules[1].enabled);
        assert!(settings(&ui).reactions.rules[0].enabled);
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
        run_until(cx, |_| settings(&ui).reactions.warudo.queue_secs == 30);
        assert_eq!(
            settings(&ui).reactions.warudo.url,
            "ws://192.168.1.20:19190"
        );
        assert!(!ui.view.read_with(cx, |view, cx| view.unsaved(cx)));
    }
}
