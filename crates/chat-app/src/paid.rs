//! The paid tiers as the app shows them: the table on Settings → Paid
//! messages and the tier list of a reaction's paid trigger. One scale for
//! every platform (see `chat_core::tiers`), so both show what a tier means
//! on each.

use chat_core::tiers::{
    BITS_TIER_STARTS, COINS_TIER_STARTS, TOP_TIER, TipSettings, YOUTUBE_TIER_STARTS,
};

/// The tiers, 1 to 7.
pub(crate) fn all() -> impl Iterator<Item = u32> {
    1..=TOP_TIER
}

/// Where `tier` starts for tips: "€10.00+", or "below €2.00" for tier 1
/// (everything under tier 2).
pub(crate) fn tips_start(tier: u32, tips: &TipSettings) -> String {
    match start_index(tier) {
        None => format!("below {}", tips.display(tips.tiers[0])),
        Some(i) => format!("{}+", tips.display(tips.tiers[i])),
    }
}

/// Where `tier` starts in bits: "1,000+ bits".
pub(crate) fn bits_start(tier: u32) -> String {
    let bits = start_index(tier).map_or(1, |i| BITS_TIER_STARTS[i]);
    format!("{}+ bits", thousands(bits))
}

/// Where `tier` starts in RPlay coins: "200+ coins".
pub(crate) fn coins_start(tier: u32) -> String {
    let coins = start_index(tier).map_or(1, |i| COINS_TIER_STARTS[i]);
    format!("{}+ coins", thousands(coins))
}

/// Where `tier` starts on YouTube, in dollars (YouTube works out the tier
/// in every currency): "$10+".
pub(crate) fn youtube_start(tier: u32) -> String {
    let dollars = start_index(tier).map_or(1, |i| YOUTUBE_TIER_STARTS[i]);
    format!("${dollars}+")
}

/// A line of the paid trigger's tier list: what "this tier or more" means
/// on each platform.
pub(crate) fn tier_label(tier: u32, tips: &TipSettings) -> String {
    if tier <= 1 {
        return "Tier 1 or more: any amount that has a tier".into();
    }
    format!(
        "Tier {tier} or more: tips {}, {}, {}, YouTube {}",
        tips_start(tier, tips),
        bits_start(tier),
        coins_start(tier),
        youtube_start(tier)
    )
}

/// The index of `tier`'s start in the `…_TIER_STARTS` lists (which begin
/// at tier 2); `None` for tier 1, which starts at the bottom.
fn start_index(tier: u32) -> Option<usize> {
    let index = tier.checked_sub(2)? as usize;
    Some(index.min(BITS_TIER_STARTS.len() - 1))
}

/// 10000 as "10,000".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        // A comma before every group of three counted from the right.
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::tiers::Amount;

    #[test]
    fn each_platform_says_where_a_tier_starts() {
        let mut tips = TipSettings::default();
        assert_eq!(tips_start(1, &tips), "below 2");
        assert_eq!(tips_start(4, &tips), "10+");
        assert_eq!(bits_start(1), "1+ bits");
        assert_eq!(bits_start(4), "1,000+ bits");
        assert_eq!(bits_start(7), "10,000+ bits");
        assert_eq!(coins_start(1), "1+ coins");
        assert_eq!(coins_start(4), "200+ coins");
        assert_eq!(coins_start(7), "2,000+ coins");
        assert_eq!(youtube_start(1), "$1+");
        assert_eq!(youtube_start(7), "$100+");
        tips.currency = "EUR".into();
        tips.tiers[2] = Amount::whole(12);
        assert_eq!(
            tier_label(4, &tips),
            "Tier 4 or more: tips €12.00+, 1,000+ bits, 200+ coins, YouTube $10+"
        );
    }

    #[test]
    fn thousands_get_commas() {
        assert_eq!(thousands(1), "1");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(250_000), "250,000");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
