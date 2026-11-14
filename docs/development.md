# Building and running from source

For developers: how to build chat-aggregator, run it without the desktop
app, and make a release. How the program is put together, and why, is in
[Architecture](architecture.md). If you only want to use the app, see
[Download and first steps](install.md).

## What you need

- A Rust toolchain (1.88 or newer; the code uses edition 2024).
- `protoc`, the protobuf compiler, on your `PATH`: the YouTube source
  compiles its protobuf schema when it's built.

| System | Installing `protoc` |
|--------|---------------------|
| Debian/Ubuntu | `apt install protobuf-compiler` |
| macOS | `brew install protobuf` |
| Windows | `winget install protobuf` (or `choco install proto`, `scoop install proto`) |
| Nix | nothing: `nix develop` (or direnv) provides the toolchain, `protoc` and the GUI libraries from the [flake](https://github.com/iSneeze/chat-aggregator/blob/master/flake.nix) |

## Build and test

```sh
cargo build
cargo test          # or: cargo nextest run
```

A plain `cargo build` leaves out the desktop app: its GUI toolkit (GPUI)
adds several hundred crates. Include it with `-p chat-app` or
`--workspace`.

## Run

The desktop app:

```sh
cargo run -p chat-app
```

The same engine without a window ("headless"), with a status light per
source in the terminal:

```sh
cargo run -p chat-engine --example run -- --demo            # every message kind, no account needed
cargo run -p chat-engine --example run -- --init-config     # once: write config.toml and profiles/Main.toml
cargo run -p chat-engine --example run                      # start the active profile
cargo run -p chat-engine --example run -- --profile Indie   # another profile
cargo run -p chat-engine --example run -- --youtube-login   # once per channel; see youtube-setup.md
cargo run -p chat-engine --example run -- --twitch <channel> --youtube-own   # sources as flags instead
```

Then open `http://127.0.0.1:7878/` in a browser, or add it as a Browser
Source in OBS.

- Both read the same settings, in the per-OS settings folder
  (`~/.config/chat-aggregator/` on Linux): `config.toml` with what all
  profiles share and which one is active, and a file per profile in
  `profiles/` with its sources and look. The app saves its changes there;
  headless mode only reads them.
- Sources can be combined. Every flag is listed at the top of
  [`run.rs`](https://github.com/iSneeze/chat-aggregator/blob/master/crates/chat-engine/examples/run.rs).
- YouTube needs your own Google Cloud project, headless too:
  [Connecting YouTube](youtube-setup.md).

### Useful while developing

```sh
# A theme rendered to one HTML file, without a server:
cargo run -p chat-render --example preview -- <theme dir> > preview.html

# The JSON API, watched with websocat:
cargo run -p chat-engine --example run -- --demo --api
websocat ws://127.0.0.1:7878/api/v1/ws

# One platform alone, printed to the terminal:
cargo run -p chat-twitch --example listen_twitch -- <channel>
YOUTUBE_API_KEY=... cargo run -p chat-youtube --example listen_youtube -- <video_id>
```

The settings behind these (themes, chat direction, the API switch) are
described in [Overlay themes](themes.md) and the
[Chat events API](api.md).

## The crates

Each platform is its own crate behind one trait (`ChatSource`); the server
never knows what Twitch or YouTube look like, only the common `ChatEvent`.

| crate | role |
|-------|------|
| `chat-core` | message model, source trait, hub, demo data |
| `chat-twitch` | Twitch IRC (anonymous read) |
| `chat-youtube` | YouTube gRPC live chat (`streamList`), login, quota |
| `chat-picarto` | Picarto chat WebSocket (anonymous read) |
| `chat-streamelements` | StreamElements tips (Overlay Token) |
| `chat-streamlabs` | Streamlabs tips (Socket API Token) |
| `chat-rplay` | RPlay chat on Amazon IVS (the streamer's widget key) |
| `chat-websocket` | what the WebSocket sources share: TLS, reconnecting |
| `chat-render` | message templates and CSS, emote replacement |
| `chat-server` | HTTP: overlay page, SSE stream, JSON API, widgets |
| `chat-reactions` | chat reactions: rules, Warudo and OBS outputs |
| `chat-engine` | runs sources, hub and server together; the config file |
| `chat-app` | the desktop app (GPUI) |

[Architecture](architecture.md) follows a message through all of them and
explains the security model: chat is untrusted input, so it only reaches
the overlay as escaped text, and the overlay's Content-Security-Policy
allows no script but its own.

## Builds and releases

Windows and Linux builds of the desktop app come from GitHub Actions
([`release.yml`](https://github.com/iSneeze/chat-aggregator/blob/master/.github/workflows/release.yml)):

- **Test build:** **Actions → Release → Run workflow**. The finished run
  has a zip (Windows) and a tar.gz (Linux) attached; downloading them needs
  a GitHub login. Ticking **RPlay capture** makes an RPlay test build
  (feature `rplay-capture`, archives named `…-rplay-capture`): RPlay's raw
  chat goes into `rplay-capture.jsonl` in the settings folder. For testers
  only, never for a release.
- **Release:** set `version` in `Cargo.toml`, commit, then push a matching
  tag to the GitHub repository (`git tag v0.1.0`, then push the tag). The
  same builds become a *draft* release with `SHA256SUMS`; check it under
  Releases, then publish it.

Good to know:

- Windows is built on Windows: GPUI compiles its DirectX shaders with the
  Windows SDK, so cross-compiling from Linux doesn't work.
- Linux is built on Ubuntu 22.04, so the binary runs on distributions from
  about 2022 on (glibc 2.35 or newer).
- Both are a single program, no installer. Each archive also carries the
  README, the license and `THIRD-PARTY-LICENSES.html`, generated by
  `cargo about` from `about.toml`.
- On NixOS, `nix build` in a checkout leaves the app in `./result/bin/`.
