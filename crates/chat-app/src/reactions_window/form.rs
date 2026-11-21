//! The rule editor form in the Reactions window: form state, validation,
//! rule save/delete, and rendering.

use chat_core::ChatPlatform;
use chat_core::tiers::{Amount, TipSettings};
use chat_engine::{Action, Compare, Crowd, Guards, Role, Rule, Trigger};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::Select;
use gpui_kit::component::{
    ActiveTheme, IndexPath, Sizable, StyledExt, h_flex, scroll::ScrollableElement, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::ReactionsWindow;
use crate::paid;
use crate::ui::{card, choice, confirm, labelled, report, segmented};

/// What fires a rule, as picked in the form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TriggerKind {
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
pub(super) enum PaidBy {
    /// The tier (`Trigger::Paid`).
    Tier,
    /// The amount as a number (`Trigger::PaidAmount`).
    Amount,
}

/// What a rule does, as picked in the form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActionKind {
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

/// Everything the form holds, as typed and picked. Compared with what was
/// loaded to tell unsaved changes, and turned into a [`Rule`] on Save.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Form {
    pub(super) name: String,
    pub(super) trigger: TriggerKind,
    pub(super) command: String,
    pub(super) keyword: String,
    pub(super) emote: String,
    pub(super) paid_by: PaidBy,
    /// Picked from the tier list; `None`: any amount.
    pub(super) min_tier: Option<u32>,
    pub(super) amount: String,
    pub(super) compare: Compare,
    pub(super) min_count: String,
    pub(super) role: Role,
    /// The ticked ones; all ticked is the same as no platform guard.
    pub(super) platforms: Vec<ChatPlatform>,
    /// How many different chatters; empty: no crowd.
    pub(super) crowd: String,
    pub(super) crowd_secs: String,
    pub(super) cooldown: String,
    pub(super) user_cooldown: String,
    pub(super) action: ActionKind,
    pub(super) warudo_action: String,
    pub(super) source: String,
    pub(super) filter: String,
    pub(super) scene: String,
    pub(super) seconds: String,
}

impl Form {
    pub(super) fn blank() -> Self {
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
            platforms: ChatPlatform::ALL.to_vec(),
            crowd: String::new(),
            crowd_secs: String::new(),
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

    pub(super) fn of(rule: &Rule) -> Self {
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
            form.platforms = ChatPlatform::ALL
                .into_iter()
                .filter(|p| guards.platforms.contains(p))
                .collect();
        }
        if let Some(crowd) = guards.crowd {
            form.crowd = crowd.chatters.to_string();
            form.crowd_secs = crowd.within_secs.to_string();
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
    pub(super) fn rule(&self, enabled: bool) -> Result<Rule, String> {
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
        let platforms = if self.platforms.len() == ChatPlatform::ALL.len() {
            Vec::new() // all of them: no guard, so platforms added later count too
        } else {
            self.platforms.clone()
        };
        // Both numbers or neither: one alone is a half-typed crowd, not
        // "no crowd".
        let crowd = match (
            optional_number(&self.crowd, "The crowd")?,
            optional_number(&self.crowd_secs, "The crowd's seconds")?,
        ) {
            (None, None) => None,
            (Some(chatters), Some(within_secs)) => Some(Crowd {
                chatters: chatters as usize,
                within_secs,
            }),
            (Some(_), None) => {
                return Err("Enter within how many seconds the crowd must gather.".into());
            }
            (None, Some(_)) => {
                return Err("Enter how many different chatters make the crowd.".into());
            }
        };
        let guards = Guards {
            min_role: self.role,
            platforms,
            crowd,
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

/// A whole number; empty is 0.
pub(super) fn number(value: &str, what: &str) -> Result<u32, String> {
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
pub(super) struct Inputs {
    pub(super) name: Entity<InputState>,
    pub(super) command: Entity<InputState>,
    pub(super) keyword: Entity<InputState>,
    pub(super) emote: Entity<InputState>,
    pub(super) amount: Entity<InputState>,
    pub(super) min_count: Entity<InputState>,
    pub(super) crowd: Entity<InputState>,
    pub(super) crowd_secs: Entity<InputState>,
    pub(super) cooldown: Entity<InputState>,
    pub(super) user_cooldown: Entity<InputState>,
    pub(super) warudo_action: Entity<InputState>,
    pub(super) source: Entity<InputState>,
    pub(super) filter: Entity<InputState>,
    pub(super) scene: Entity<InputState>,
    pub(super) seconds: Entity<InputState>,
}

impl Inputs {
    pub(super) fn new(window: &mut Window, cx: &mut App) -> Self {
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
            crowd: input("off"),
            crowd_secs: input(""),
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
    pub(super) fn read(&self, picked: &Form, cx: &App) -> Form {
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        Form {
            name: text(&self.name),
            command: text(&self.command),
            keyword: text(&self.keyword),
            emote: text(&self.emote),
            amount: text(&self.amount),
            min_count: text(&self.min_count),
            crowd: text(&self.crowd),
            crowd_secs: text(&self.crowd_secs),
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

    pub(super) fn fill(&self, form: &Form, window: &mut Window, cx: &mut App) {
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
        set(&self.crowd, &form.crowd);
        set(&self.crowd_secs, &form.crowd_secs);
        set(&self.cooldown, &form.cooldown);
        set(&self.user_cooldown, &form.user_cooldown);
        set(&self.warudo_action, &form.warudo_action);
        set(&self.source, &form.source);
        set(&self.filter, &form.filter);
        set(&self.scene, &form.scene);
        set(&self.seconds, &form.seconds);
    }
}

/// The paid trigger's tier list: "Any amount", then every tier.
pub(super) fn tier_items(tips: &TipSettings) -> Vec<SharedString> {
    std::iter::once("Any amount".to_string())
        .chain(paid::all().map(|tier| paid::tier_label(tier, tips)))
        .map(SharedString::from)
        .collect()
}

/// A text field with an id the tests can find it by.
fn input(state: &Entity<InputState>, id: &'static str) -> Input {
    Input::new(state).id(id)
}

/// A rule in one line, for the list: "!bonk → Warudo throw_item".
pub(super) fn summary(rule: &Rule) -> String {
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
    let when = match rule.guards.crowd {
        Some(crowd) => format!(
            "{when}, {} chatters in {} s",
            crowd.chatters, crowd.within_secs
        ),
        None => when,
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

impl ReactionsWindow {
    /// Shows the saved rule `name` in the form (`None`: a new, blank one),
    /// asking first if that would discard unsaved changes to the form.
    pub(super) fn open_rule(
        &mut self,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn load(
        &mut self,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
    pub(super) fn show_tier(&self, tier: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let index = IndexPath::new(tier.map_or(0, |t| t as usize));
        self.tier_select.update(cx, |select, cx| {
            select.set_selected_index(Some(index), window, cx);
        });
    }

    /// New tip settings: the tier list says the new amounts, and keeps the
    /// picked tier (its labels changed, so the list would drop it).
    pub(super) fn relabel_tiers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tips = self.config.read(cx).settings().profile.tips;
        if tips == self.tier_labels_for {
            return;
        }
        self.tier_select.update(cx, |select, cx| {
            select.set_items(tier_items(&tips), window, cx);
        });
        self.tier_labels_for = tips;
        self.show_tier(self.picked.min_tier, window, cx);
    }

    pub(super) fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            let rules = &mut s.profile.reactions.rules;
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

    pub(super) fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            .edit_settings(move |s| s.profile.reactions.rules.retain(|r| r.name != gone));
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
                let hint = match self.config.read(cx).settings().profile.tips.currency() {
                    Some(code) => {
                        format!(
                            "Tips and Super Chats in {code}, Twitch in bits, RPlay in coins. \
                             No conversion."
                        )
                    }
                    None => "Tips as sent, Twitch in bits, RPlay in coins. Super Chats count \
                             once a tip currency is set (⚙ Settings → Paid messages)."
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

    pub(super) fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                ChatPlatform::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(n, platform)| {
                        Checkbox::new(("platform", n))
                            .label(platform.label())
                            .checked(p.platforms.contains(&platform))
                            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                                let ticked = &mut this.picked.platforms;
                                ticked.retain(|p| *p != platform);
                                if *on {
                                    ticked.push(platform);
                                    // In canonical order, so ticking off and on
                                    // again isn't an unsaved change.
                                    ticked.sort_by_key(|p| {
                                        ChatPlatform::ALL.iter().position(|q| q == p)
                                    });
                                }
                                cx.notify();
                            }))
                    }),
            );
        let crowd = h_flex()
            .gap_2()
            .items_center()
            .child(div().w(px(64.)).child(input(&i.crowd, "rule-crowd")))
            .child(div().text_color(muted).child("different chatters within"))
            .child(
                div()
                    .w(px(64.))
                    .child(input(&i.crowd_secs, "rule-crowd-secs")),
            )
            .child(div().text_color(muted).child("s"));
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
                        .child(labelled("Crowd", crowd, cx))
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
}
