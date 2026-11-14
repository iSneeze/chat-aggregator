//! Which messages fire which reactions: a rule's trigger, its guards, and
//! the cooldown bookkeeping.
//!
//! Pure logic: no network, and no clock of its own. The time comes in as a
//! parameter (`now`), so a test can say "61 seconds later" without waiting
//! 61 seconds.

use std::collections::HashMap;
use std::time::Duration;

use chat_core::tiers::{Amount, TOP_TIER};
use chat_core::{ChatMessage, ChatPlatform, MessageKind};
use tokio::time::Instant;

use crate::ReactionSettings;

/// One `[[reactions.rules]]` block: when (trigger), who and how often
/// (guards), and what happens (action).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Shown in logs and the app. Cooldowns are remembered by name, so
    /// editing a rule keeps its cooldowns running; names are unique.
    pub name: String,
    /// Switched off: kept in the list, never fires. Only written when
    /// `false`, like a source's.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub enabled: bool,
    pub trigger: Trigger,
    #[serde(default, skip_serializing_if = "Guards::is_none")]
    pub guards: Guards,
    pub action: Action,
}

fn on() -> bool {
    true
}

// serde passes the field by reference, hence `&bool`.
fn is_on(enabled: &bool) -> bool {
    *enabled
}

/// Which messages a rule reacts to.
///
/// "Internally tagged" (`type`), like the sources: in `config.toml` that's
/// `trigger = { type = "command", command = "!bonk" }`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    /// A message whose first word is this, e.g. `!bonk`, in any case.
    Command { command: String },
    /// A message with this word or phrase in it, as whole words, in any
    /// case: `hype` matches "HYPE!" but not "hyperion".
    Keyword { keyword: String },
    /// A message with this emote in it, e.g. `Kappa` or `:_hypeWave:`
    /// (exact case: Twitch tells emotes apart by it). Only real emotes
    /// count, as the platform marked them, not the code typed as text.
    Emote { emote: String },
    /// A paid message (bits, Super Chat, Super Sticker, tip), of at least
    /// this tier if given: 1 to 7, on the scale every platform shares (see
    /// `chat_core::tiers`), picked from a list in the app.
    Paid {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_tier: Option<u32>,
    },
    /// A paid message of this amount, compared as a number in the
    /// platform's unit (`MessageKind::Donation`'s `value`): bits on Twitch,
    /// coins on RPlay, money otherwise. Nothing is converted: money counts
    /// only in the tip currency (`[tips]`), so a ¥69 Super Chat doesn't play
    /// the $69 clip. Bits and coins both count as they are, so "500" means
    /// 500 of either (a coin is worth about 5 bits); a rule for one of them
    /// or for tips alone adds a platform guard.
    PaidAmount {
        amount: Amount,
        #[serde(default)]
        compare: Compare,
    },
    /// A new subscriber or member, a resub or a milestone.
    Membership,
    /// Gifted subs or memberships, at least this many at once.
    Gift {
        #[serde(default = "one")]
        min_count: usize,
    },
}

fn one() -> usize {
    1
}

/// How [`Trigger::PaidAmount`] compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compare {
    /// The amount or more ("50+", as tiers count: a 50 tip is in).
    #[default]
    AtLeast,
    /// Just this amount, to the cent: 69 and 69.00, not 69.50.
    Exactly,
}

/// Who may fire a rule, how many must, and how often. A struct rather than
/// a list of guards: each guard exists once, and "all of them must pass"
/// needs no explaining. A list could hold two contradicting role guards.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Guards {
    /// At least this role (see [`Role`]).
    pub min_role: Role,
    /// Only messages from these platforms; empty: all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<ChatPlatform>,
    /// Fires only once enough different chatters triggered it (see
    /// [`Crowd`]); `None`: the first one does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crowd: Option<Crowd>,
    /// After firing, the rule rests this long, whoever triggers it.
    pub cooldown_secs: u32,
    /// After firing for a chatter, the rule rests this long for them.
    pub user_cooldown_secs: u32,
}

/// "10 different chatters within 30 seconds": a rule for the chat as a
/// whole. One chatter repeating it counts once, so nobody fires it alone,
/// and chatters from every platform add up.
///
/// One struct in an `Option` rather than two numbers with "0 is off": the
/// two only mean something together, and this way a crowd can't exist
/// with one of them missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Crowd {
    /// How many different chatters, from 2 to [`MAX_CROWD`].
    pub chatters: usize,
    /// Within how many seconds, counted back from the latest one: 1 to
    /// [`MAX_CROWD_SECS`].
    pub within_secs: u32,
}

/// The biggest crowd. More is almost certainly a typo, and it also caps
/// how many chatters a rule has to remember while one gathers.
pub const MAX_CROWD: usize = 1000;

/// The longest a crowd may take to gather: an hour, like effects.
pub const MAX_CROWD_SECS: u32 = 3600;

impl Guards {
    /// No guards at all: not written to the file.
    fn is_none(&self) -> bool {
        *self == Self::default()
    }

    /// The guards that don't depend on time.
    fn admit(&self, platform: ChatPlatform, role: Role) -> bool {
        role >= self.min_role && (self.platforms.is_empty() || self.platforms.contains(&platform))
    }
}

/// A chatter's standing, across platforms. Ordered from least to most, so a
/// guard is simply `role >= min_role` (`PartialOrd`/`Ord` derived on an
/// enum compare by declaration order).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Everyone,
    /// Twitch subscriber (or founder), YouTube member, Picarto subscriber.
    #[serde(alias = "member")]
    Subscriber,
    /// Twitch VIP.
    Vip,
    Moderator,
    /// The channel's owner.
    Broadcaster,
}

impl Role {
    /// The highest role among the author's badges. Platforms name them
    /// differently (Twitch `broadcaster`, YouTube `owner`), so the badges are
    /// translated here, in one place.
    pub fn of(msg: &ChatMessage) -> Role {
        msg.author
            .badges
            .iter()
            .map(|badge| match badge.as_str() {
                "broadcaster" | "owner" => Role::Broadcaster,
                "moderator" | "lead_moderator" => Role::Moderator,
                "vip" => Role::Vip,
                "subscriber" | "founder" | "member" => Role::Subscriber,
                _ => Role::Everyone,
            })
            .max()
            .unwrap_or(Role::Everyone)
    }
}

/// What a rule does when it fires. Tagged by `target`:
/// `action = { target = "warudo", action = "throw_item" }`.
///
/// Every value here comes from the streamer's settings. Chat never fills in
/// a part of an action: that's what keeps a hostile message from making
/// another program do something the streamer didn't set up.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Fires Warudo's "On WebSocket Action" blueprint nodes whose Action is
    /// this name. What happens then is up to the streamer's blueprint.
    Warudo { action: String },
    /// Switches on an OBS filter of a source (e.g. a voice changer on the
    /// mic) for `seconds`, then back off; without `seconds` it stays on.
    ObsFilter {
        source: String,
        filter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<u32>,
    },
    /// Shows a source in a scene (an image, a GIF) for `seconds`, then
    /// hides it again; without `seconds` it stays visible.
    ObsSource {
        scene: String,
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<u32>,
    },
    /// Plays a media source from the start (a sound, a clip).
    ObsMedia { source: String },
}

/// The longest effect `seconds` may ask for: an hour. More is almost
/// certainly a typo (a value meant as milliseconds).
pub const MAX_EFFECT_SECS: u32 = 3600;

/// The program an action goes to: one output each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Warudo,
    Obs,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Target::Warudo => "Warudo",
            Target::Obs => "OBS",
        }
    }
}

impl Action {
    pub fn target(&self) -> Target {
        match self {
            Action::Warudo { .. } => Target::Warudo,
            Action::ObsFilter { .. } | Action::ObsSource { .. } | Action::ObsMedia { .. } => {
                Target::Obs
            }
        }
    }
}

impl Rule {
    /// What makes this rule unable to work, if anything (see
    /// `ReactionSettings::check`, which also checks the names). Public for
    /// the app's form, so it says the same as `config.toml` would.
    pub fn check(&self) -> Result<(), &'static str> {
        match &self.trigger {
            Trigger::Command { command } if command.split_whitespace().count() != 1 => {
                return Err("a command is one word, e.g. \"!bonk\"");
            }
            Trigger::Keyword { keyword } if keyword.trim().is_empty() => {
                return Err("the keyword is empty");
            }
            Trigger::Emote { emote } if emote.trim().is_empty() => {
                return Err("the emote is empty");
            }
            Trigger::Gift { min_count: 0 } => return Err("min_count starts at 1"),
            // A tier no platform reaches would never fire, silently.
            Trigger::Paid {
                min_tier: Some(tier),
            } if !(1..=TOP_TIER).contains(tier) => {
                return Err("min_tier is a tier from 1 to 7 (pick it from the list in the app)");
            }
            Trigger::PaidAmount { amount, .. } if *amount == Amount::default() => {
                return Err("the amount starts above 0");
            }
            _ => {}
        }
        if let Some(crowd) = &self.guards.crowd {
            // A crowd of one is just the rule without a crowd.
            if !(2..=MAX_CROWD).contains(&crowd.chatters) {
                return Err("a crowd is 2 to 1000 chatters");
            }
            if !(1..=MAX_CROWD_SECS).contains(&crowd.within_secs) {
                return Err("a crowd gathers within 1 to 3600 seconds");
            }
            // Each chatter counts once already, and which of them a
            // per-chatter cooldown should rest after firing has no clear
            // answer.
            if self.guards.user_cooldown_secs > 0 {
                return Err(
                    "a crowd counts each chatter once already: leave the per-chatter cooldown at 0",
                );
            }
        }
        let empty = |name: &str| name.trim().is_empty();
        let (names, seconds) = match &self.action {
            Action::Warudo { action } => (vec![action], None),
            Action::ObsFilter {
                source,
                filter,
                seconds,
            } => (vec![source, filter], *seconds),
            Action::ObsSource {
                scene,
                source,
                seconds,
            } => (vec![scene, source], *seconds),
            Action::ObsMedia { source } => (vec![source], None),
        };
        if names.into_iter().any(|name| empty(name)) {
            return Err("the action is missing a name (an action, source, filter or scene)");
        }
        match seconds {
            Some(0) => Err("seconds starts at 1 (leave it out for \"stays on\")"),
            Some(secs) if secs > MAX_EFFECT_SECS => Err("seconds can be at most 3600"),
            _ => Ok(()),
        }
    }
}

impl Trigger {
    /// `tip_currency`: the streamer's tip currency (`[tips]`), for amount
    /// rules; `None` if not set.
    pub fn matches(&self, msg: &ChatMessage, tip_currency: Option<&str>) -> bool {
        match self {
            Trigger::Command { command } => msg
                .text
                .split_whitespace()
                .next()
                .is_some_and(|first| first.to_lowercase() == command.to_lowercase()),
            Trigger::Keyword { keyword } => contains_words(&msg.text, keyword),
            Trigger::Emote { emote } => msg.emotes.iter().any(|e| e.code == *emote),
            Trigger::Paid { min_tier } => match &msg.kind {
                MessageKind::Donation { tier, .. } | MessageKind::Special { tier, .. } => {
                    // Without a minimum any paid message counts; with one, a
                    // message without a tier (the platform has none) doesn't.
                    min_tier.is_none_or(|min| tier.is_some_and(|tier| tier >= min))
                }
                _ => false,
            },
            Trigger::PaidAmount { amount, compare } => match &msg.kind {
                MessageKind::Donation {
                    value, currency, ..
                }
                | MessageKind::Special {
                    value, currency, ..
                } => {
                    let comparable = match currency {
                        Some(code) => Some(code.as_str()) == tip_currency,
                        // No currency: bits, and tips while none is set,
                        // count as they are. YouTube's jewels don't: a
                        // jewel is worth about a cent.
                        None => msg.platform != ChatPlatform::YouTube,
                    };
                    // Compared in whole cents (`Amount`), so "exactly"
                    // doesn't trip over floating-point dust.
                    let value = value.and_then(Amount::of);
                    comparable
                        && value.is_some_and(|value| match compare {
                            Compare::AtLeast => value >= *amount,
                            Compare::Exactly => value == *amount,
                        })
                }
                _ => false,
            },
            Trigger::Membership => matches!(msg.kind, MessageKind::MembershipJoin { .. }),
            Trigger::Gift { min_count } => {
                matches!(msg.kind, MessageKind::MembershipGift { count } if count >= *min_count)
            }
        }
    }
}

/// Whether `phrase` appears in `text` as whole words, ignoring case: with no
/// letter or digit right before or after it.
fn contains_words(text: &str, phrase: &str) -> bool {
    let phrase = phrase.trim().to_lowercase();
    if phrase.is_empty() {
        return false;
    }
    let text = text.to_lowercase();
    let word_char = |c: char| c.is_alphanumeric();
    // `match_indices` finds every place it occurs; one with word
    // boundaries on both sides is enough.
    text.match_indices(&phrase).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + phrase.len()..].chars().next();
        !before.is_some_and(word_char) && !after.is_some_and(word_char)
    })
}

/// A rule that fired, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fire {
    pub(crate) rule: String,
    pub(crate) action: Action,
}

/// The rules plus what they remember between messages: when each one last
/// fired, overall and per chatter.
pub(crate) struct Rules {
    rules: Vec<Rule>,
    paused: bool,
    /// The streamer's tip currency, for amount rules (see
    /// [`Trigger::PaidAmount`]); kept up to date by the reactor.
    pub(crate) tip_currency: Option<String>,
    /// By rule name, so it survives edits of the rules.
    state: HashMap<String, RuleState>,
}

/// Above this many remembered chatters, a rule forgets those whose cooldown
/// is over, so a long stream with a busy chat doesn't grow the map forever.
const PRUNE_ABOVE: usize = 1000;

#[derive(Default)]
struct RuleState {
    last: Option<Instant>,
    /// When the rule last fired for each chatter. Chatter ids are only
    /// unique within a platform, hence the pair.
    users: HashMap<(ChatPlatform, String), Instant>,
    /// The crowd gathering so far: each chatter with when they last
    /// joined in. Never more than the crowd's size, since it starts over
    /// once it's complete.
    crowd: HashMap<(ChatPlatform, String), Instant>,
    /// Since the engine started, for the status.
    fired: u64,
}

impl Rules {
    pub(crate) fn new(settings: &ReactionSettings) -> Self {
        let mut rules = Self {
            rules: Vec::new(),
            paused: false,
            tip_currency: None,
            state: HashMap::new(),
        };
        rules.update(settings);
        rules
    }

    /// New rules: cooldowns and counts of rules that are still there (by
    /// name) carry on, those of removed ones are forgotten.
    pub(crate) fn update(&mut self, settings: &ReactionSettings) {
        self.rules = settings.rules.clone();
        self.paused = settings.paused;
        let rules = &self.rules;
        self.state
            .retain(|name, _| rules.iter().any(|rule| rule.name == *name));
    }

    /// The rules `msg` fires, in rule order, and records that they did.
    pub(crate) fn check(&mut self, msg: &ChatMessage, now: Instant) -> Vec<Fire> {
        if self.paused || chat_core::demo::is_sample(msg) {
            return Vec::new();
        }
        let role = Role::of(msg);
        let mut fired = Vec::new();
        // `self.rules` is read while `self.state` is changed: two different
        // fields, so the borrow checker allows both at once. (Calling a
        // `&mut self` method inside this loop would not be allowed: that
        // borrows all of `self`.)
        for rule in &self.rules {
            // Cheap and timeless first; cooldowns last.
            if !rule.enabled
                || !rule.trigger.matches(msg, self.tip_currency.as_deref())
                || !rule.guards.admit(msg.platform, role)
            {
                continue;
            }
            let state = self.state.entry(rule.name.clone()).or_default();
            let chatter = (msg.platform, msg.author.id.clone());
            // Nothing counts towards a crowd while the rule rests, so it
            // doesn't go off again the moment its cooldown ends.
            if state.resting(&rule.guards, &chatter, now) {
                continue;
            }
            if let Some(crowd) = rule.guards.crowd
                && !state.gathered(crowd, &chatter, now)
            {
                continue;
            }
            // Only now, with every guard passed, do the cooldowns start:
            // otherwise someone who isn't allowed to fire a rule could
            // keep it resting for everyone else by spamming it.
            state.record(&rule.guards, chatter, now);
            fired.push(Fire {
                rule: rule.name.clone(),
                action: rule.action.clone(),
            });
        }
        fired
    }

    /// The rules, with how often each has fired.
    pub(crate) fn counts(&self) -> Vec<(String, u64)> {
        self.rules
            .iter()
            .map(|rule| {
                let fired = self.state.get(&rule.name).map_or(0, |s| s.fired);
                (rule.name.clone(), fired)
            })
            .collect()
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

impl RuleState {
    fn resting(&self, guards: &Guards, chatter: &(ChatPlatform, String), now: Instant) -> bool {
        // `duration_since` is 0 for a time in the future, never negative.
        let within = |at: Instant, secs: u32| now.duration_since(at) < seconds(secs);
        self.last.is_some_and(|at| within(at, guards.cooldown_secs))
            || self
                .users
                .get(chatter)
                .is_some_and(|&at| within(at, guards.user_cooldown_secs))
    }

    /// Counts `chatter` into the crowd; whether it's complete now. A
    /// complete crowd starts over, or the next chatter would fire the rule
    /// again at once.
    fn gathered(&mut self, crowd: Crowd, chatter: &(ChatPlatform, String), now: Instant) -> bool {
        // Those who joined in too long ago drop out first. A map rather
        // than a list ordered by time: it's at most a crowd's size, and a
        // chatter repeating themselves just moves their time forward.
        let window = seconds(crowd.within_secs);
        self.crowd.retain(|_, at| now.duration_since(*at) < window);
        self.crowd.insert(chatter.clone(), now);
        if self.crowd.len() < crowd.chatters {
            return false;
        }
        self.crowd.clear();
        true
    }

    fn record(&mut self, guards: &Guards, chatter: (ChatPlatform, String), now: Instant) {
        self.last = Some(now);
        self.fired += 1;
        if guards.user_cooldown_secs == 0 {
            return; // nothing to remember per chatter
        }
        if self.users.len() >= PRUNE_ABOVE {
            let rest = seconds(guards.user_cooldown_secs);
            self.users.retain(|_, at| now.duration_since(*at) < rest);
        }
        self.users.insert(chatter, now);
    }
}

fn seconds(secs: u32) -> Duration {
    Duration::from_secs(secs.into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chat_core::{Author, EmoteRef};

    pub(crate) fn message(text: &str) -> ChatMessage {
        ChatMessage {
            id: "LCC.ExampleMessageId0001".into(),
            platform: ChatPlatform::Twitch,
            author: Author {
                id: "42".into(),
                name: "Viewer42".into(),
                color: None,
                badges: vec![],
                avatar_url: None,
            },
            text: text.into(),
            emotes: vec![],
            timestamp: jiff::Timestamp::now(),
            kind: MessageKind::Text,
        }
    }

    fn by(mut msg: ChatMessage, author_id: &str, badges: &[&str]) -> ChatMessage {
        msg.author.id = author_id.into();
        msg.author.badges = badges.iter().map(|b| b.to_string()).collect();
        msg
    }

    fn kind(kind: MessageKind) -> ChatMessage {
        ChatMessage {
            kind,
            ..message("")
        }
    }

    pub(crate) fn rule(name: &str, trigger: Trigger, guards: Guards) -> Rule {
        Rule {
            name: name.into(),
            enabled: true,
            trigger,
            guards,
            action: Action::Warudo {
                action: name.into(),
            },
        }
    }

    pub(crate) fn command(command: &str) -> Trigger {
        Trigger::Command {
            command: command.into(),
        }
    }

    fn rules(list: Vec<Rule>) -> Rules {
        Rules::new(&ReactionSettings {
            rules: list,
            ..ReactionSettings::default()
        })
    }

    fn names(fired: Vec<Fire>) -> Vec<String> {
        fired.into_iter().map(|f| f.rule).collect()
    }

    fn after(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn commands_are_the_first_word_in_any_case() {
        let bonk = command("!bonk");
        assert!(bonk.matches(&message("!bonk"), None));
        assert!(bonk.matches(&message("  !BONK the streamer"), None));
        assert!(
            !bonk.matches(&message("lol !bonk"), None),
            "not the first word"
        );
        assert!(!bonk.matches(&message("!bonkers"), None));
        assert!(!bonk.matches(&message(""), None));
    }

    #[test]
    fn keywords_are_whole_words_anywhere() {
        let hype = Trigger::Keyword {
            keyword: "hype".into(),
        };
        assert!(hype.matches(&message("HYPE!"), None));
        assert!(hype.matches(&message("so much hype in here"), None));
        assert!(
            hype.matches(&message("hyperion hype"), None),
            "the second one"
        );
        assert!(!hype.matches(&message("hyperion"), None));
        assert!(!hype.matches(&message("unhyped"), None));

        let phrase = Trigger::Keyword {
            keyword: "good night".into(),
        };
        assert!(phrase.matches(&message("Good night, chat"), None));
        assert!(!phrase.matches(&message("good nightmare"), None));
        let unicode = Trigger::Keyword {
            keyword: "größe".into(),
        };
        assert!(unicode.matches(&message("GRÖSSE? nein: Größe!"), None));
    }

    #[test]
    fn emotes_must_be_real_emotes() {
        let trigger = Trigger::Emote {
            emote: ":_hypeWave:".into(),
        };
        let typed = message("look :_hypeWave:");
        assert!(
            !trigger.matches(&typed, None),
            "typed as text, not an emote"
        );
        let mut real = typed;
        real.emotes.push(EmoteRef {
            code: ":_hypeWave:".into(),
            url: "https://example.com/e.png".into(),
        });
        assert!(trigger.matches(&real, None));
    }

    #[test]
    fn paid_messages_by_tier() {
        let donation = |tier| {
            kind(MessageKind::Donation {
                amount: "€5.00".into(),
                tier,
                value: Some(5.0),
                currency: Some("EUR".into()),
            })
        };
        let sticker = kind(MessageKind::Special {
            image_url: None,
            amount: Some("€2.00".into()),
            info: None,
            tier: Some(2),
            value: Some(2.0),
            currency: Some("EUR".into()),
        });
        let any = Trigger::Paid { min_tier: None };
        let big = Trigger::Paid { min_tier: Some(4) };
        assert!(any.matches(&donation(None), None));
        assert!(any.matches(&sticker, None));
        assert!(!any.matches(&message("free"), None));
        assert!(big.matches(&donation(Some(4)), None));
        assert!(big.matches(&donation(Some(7)), None));
        assert!(!big.matches(&donation(Some(3)), None));
        assert!(!big.matches(&donation(None), None), "no tier, no match");
        assert!(!big.matches(&sticker, None));
    }

    /// A paid message on `platform` of `value`, in `currency`.
    fn paid(platform: ChatPlatform, value: f64, currency: Option<&str>) -> ChatMessage {
        let mut msg = kind(MessageKind::Donation {
            amount: value.to_string(),
            tier: None,
            value: Some(value),
            currency: currency.map(str::to_owned),
        });
        msg.platform = platform;
        msg
    }

    fn amount(amount: f64, compare: Compare) -> Trigger {
        Trigger::PaidAmount {
            amount: Amount::of(amount).unwrap(),
            compare,
        }
    }

    #[test]
    fn paid_messages_by_amount() {
        use ChatPlatform::{StreamElements, Twitch};
        let nice = amount(69.0, Compare::Exactly);
        let eur = Some("EUR");
        // Exactly: to the cent.
        assert!(nice.matches(&paid(StreamElements, 69.0, eur), eur));
        assert!(!nice.matches(&paid(StreamElements, 69.5, eur), eur));
        assert!(!nice.matches(&paid(StreamElements, 70.0, eur), eur));
        // Tips while no currency is set count as they are.
        assert!(nice.matches(&paid(StreamElements, 69.0, None), None));
        // Bits are counted, not converted.
        assert!(nice.matches(&paid(Twitch, 69.0, None), eur));
        // At least: the amount itself is in.
        let fifty = amount(50.0, Compare::AtLeast);
        assert!(fifty.matches(&paid(StreamElements, 50.0, eur), eur));
        assert!(fifty.matches(&paid(StreamElements, 420.0, eur), eur));
        assert!(!fifty.matches(&paid(StreamElements, 49.99, eur), eur));
        assert!(!fifty.matches(&message("50"), eur), "free chat");
    }

    #[test]
    fn amounts_in_another_currency_never_match() {
        let nice = amount(69.0, Compare::Exactly);
        let eur = Some("EUR");
        let super_chat = |currency| paid(ChatPlatform::YouTube, 69.0, Some(currency));
        assert!(nice.matches(&super_chat("EUR"), eur));
        assert!(!nice.matches(&super_chat("JPY"), eur), "¥69 isn't €69");
        assert!(
            !nice.matches(&super_chat("EUR"), None),
            "no tip currency set"
        );
        // Jewels have no currency, but aren't money either.
        assert!(!nice.matches(&paid(ChatPlatform::YouTube, 69.0, None), eur));
    }

    #[test]
    fn memberships_and_gifts() {
        let join = kind(MessageKind::MembershipJoin {
            info: "subscribed".into(),
            months: None,
        });
        let gift = |count| kind(MessageKind::MembershipGift { count });
        assert!(Trigger::Membership.matches(&join, None));
        assert!(!Trigger::Membership.matches(&gift(1), None));
        let five = Trigger::Gift { min_count: 5 };
        assert!(five.matches(&gift(5), None));
        assert!(!five.matches(&gift(4), None));
        assert!(!five.matches(&join, None));
    }

    #[test]
    fn roles_across_platforms() {
        let role = |badges: &[&str]| Role::of(&by(message(""), "1", badges));
        assert_eq!(role(&[]), Role::Everyone);
        assert_eq!(role(&["verified"]), Role::Everyone);
        assert_eq!(role(&["subscriber"]), Role::Subscriber);
        assert_eq!(role(&["member"]), Role::Subscriber);
        assert_eq!(role(&["vip"]), Role::Vip);
        assert_eq!(
            role(&["subscriber", "moderator"]),
            Role::Moderator,
            "the highest"
        );
        assert_eq!(role(&["owner"]), Role::Broadcaster);
        assert_eq!(role(&["broadcaster"]), Role::Broadcaster);
    }

    #[test]
    fn guards_check_role_and_platform() {
        let mods = Guards {
            min_role: Role::Moderator,
            ..Guards::default()
        };
        let mut list = rules(vec![rule("mods", command("!x"), mods)]);
        let now = Instant::now();
        assert!(list.check(&message("!x"), now).is_empty());
        assert!(
            list.check(&by(message("!x"), "1", &["vip"]), now)
                .is_empty()
        );
        let fired = list.check(&by(message("!x"), "1", &["moderator"]), now);
        assert_eq!(names(fired), ["mods"]);
        assert_eq!(
            names(list.check(&by(message("!x"), "2", &["broadcaster"]), now)),
            ["mods"]
        );

        let youtube = Guards {
            platforms: vec![ChatPlatform::YouTube],
            ..Guards::default()
        };
        let mut list = rules(vec![rule("yt", command("!x"), youtube)]);
        assert!(list.check(&message("!x"), now).is_empty(), "Twitch");
        let mut yt = message("!x");
        yt.platform = ChatPlatform::YouTube;
        assert_eq!(names(list.check(&yt, now)), ["yt"]);
    }

    #[test]
    fn a_rule_rests_after_firing() {
        let guards = Guards {
            cooldown_secs: 10,
            ..Guards::default()
        };
        let mut list = rules(vec![rule("bonk", command("!bonk"), guards)]);
        let start = Instant::now();
        let a = by(message("!bonk"), "1", &[]);
        let b = by(message("!bonk"), "2", &[]);
        assert_eq!(list.check(&a, start).len(), 1);
        assert!(list.check(&b, after(start, 9)).is_empty(), "anyone's");
        assert_eq!(list.check(&b, after(start, 10)).len(), 1);
    }

    #[test]
    fn a_chatter_rests_on_their_own() {
        let guards = Guards {
            user_cooldown_secs: 60,
            ..Guards::default()
        };
        let mut list = rules(vec![rule("bonk", command("!bonk"), guards)]);
        let start = Instant::now();
        let a = by(message("!bonk"), "1", &[]);
        let b = by(message("!bonk"), "2", &[]);
        assert_eq!(list.check(&a, start).len(), 1);
        assert!(list.check(&a, after(start, 30)).is_empty());
        assert_eq!(list.check(&b, after(start, 30)).len(), 1, "others can");
        let mut same_id_elsewhere = a.clone();
        same_id_elsewhere.platform = ChatPlatform::YouTube;
        assert_eq!(
            list.check(&same_id_elsewhere, after(start, 30)).len(),
            1,
            "ids are per platform"
        );
        assert_eq!(list.check(&a, after(start, 60)).len(), 1);
    }

    #[test]
    fn rejected_attempts_dont_start_a_cooldown() {
        let guards = Guards {
            min_role: Role::Moderator,
            cooldown_secs: 30,
            ..Guards::default()
        };
        let mut list = rules(vec![rule("bonk", command("!bonk"), guards)]);
        let now = Instant::now();
        // A viewer spams it: they're not allowed, so the rule doesn't rest...
        for _ in 0..5 {
            assert!(list.check(&message("!bonk"), now).is_empty());
        }
        // ...and the mod right after still gets it.
        let fired = list.check(&by(message("!bonk"), "7", &["moderator"]), now);
        assert_eq!(names(fired), ["bonk"]);

        // Both cooldowns must pass before either starts: a chatter resting
        // on their own cooldown doesn't put the rule to rest for everyone.
        let both = Guards {
            cooldown_secs: 10,
            user_cooldown_secs: 100,
            ..Guards::default()
        };
        let mut list = rules(vec![rule("both", command("!x"), both)]);
        let a = by(message("!x"), "1", &[]);
        let b = by(message("!x"), "2", &[]);
        assert_eq!(list.check(&a, now).len(), 1);
        assert!(list.check(&a, after(now, 20)).is_empty(), "a still rests");
        assert_eq!(list.check(&b, after(now, 21)).len(), 1, "the rule doesn't");
    }

    fn crowd(chatters: usize, within_secs: u32) -> Guards {
        Guards {
            crowd: Some(Crowd {
                chatters,
                within_secs,
            }),
            ..Guards::default()
        }
    }

    #[test]
    fn a_crowd_fires_once_enough_different_chatters_join_in() {
        let mut list = rules(vec![rule("f", command("F"), crowd(3, 30))]);
        let start = Instant::now();
        let f = |id: &str| by(message("F"), id, &[]);
        assert!(list.check(&f("1"), start).is_empty());
        assert!(
            list.check(&f("1"), after(start, 1)).is_empty(),
            "the same chatter again counts once"
        );
        let mut same_id_elsewhere = f("1");
        same_id_elsewhere.platform = ChatPlatform::YouTube;
        assert!(
            list.check(&same_id_elsewhere, after(start, 2)).is_empty(),
            "ids are per platform: that's a second chatter"
        );
        assert_eq!(names(list.check(&f("3"), after(start, 3))), ["f"]);
        // Complete crowds start over: the next one doesn't fire it again.
        assert!(list.check(&f("4"), after(start, 4)).is_empty());
        assert!(list.check(&f("5"), after(start, 5)).is_empty());
        assert_eq!(list.check(&f("6"), after(start, 6)).len(), 1);
    }

    #[test]
    fn a_crowd_forgets_who_joined_in_too_long_ago() {
        let mut list = rules(vec![rule("f", command("F"), crowd(2, 30))]);
        let start = Instant::now();
        let f = |id: &str| by(message("F"), id, &[]);
        list.check(&f("1"), start);
        assert!(
            list.check(&f("2"), after(start, 30)).is_empty(),
            "30 s later, the first one is out"
        );
        assert_eq!(list.check(&f("3"), after(start, 31)).len(), 1);

        // Joining in again keeps a chatter in.
        let mut list = rules(vec![rule("f", command("F"), crowd(2, 30))]);
        list.check(&f("1"), start);
        list.check(&f("1"), after(start, 25));
        assert_eq!(list.check(&f("2"), after(start, 50)).len(), 1);
    }

    #[test]
    fn a_crowd_counts_who_passes_the_guards_and_not_while_resting() {
        let guards = Guards {
            min_role: Role::Subscriber,
            cooldown_secs: 60,
            ..crowd(2, 600)
        };
        let mut list = rules(vec![rule("f", command("F"), guards)]);
        let start = Instant::now();
        let viewer = |id: &str| by(message("F"), id, &[]);
        let sub = |id: &str| by(message("F"), id, &["subscriber"]);
        assert!(list.check(&viewer("v1"), start).is_empty());
        assert!(list.check(&sub("s1"), start).is_empty());
        assert!(
            list.check(&viewer("v2"), start).is_empty(),
            "viewers don't count"
        );
        assert_eq!(list.check(&sub("s2"), start).len(), 1);
        // Resting: this one doesn't count towards the next crowd...
        assert!(list.check(&sub("s3"), after(start, 10)).is_empty());
        // ...so the first one after the rest doesn't complete it.
        assert!(list.check(&sub("s4"), after(start, 61)).is_empty());
        assert_eq!(list.check(&sub("s5"), after(start, 62)).len(), 1);
    }

    #[test]
    fn every_matching_rule_fires_unless_off_or_paused() {
        let mut list = rules(vec![
            rule("first", command("!bonk"), Guards::default()),
            rule("other", command("!hug"), Guards::default()),
            rule(
                "second",
                Trigger::Keyword {
                    keyword: "bonk".into(),
                },
                Guards::default(),
            ),
            Rule {
                enabled: false,
                ..rule("off", command("!bonk"), Guards::default())
            },
        ]);
        let now = Instant::now();
        assert_eq!(
            names(list.check(&message("!bonk"), now)),
            ["first", "second"]
        );

        let mut paused = ReactionSettings {
            rules: list.rules().to_vec(),
            ..ReactionSettings::default()
        };
        paused.paused = true;
        list.update(&paused);
        assert!(list.check(&message("!bonk"), now).is_empty());
    }

    #[test]
    fn demo_messages_never_fire() {
        let mut list = rules(vec![rule(
            "any",
            Trigger::Paid { min_tier: None },
            Guards::default(),
        )]);
        let now = Instant::now();
        for msg in chat_core::demo::sample_messages(0) {
            assert!(list.check(&msg, now).is_empty(), "{msg:?}");
        }
    }

    #[test]
    fn edits_keep_cooldowns_and_counts_of_rules_still_there() {
        let rest = Guards {
            cooldown_secs: 60,
            ..Guards::default()
        };
        let mut list = rules(vec![
            rule("kept", command("!a"), rest.clone()),
            rule("gone", command("!b"), rest.clone()),
        ]);
        let now = Instant::now();
        list.check(&message("!a"), now);
        list.check(&message("!b"), now);
        assert_eq!(list.counts(), [("kept".into(), 1), ("gone".into(), 1)]);

        let mut edited = rule("kept", command("!a"), rest.clone());
        edited.action = Action::Warudo {
            action: "something else".into(),
        };
        list.update(&ReactionSettings {
            rules: vec![edited, rule("new", command("!b"), rest.clone())],
            ..ReactionSettings::default()
        });
        assert!(
            list.check(&message("!a"), after(now, 1)).is_empty(),
            "still resting"
        );
        assert_eq!(list.counts(), [("kept".into(), 1), ("new".into(), 0)]);
        // Removed and added back: a fresh start.
        list.update(&ReactionSettings {
            rules: vec![rule("gone", command("!b"), rest)],
            ..ReactionSettings::default()
        });
        assert_eq!(list.check(&message("!b"), after(now, 1)).len(), 1);
    }

    #[test]
    fn many_chatters_are_forgotten_once_their_cooldown_is_over() {
        let guards = Guards {
            user_cooldown_secs: 10,
            ..Guards::default()
        };
        let mut list = rules(vec![rule("x", command("!x"), guards)]);
        let start = Instant::now();
        for n in 0..PRUNE_ABOVE {
            list.check(&by(message("!x"), &n.to_string(), &[]), start);
        }
        // Past everyone's cooldown, the next one clears the map.
        list.check(&by(message("!x"), "new", &[]), after(start, 11));
        assert_eq!(list.state["x"].users.len(), 1);
    }
}
