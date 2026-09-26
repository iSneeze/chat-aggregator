# chat-aggregator (name pending)

*Multi platform, single chat.*

Website and docs: <https://isneeze.github.io/chat-aggregator/>

Aggregates live chat from Twitch, YouTube, and Rplay (soon™️) into a single unified
message stream, exposed as a stylable HTML overlay for OBS/browser sources.

## How it works

sources → mpsc → hub (broadcast + replay history) ─┬→ SSE → your overlay
                                                    └→ WebSocket → your programs (JSON API)

Each platform is its own crate behind a `ChatSource` trait; the server never
knows what Twitch or YouTube look like, only `ChatEvent`s (messages plus
moderation: deletes, bans, clears).

| crate          | role                                        |
|----------------|---------------------------------------------|
| `chat-core`    | message model, source trait, hub, demo data |
| `chat-twitch`  | Twitch IRC (anonymous read)                 |
| `chat-youtube` | YouTube gRPC live chat (`streamList`)       |
| `chat-rplay`   | rplay — research in progress                |
| `chat-render`  | templates + CSS, emote replacement          |
| `chat-server`  | HTTP: overlay page, SSE stream, JSON API    |
| `chat-engine`  | runs sources, hub and server together       |

## Building

*Prerequisites*: a Rust toolchain (1.88+ / edition 2024) and `protoc` on
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

The whole pipeline, headless:

```sh
cargo run -p chat-engine --example run -- --demo                  # every message kind, no account needed
cargo run -p chat-engine --example run -- --twitch <channel>
cargo run -p chat-engine --example run -- --youtube-own         # your own broadcast, after logging in:
cargo run -p chat-engine --example run -- --youtube-login       # once; see docs/youtube-setup.md
YOUTUBE_API_KEY=... cargo run -p chat-engine --example run -- --youtube <video_id>   # any public video (testing)
```

YouTube needs your own (free) Google Cloud project, because YouTube's API
quota is counted per project: [docs/youtube-setup.md](docs/youtube-setup.md)
walks through it.

Sources can be combined; see the top of `crates/chat-engine/examples/run.rs`
for all flags. Then add `http://127.0.0.1:7878/` as a **Browser Source** in
OBS (e.g. 450×800).

**Styling:** pass `--theme <dir>` with your own `message.html` and/or
`overlay.css` (start from the defaults in `crates/chat-render/templates/`).
Both are re-read when the overlay (re)connects: edit, then hit *Refresh* on
the browser source. To iterate without a server:
`cargo run -p chat-render --example preview -- <dir> > preview.html`.

**JSON API** for your own programs (games, bots, ...):
`ws://127.0.0.1:7878/api/v1/ws`, one JSON event per WebSocket message.
See [docs/api.md](docs/api.md); formal spec in
[docs/asyncapi.yaml](docs/asyncapi.yaml). Quick look:
`websocat ws://127.0.0.1:7878/api/v1/ws`.

Standalone listeners for a single platform:

```sh
cargo run -p chat-twitch --example listen_twitch -- <channel>
YOUTUBE_API_KEY=... cargo run -p chat-youtube --example listen_youtube -- <video_id>
# optional: YOUTUBE_EMOJIS=export.json (from scripts/yt-emoji-export.js)
```

## Status

Details, design decisions and next steps: [ROADMAP.md](ROADMAP.md).

- [x] Twitch IRC source
- [ ] YouTube gRPC source (quota impact of reconnects under measurement)
- [ ] rplay source
- [x] hub (broadcast + replay history)
- [x] HTML overlay + templates (SSE)
- [x] JSON WebSocket API
- [ ] control plane + GPUI app
