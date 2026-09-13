# chat-aggregator (name pending)

*Multi platform, single chat.*

Aggregates live chat from Twitch, YouTube, and Rplay (soon™️) into a single unified
message stream, exposed as a stylable HTML overlay for OBS/browser sources.

## How it works

sources → mpsc → merger → broadcast → SSE → your overlay

Each platform is its own crate behind a `ChatSource` trait; the server never
knows what Twitch or YouTube look like, only `ChatMessage`.

| crate          | role                                        |
|----------------|---------------------------------------------|
| `chat-core`    | stable message model + source trait         |
| `chat-twitch`  | Twitch IRC (anonymous read)                 |
| `chat-youtube` | YouTube gRPC live chat (`streamList`)       |
| `chat-rplay`   | rplay — research in progress                |
| `chat-render`  | templates + CSS, emote replacement          |
| `chat-server`  | HTTP server, SSE, source orchestration      |

## Building

*Prerequisites*: a Rust toolchain (1.85+ / edition 2024) and `protoc` on
your `PATH` — `chat-youtube` compiles its protobuf schema at build time
via a build script.

Debian/Ubuntu:
```sh
apt install protobuf-compiler
```

macOS (brew):
```sh
brew install protobuf
```

Windows (use your package manager of choice): 
```powershell
winget install protobuf
choco install proto
scoop install proto
```

Nix: a [flake](./flake.nix) is provided — `nix develop` puts everything (toolchain,
protoc, etc.) on PATH. `direnv` works too. No manual setup needed.

Then the usual:
```sh
cargo build
cargo test          # or: cargo nextest run
```

## Running

See each crate's `examples/` for standalone listeners:

```sh
cargo run -p chat-twitch --example listen -- <channel>
YOUTUBE_API_KEY=... cargo run -p chat-youtube --example listen_youtube -- <video_id>
```

## Status

- [x] Twitch IRC source
- [ ] YouTube gRPC source (quota impact of reconnects under measurement)
- [ ] rplay source
- [ ] merger + broadcast
- [ ] HTML overlay + templates
