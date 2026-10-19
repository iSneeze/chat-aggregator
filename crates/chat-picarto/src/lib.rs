//! Picarto chat source: not built yet. What's known about Picarto's chat
//! is in PICARTO.md at the repository root; `examples/raw_picarto.rs`
//! captures raw chat traffic to build the source from. So far this crate
//! has the emote handling the source will use.

mod emotes;

pub use emotes::{Body, ChannelEmote, SiteEmotes};
