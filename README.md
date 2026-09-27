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
moderation: deletes, bans, clears). How the pieces fit together, and why:
[docs/architecture.md](docs/architecture.md).

| crate          | role                                        |
|----------------|---------------------------------------------|
| `chat-core`    | message model, source trait, hub, demo data |
| `chat-twitch`  | Twitch IRC (anonymous read)                 |
| `chat-youtube` | YouTube gRPC live chat (`streamList`)       |
| `chat-rplay`   | rplay — research in progress                |
| `chat-render`  | templates + CSS, emote replacement          |
| `chat-server`  | HTTP: overlay page, SSE stream, JSON API    |
| `chat-engine`  | runs sources, hub and server together       |
| `chat-app`     | the desktop app (GPUI)                      |

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

The desktop app: sources with status lights and on/off switches, YouTube
login and custom emoji, overlay themes, a test messages window and ⚙
settings (appearance, replay, pacing, port). Everything is saved to the
same config file the headless mode uses:

```sh
cargo run -p chat-app
```

The whole pipeline, headless:

```sh
cargo run -p chat-engine --example run -- --demo            # every message kind, no account needed
cargo run -p chat-engine --example run -- --init-config     # once: write a commented config.toml
cargo run -p chat-engine --example run                      # start everything in the config file
cargo run -p chat-engine --example run -- --youtube-login   # once; see docs/youtube-setup.md
cargo run -p chat-engine --example run -- --twitch <channel> --youtube-own   # sources as flags instead
```

The config file lives in the per-OS settings folder
(`~/.config/chat-aggregator/config.toml` on Linux). The terminal shows a
status light per source (🟢 🟡 🔴 ⚪) and how many overlays are connected.

YouTube needs your own (free) Google Cloud project, because YouTube's API
quota is counted per project: [docs/youtube-setup.md](docs/youtube-setup.md)
walks through it.

Sources can be combined; see the top of `crates/chat-engine/examples/run.rs`
for all flags. Then add `http://127.0.0.1:7878/` as a **Browser Source** in
OBS (e.g. 450×800).

**Styling:** themes are folders in `themes/` next to the config file, picked
in the app (or `theme = "name"` in `config.toml`, `--theme <name|dir>`
headless); see [docs/themes.md](docs/themes.md). Chat direction (newest at
the bottom or top): the app's dropdown, `newest = "top"` / `--newest top`,
or per browser source `?newest=top` in the overlay URL. To iterate without a
server: `cargo run -p chat-render --example preview -- <dir> > preview.html`.

**JSON API** for your own programs (games, bots, ...):
`ws://127.0.0.1:7878/api/v1/ws`, one JSON event per WebSocket message.
Off by default: switch it on in the app's settings (headless: `--api` or
`api = true` in `[server]`). See [docs/api.md](docs/api.md); formal spec in
[docs/asyncapi.yaml](docs/asyncapi.yaml). Quick look:
`cargo run -p chat-engine --example run -- --demo --api`, then
`websocat ws://127.0.0.1:7878/api/v1/ws`.

**Security:** chat is untrusted input. It reaches the overlay only through
`chat-render` (auto-escaped template, body pre-split into text/emote
parts), and the overlay page sends a Content-Security-Policy that allows
only its own script (by hash), so neither chat nor a hostile theme can run
code in OBS's unsandboxed browser (cf. CVE-2024-7971). API consumers must
insert chat as text too: see [docs/api.md](docs/api.md#security).

Standalone listeners for a single platform:

```sh
cargo run -p chat-twitch --example listen_twitch -- <channel>
YOUTUBE_API_KEY=... cargo run -p chat-youtube --example listen_youtube -- <video_id>
# optional: YOUTUBE_EMOJIS=export.json (from scripts/yt-emoji-export.user.js)
```

## Status

Details, design decisions and next steps: [ROADMAP.md](ROADMAP.md).

- [x] Twitch IRC source
- [x] YouTube gRPC source (OAuth login or API key; quota handling)
- [ ] rplay source
- [x] hub (broadcast + replay history)
- [x] HTML overlay + themes (SSE)
- [x] JSON WebSocket API
- [x] control plane + GPUI desktop app
- [ ] release builds / installers

## License

[MIT](LICENSE) © 2026 iSneeze
