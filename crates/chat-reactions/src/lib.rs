//! Chat reactions: chat makes things happen in other programs (an item
//! thrown at the model in Warudo, a voice filter in OBS for 30 seconds, …).
//!
//! ```text
//!                 ┌──────────── reactor (one task) ─────────────┐
//! Hub ─subscribe─→│ per live message, per rule:                 │
//! (history        │   trigger matches? → guards pass? → fire    │──try_send──→ Warudo output
//!  dropped)       │ (rules.rs: pure logic, cooldown bookkeeping) │──try_send──→ OBS output
//!                 └──────────────────────────────────────────────┘   (each a WebSocket client
//!                                                                     that reconnects itself)
//! ```
//!
//! The streamer writes the rules ([`ReactionSettings`], `[reactions]` in
//! `config.toml`). Chat is untrusted: a message only ever *picks* a rule,
//! and an output only ever sends what the rule's action says. No chat text
//! reaches another program.

use std::collections::HashSet;

mod obs;
mod output;
mod reactor;
mod rules;
mod warudo;

pub use reactor::{OutputState, OutputStatus, ReactionRequest, ReactionStatus, RuleStatus, run};
pub use rules::{
    Action, Compare, Crowd, Guards, MAX_CROWD, MAX_CROWD_SECS, MAX_EFFECT_SECS, Role, Rule, Target,
    Trigger,
};

/// Warudo's own default: its "On WebSocket Action" blueprint node listens
/// here.
pub const DEFAULT_WARUDO_URL: &str = "ws://127.0.0.1:19190";

/// obs-websocket's default (OBS: Tools → WebSocket Server Settings).
pub const DEFAULT_OBS_URL: &str = "ws://127.0.0.1:4455";

/// The OBS WebSocket password's name in the secret store
/// (`chat_core::secrets`): never in `config.toml`, since it gives full
/// control over OBS.
pub const OBS_PASSWORD: &str = "obs-websocket";

/// The longest `queue_secs`: an effect that shows up minutes after the
/// message that caused it makes no sense any more.
pub const MAX_QUEUE_SECS: u32 = 300;

/// Everything under `[reactions]` in `config.toml`.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReactionSettings {
    /// Switches every rule off at once (e.g. for a serious part of the
    /// stream); outputs stay connected.
    pub paused: bool,
    pub warudo: WarudoSettings,
    pub obs: ObsSettings,
    /// In the order they're checked. Every matching rule fires, not just
    /// the first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
}

/// Where Warudo is, and what happens while it isn't reachable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WarudoSettings {
    /// `ws://<host>:<port>`; another computer's address in a two-PC setup.
    pub url: String,
    /// How long a reaction may wait for Warudo to (re)connect, in seconds.
    /// 0: reactions fired while Warudo is unreachable are dropped.
    pub queue_secs: u32,
}

impl Default for WarudoSettings {
    fn default() -> Self {
        Self {
            url: DEFAULT_WARUDO_URL.into(),
            queue_secs: 0,
        }
    }
}

/// Where OBS is, and what happens while it isn't reachable. The password
/// isn't here: it's kept with the other secrets ([`OBS_PASSWORD`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObsSettings {
    /// `ws://<host>:<port>`, as shown in OBS's WebSocket Server Settings.
    pub url: String,
    /// How long a reaction may wait for OBS to (re)connect, in seconds.
    /// 0: reactions fired while OBS is unreachable are dropped.
    pub queue_secs: u32,
}

impl Default for ObsSettings {
    fn default() -> Self {
        Self {
            url: DEFAULT_OBS_URL.into(),
            queue_secs: 0,
        }
    }
}

/// See [`ReactionSettings::check`]: the settings every output has.
fn check_output(name: &str, url: &str, queue_secs: u32) -> Result<(), String> {
    if !url.starts_with("ws://") {
        return Err(format!(
            "reactions: the {name} url must start with ws:// (got \"{url}\")"
        ));
    }
    if queue_secs > MAX_QUEUE_SECS {
        return Err(format!(
            "reactions: {name}'s queue_secs can be at most {MAX_QUEUE_SECS}"
        ));
    }
    Ok(())
}

impl ReactionSettings {
    /// Finds what would make a rule silently never fire, or fire for the
    /// wrong reason: a typo here should be an error, like an unknown key.
    pub fn check(&self) -> Result<(), String> {
        check_output("Warudo", &self.warudo.url, self.warudo.queue_secs)?;
        check_output("OBS", &self.obs.url, self.obs.queue_secs)?;
        // Cooldowns are remembered by rule name: two rules with one name
        // would share them.
        let mut names = HashSet::new();
        for rule in &self.rules {
            if rule.name.trim().is_empty() {
                return Err("reactions: every rule needs a name".into());
            }
            if !names.insert(rule.name.as_str()) {
                return Err(format!(
                    "reactions: two rules are called \"{}\"; names must be unique",
                    rule.name
                ));
            }
            rule.check()
                .map_err(|e| format!("reactions: rule \"{}\": {e}", rule.name))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::tiers::Amount;

    fn parse(text: &str) -> Result<ReactionSettings, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn a_full_section_parses() {
        let settings = parse(
            r#"
            paused = true
            warudo = { url = "ws://192.168.1.20:19190", queue_secs = 30 }
            obs = { url = "ws://127.0.0.1:4456", queue_secs = 10 }

            [[rules]]
            name = "bonk"
            trigger = { type = "command", command = "!bonk" }
            guards = { min_role = "member", platforms = ["twitch", "youtube"], cooldown_secs = 10, user_cooldown_secs = 60 }
            action = { target = "warudo", action = "throw_item" }

            [[rules]]
            name = "big super chat"
            enabled = false
            trigger = { type = "paid", min_tier = 4 }
            action = { target = "warudo", action = "dance" }

            [[rules]]
            name = "chipmunk"
            trigger = { type = "gift", min_count = 5 }
            action = { target = "obs_filter", source = "Mic", filter = "Chipmunk", seconds = 30 }

            [[rules]]
            name = "confetti"
            trigger = { type = "membership" }
            action = { target = "obs_source", scene = "Main", source = "Confetti" }

            [[rules]]
            name = "airhorn"
            trigger = { type = "command", command = "!horn" }
            action = { target = "obs_media", source = "Airhorn" }

            [[rules]]
            name = "nice"
            trigger = { type = "paid_amount", amount = 69, compare = "exactly" }
            guards = { platforms = ["streamelements"] }
            action = { target = "obs_media", source = "Nice" }

            [[rules]]
            name = "big tip"
            trigger = { type = "paid_amount", amount = 50.5 }
            action = { target = "obs_media", source = "Wow" }

            [[rules]]
            name = "respects"
            trigger = { type = "keyword", keyword = "F" }
            guards = { crowd = { chatters = 10, within_secs = 30 }, cooldown_secs = 120 }
            action = { target = "obs_source", scene = "Main", source = "Candles", seconds = 10 }
            "#,
        )
        .unwrap();
        assert_eq!(
            settings.rules[7].guards,
            Guards {
                crowd: Some(Crowd {
                    chatters: 10,
                    within_secs: 30,
                }),
                cooldown_secs: 120,
                ..Guards::default()
            }
        );
        assert_eq!(
            settings.rules[0].guards.crowd, None,
            "no crowd: one will do"
        );
        assert!(settings.paused);
        assert_eq!(settings.warudo.url, "ws://192.168.1.20:19190");
        assert_eq!(settings.warudo.queue_secs, 30);
        let bonk = &settings.rules[0];
        assert!(bonk.enabled, "on unless switched off");
        assert_eq!(
            bonk.trigger,
            Trigger::Command {
                command: "!bonk".into()
            }
        );
        assert_eq!(
            bonk.guards.min_role,
            Role::Subscriber,
            "member = subscriber"
        );
        assert_eq!(bonk.guards.user_cooldown_secs, 60);
        assert_eq!(
            bonk.action,
            Action::Warudo {
                action: "throw_item".into()
            }
        );
        let paid = &settings.rules[1];
        assert!(!paid.enabled);
        assert_eq!(paid.guards, Guards::default(), "no guards: anyone, anytime");
        assert_eq!(paid.trigger, Trigger::Paid { min_tier: Some(4) });
        assert_eq!(
            settings.rules[5].trigger,
            Trigger::PaidAmount {
                amount: Amount::whole(69),
                compare: Compare::Exactly,
            }
        );
        assert_eq!(
            settings.rules[6].trigger,
            Trigger::PaidAmount {
                amount: Amount::of(50.5).unwrap(),
                compare: Compare::AtLeast,
            },
            "at least, unless said otherwise"
        );
        assert_eq!(settings.obs.url, "ws://127.0.0.1:4456");
        assert_eq!(settings.obs.queue_secs, 10);
        assert_eq!(
            settings.rules[2].action,
            Action::ObsFilter {
                source: "Mic".into(),
                filter: "Chipmunk".into(),
                seconds: Some(30),
            }
        );
        assert_eq!(
            settings.rules[3].action,
            Action::ObsSource {
                scene: "Main".into(),
                source: "Confetti".into(),
                seconds: None,
            },
            "no seconds: stays on"
        );
        assert_eq!(
            settings.rules[4].action,
            Action::ObsMedia {
                source: "Airhorn".into()
            }
        );
        assert_eq!(settings.rules[4].action.target(), Target::Obs);
        assert_eq!(settings.check(), Ok(()));
    }

    #[test]
    fn missing_parts_take_defaults() {
        let settings = parse("").unwrap();
        assert_eq!(settings, ReactionSettings::default());
        assert_eq!(settings.warudo.url, DEFAULT_WARUDO_URL);
        assert_eq!(settings.warudo.queue_secs, 0, "dropped unless asked for");
        assert_eq!(settings.obs.url, DEFAULT_OBS_URL);
        assert_eq!(settings.obs.queue_secs, 0);
        assert!(!settings.paused);
        assert_eq!(settings.check(), Ok(()));
    }

    #[test]
    fn typos_are_errors() {
        for text in [
            "pasued = true",
            "[warudo]\nulr = \"ws://x\"",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"comand\", command = \"!a\" }\naction = { target = \"warudo\", action = \"a\" }",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"command\", comand = \"!a\" }\naction = { target = \"warudo\", action = \"a\" }",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"membership\" }\nguards = { cooldown = 5 }\naction = { target = \"warudo\", action = \"a\" }",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"membership\" }\nguards = { min_role = \"mod\" }\naction = { target = \"warudo\", action = \"a\" }",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"membership\" }\nguards = { crowd = { chatters = 3, within = 30 } }\naction = { target = \"warudo\", action = \"a\" }",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"membership\" }\nguards = { crowd = { chatters = 3 } }\naction = { target = \"warudo\", action = \"a\" }",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"membership\" }\naction = { target = \"warduo\", action = \"a\" }",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"membership\" }\naction = { target = \"obs_filter\", source = \"Mic\", filtr = \"x\" }",
            "[[rules]]\nname = \"a\"\ntrigger = { type = \"membership\" }\naction = { target = \"obs_media\", source = \"Mic\", seconds = 3 }",
            "obs = { password = \"hunter2\" }",
        ] {
            assert!(parse(text).is_err(), "accepted: {text}");
        }
    }

    #[test]
    fn check_finds_rules_that_cant_work() {
        let rule = |name: &str, trigger: Trigger, action: &str| Rule {
            name: name.into(),
            enabled: true,
            trigger,
            guards: Guards::default(),
            action: Action::Warudo {
                action: action.into(),
            },
        };
        let command = |c: &str| Trigger::Command { command: c.into() };
        let obs = |action| Rule {
            action,
            ..rule("obs", command("!x"), "x")
        };
        let with = |rules: Vec<Rule>| ReactionSettings {
            rules,
            ..ReactionSettings::default()
        };

        let crowd = |chatters, within_secs, user_cooldown_secs| Rule {
            guards: Guards {
                crowd: Some(Crowd {
                    chatters,
                    within_secs,
                }),
                user_cooldown_secs,
                ..Guards::default()
            },
            ..rule("crowd", command("F"), "x")
        };

        assert_eq!(with(vec![rule("a", command("!a"), "x")]).check(), Ok(()));
        assert_eq!(with(vec![crowd(2, 1, 0)]).check(), Ok(()));
        assert_eq!(
            with(vec![crowd(MAX_CROWD, MAX_CROWD_SECS, 0)]).check(),
            Ok(())
        );
        let bad = [
            // A crowd of one, or too big or too long to make sense.
            with(vec![crowd(1, 30, 0)]),
            with(vec![crowd(MAX_CROWD + 1, 30, 0)]),
            with(vec![crowd(10, 0, 0)]),
            with(vec![crowd(10, MAX_CROWD_SECS + 1, 0)]),
            // Decided 2026-11-13: a crowd has no per-chatter cooldown.
            with(vec![crowd(10, 30, 60)]),
            with(vec![rule(" ", command("!a"), "x")]),
            with(vec![
                rule("a", command("!a"), "x"),
                rule("a", command("!b"), "y"),
            ]),
            with(vec![rule("a", command(""), "x")]),
            with(vec![rule("a", command("!a b"), "x")]),
            with(vec![rule(
                "a",
                Trigger::Keyword {
                    keyword: "  ".into(),
                },
                "x",
            )]),
            with(vec![rule("a", Trigger::Emote { emote: "".into() }, "x")]),
            with(vec![rule("a", command("!a"), "")]),
            with(vec![rule("a", Trigger::Gift { min_count: 0 }, "x")]),
            // A tier no platform reaches (typed as an amount, once).
            with(vec![rule("a", Trigger::Paid { min_tier: Some(20) }, "x")]),
            with(vec![rule("a", Trigger::Paid { min_tier: Some(0) }, "x")]),
            with(vec![rule(
                "a",
                Trigger::PaidAmount {
                    amount: Amount::default(),
                    compare: Compare::Exactly,
                },
                "x",
            )]),
            with(vec![obs(Action::ObsFilter {
                source: "Mic".into(),
                filter: " ".into(),
                seconds: None,
            })]),
            with(vec![obs(Action::ObsSource {
                scene: "".into(),
                source: "Confetti".into(),
                seconds: None,
            })]),
            with(vec![obs(Action::ObsMedia { source: "".into() })]),
            with(vec![obs(Action::ObsFilter {
                source: "Mic".into(),
                filter: "Chipmunk".into(),
                seconds: Some(0),
            })]),
            with(vec![obs(Action::ObsFilter {
                source: "Mic".into(),
                filter: "Chipmunk".into(),
                seconds: Some(MAX_EFFECT_SECS + 1),
            })]),
            ReactionSettings {
                obs: ObsSettings {
                    url: "localhost:4455".into(),
                    queue_secs: 0,
                },
                ..ReactionSettings::default()
            },
            ReactionSettings {
                warudo: WarudoSettings {
                    url: "http://127.0.0.1:19190".into(),
                    queue_secs: 0,
                },
                ..ReactionSettings::default()
            },
            ReactionSettings {
                warudo: WarudoSettings {
                    queue_secs: MAX_QUEUE_SECS + 1,
                    ..WarudoSettings::default()
                },
                ..ReactionSettings::default()
            },
        ];
        for settings in bad {
            assert!(settings.check().is_err(), "accepted: {settings:?}");
        }
    }

    #[test]
    fn written_settings_read_back_the_same() {
        let settings = parse(
            r#"
            [[rules]]
            name = "bonk"
            trigger = { type = "command", command = "!bonk" }
            guards = { min_role = "moderator", cooldown_secs = 10 }
            action = { target = "warudo", action = "throw_item" }

            [[rules]]
            name = "gift"
            enabled = false
            trigger = { type = "gift", min_count = 5 }
            action = { target = "warudo", action = "confetti" }

            [[rules]]
            name = "respects"
            trigger = { type = "keyword", keyword = "F" }
            guards = { crowd = { chatters = 10, within_secs = 30 } }
            action = { target = "warudo", action = "candles" }
            "#,
        )
        .unwrap();
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(parse(&text).unwrap(), settings, "{text}");
        assert_eq!(
            text.matches("enabled").count(),
            1,
            "only written when off:\n{text}"
        );
        assert_eq!(
            text.matches("[rules.guards]").count(),
            2,
            "no guards aren't written:\n{text}"
        );
        assert_eq!(
            text.matches("[rules.guards.crowd]").count(),
            1,
            "no crowd isn't written:\n{text}"
        );
    }
}
