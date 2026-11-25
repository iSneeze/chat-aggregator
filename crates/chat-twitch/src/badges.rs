//! Twitch chat badge image lookup (global + channel-specific badges).
//!
//! Twitch IRC only sends badge `name/version` pairs (e.g. `subscriber/12`,
//! `moderator/1`). Because Twitch retired the old unauthenticated
//! `badges.twitch.tv` endpoint and `chat-twitch` connects anonymously without
//! an OAuth login, we:
//!
//! 1. Start immediately with [`BadgeMap::builtin`], a small verified table of
//!    core global Twitch CDN icons (`broadcaster`, `moderator`, `vip`,
//!    `founder`, `subscriber`, `sub-gifter`, `bits`, etc.) so chat renders
//!    icons from the first message with zero startup delay.
//! 2. Fetch the full global + channel badge sets once in the background from
//!    Twitch's public web GraphQL endpoint (`gql.twitch.tv/gql`) so custom
//!    channel subscriber/bits badges and newer global badges show their exact
//!    version icon too.
//!
//! If the GQL fetch ever fails or changes shape, the source keeps working with
//! the built-in table, and any badge without an image URL falls back cleanly
//! to a styled text pill in overlay themes.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::json;
use twitch_irc::message::Badge;

const GQL_URL: &str = "https://gql.twitch.tv/gql";
/// Twitch's public web client ID (used by Twitch's own web client for
/// unauthenticated read-only queries).
const TWITCH_WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const BADGE_CDN_PREFIX: &str = "https://static-cdn.jtvnw.net/";

const GQL_QUERY: &str = r#"
query($login: String!) {
  badges {
    setID
    version
    imageURL(size: DOUBLE)
  }
  user(login: $login) {
    broadcastBadges {
      setID
      version
      imageURL(size: DOUBLE)
    }
  }
}
"#;

/// Core global Twitch badge icons (`static-cdn.jtvnw.net`, 2x / 36x36).
/// Used immediately on connect and as a fallback if the GQL fetch fails.
const BUILTIN_BADGES: &[(&str, &str, &str)] = &[
    (
        "admin",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/9ef7e029-4cdf-4d4d-a0d5-e2b3fb2583fe/2",
    ),
    (
        "ambassador",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/2cbc339f-34f4-488a-ae51-efdf74f4e323/2",
    ),
    (
        "artist-badge",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/4300a897-03dc-4e83-8c0e-c332fee7057f/2",
    ),
    (
        "bits",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/73b5c3fb-24f9-4a82-a852-2f475b59411c/2",
    ),
    (
        "bits",
        "100",
        "https://static-cdn.jtvnw.net/badges/v1/09d93036-e7ce-431c-9a9e-7044297133f2/2",
    ),
    (
        "bits",
        "1000",
        "https://static-cdn.jtvnw.net/badges/v1/0d85a29e-79ad-4c63-a285-3acd2c66f2ba/2",
    ),
    (
        "bits",
        "5000",
        "https://static-cdn.jtvnw.net/badges/v1/57cd97fc-3e9e-4c6d-9d41-60147137234e/2",
    ),
    (
        "bits",
        "10000",
        "https://static-cdn.jtvnw.net/badges/v1/68af213b-a771-4124-b6e3-9bb6d98aa732/2",
    ),
    (
        "bits-leader",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/8bedf8c3-7a6d-4df2-b62f-791b96a5dd31/2",
    ),
    (
        "broadcaster",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/5527c58c-fb7d-422d-b71b-f309dcb85cc1/2",
    ),
    (
        "founder",
        "0",
        "https://static-cdn.jtvnw.net/badges/v1/511b78a9-ab37-472f-9569-457753bbe7d3/2",
    ),
    (
        "game-developer",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/85856a4a-eb7d-4e26-a43e-d204a977ade4/2",
    ),
    (
        "hype-train",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/fae4086c-3190-44d4-83c8-8ef0cbe1a515/2",
    ),
    (
        "lead_moderator",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/0822047b-65e0-46f2-94a9-d1091d685d33/2",
    ),
    (
        "moderator",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/3267646d-33f0-4b17-b3df-f923a41db1d0/2",
    ),
    (
        "no_audio",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/aef2cd08-f29b-45a1-8c12-d44d7fd5e6f0/2",
    ),
    (
        "no_video",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/199a0dba-58f3-494e-a7fc-1fa0a1001fb8/2",
    ),
    (
        "partner",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/d12a2e27-16f6-41d0-ab77-b780518f00a3/2",
    ),
    (
        "predictions",
        "blue-1",
        "https://static-cdn.jtvnw.net/badges/v1/e33d8b46-f63b-4e67-996d-4a7dcec0ad33/2",
    ),
    (
        "predictions",
        "pink-2",
        "https://static-cdn.jtvnw.net/badges/v1/4b76d5f2-91cc-4400-adf2-908a1e6cfd1e/2",
    ),
    (
        "premium",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/bbbe0db0-a598-423e-86d0-f9fb98ca1933/2",
    ),
    (
        "staff",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/d97c37bd-a6f5-4c38-8f57-4e4bef88af34/2",
    ),
    (
        "sub-gift-leader",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/21656088-7da2-4467-acd2-55220e1f45ad/2",
    ),
    (
        "sub-gifter",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/a5ef6c17-2e5b-4d8f-9b80-2779fd722414/2",
    ),
    (
        "subscriber",
        "0",
        "https://static-cdn.jtvnw.net/badges/v1/5d9f2208-5dd8-11e7-8513-2ff4adfae661/2",
    ),
    (
        "subscriber",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/5d9f2208-5dd8-11e7-8513-2ff4adfae661/2",
    ),
    (
        "turbo",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/bd444ec6-8f34-4bf9-91f4-af1e3428d80f/2",
    ),
    (
        "vip",
        "1",
        "https://static-cdn.jtvnw.net/badges/v1/b817aba4-fad8-49e2-b88a-7cc744dfa6ec/2",
    ),
];

/// Maps `(set_id, version)` pairs to Twitch CDN image URLs.
#[derive(Debug, Clone, Default)]
pub(crate) struct BadgeMap {
    by_key: HashMap<(String, String), String>,
}

impl BadgeMap {
    /// Core global Twitch badges bundled with the binary.
    pub(crate) fn builtin() -> Self {
        let mut by_key = HashMap::with_capacity(BUILTIN_BADGES.len());
        for &(set_id, version, url) in BUILTIN_BADGES {
            by_key.insert((set_id.to_owned(), version.to_owned()), url.to_owned());
        }
        Self { by_key }
    }

    pub(crate) fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Looks up the icon URL for a single `(name, version)` badge.
    pub(crate) fn get(&self, name: &str, version: &str) -> Option<&str> {
        if let Some(url) = self.by_key.get(&(name.to_owned(), version.to_owned())) {
            return Some(url.as_str());
        }
        // Before channel badges finish loading (or if a channel hasn't
        // uploaded custom subscriber art for a tier), fall back to the
        // channel's base subscriber badge rather than dropping the icon.
        if name == "subscriber" {
            return self
                .by_key
                .get(&("subscriber".to_owned(), "0".to_owned()))
                .map(String::as_str);
        }
        None
    }

    /// Resolves `badges` from `twitch-irc` into `Author::badge_urls`.
    pub(crate) fn resolve(&self, badges: &[Badge]) -> BTreeMap<String, String> {
        self.resolve_pairs(badges.iter().map(|b| (b.name.as_str(), b.version.as_str())))
    }

    /// Resolves raw `(name, version)` badge pairs into `Author::badge_urls`.
    pub(crate) fn resolve_pairs<'a>(
        &self,
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (name, version) in pairs {
            if let Some(url) = self.get(name, version) {
                out.insert(name.to_owned(), url.to_owned());
            }
        }
        out
    }

    /// Fetches Twitch's global badges and `channel`'s custom badges in one
    /// unauthenticated GraphQL request, merged on top of [`Self::builtin`].
    pub(crate) async fn fetch_for_channel(channel: &str) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(8))
            .build()
            .context("building HTTP client for Twitch badges")?;
        let resp = client
            .post(GQL_URL)
            .header("Client-ID", TWITCH_WEB_CLIENT_ID)
            .json(&json!({
                "query": GQL_QUERY,
                "variables": { "login": channel },
            }))
            .send()
            .await
            .context("requesting Twitch badges")?
            .error_for_status()
            .context("Twitch badges endpoint returned an error")?;
        let body = resp
            .text()
            .await
            .context("reading Twitch badges response")?;
        Self::parse_gql_response(&body)
    }

    /// Parses a GQL response body into a [`BadgeMap`], starting from
    /// [`Self::builtin`] and applying global then channel badges (so custom
    /// channel subscriber/bits art overrides the global defaults).
    pub(crate) fn parse_gql_response(body: &str) -> anyhow::Result<Self> {
        let parsed: GqlResponse =
            serde_json::from_str(body).context("parsing Twitch badges JSON")?;
        let mut map = Self::builtin();
        if let Some(data) = parsed.data {
            for badge in data.badges.into_iter().flatten() {
                map.insert_if_valid(badge);
            }
            if let Some(user) = data.user {
                for badge in user.broadcast_badges.into_iter().flatten() {
                    map.insert_if_valid(badge);
                }
            }
        }
        Ok(map)
    }

    fn insert_if_valid(&mut self, badge: GqlBadge) {
        if !badge.set_id.is_empty()
            && !badge.version.is_empty()
            && badge.image_url.starts_with(BADGE_CDN_PREFIX)
        {
            self.by_key
                .insert((badge.set_id, badge.version), badge.image_url);
        }
    }
}

#[derive(Deserialize)]
struct GqlResponse {
    data: Option<GqlData>,
}

#[derive(Deserialize)]
struct GqlData {
    badges: Option<Vec<GqlBadge>>,
    user: Option<GqlUser>,
}

#[derive(Deserialize)]
struct GqlUser {
    #[serde(rename = "broadcastBadges")]
    broadcast_badges: Option<Vec<GqlBadge>>,
}

#[derive(Deserialize)]
struct GqlBadge {
    #[serde(rename = "setID")]
    set_id: String,
    version: String,
    #[serde(rename = "imageURL")]
    image_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_map_resolves_core_badges_and_subscriber_fallback() {
        let map = BadgeMap::builtin();
        assert!(
            map.get("moderator", "1")
                .unwrap()
                .starts_with(BADGE_CDN_PREFIX)
        );
        assert!(
            map.get("broadcaster", "1")
                .unwrap()
                .starts_with(BADGE_CDN_PREFIX)
        );
        // Unlisted subscriber month tier falls back to base subscriber badge.
        assert_eq!(map.get("subscriber", "12"), map.get("subscriber", "0"));
        // Unknown badge returns None (rendered as text pill).
        assert_eq!(map.get("custom-unknown", "1"), None);
    }

    #[test]
    fn gql_response_merges_global_and_channel_overrides_and_rejects_bad_urls() {
        let json = r#"{
            "data": {
                "badges": [
                    { "setID": "subscriber", "version": "12", "imageURL": "https://static-cdn.jtvnw.net/badges/v1/global-sub-12/2" },
                    { "setID": "evil", "version": "1", "imageURL": "javascript:alert(1)" },
                    { "setID": "offsite", "version": "1", "imageURL": "https://evil.example/badge.png" }
                ],
                "user": {
                    "broadcastBadges": [
                        { "setID": "subscriber", "version": "12", "imageURL": "https://static-cdn.jtvnw.net/badges/v1/channel-sub-12/2" },
                        { "setID": "bits", "version": "25000", "imageURL": "https://static-cdn.jtvnw.net/badges/v1/channel-bits-25k/2" }
                    ]
                }
            }
        }"#;
        let map = BadgeMap::parse_gql_response(json).unwrap();
        assert_eq!(
            map.get("subscriber", "12"),
            Some("https://static-cdn.jtvnw.net/badges/v1/channel-sub-12/2")
        );
        assert_eq!(
            map.get("bits", "25000"),
            Some("https://static-cdn.jtvnw.net/badges/v1/channel-bits-25k/2")
        );
        assert_eq!(map.get("evil", "1"), None);
        assert_eq!(map.get("offsite", "1"), None);
    }
}
