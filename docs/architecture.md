# Architecture

How chat-aggregator is put together, and why it's built this way. For
building and running, see the [README](https://github.com/iSneeze/chat-aggregator#readme);
for the current state and next steps, [ROADMAP.md](https://github.com/iSneeze/chat-aggregator/blob/master/ROADMAP.md).

## The big picture

chat-aggregator reads live chat from several platforms, turns every
message into one common format, and hands it to whoever wants it: the OBS
overlay, other programs through a JSON API, and the desktop app's status
display.

```text
 Twitch (IRC) ──┐
 YouTube (gRPC)─┤                          ┌─ SSE /events ──────→ overlay page in OBS
 Demo ──────────┼─→ forwarder ─→  Hub  ────┤
 Test window ───┘   (counts)     (fan-out  └─ WebSocket /api/v1/ws → games, bots
                                  + history)
        ▲                                    ▲
        │ start / stop / restart             │ theme, pacing, API on/off, direction
 ┌──────┴─────────── engine (actor) ─────────┴──┐
 │  EngineHandle commands in, watch<Status> out │ ←── desktop app (GPUI) or headless `run`
 └──────────────────────────────────────────────┘
```

Two planes, kept apart on purpose:

- **Data plane**: chat flows sources → hub → consumers. It never passes
  through the control plane, so a chat flood can't make the app's buttons
  slow, and a slow command can't hold up chat.
- **Control plane**: what runs, with which settings, and how it's doing.
  The app (or headless mode) sends commands; the engine publishes a status.

Everything runs on the streamer's own computer. There is no server of ours:
the overlay and the API are served on `127.0.0.1` only.

## Crates

```text
chat-app ──→ chat-engine ──→ chat-server ──→ chat-render ──→ chat-core
                         ├─→ chat-twitch ─────────────────→ chat-core
                         └─→ chat-youtube ────────────────→ chat-core
```

| crate          | responsibility |
|----------------|----------------|
| `chat-core`    | the shared vocabulary: `ChatEvent`, `ChatMessage`, `MessageKind`, the `ChatSource` trait, the `Hub`, activity reports, demo data |
| `chat-twitch`  | Twitch chat over IRC (anonymous, read-only), converted to `ChatEvent`s |
| `chat-youtube` | YouTube live chat: finding the broadcast (REST), reading it (gRPC), OAuth login, quota handling, custom emoji |
| `chat-render`  | one message → HTML, through a theme (template + CSS); named theme folders |
| `chat-server`  | HTTP: overlay page, theme files, SSE stream, JSON WebSocket API, burst pacing |
| `chat-engine`  | owns everything at runtime: the control-plane actor, source restarts, status, config file |
| `chat-app`     | the desktop app (GPUI): windows, settings, login, test messages |
| `chat-rplay`   | placeholder for a future source |

Why so many crates? Each one has one job and only knows what it must.
`chat-server` never sees Twitch or YouTube, only `ChatEvent`s; the platform
crates never see HTML. That keeps each piece testable on its own, lets the
compiler enforce the boundaries (a crate can only use what it depends on),
and makes adding a platform a matter of adding a crate. Separate crates also
compile in parallel and only rebuild when they change.

`chat-engine` knows nothing about GUIs. The same engine runs headless from a
config file (`cargo run -p chat-engine --example run`) and inside the app.
`chat-app` is the only crate that depends on GPUI, and it's left out of the
workspace's default members because GPUI adds hundreds of dependencies.

## Life of a chat message

Follow one Twitch message from a viewer's keyboard to OBS:

1. **Source.** `chat-twitch` receives an IRC `PRIVMSG` with tags (display
   name, colour, badges, emote positions) and converts it into a
   `ChatEvent::Message(ChatMessage { … })`. Emote positions become
   `EmoteRef { code, url }`s; the URL is built from Twitch's emote id.
2. **Channel.** The source sends it into an `mpsc` channel (many producers,
   one consumer) that the engine created for this source.
3. **Forwarder.** A small task per source reads the channel, counts the
   message for the status display ("12 msgs") and publishes it to the hub.
4. **Hub.** `Hub::publish` records it in the replay history and sends it on
   a `broadcast` channel: every subscriber gets its own copy.
5. **Overlay stream.** Each connected overlay has an SSE stream subscribed
   to the hub. The message goes through the pacer (bursts are spread out),
   is rendered to HTML by the theme's template, and is sent as a named
   `chat` event.
6. **Overlay page.** The page's small script inserts the HTML into the chat
   list and removes the oldest messages beyond `--max-messages`.
7. **JSON API** (if switched on): the same event, as JSON, to every
   WebSocket client. No pacing, no HTML.

Moderation (a deleted message, a banned user, a cleared chat) takes the same
path as `ChatEvent::Delete`, `ClearUser` or `ClearAll`: the hub removes the
messages from its history, and overlays remove them from the page.

## The data model (`chat-core`)

```text
ChatEvent
├── Message(ChatMessage)        id, platform, author, text, emotes, timestamp, kind
├── Delete { platform, message_id }
├── ClearUser { platform, user_id }
└── ClearAll { platform }

MessageKind: Text | EmoteOnly | Donation { amount, tier } | Special { image_url, amount, info, tier }
           | MembershipJoin { info, months } | MembershipGift { count } | SystemNotice { info }
```

Every platform is normalised into these types. Their JSON form (defined by
serde attributes, e.g. `{"type": "message", …}`) *is* the public JSON API,
so it's treated as a contract: a test pins the format, a JSON Schema is
generated from the types (`docs/schema/chat-event.json`, checked by a test),
and only additive changes are allowed within `v1`. Missing values are
`null` rather than omitted, so typed languages get a stable shape.

Amounts stay display strings (`"€5.00"`, `"100 bits"`): platforms format
them in the viewer's currency, and the overlay only shows them. How *big*
an amount is comes as a `tier` on the platform's own scale instead: YouTube
reports one with every Super Chat (so no exchange rates on our side), and
Twitch bits are sorted into Twitch's cheer steps. Themes colour paid
messages by it.

## Sources

```rust
pub trait ChatSource {
    fn run(
        self,
        tx: mpsc::Sender<ChatEvent>,
        activity: Reporter,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}
```

(Implementations just write `async fn run(…)`. The trait spells out
`impl Future + Send` because the engine spawns each source on tokio's worker
threads, and only `Send` futures may move between threads; a plain
`async fn` in a trait can't promise that.)

- **`self` by value**: a source is consumed by the task that runs it. A
  restart builds a fresh one from its settings (`SourceConfig`), so no half-
  used state carries over.
- **`Ok(())`** means the stream ended normally (the broadcast is over),
  **`Err`** means it failed and the engine may retry.
- **`activity`** lets the source say what it's doing (connecting, receiving,
  idle, degraded, blocked until the quota resets), because only the source
  knows: a YouTube source waiting for its quota is "running" but delivers
  nothing for hours. This drives the status lights.

The engine picks the concrete source from an **enum** (`SourceSpec`), not a
trait object (`Box<dyn ChatSource>`). There are few source types, all known
at compile time, and a trait whose method returns `impl Future` can't be
used as a trait object anyway; an enum is simpler and costs nothing at
runtime.

**Twitch** uses the `twitch-irc` crate with an anonymous login: public chat
needs no account. Gift subs sent to many viewers at once arrive as one
summary plus one notice per recipient; the per-recipient ones are skipped so
the total isn't counted twice.

**YouTube** is the most involved source, because of how the API is billed:

- Every Google Cloud project gets 10,000 quota units a day. Streamers bring
  their own project (see [youtube-setup.md](youtube-setup.md)), so the
  OAuth client is a setting, not a constant.
- **Finding the chat** uses REST (`liveBroadcasts.list?mine=true` for your
  own broadcasts, `videos.list` for a given video): 1 unit per call. While
  nothing is live, the source polls this cheaply ("scan mode") instead of
  holding an expensive chat connection open.
- **Reading the chat** uses the gRPC `streamList` call. Google currently
  closes it every ~10 seconds (a bug on their side); the source reconnects
  with the last `page_token`, so nothing is lost. Each reconnect costs 5
  units, which is why a day's quota lasts roughly 5–6 streaming hours.
- **Quota used up** ("resource exhausted"): the source reports *Blocked*
  and sleeps until midnight Pacific time instead of failing or retrying
  pointlessly. A short-term rate limit only backs off briefly; when unsure
  which of the two it is, it's treated as the short one.
- **Login**: OAuth for desktop apps. The browser shows Google's consent
  page and redirects to a short-lived server on `127.0.0.1` (with PKCE, so
  an intercepted code is useless). The long-lived refresh token goes into
  the OS keyring (a file only the user can read if there's none);
  `TokenProvider` hands out access tokens and renews them before they
  expire.
- **Custom emoji**: the API only sends `:codes:`. A userscript exports the
  channel's codes and image URLs from YouTube's emoji picker; the source
  maps codes to images with it.

**Demo** sends sample messages of every kind, for styling themes without a
live chat. **Manual** is fed by the app's test window; it exists exactly as
long as that window (see below).

## The hub (`chat-core`)

The hub fans events out to any number of consumers and keeps the last N
messages (20 by default) so a consumer that connects late, like OBS
reloading the overlay, doesn't start from an empty chat.

- A `tokio::sync::broadcast` channel carries live events. A consumer that
  falls more than 256 events behind gets a "lagged" notice and skips ahead,
  instead of slowing everyone down.
- The history is a `VecDeque` behind a `std::sync::Mutex`. `publish` records
  and sends *while holding the lock*, and `subscribe` takes the snapshot and
  subscribes under the same lock. So every event is either in the snapshot
  or arrives live: never both (a duplicate) and never neither (a gap).
- It's a *std* mutex, not tokio's: it's only held for a few non-async
  steps, never across an `.await`.
- Moderation events are applied to the history too, so a replay never
  brings back a deleted message.

## Rendering and themes (`chat-render`)

A **theme** is a message template (`message.html`, [MiniJinja](https://docs.rs/minijinja))
plus a stylesheet (`overlay.css`). The built-in theme is compiled into the
binary; a theme folder overrides any file it has. Named themes live in
`themes/<name>/` next to the config file.

- The server renders messages to HTML, so the overlay page stays a tiny
  script and all the look is in the theme. Streamers only edit HTML and CSS.
- The template sees a **view model** (`MessageView`), not `ChatMessage`
  itself, so the core types can evolve without breaking people's templates.
  It borrows from the message instead of copying (`&'a str`): it only lives
  for one render call.
- The message body is split into text and emote parts *before* rendering,
  so the template writes the `<img>` tags and chat text never reaches the
  page as HTML (see Security).
- **Per-chatter looks**: the `seed` filter turns a value (usually
  `author.id`) into a stable number for the template, e.g. a hue or one of
  six shapes. It's our own fixed hash (FNV-1a plus MurmurHash3's
  finalizer), not Rust's standard hasher, which is randomised per process
  and may change between versions: themes rely on a chatter's look never
  changing. A test pins the results.
- Overlay behaviour that streamers may want to tune (how many messages stay,
  how long a deleted one fades) are CSS variables, read by the page's
  script: streamers only ever touch CSS.

## The server (`chat-server`)

Built on axum. All routes share a `ServerState`, cloned per request (cheap:
every field is an `Arc` or a small handle around one).

| route | what |
|-------|------|
| `/` | the overlay page (an OBS Browser Source) |
| `/theme/overlay.css`, `/theme/…` | the theme's CSS and files (images, fonts) |
| `/themes/<name>/…` | the same for a theme picked in the overlay URL (`/?theme=<name>`) |
| `/events` | Server-Sent Events: history, then live chat as rendered HTML |
| `/api/v1/ws` | WebSocket: live events as JSON (off by default) |

- **SSE for the overlay, WebSocket for the API.** SSE is one-way, built into
  every browser and reconnects by itself: ideal for a page that only
  listens. WebSocket works in every game engine and allows messages from
  the client later.
- **Per connection**, the overlay stream loads the current theme (so editing
  files and refreshing the source shows changes without a restart), replays
  the history, then streams live events through the pacer.
- **The pacer** (`stagger.rs`) spreads bursts (YouTube's reconnects deliver
  several messages at once) a few hundred milliseconds apart, but never
  delays a message by more than the configured maximum (≤ 5 s). Moderation
  is never delayed, and it removes matching messages still waiting. The API
  and the history replay are never paced.
- **Runtime settings** (theme folder, pacing, JSON API on/off, default chat
  direction) are `tokio::sync::watch` channels: one value, readable any
  time, plus a notification when it changes. A theme change makes every
  overlay stream send a `reload` event; switching the API off closes the
  connected clients. These bypass the engine's actor because they're server
  settings, not source state.
- **Chat direction**: the default (`newest` = bottom or top) is written into
  the page as `data-newest` when it loads; `?newest=` in the overlay URL
  overrides it, so each OBS scene can have its own. The page flips the list
  with `column-reverse` (the page order stays oldest-first, so inserting and
  trimming don't change) and pins the view to the newest message after every
  insert, with the browser's scroll anchoring switched off.
- **A theme per browser source**: `?theme=<name>` in the overlay URL pins
  that overlay to a theme from the themes folder (`default` = built-in),
  whatever the app's theme. The page links `/themes/<name>/overlay.css`
  (so `url(bg.png)` in it finds the theme's own files) and passes its query
  on to `/events`, which renders with that theme's template. Only valid
  theme names of existing folders count (so no `..`); anything else gets
  the app's theme, logged. The server knows the themes folder from
  startup; it's the only piece of the config it needs for this.
- **Counting connections** uses guards: each connection holds a value whose
  `Drop` counts down again, so the count is right however the connection
  ends (see Rust idioms).
- **Shutdown**: every stream ends when shutdown starts (SSE streams stop,
  WebSocket clients get a Close frame), otherwise graceful shutdown would
  wait forever for an open overlay.

## The engine (`chat-engine`)

### The actor

The control plane is an **actor**: one task owns all source state (which
sources exist, their settings, their running task, retry timers), and
everyone else talks to it through messages.

```text
EngineHandle ──Command + oneshot reply──→ actor task ──→ watch<Status> ──→ app, headless
 (cheap clone)                              │
                                            ├─ starts/stops source tasks
                                            └─ receives "ended", "retry due", "activity changed"
```

- **Why an actor?** Only one task ever changes the state, so it needs no
  locks, and commands can't interleave in surprising ways: they're handled
  one after another. The alternative, a shared `Mutex<State>` locked by
  every caller, is easy to get wrong across `.await`s.
- **`EngineHandle`** is a cheap-to-clone front: each method sends a
  `Command` with a `oneshot` channel (a channel used exactly once) and awaits
  the answer. The app, headless mode and tests each hold one.
- **`watch<Status>`**: the status (every source's state, health, message
  count, connected overlays and API clients) is published after every
  change. A `watch` always holds the latest value, so a slow reader just
  sees the newest state instead of a backlog.

### Restarts and health

- A source that fails is restarted with **exponential backoff**: 2 s,
  doubling up to 5 minutes; after 60 s of running fine the counter resets.
- Errors only the user can fix (`SetupError` such as an invalid channel
  name, `LoginRequired`) are **not** retried: the source waits in
  *NeedsAttention* until the settings change.
- Each start/stop bumps a **generation** number. Messages from an older run
  (a late "ended", a stale retry timer) carry the old number and are
  ignored, so a quick stop/start can't be undone by leftovers.
- **Health** (off / ok / warning / error, the traffic lights) is computed
  from state plus the source's reported activity, in `chat-engine`, so the
  app and headless mode always agree.

### Sources are built by a factory

The actor is generic over a `SourceFactory`: production builds real Twitch,
YouTube and demo sources; tests use scripted ones ("fails twice, then
runs"). This is static dispatch: the compiler creates one actor per factory
type, and the production code carries no test hooks.

### The config file

`config.toml` in the per-OS settings folder (`~/.config/chat-aggregator/` on
Linux) with `[server]`, `[youtube]`, `[app]` and `[[sources]]`:

- **`deny_unknown_fields`** everywhere: a typo like `vidoe_id` is an error
  naming the line, instead of being silently ignored (which would turn
  "this video" into "your own broadcast").
- **Every setting has a default**, so a missing line or section is fine.
- **Sources remember their on/off switch** (`enabled = false`, only written
  when off).
- **Written with mode 0600** (only your user can read it): it contains the
  OAuth client secret. Secrets are redacted in debug output.
- The engine only *reads* the file; the app writes it. Saving rewrites the
  file without comments (a known limitation).

## The desktop app (`chat-app`)

### Two runtimes side by side

- **GPUI** (through gpui-kit) owns the main thread: windows, input,
  drawing, with its own executor.
- **tokio** runs the engine on two worker threads.

They only talk through `tokio::sync` channels (`EngineHandle` commands, the
`watch` status), which can be awaited from either side: awaiting only needs
a waker, not the tokio runtime. So a GPUI task can `await
engine.add_source(…)` directly. Work that needs tokio itself (the OAuth
login's local server) is spawned on the tokio runtime and its `JoinHandle`
awaited from GPUI.

### Views and shared state

- **Windows**: the main window (status, theme row, sources, YouTube section,
  add form), the test messages window and the settings window. The side
  windows open once; clicking again brings them to the front.
- **`Entity<AppConfig>`**: the app's view of `config.toml`, shared by all
  windows. A GPUI entity is shared, reference-counted state (like
  `Rc<RefCell<…>>`, but GPUI hands out access and tracks changes), so no
  window works on a stale copy. `AppConfig` itself is plain Rust, tested
  without GPUI, and changes are saved right after the engine confirms them.
- **The test window's source** lives exactly as long as the window: it's
  added when the window opens and removed when the view is released (window
  closed). The window holds a sender; dropping it ends the source.
- **The settings window** applies edits after a short pause (a "debounce"):
  settings fields report every keystroke, and typing "25" over "20" must not
  briefly shrink the replay history to 2. Each keystroke replaces a pending
  timer task; dropping a GPUI `Task` cancels it. Closing the window applies
  what's pending. Switches (appearance, JSON API) apply at once.
- **Logging** goes to the console (none on Windows release builds:
  `windows_subsystem = "windows"`) and to `chat-aggregator.log` in the
  settings folder (the previous start's kept as `chat-aggregator.old.log`,
  so restarting after a crash doesn't erase it). A panic hook and the
  error `main` returns are logged too: the file is what a tester sends.
- **Appearance** is gpui-kit's global `Theme`: System follows the OS
  light/dark mode; Light, Dark and High contrast (our own theme file) stay
  put.

## Security model

The threat that matters most is **chat itself**: anyone can type anything.
OBS runs browser sources without Chromium's sandbox, so script injected into
an overlay can be one browser bug away from the streamer's computer (this
happened to another overlay: CVE-2024-7971).

1. **Chat never becomes markup.** It reaches HTML only through the
   auto-escaping template; the body is pre-split into text and emote parts;
   author colours must be plain `#hex`; ids go through `CSS.escape` in the
   page's selectors. A test feeds classic attack strings into every field
   of every message kind and checks the markup doesn't change.
2. **The overlay only runs its own script.** The page sends a
   Content-Security-Policy whose `script-src` is the SHA-256 hash of its
   inline script (computed from the served text, so they can't drift apart).
   Injected handlers like `onerror=`, foreign scripts, frames and plugins are
   blocked, even from a hostile theme. Theme files get `nosniff` and
   `script-src 'none'`.
3. **Localhost only.** The server binds `127.0.0.1`: other machines can't
   reach it. Web pages in the streamer's browser can, which is why the JSON
   API is **off by default** and closes its clients when switched off.
4. **No reading outside the theme.** `/theme/…` accepts only plain path
   segments, so `../config.toml` (with the client secret) can't be fetched.
5. **Secrets**: the refresh token in the OS keyring, the config file 0600,
   redacted debug output, API keys sent as headers (never in URLs, which end
   up in logs).

## Testing

- **Real servers on port 0**: server and engine tests bind `127.0.0.1:0`,
  so the OS picks a free port and tests run in parallel.
- **A paused clock** (`#[tokio::test(start_paused = true)]`): "the 3rd retry
  after backoff" takes no real time and is exact.
- **Scripted sources** through the factory test the actor's restart logic
  without any network.
- **Headless UI tests** (`#[gpui_kit::test]`): real windows rendered
  invisibly, driven by simulated clicks and typing.
- **Contract tests** pin the JSON format and the generated schema.
- **Mutation checks**: for important tests, the guarded code was
  deliberately broken once to confirm the test fails.
- What can't be tested offline (real platforms, real OBS) is tracked as
  "pending live verification" in the ROADMAP.

## Rust idioms you'll meet in the code

- **Channels as the glue.** `mpsc` (many senders, one receiver: sources →
  forwarder, commands → actor), `broadcast` (one sender, every receiver gets
  a copy: the hub), `watch` (the latest value plus change notifications:
  status and server settings), `oneshot` (one reply to one command).
- **Don't hold a lock across `.await`.** A guard held while a task is paused
  blocks everyone else and makes the future non-`Send`. The code copies
  values out first (`*watch.borrow()` for `Copy` types), limits guards to a
  block, or moves the waiting into a small `async fn` so the guard is gone
  before the next `.await`.
- **RAII guards.** A value whose `Drop` undoes something: connection
  counters, `AbortOnDropHandle` (dropping it stops the task), GPUI
  `Subscription`s and `Task`s. Cleanup can't be forgotten, whatever path the
  code takes.
- **Ownership as lifetime.** A source consumed by its task, the test
  window's sender whose drop ends the source, weak handles
  (`WeakEntity`) in callbacks so they don't keep a closed window's view
  alive.
- **Generics over trait objects** where the set of types is known (the
  source factory, `SourceSpec`): static dispatch, no allocation, no
  object-safety limits.
- **`Arc`** where several owners share read-mostly data across threads
  (the hub, the server state).
- **`LazyLock`** for values computed once on first use (the overlay's CSP
  header).
- **Edition 2024's `impl Trait + use<>`**: says a returned value borrows
  nothing, so UI rows can be built in a loop.
