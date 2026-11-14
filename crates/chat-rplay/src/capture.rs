//! The raw chat capture of RPlay test builds (feature `capture`): every
//! frame RPlay's chat server sends, as it came, into
//! `rplay-capture.jsonl` in the settings folder. Unlike the log, it holds
//! chat (names, messages, user ids): it's for learning RPlay's chat from a
//! tester's stream, and release builds don't have it (see RPLAY.md, "Test
//! builds with a capture").
//!
//! Only the chat connection's frames: never the HTTP answers, which carry
//! the stream key (`/live/play`) and the session token. The frames carry
//! no token (the chat token travels in the connection's header), but
//! values under a name that sounds secret are starred out anyway, in case
//! an event we haven't seen yet carries one (a stream's access password,
//! say).

use std::fs::File;
use std::io::Write as _;
use std::path::Path;

use serde_json::{Value, json};
use tracing::{info, warn};

pub(crate) const FILE_NAME: &str = "rplay-capture.jsonl";
/// The capture stops growing here: a test build left running for weeks
/// shouldn't fill the disk. A busy hour of chat is a few MB.
const MAX_BYTES: u64 = 50 * 1024 * 1024;
/// Parts of a field name that mean its value may be a secret.
const SECRET_WORDS: &[&str] = &["key", "token", "secret", "password"];

/// The capture of one connection.
pub(crate) struct Capture {
    file: File,
    /// The source's label, on every line: with two channels, whose frame.
    label: String,
    /// The file's size so far.
    written: u64,
    /// Full or broken: nothing more is written.
    stopped: bool,
}

impl Capture {
    /// Opens the capture in `dir` for one connection, appending: an app
    /// restart doesn't cost the frames before it. `None` (with a warning)
    /// if it can't be opened: the chat works without it.
    pub(crate) fn open(dir: &Path, label: &str) -> Option<Self> {
        let opened = std::fs::create_dir_all(dir).and_then(|()| {
            File::options()
                .create(true)
                .append(true)
                .open(dir.join(FILE_NAME))
        });
        // The io error alone, not the path: that has the user's name in it
        // on Windows, and the log gets sent around.
        let file = match opened {
            Ok(file) => file,
            Err(e) => {
                warn!("RPlay test build: can't open {FILE_NAME}: {e}");
                return None;
            }
        };
        let written = file.metadata().map_or(0, |meta| meta.len());
        info!("RPlay test build: capturing the raw chat into {FILE_NAME}");
        let mut capture = Self {
            file,
            label: label.to_owned(),
            written,
            stopped: false,
        };
        capture.note("connected");
        Some(capture)
    }

    /// A frame from the chat server, as it came (starred out where it
    /// names a secret). Text that isn't JSON is kept as text.
    pub(crate) fn frame(&mut self, text: &str) {
        let line = match serde_json::from_str::<Value>(text) {
            Ok(mut frame) => {
                redact(&mut frame);
                json!({ "frame": frame })
            }
            Err(_) => json!({ "text": text }),
        };
        self.write(line);
    }

    /// What happened to the connection (connected, closed and why): so the
    /// frames can be lined up with the log.
    pub(crate) fn note(&mut self, note: &str) {
        self.write(json!({ "note": note }));
    }

    fn write(&mut self, mut line: Value) {
        if self.stopped {
            return;
        }
        line["at"] = jiff::Timestamp::now().to_string().into();
        line["source"] = self.label.as_str().into();
        let mut text = line.to_string();
        text.push('\n');
        let mut size = text.len() as u64;
        if self.written + size > MAX_BYTES {
            // The last line says why the capture ends here.
            text = json!({ "note": "capture full", "source": self.label }).to_string() + "\n";
            size = text.len() as u64;
            self.stopped = true;
            warn!("RPlay test build: {FILE_NAME} is full, the capture stops");
        }
        // Written right away, unbuffered: a crash or a killed app keeps
        // every frame up to it. One small write per frame, as fast as chat
        // comes, is nothing for a disk.
        if let Err(e) = self.file.write_all(text.as_bytes()) {
            warn!("RPlay test build: writing {FILE_NAME} failed, the capture stops: {e}");
            self.stopped = true;
            return;
        }
        self.written += size;
    }
}

/// Stars out every value under a name that sounds secret, also in JSON
/// that's inside a string (RPlay puts JSON into some text fields). Whether
/// anything was starred out.
fn redact(value: &mut Value) -> bool {
    match value {
        Value::Object(fields) => {
            let mut changed = false;
            for (name, inner) in fields.iter_mut() {
                if sounds_secret(name) && !harmless(inner) {
                    *inner = "***".into();
                    changed = true;
                } else {
                    changed |= redact(inner);
                }
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| redact(item) | changed),
        Value::String(text) => {
            let Ok(mut inner) = serde_json::from_str::<Value>(text) else {
                return false;
            };
            // Written back only if something changed: otherwise the text
            // stays exactly as RPlay sent it.
            let changed = (inner.is_object() || inner.is_array()) && redact(&mut inner);
            if changed {
                *text = inner.to_string();
            }
            changed
        }
        _ => false,
    }
}

fn sounds_secret(name: &str) -> bool {
    let name = name.to_lowercase();
    SECRET_WORDS.iter().any(|word| name.contains(word))
}

/// Values that can't be a secret, so they stay readable under any name:
/// a switch like `useSecretKey: "true"` says something without giving
/// anything away.
fn harmless(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) => true,
        Value::String(text) => matches!(text.as_str(), "" | "true" | "false"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("chat-rplay-capture-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn lines(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join(FILE_NAME))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn frames_are_kept_as_they_came_with_secrets_starred_out() {
        let mut frame = json!({
            "Type": "EVENT",
            "EventName": "studioSetting",
            "Attributes": {
                "streamKey": "sk_live_example",
                "useSecretKey": "true",
                "settings": r#"{"password":"hunter2","title":"Hi"}"#,
                "plain": r#"{"title":"Hi"}"#,
            },
            "Sender": { "UserId": "aaaaaaaaaaaaaaaaaaaaaaaa", "Token": null },
            "Items": [{ "apiKey": 5 }],
        });
        assert!(redact(&mut frame));
        assert_eq!(
            frame,
            json!({
                "Type": "EVENT",
                "EventName": "studioSetting",
                "Attributes": {
                    "streamKey": "***",
                    // A switch says nothing secret.
                    "useSecretKey": "true",
                    // JSON in a string: starred out inside...
                    "settings": r#"{"password":"***","title":"Hi"}"#,
                    // ...and left exactly as it was when nothing is secret.
                    "plain": r#"{"title":"Hi"}"#,
                },
                "Sender": { "UserId": "aaaaaaaaaaaaaaaaaaaaaaaa", "Token": null },
                "Items": [{ "apiKey": "***" }],
            })
        );
        // A chat message keeps everything: that's what the capture is for.
        let mut message = json!({ "Type": "MESSAGE", "Content": r#"{"m":"hi :_hypeWave:"}"# });
        assert!(!redact(&mut message));
    }

    #[test]
    fn the_capture_appends_lines_with_time_and_source() {
        let dir = dir("append");
        let mut capture = Capture::open(&dir, "Main").unwrap();
        capture.frame(r#"{"Type":"MESSAGE","Content":"hi"}"#);
        capture.frame("not json");
        drop(capture);
        // A second connection (an app restart, a renewal) adds to the file.
        Capture::open(&dir, "Main").unwrap().note("closed");

        let lines = lines(&dir);
        let notes: Vec<_> = lines.iter().map(|line| &line["note"]).collect();
        assert_eq!(
            notes,
            [
                &json!("connected"),
                &Value::Null,
                &Value::Null,
                &json!("connected"),
                &json!("closed")
            ]
        );
        assert_eq!(lines[1]["frame"]["Content"], "hi");
        assert_eq!(lines[2]["text"], "not json");
        assert!(lines.iter().all(|line| line["source"] == "Main"));
        assert!(
            lines[1]["at"]
                .as_str()
                .unwrap()
                .parse::<jiff::Timestamp>()
                .is_ok()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_full_capture_stops_with_a_note() {
        let dir = dir("full");
        let mut capture = Capture::open(&dir, "Main").unwrap();
        // As if the file were nearly full already.
        capture.written = MAX_BYTES - 10;
        capture.frame(r#"{"Type":"MESSAGE"}"#);
        capture.frame(r#"{"Type":"MESSAGE"}"#);
        let lines = lines(&dir);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1]["note"], "capture full");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
