//! Keeps the published JSON Schema (docs/schema/chat-event.json) in sync
//! with the Rust types it's generated from. If this fails after an
//! intentional change to the event types, regenerate the file:
//!
//!   UPDATE_SCHEMA=1 cargo nextest run -p chat-server --test api_schema
//!
//! and review the diff: it shows exactly how the API changed for consumers.

use std::path::Path;

use chat_core::ChatEvent;
use schemars::generate::SchemaSettings;

const SCHEMA_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/schema/chat-event.json"
);

fn generate() -> String {
    // Draft 7: the JSON Schema version AsyncAPI tooling supports best.
    // `for_serialize`: describe the JSON we *send* (e.g. `null` fields are
    // always present), not what we would accept as input.
    let schema = SchemaSettings::draft07()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<ChatEvent>();
    serde_json::to_string_pretty(&schema).unwrap() + "\n"
}

#[test]
fn committed_schema_matches_the_code() {
    let generated = generate();

    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::create_dir_all(Path::new(SCHEMA_PATH).parent().unwrap()).unwrap();
        std::fs::write(SCHEMA_PATH, &generated).unwrap();
        return;
    }

    let committed = std::fs::read_to_string(SCHEMA_PATH).unwrap_or_default();
    assert!(
        committed == generated,
        "docs/schema/chat-event.json is out of date with the Rust types.\n\
         If the change is intentional, regenerate it:\n  \
         UPDATE_SCHEMA=1 cargo nextest run -p chat-server --test api_schema"
    );
}
