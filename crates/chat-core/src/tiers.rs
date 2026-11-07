//! How big a paid message is: the tiers 1–7 that every platform's paid
//! messages are sorted into, all on one scale, YouTube's Super Chat tiers
//! (decided 2026-11-07). No currency is converted anywhere:
//!
//! - YouTube: its own tier, which YouTube works out in every currency.
//! - Twitch bits: YouTube's dollar steps × 100 (100 bits cost about a
//!   dollar). Everything below 200 bits is tier 1.
//! - Tips (StreamElements, Streamlabs): the streamer's own amounts
//!   ([`TipSettings`], `[tips]` in `config.toml`), in their tip page's
//!   currency. By default YouTube's steps.
//!
//! One scale, so a reaction rule's "tier 4 or more" means about the same on
//! every platform, and the app can show the whole table.

use std::fmt;
use std::str::FromStr;

/// The highest tier. YouTube's top band may report more (bigger Super
/// Chats stay pinned longer); those count as 7 and above.
pub const TOP_TIER: u32 = 7;

/// Where YouTube's tiers 2–7 start, in US dollars (tier 1 is $1+).
pub const YOUTUBE_TIER_STARTS: [u64; 6] = [2, 5, 10, 20, 50, 100];

/// Bits per dollar on the tier scale: 100 bits cost about a dollar.
pub const BITS_PER_DOLLAR: u64 = 100;

/// Where the bits' tiers 2–7 start: YouTube's steps × 100.
pub const BITS_TIER_STARTS: [u64; 6] = {
    // A `for` loop isn't allowed in a `const` yet; `while` is.
    let mut starts = YOUTUBE_TIER_STARTS;
    let mut i = 0;
    while i < starts.len() {
        starts[i] *= BITS_PER_DOLLAR;
        i += 1;
    }
    starts
};

/// The tier of a cheer of `bits`.
pub fn bits_tier(bits: u64) -> u32 {
    tier_of(&BITS_TIER_STARTS, &bits)
}

/// The tier of a Super Chat of `dollars`, as YouTube would give it (the
/// app's test window; real Super Chats come with YouTube's own tier).
pub fn youtube_tier(dollars: Amount) -> u32 {
    tier_of(&YOUTUBE_TIER_STARTS.map(Amount::whole), &dollars)
}

/// 1, plus one for every tier start that `value` reaches. `starts` must be
/// in increasing order ([`TipSettings::check`] makes sure of that).
fn tier_of<T: PartialOrd>(starts: &[T], value: &T) -> u32 {
    // At most 6 starts: the cast can't overflow.
    1 + starts.iter().filter(|start| *start <= value).count() as u32
}

/// An amount exact to the hundredth (a cent): `69` and `69.00` are the
/// same, `69.50` isn't. Kept as a whole number of hundredths because
/// decimal fractions are inexact as floating-point numbers (in an `f64`,
/// 0.1 + 0.2 isn't 0.3), and "exactly 69" must be exact. Written as a plain
/// number in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, serde::Deserialize)]
#[serde(try_from = "f64")]
pub struct Amount(u64);

impl Amount {
    pub const fn whole(n: u64) -> Self {
        Amount(n * 100)
    }

    /// From a number as platforms send it, rounded to the hundredth; `None`
    /// for one that can't be an amount (negative, NaN, absurdly big).
    pub fn of(value: f64) -> Option<Self> {
        // 1e15 hundredths: far above any real payment, well inside u64.
        let hundredths = (value * 100.0).round();
        (hundredths.is_finite() && (0.0..1e15).contains(&hundredths))
            .then_some(Amount(hundredths as u64))
    }

    pub fn as_f64(self) -> f64 {
        self.0 as f64 / 100.0
    }

    /// "69" for a whole amount, "2.50" otherwise.
    fn plain(self) -> String {
        let (units, hundredths) = (self.0 / 100, self.0 % 100);
        if hundredths == 0 {
            units.to_string()
        } else {
            format!("{units}.{hundredths:02}")
        }
    }
}

impl TryFrom<f64> for Amount {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, String> {
        Amount::of(value).ok_or_else(|| format!("{value} isn't an amount (0 or more)"))
    }
}

/// Written back as typed: `10`, not `10.0`, for a whole amount.
impl serde::Serialize for Amount {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0.is_multiple_of(100) {
            serializer.serialize_u64(self.0 / 100)
        } else {
            serializer.serialize_f64(self.as_f64())
        }
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.plain())
    }
}

/// As typed in the app: "69", "69.5", "69,50".
impl FromStr for Amount {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let text = text.trim().replace(',', ".");
        let value: f64 = text
            .parse()
            .map_err(|_| format!("\"{text}\" isn't a number"))?;
        Amount::try_from(value)
    }
}

/// `[tips]` in `config.toml`: what the streamer's tips (StreamElements,
/// Streamlabs) are in, and their tiers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TipSettings {
    /// The tip page's currency as an ISO code ("EUR"); empty: not set.
    /// Only says what tips are in: nothing is converted.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub currency: String,
    /// Where tiers 2–7 start, in that currency; below the first is tier 1.
    pub tiers: [Amount; 6],
}

impl Default for TipSettings {
    fn default() -> Self {
        Self {
            currency: String::new(),
            tiers: YOUTUBE_TIER_STARTS.map(Amount::whole),
        }
    }
}

impl TipSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// What would make tiers or amounts come out wrong.
    pub fn check(&self) -> Result<(), String> {
        let code = &self.currency;
        if !code.is_empty() && !(code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase())) {
            return Err(format!(
                "tips: the currency is a 3-letter code like EUR or USD (got \"{code}\")"
            ));
        }
        if self.tiers[0] == Amount::default() {
            return Err("tips: tier 2 starts above 0".into());
        }
        // `windows(2)` walks the neighbours: (tier 2, tier 3), (tier 3, …).
        if let Some(pair) = self.tiers.windows(2).find(|pair| pair[0] >= pair[1]) {
            return Err(format!(
                "tips: each tier starts above the one before ({} isn't above {})",
                pair[1], pair[0]
            ));
        }
        Ok(())
    }

    /// The currency, if set.
    pub fn currency(&self) -> Option<&str> {
        (!self.currency.is_empty()).then_some(self.currency.as_str())
    }

    /// The tier of a tip of `amount`.
    pub fn tier(&self, amount: Amount) -> u32 {
        tier_of(&self.tiers, &amount)
    }

    /// A tip's amount for display, in the tip currency (see
    /// [`display_amount`]).
    pub fn display(&self, amount: Amount) -> String {
        display_amount(amount, self.currency())
    }
}

/// An amount for display: "€10.00" with a currency (an ISO code), "10"
/// without (a bare number shouldn't pretend to cents it may not have).
/// Shared by the tip services, so their donations look alike.
pub fn display_amount(amount: Amount, currency: Option<&str>) -> String {
    let Some(code) = currency else {
        return amount.plain();
    };
    let two = format!("{:.2}", amount.as_f64());
    match code {
        "EUR" => format!("€{two}"),
        "USD" => format!("${two}"),
        "GBP" => format!("£{two}"),
        // Yen have no cents.
        "JPY" => format!("¥{:.0}", amount.as_f64()),
        _ => format!("{two} {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_follow_youtubes_dollar_steps() {
        let tiers = [
            1, 199, 200, 499, 500, 999, 1_000, 1_999, 2_000, 4_999, 5_000, 9_999, 10_000,
        ]
        .map(bits_tier);
        assert_eq!(tiers, [1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7]);
        assert_eq!(bits_tier(250_000), TOP_TIER);
    }

    #[test]
    fn tips_follow_the_streamers_amounts() {
        let mut tips = TipSettings::default();
        let tier = |tips: &TipSettings, n: f64| tips.tier(Amount::of(n).unwrap());
        assert_eq!(tier(&tips, 1.0), 1);
        assert_eq!(tier(&tips, 9.99), 3);
        assert_eq!(tier(&tips, 10.0), 4, "a start belongs to its tier");
        assert_eq!(tier(&tips, 500.0), 7);
        tips.tiers = [3, 6, 9, 69, 100, 420].map(Amount::whole);
        assert_eq!(tier(&tips, 68.99), 4);
        assert_eq!(tier(&tips, 69.0), 5);
    }

    #[test]
    fn amounts_are_exact_to_the_cent() {
        assert_eq!(Amount::of(69.0), Some(Amount::whole(69)));
        assert_eq!("69,00".parse(), Ok(Amount::whole(69)));
        assert_ne!(Amount::of(69.5), Some(Amount::whole(69)));
        // 0.1 + 0.2 is 0.30000000000000004 as an f64.
        assert_eq!(Amount::of(0.1 + 0.2), Amount::of(0.3));
        assert_eq!(Amount::of(-1.0), None);
        assert_eq!(Amount::of(f64::NAN), None);
        assert!("lots".parse::<Amount>().is_err());
        assert_eq!(Amount::of(2.5).unwrap().to_string(), "2.50");
    }

    #[test]
    fn tip_settings_are_checked() {
        let ok = TipSettings {
            currency: "EUR".into(),
            ..TipSettings::default()
        };
        assert_eq!(ok.check(), Ok(()));
        for currency in ["eur", "EURO", "€"] {
            let bad = TipSettings {
                currency: currency.into(),
                ..ok.clone()
            };
            assert!(bad.check().is_err(), "{currency}");
        }
        let mut unordered = ok.clone();
        unordered.tiers[3] = Amount::whole(5);
        assert!(unordered.check().unwrap_err().contains("above"));
        let mut zero = ok;
        zero.tiers[0] = Amount::default();
        assert!(zero.check().is_err());
    }

    #[test]
    fn tips_display_in_the_currency() {
        let mut tips = TipSettings::default();
        let ten = Amount::whole(10);
        assert_eq!(tips.display(ten), "10");
        assert_eq!(tips.display(Amount::of(2.5).unwrap()), "2.50");
        tips.currency = "EUR".into();
        assert_eq!(tips.display(ten), "€10.00");
        tips.currency = "CHF".into();
        assert_eq!(tips.display(ten), "10.00 CHF");
        tips.currency = "JPY".into();
        assert_eq!(tips.display(Amount::whole(500)), "¥500");
    }

    #[test]
    fn amounts_are_written_as_typed() {
        #[derive(serde::Serialize)]
        struct Row {
            a: Amount,
            b: Amount,
        }
        let row = Row {
            a: Amount::whole(10),
            b: Amount::of(2.5).unwrap(),
        };
        assert_eq!(serde_json::to_string(&row).unwrap(), r#"{"a":10,"b":2.5}"#);
    }
}
