//! Type-safe bindings for YouTube's live chat streamList RPC,
//! generated from proto/stream_list.proto at compile time.

// Generated prost/tonic code trips lints we can't fix upstream:
// unused wrapper types, shared enum postfixes, oversized `Status`.
#![allow(dead_code, clippy::enum_variant_names, clippy::result_large_err)]

include!(concat!(env!("OUT_DIR"), "/youtube.api.v3.rs"));
