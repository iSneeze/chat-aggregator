# Architecture

How chat-aggregator is put together, and why it's built this way. For
building and running, see [Building and running from source](development.md).

## The big picture

chat-aggregator reads live chat from several platforms, turns every
message into one common format, and hands it to whoever wants it: the OBS
overlay, other programs through a JSON API, chat reactions (chat makes
things happen in other programs, e.g. Warudo), and the desktop app's status
display.

```text
 Twitch (IRC) ──┐
 YouTube (gRPC)─┤                          ┌─ SSE /events ──────→ overlay page in OBS
 Picarto (WS) ──┤                          │
 RPlay (WS) ────┤                          │
 StreamElements ┤                          │
 Streamlabs ────┤                          │
 Demo ──────────┼─→ forwarder ─→  Hub  ────┼─ WebSocket /api/v1/ws → games, bots
 Test window ───┘   (counts)     (fan-out  │
                                  + history)└─ reactor (rules) ───→ Warudo, OBS (WebSocket)
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
the overlay and the API are served on `127.0.0.1` only. (Chat reactions
connect *out* to programs the streamer names, on this computer by default.)

## Crates

```text
chat-app ──→ chat-engine ──→ chat-server ──→ chat-render ──→ chat-core
                         ├─→ chat-twitch ─────────────────→ chat-core
                         ├─→ chat-youtube ────────────────→ chat-core
                         ├─→ chat-picarto ────────┐
                         ├─→ chat-rplay ──────────┤
                         ├─→ chat-streamelements ─┼─→ chat-websocket ─→ chat-core
                         ├─→ chat-streamlabs ─────┘
                         └─→ chat-reactions ──────────────→ chat-core
```

| crate          | responsibility |
|----------------|----------------|
| `chat-core`    | the shared vocabulary: `ChatEvent`, `ChatMessage`, `MessageKind`, the `ChatSource` trait, the `Hub`, activity reports, demo data; the secret store (keyring) |
| `chat-twitch`  | Twitch chat over IRC (anonymous, read-only), converted to `ChatEvent`s |
| `chat-youtube` | YouTube live chat: finding the broadcast (REST), reading it (gRPC), OAuth login, quota handling, custom emoji |
| `chat-render`  | one message → HTML, through a theme (template + CSS); named theme folders |
| `chat-server`  | HTTP: overlay page, theme files, SSE stream, JSON WebSocket API, burst pacing |
| `chat-engine`  | owns everything at runtime: the control-plane actor, source restarts, status, config file |
| `chat-app`     | the desktop app (GPUI): windows, settings, login, test messages |
| `chat-picarto` | Picarto chat over the website's WebSocket (anonymous, read-only), its emotes, converted to `ChatEvent`s |
| `chat-rplay`   | the streamer's RPlay chat (Amazon IVS, with the toolkey from a widget link), its emotes and display rules, converted to `ChatEvent`s |
| `chat-streamelements` | tips through StreamElements (its Astro event gateway, with the streamer's Overlay Token), converted to donation `ChatEvent`s |
| `chat-streamlabs` | tips through Streamlabs (its Socket API, socket.io 2 by hand, with the account's Socket API Token), converted to donation `ChatEvent`s |
| `chat-websocket` | what the WebSocket sources share: `connect` (a `wss://` connection with one TLS setup for the whole app) and `keep_connected` (reconnecting with growing pauses, the status while waiting, giving up on a `SetupError`) |
| `chat-reactions` | chat reactions: rules (trigger, guards, cooldowns) and outputs to other programs (Warudo, OBS) |

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
8. **Chat reactions**: the reactor checks the message against the rules;
   if it's `!bonk` and a rule says so, Warudo gets `{"action":"throw_item"}`
   right away (not in step with the overlay's pacing).

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

MessageKind: Text | EmoteOnly | Donation { amount, tier, value, currency } | Special { image_url, amount, info, tier, value, currency }
           | MembershipJoin { info, months } | MembershipGift { count } | SystemNotice { info }
```

Every platform is normalised into these types. Their JSON form (defined by
serde attributes, e.g. `{"type": "message", …}`) *is* the public JSON API,
so it's treated as a contract: a test pins the format, a JSON Schema is
generated from the types (`docs/schema/chat-event.json`, checked by a test),
and only additive changes are allowed within `v1`. Missing values are
`null` rather than omitted, so typed languages get a stable shape.

Amounts are display strings (`"€5.00"`, `"500 bits"`), as platforms
format them, plus the number (`value`) and its `currency`. Nothing is ever
converted (decided 2026-11-07: rates drift, and a $50 Super Chat arriving
as €49 must not miss a "50+" rule). How *big* an amount is comes as a
`tier` from 1 to 7 on **one scale for every platform**, YouTube's Super
Chat tiers (`chat_core::tiers`): YouTube reports its tier with every Super
Chat, in every currency; bits use YouTube's dollar steps × 100; tips start
where the streamer says (`[tips]`, in their tip page's currency; a
Streamlabs tip in another currency is tier 1, its worth unknown). Themes
colour paid messages by it, and reaction rules compare it.

```text
tier        1       2      3      4        5        6        7
YouTube     $1+     $2+    $5+    $10+     $20+     $50+     $100+
Twitch      1+      200+   500+   1,000+   2,000+   5,000+   10,000+ bits
tips        <2      2+     5+     10+      20+      50+      100+   (default)
```

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
needs no account. It connects over rustls like everything else, so the app
links no OpenSSL; rustls's default crypto backend is chosen at the start of
`TwitchSource::run`, since twitch-irc relies on one being set. Gift subs sent to many viewers at once arrive as one
summary plus one notice per recipient; the per-recipient ones are skipped so
the total isn't counted twice.

**YouTube** is the most involved source, because of how the API is billed:

- Every Google Cloud project gets 10,000 quota units a day. Streamers bring
  their own project (see [youtube-setup.md](youtube-setup.md)), so the
  OAuth client is a setting, not a constant.
- **Finding the chat** uses REST (`liveBroadcasts.list?mine=true` for your
  own broadcasts, `videos.list` for a given video): 1 unit per call. While
  nothing is live, the source polls this cheaply ("scan mode") instead of
  holding an expensive chat connection open: every 2 min with nothing
  coming up, every 30 s from 5 min before a scheduled start. A scheduled
  broadcast more than 3 h late counts as *stale* (a "free chat" waiting
  room, a forgotten draft) and isn't waited for; otherwise it would look
  like "starting any minute" forever.
- **Reading the chat** uses the gRPC `streamList` call. Google currently
  closes it every ~10 seconds (a bug on their side); the source reconnects
  with the last `page_token`, so nothing is lost. Each reconnect costs 5
  units, which is why a day's quota lasts roughly 5–6 streaming hours.
  While connections end without delivering anything (a quiet chat), the
  wait before the next one grows from 2 s to at most 20 s, which saves
  quota; it drops back to 2 s once messages flow. A closed chat
  (`FailedPrecondition`) is the normal end of your broadcast: back to scan
  mode, not an error. A connection that dies without a word (Wi-Fi switch, laptop sleep) would
  leave that read waiting forever, so a stream that stays completely silent
  for 60 s counts as dead: the source opens a fresh connection and resumes
  with the same `page_token`.
- **Quota used up** ("resource exhausted"): the source reports *Blocked*
  and sleeps until midnight Pacific time instead of failing or retrying
  pointlessly. A short-term rate limit only backs off briefly; when unsure
  which of the two it is, it's treated as the short one.
- **Login**: OAuth for desktop apps. The browser shows Google's consent
  page and redirects to a short-lived server on `127.0.0.1` (with PKCE, so
  an intercepted code is useless). The long-lived refresh token goes into
  the OS keyring (a file only the user can read if there's none);
  `TokenProvider` hands out access tokens and renews them before they
  expire. The app asks the engine for all of it (`youtube_accounts`,
  `youtube_login`, `youtube_logout`): the engine has the client in its
  settings and the secret store, runs the login on its own tokio runtime
  and restarts the YouTube sources afterwards. Cancelling a login is
  dropping it (`YouTubeLogin`), which stops the local server.
- **Several channels**: a login belongs to one channel, the one picked on
  Google's page (a Google account can own several, as brand accounts), so
  a streamer with several channels logs in once per channel. The consent
  URL says `prompt=select_account consent`: without `select_account`,
  Google may quietly reuse the account signed in to the browser, and
  "Add channel" would log in the first channel again. After the code
  exchange, `channels.list` (1 unit) says which channel it was, and the
  token is stored under it (`youtube:<client id>:<channel id>`, chat-youtube's
  `Account`). The keyring can't list what it holds, so the settings list
  the connected channels (`[[youtube.accounts]]`: channel id and name, no
  secrets), and each YouTube source names its channel (`account`; without
  one, the first listed). The factory keeps one `TokenProvider` per
  account, shared by the sources using it. Checking the logins costs no
  quota: a token refresh says whether Google still accepts each one.
  All channels share the one Google project's quota.
- **The login of older versions** (one per client, `youtube:<client id>`,
  channel unknown) is moved by the app's first check
  (`EngineHandle::youtube_accounts`): `adopt_legacy_login` asks whose it is
  (1 unit) and stores a copy under the channel; the channel goes into the
  settings, and only once they're saved is the old entry deleted. Every
  step that fails leaves the old entry, which keeps working (sources
  without an `account` use it while no channel is listed) and is moved at
  the next check, so nobody logs in again. One Google rejects is removed.
  Headless mode never saves, so it keeps using the old entry.
- **Custom emoji**: the API only sends `:codes:`. While a broadcast's
  chat is read, a second task (`web_chat.rs`) follows the same chat on
  YouTube's web chat page, logged out, and picks the emoji (codes and
  picture) out of every message there; the messages themselves still come
  from the API. Unofficial, so best effort: its failures are logged and
  retried slowly, never shown as a source problem. It sends what it finds
  over a channel; the run loop takes it in before converting each batch
  of messages (`BroadcastEmojis` in `emoji_cache.rs`, the only owner of
  the map, so no lock) and saves it to the channel's cache
  (`youtube-emojis/<channel id>.json`; codes are only unique within a
  channel) at most every 30 s, and when the broadcast's state is dropped:
  that also happens when the source is switched off mid-`await`. A
  userscript export of the whole emoji picker can be loaded on top
  (learned emoji win: newer).

**Picarto** has no public chat API, so the source does what Picarto's
website does for a logged-out visitor: look the
channel up in the public API (a wrong name is a `SetupError`, which waits
for the user), get an anonymous token from the
site's GraphQL API, open the chat WebSocket. Frames are compact JSON and
read loosely (`serde_json::Value`): the format is undocumented, so a
message missing what we need is skipped instead of failing the connection.
Like the tip services, it reconnects by itself rather than letting the
engine restart it: a restart builds a new source, and each one downloads
the site's 1.4 MB script once for the emote tables (Picarto's own emotes
are `data:` images in it, emoji names map to unicode). The reconnecting is
chat-websocket's `keep_connected`, shared by all three WebSocket sources:
3 s after a connection ends, twice as long after each quick failure in a
row (up to 2 minutes), back to 3 s once a connection lasted a minute, the
reason and the wait in the source's status, and a `SetupError` ends it
(and right away, without a pause, after a server restart or a planned
session renewal).
Their TLS comes from there too (`connect`): rustls with its aws-lc-rs
backend chosen explicitly (the process-wide default panics when both
backends are built in, as in the app), set up once for the whole app.
Picarto's, RPlay's and Streamlabs' log lines carry the channel or label (a
tracing span), as several can run at once. It pings every 30 s
as the site does; 75 s without any frame counts as a dead connection. In a
multistream the channels share one chat, and all of it is shown, as on
Picarto; only the configured channel's own streamer gets the
`broadcaster` badge. Tips, subscriptions and raids aren't converted yet:
none has been captured.

**StreamElements** is not a chat but a tipping service: every message is a
donation. The source connects to Astro, StreamElements' documented event
gateway (a plain WebSocket with JSON), and subscribes to the channel's
activity feed with the streamer's **Overlay Token**. That token may not
read Astro's tips topic, but tips arrive in the activity feed too; the
stronger JWT is never needed. The feed also relays the platforms' own
events (Super Chats, bits, subs), which the platform sources show already,
so the source keeps only `type: "tip"`: an allow-list, so nothing
StreamElements adds later can appear twice. Tips emulated from the
dashboard are kept too (they're how streamers rehearse alerts and
reactions). Amounts come without a currency, in the one the streamer set
for their tip page: the streamer's tip settings (`TipSettings`: currency,
tier starts) say both, read from a `watch` channel for every tip, so a
change in Settings applies to the next tip without a reconnect. It's one
channel for everything that reads the tip settings: `Engine::start_with`
creates it, builds the source factory with its receiver (each tip source
gets a copy) and gives the reactor's amount rules another, and the
actor's `Controls` sends every change. The frame parser
doesn't see those settings: it gives a `Tip` (`chat_core::tiers`) with
what StreamElements sent, and the source turns it into a donation
message with the settings of the moment (`Tip::into_message`, which uses
`TipSettings::donation`, the one place that decides a tip's amount text,
tier and currency for both tip services and the test window). The token lives in the secret
store under the account id, never in `config.toml`:
`EngineHandle::add_source_with_token` stores it and adds the source,
the factory reads it at every start (a missing one is a `SetupError`), and
removing the last source of that account deletes it. An update can't move
a source to another account (`update_source` refuses with a
`SetupError`): its token would stay behind unused. A refused token is a
`SetupError` and waits for the user. Like Picarto, it reconnects by
itself (`keep_connected`), right away when Astro announces a server
restart; 75 s without a frame (Astro pings every 30 s) is a dead
connection.

**Streamlabs** is a tipping service too. Its Socket API is socket.io 2
(Engine.IO 3), which no Rust crate speaks anymore, so the little it takes
is done by hand on a plain WebSocket: the token goes in the address, the
open packet says how often to ping, and the *client* pings (`2`, answered
by `3`; without pings Streamlabs drops the connection). Events come as
`42["event",{…}]`. The socket carries much more than tips (alert box and
tip page settings, totals, the platforms' relayed events): the source keeps
only `type: "donation"` from the tip page (`for: "streamlabs"`, or none).
Test donations from the dashboard (`isTest`) count like real ones; the
alert box editor's previews (`isPreview`, a fake alert every ~18 s while
it's open) never do. Unlike StreamElements, Streamlabs reports each tip's
currency: tiers come from the same `TipSettings` (through the same
`Tip`), but a tip in another currency than the tip currency gets tier 1
(nothing is converted). A
streamer can have several accounts (a VTuber with a corporate and an indie
persona), and the token doesn't say whose account it is, so each source has
a **label** the streamer picks: the token lives in the secret store under
it (the same `add_source_with_token`: the token handling is one piece of
code for both tip services, in chat-engine's `tokens.rs`, and all that
differs between them, where a token is kept and what it's called, is in
`SourceConfig::token`). A wrong token is
answered with `44"Authentication error"` and the connection stays open: the
source turns it into a `SetupError` and waits for the user. The server
speaks only TLS 1.2, hence rustls' `tls12` feature (in chat-websocket,
whose TLS setup all WebSocket sources share). The token is blanked out of
connection errors, which could quote the address it's in.

**RPlay** has no public chat API either; its chat runs on Amazon IVS
Chat. The source joins it the way RPlay's own OBS widgets do, never
anonymously: a channel that asks viewers to log in (even just to follow)
gives anonymous visitors no chat. The streamer pastes a widget link from
their RPlay studio; only its **toolkey** is kept
(`SourceConfig::prepare_token` takes it out of the link, so the add form
can say right away when a link has none), in the secret store under a
label, like a
Streamlabs token (same `tokens.rs`). Each connection trades the key for a
live-tool session that acts as the streamer (a refused key is a
`SetupError`), asks for the channel's state and a chat token as the
streamer, and opens the IVS WebSocket with the token as its subprotocol.
The key can do much more than read chat (moderation, stream settings), so
it never reaches a log, and the channel state is read into a struct that
names only the fields used: serde drops the rest, the stream key the
answer carries included. IVS ends a session after 60 minutes; the source
ends it 30 s earlier itself (`Ended::Renewing`) and `keep_connected`
connects again at once. A connection starts from scratch (session, state,
emote lists), so nothing has to survive between connections. What RPlay
works out in each viewer's browser happens in the source too
(`display.rs`): name colours from the user id, "AnonAru-NNNNN" for hidden
names in a channel with anonymous chat (the real name is in every message,
so leaving it out is on us), the streamer's banned words starred out.
Frames are read loosely like Picarto's (`frames.rs`, with a `Room` that
follows the channel's managers, banned words and anonymous-chat setting as
RPlay announces changes); donations carry no sender name, so it's looked
up (or taken from the sender's earlier chat). Profile pictures have a fixed
address, but most chatters have none (S3 answers 403), so `avatars.rs`
checks each sender's once per connection with a `HEAD` request before
their message goes out; it waits, so chat stays in order, but at most a
second, and pauses after repeated failures. A sender who hid their profile
gets none, as on RPlay. The first event and the first
message of each kind, frames of unknown types and messages the source
can't read go into the log with their field names only, so a tester's log
teaches us RPlay's chat without containing it. For a first test there are
test builds (the `rplay-capture` feature, a box on the hand-started release
workflow) that also save every chat frame as it came to
`rplay-capture.jsonl` (`capture.rs`: chat included, values under secret
sounding names starred out, never the HTTP answers, which carry the stream
key); release builds don't have that code. It stays for debugging: RPlay
has no stable API, so when its chat changes, a capture build for an
affected streamer shows how.

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
plus a stylesheet (`overlay.css`). The built-in themes (`Builtin`:
Default and Minimal) are compiled into the binary; a theme folder overrides
any file of Default it has. Named themes live in `themes/<name>/` next to
the config file. Which one the overlay uses is a `ThemeSource`: a built-in
theme or a folder (their names can't clash: folder names that are
built-in names are refused).

- The server renders messages to HTML, so the overlay page stays a tiny
  script and all the look is in the theme. Streamers only edit HTML and CSS.
- The template and the stylesheet are loaded separately
  (`ThemeSource::load` gives the template as a `Theme`, `ThemeSource::css`
  the stylesheet), so a broken one never takes the other down: a template
  with a mistake falls back to the built-in one (logged), while the
  theme's CSS still applies.
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
| `/avatar/<key>` | a chatter's profile picture, downloaded once for all overlays |
| `/api/v1/ws` | WebSocket: live events as JSON (off by default) |
| `/widgets/<name>` | a widget: a page that reads the JSON API (built in: `live-emoji`, `danmaku`, `timer`) |

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
- **Runtime settings** (theme, pacing, JSON API on/off, default chat
  direction, what overlays hide) are `tokio::sync::watch` channels: one value, readable any
  time, plus a notification when it changes. A theme change makes every
  overlay stream send a `reload` event; switching the API off closes the
  connected clients. The engine sets them when the settings change (see
  [Settings](#settings)).
- **Chat direction**: the default (`newest` = bottom or top) is written into
  the page as `data-newest` when it loads; `?newest=` in the overlay URL
  overrides it, so each OBS scene can have its own. The page flips the list
  with `column-reverse` (the page order stays oldest-first, so inserting and
  trimming don't change) and pins the view to the newest message after every
  insert, with the browser's scroll anchoring switched off.
- **What an overlay shows** (`hide.rs`): the settings' `hide` list
  (platforms and kinds of message, by their API names) or the overlay
  URL's `?hide=`, which wins. Filtered in `/events` before the pacer and in
  the history replay, so a hidden message takes no pacing slot and doesn't
  crowd the replay; moderation always passes. Only the overlay: the JSON
  API and the reactions are the hub's other readers and see everything.
  The app's list is read from its `watch` channel for every message, so a
  change applies without a reload. Both `match`es from platform and kind to
  a name are exhaustive: a new platform or kind doesn't compile until it
  has a name.
- **A theme per browser source**: `?theme=<name>` in the overlay URL pins
  that overlay to a theme from the themes folder or a built-in one,
  whatever the app's theme. The page links `/themes/<name>/overlay.css`
  (so `url(bg.png)` in it finds the theme's own files) and passes its query
  on to `/events`, which renders with that theme's template. Only valid
  theme names of existing folders count (so no `..`); anything else gets
  the app's theme, logged. The server knows the themes folder from
  startup; it's the only piece of the config it needs for this.
- **Avatars go through the server** (`avatars.rs`). YouTube's image
  server rate-limits per IP address, and with every browser source loading
  every avatar straight from Google, a busy chat got `429`s. So before a
  message is rendered for the overlay, a Google avatar URL is registered
  and replaced by `avatar/<key>` (a hash of the URL); the server downloads
  each one once and keeps it in memory (bounded in number and age). One
  `tokio::sync::OnceCell` per avatar makes simultaneous requests share one
  download, and a `429` pauses all downloads for a while. Only URLs that
  came in through chat and point at Google's image hosts are ever
  registered, so the route can't be used to make the server fetch other
  addresses. The JSON API keeps the platform's own URL.
- **Widgets** (`widgets.rs`) are single HTML files that read the JSON API
  and show chat their own way (`examples/live-emoji.html`,
  `examples/danmaku.html` and `examples/timer.html` are built in,
  compiled into the binary with `include_str!`). A widget keeps its own
  state, if any, in the page's `localStorage` (the Timer: where it is,
  so it survives OBS reloading it); the server knows nothing of it.
  The server serves them, rather than OBS opening them as files, so their
  settings can go into the URL: OBS's "Local file" field takes no query,
  and a page from a `file://` URL can't use the API (its origin is
  `null`). A widget describes its settings in a JSON block,
  `<script type="application/json" id="widget">`; the page reads its
  defaults from it, and the app reads it to know what a widget offers.
  `Widget::from_page` reads that block into typed settings (an enum per
  type, so a number always has its limits) and refuses pages that don't
  fit, saying why. The scan for `<script>` elements is a small one, not
  an HTML parser: it skips comments and ends a script at the first
  `</script`, like browsers do. Imported widgets are files in the
  `widgets` folder next to `config.toml`, read on every request like theme
  files (an edit shows on the next refresh); the import names them after
  their title and checks them with the same `from_page`. A built-in name
  always wins. An API client whose `Origin` is the server's own address is
  one of its widgets (`api::is_own_page`), counted separately in the status
  for the main window.
- **Counting connections** uses guards: each connection holds a value whose
  `Drop` counts down again, so the count is right however the connection
  ends (see Rust idioms).
- **Shutdown**: every stream ends when shutdown starts (SSE streams stop,
  WebSocket clients get a Close frame), otherwise graceful shutdown would
  wait forever for an open overlay.

## Chat reactions (`chat-reactions`)

Chat can make things happen in other programs: `!bonk` throws an item at
the model in Warudo, a big Super Chat puts a voice filter on the mic in
OBS for 30 seconds. The streamer writes
**rules** (`[[reactions.rules]]` in `config.toml`); each has three parts,
checked in this order:

1. **Trigger**: which messages. A command (`!bonk` as the first word), a
   keyword (whole words, any case), an emote (a real one, as the platform
   marked it, not the code typed as text), a paid message (of a minimum
   tier, picked from a list in the app; or of an amount, at least or
   exactly, compared in whole cents as `Amount`), a membership, or gifts
   (a minimum count). Amount rules compare `value` as it is: bits as bits,
   RPlay coins as coins, money only in the tip currency (a ¥69 Super Chat
   isn't $69), so the reactor watches the tip settings too.
2. **Guards**: who, how many and how often. A minimum role, platforms, a
   crowd, a cooldown for the rule and one per chatter. A struct, not a
   list of guards: each exists once, and "all must pass" needs no rules
   about duplicates.
   - **Roles** come from badges, which platforms name differently (Twitch
     `broadcaster`, YouTube `owner`): translated in one place
     (`Role::of`) into an ordered enum, so the guard is `role >= min_role`.
   - **Cooldowns start only when every guard has passed.** Otherwise a
     viewer spamming a mods-only command would keep it resting for the
     mods.
   - **A crowd** ("10 different chatters within 30 s") makes a rule about
     the chat as a whole. It counts chatters, not messages, by (platform,
     user id): one person spamming counts once, so nobody fires it alone,
     and Twitch and YouTube chatters add up, which a bot on one platform
     can't do. Only messages that passed the role and platform guards
     count, and none while the rule rests, so it doesn't go off the moment
     its cooldown ends. A complete crowd fires and starts over. It's an
     `Option<Crowd>` (both numbers or none) and remembers at most a
     crowd's size of chatters, each with when they last joined in.
     Refused together with a per-chatter cooldown: each chatter counts
     once already, and which of them should rest has no clear answer.
3. **Action**: what happens where (`target`). Every rule that matches
   fires, not just the first.

```text
                 ┌────────── reactor (one task) ──────────┐
 Hub ─subscribe─→│ trigger? → guards? → cooldowns? → fire │──try_send──→ Warudo output
 (history        │ rules.rs: pure logic, time as argument │──try_send──→ OBS output
  dropped)       └────────────────────────────────────────┘   (each owns its connection)
```

- **The reactor is one more hub subscriber**, in the data plane like an
  overlay: a busy chat can't slow the app down. It **drops the replay
  history**: a reaction belongs to the moment a message arrives, and
  reacting to history would fire everything again after every restart.
  Demo messages never fire (`demo::is_sample`): styling a theme with the
  demo running mustn't throw things at the model. The test window's do,
  which is how a streamer tries a rule out.
- **The rules are pure logic** (`rules.rs`): no network, and the time comes
  in as an argument, so "61 seconds later" is a plain unit test. Cooldowns
  (and a crowd gathering) are remembered by rule name, so editing a rule
  doesn't reset them; per chatter by (platform, user id), forgotten once
  over when the map grows large.
- **Outputs own their connection.** Each is a task that connects, reports
  its state (a status light: red while the program can't be reached, as
  only the streamer can fix that), reconnects by itself with a short
  backoff (1 s up to 30 s: the program is usually on the same machine),
  and runs only while an enabled rule sends to it, so nobody gets a red
  light for a program they don't use. New output settings (an address)
  restart it; a new rule doesn't. What the outputs share (the job, the
  waiting queue, the backoff) is in `output.rs`.
- **Outputs end themselves.** Dropping an output closes its jobs channel,
  and the task finishes on its own: it isn't aborted, because OBS first
  switches back what it switched on. At shutdown the reactor waits for
  its outputs (at most 3 s), and the engine waits for the reactor.
- **The reactor never waits for an output**: actions go in with
  `try_send`. While the program can't be reached, an action is dropped,
  or waits up to `queue_secs` (per output, off by default, at most 32
  waiting). Without that limit a flood of `!bonk`s during a Warudo restart
  would all fire at once when it's back.
- **Settings and status** take the usual paths: the rules are part of
  `Settings` (a `watch` channel in `Controls` hands changes to the
  reactor, which applies them at once), and the reactor's report (outputs,
  and per rule how often it fired and its problem) is a `watch` the actor
  puts into `Status`. `edit_settings` refuses reaction settings that
  `config.toml` couldn't load (`ReactionSettings::check`: duplicate names,
  an empty action, a URL that isn't `ws://`), so a bad edit can't make the
  next start fail.
- **Warudo** needs no plugin: a blueprint's *On WebSocket Action* node
  fires on `{"action":"<name>"}` sent to its WebSocket
  (`ws://127.0.0.1:19190` by default). The output keeps one connection
  and also reads from it, only to notice when Warudo closes it, so the
  next action doesn't go into a dead socket; an action whose sending fails
  gets one more try on a fresh connection.
- **OBS** has a remote control built in since OBS 28: obs-websocket, a
  WebSocket on port 4455. We speak its protocol version 5 with a small
  client of our own (`obs.rs`): only four kinds of request are needed,
  and it keeps the dependencies down. On connecting, OBS says hello and,
  if a password is set, sends a challenge; the answer is
  `base64(sha256(base64(sha256(password + salt)) + challenge))` (a test
  checks it against values computed independently). We subscribe to no
  events; requests go one at a time and wait for their answer (matched
  by id, at most 5 s).
  - **Actions**: a filter on (`obs_filter`), a source shown
    (`obs_source`), each for `seconds` or for good; a media source played
    from the start (`obs_media`). Names come from the settings, never
    from chat.
  - **Timed effects** belong to the OBS task (`Timed`: what's on, until
    when; one owner, no lock). Firing again while one runs makes it last
    longer instead of stacking. When it ends, the switch goes back off,
    unless it was already on before the reaction (asked first: a filter
    the streamer had on stays on). Effects survive a reconnect; one that
    ended while OBS was away is switched off right after reconnecting.
    When the output stops (rules changed, quitting), everything still on
    is switched off first.
  - **A name OBS doesn't know** (a typo in a filter name) is OBS's answer,
    not a broken connection: logged as a warning, shown on the rule (see
    below), and the next reaction works as usual.
  - **The password** isn't in `config.toml` (it controls all of OBS) but
    in the secret store (`chat_core::secrets`: the OS keyring, as for the
    YouTube login). The task reads it only when OBS asks for one, keeps
    it, and reads it again only after OBS refused it: a locked keyring may
    ask the user to unlock it every time. A wrong or missing password is a
    red light ("OBS refused the stored password") and is retried, so a
    newly stored one works without a restart. The app stores it with
    `EngineHandle::set_obs_password`, which also wakes the output from its
    pause between attempts (a `Notify`), so it tries at once instead of
    within 30 s; headless mode stores it with `--obs-password`.
- **Outcomes flow back.** Every job carries a sender back to the reactor
  (unbounded: an output must never wait for the reactor, and there are
  never more answers than jobs), and the output reports how it went:
  sent, or refused by the program. The reactor keeps each rule's last
  refusal as its `problem` in the status (next to how often it fired), so
  the app shows "filter "Typo" on "Mic": No filter was found" on the rule
  itself instead of only in the log. A sent reaction clears it, and so does
  editing the rule's action. A reaction dropped because the program isn't
  connected reports nothing: the output's light already says that.
- **Testing a rule** (the app's Test button) is a request on a channel of
  its own (`ReactionRequest::Test`, with a `oneshot` for the answer): the
  saved rule's action goes out right away, without trigger or guards, also
  when the rule is switched off or everything is paused, and isn't counted
  as fired. Only while its program is connected: a test never starts an
  output, so it never connects to a program no switched-on rule uses. The
  reactor doesn't wait for the answer itself (it never waits for an
  output); a small task does, at most 15 s, and replies "sent", the
  program's refusal, or "isn't connected". A test sent right after saving
  a rule must find it: before a request, the reactor applies settings that
  changed but whose change it hasn't handled yet (`select!` picks among
  ready branches at random).

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

### Settings

The engine owns the settings (`Settings`: what `config.toml` holds for all
profiles, and in `profile` the active profile's, its sources aside; see
[Profiles](#profiles)). `EngineHandle::settings()` is a `watch` of them, so
every window reads the current ones instead of keeping a copy.

- **A change is a closure**: `edit_settings(|s| s.profile.overlay.newest = Top)`.
  The actor runs it on the current settings, so two windows changing
  different things at the same moment both get their change in. Sending
  whole new settings instead would let a window undo another one's change
  with an older copy.
- **One place decides what a change means** (`Controls::apply` in
  `settings.rs`): a new replay history goes to the hub, pacing, the JSON API
  switch, the chat direction and the theme go to the server's `watch`
  channels (a new direction or theme makes the overlays reload once), the
  allowed sites to the API's gate, the chat reactions to the reactor, new
  YouTube settings restart the YouTube sources, and a new port waits for
  the next start.
- **Changes the settings can't see** need their own command. A new emoji
  export imported over the old file leaves `emojis` as it was, so
  `use_youtube_emojis` restarts the YouTube sources anyway; a login made
  or removed does the same internally. (Edited theme files have "Reload
  overlays".)
- Each closure is its own type, so the actor's command holds it as a
  `Box<dyn FnOnce(&mut Settings) + Send>`: a trait object, because here
  (unlike the sources) the set of types isn't known in advance.
- **The engine saves** `config.toml` and the active profile's file after
  every change to the settings or the sources (`EngineConfig::save_to`, the
  app's file; headless mode has none). A failed save goes into the status
  (`save_error`), and the main window says so.
- **The last change before quitting still counts**: `edit_settings` sends
  its command right away, not when its future is first awaited, and the
  actor handles the commands already waiting before it stops. The app
  starts the engine's shutdown only after GPUI has closed the windows, so
  the settings window's pending edit (applied when it closes) is saved.

### Profiles

A profile is a setup of its own, for a streamer with several personas: its
sources, overlay look (`[overlay]`: theme, chat direction, what's hidden),
tips, chat reactions and widget values, in `profiles/<name>.toml` next to
`config.toml`. `config.toml` keeps what they share (server, YouTube's
Google project and connected channels, the app's look) and names the
active one. YouTube logins are shared too: any profile's source can read
any connected channel (`account = "UC…"`).

- **The actor owns profiles too** (`profiles.rs` is the handle's side):
  the active one's name, the list (`list_profiles` at startup, kept up to
  date after), and a switch counter, all in the status.
- **A switch** saves first (the profile being left must be on disk: once
  another is active, its file is all there is of it), then reads the
  other's file (a broken one is an error, and nothing changes), stops the
  current sources (the test window's stays: it's no profile's), clears the
  hub's replay history, hands the new settings to `Controls::apply` (the
  shared ones are the same, so YouTube doesn't restart), reloads the
  overlays, adds the new sources and saves `config.toml` naming it. The
  chat starts over: the overlays reload into an empty history.
- **New** writes a file (empty, or a copy of the active profile);
  **rename** of the active profile writes the new file, then `config.toml`,
  then deletes the old (stopped anywhere, the profile is still there);
  a change of case only, or an inactive profile, is a plain file rename
  (written anew and deleted, a case change would delete the file on
  Windows and macOS, where both names are one file). **Delete** refuses the
  active one. Names are file names too (`normalize_profile_name`: letters,
  digits, spaces, `-`, `_`, not Windows' reserved names), and case doesn't
  count when comparing them, as on Windows and macOS.
- **Secrets stay while any profile uses them**: a copied profile shares its
  tip sources' tokens, so `forget_secrets_of` asks the actor
  (`SecretInUse`), which checks its own sources and the other profiles'
  files (one it can't read counts as using it).
- **The switch counter** (`Status::profile_switches`) tells windows when to
  reload what they hold of a profile: renaming the active profile changes
  its name, but must not drop what's being typed into its settings.

### Restarts and health

- A source that fails is restarted with **exponential backoff**: 2 s,
  doubling up to 5 minutes; after 60 s of running fine the counter resets.
- Errors only the user can fix (`SetupError` such as an invalid channel
  name or a refused token, `LoginRequired`) are **not** retried: the
  source waits in *NeedsAttention* until the settings change.
  `SetupError` lives in `chat-core`: the engine recognises it by its type
  (`anyhow::Error::is`, a check at run time), so every source has to
  return that same type, from a crate both sides depend on, instead of
  a type of its own that the engine would have to list.
- Each start/stop bumps a **generation** number. Messages from an older run
  (a late "ended", a stale retry timer) carry the old number and are
  ignored, so a quick stop/start can't be undone by leftovers.
- **A source's state is one enum** (`SourceState`: stopped, running,
  retrying, needs attention, finished), and what belongs to one state
  only lives inside its variant: `Running` carries the activity the source
  reports, `Retrying` the time of the next try. The actor's own version
  (`Run`) also holds the running task there, so there's no "stopped, but
  its task still runs" to get wrong: stopping is assigning `Stopped`,
  which drops the task's handle and so aborts it.
- **Health** (off / ok / warning / error, the traffic lights) is computed
  from that state, in `chat-engine`, so the app and headless mode always
  agree.

### Sources are built by a factory

The actor is generic over a `SourceFactory`: production builds real Twitch,
YouTube and demo sources; tests use scripted ones ("fails twice, then
runs"). This is static dispatch: the compiler creates one actor per factory
type, and the production code carries no test hooks.

### The settings files

`config.toml` in the per-OS settings folder (`~/.config/chat-aggregator/` on
Linux) with `profile` (the active one), `[server]`, `[youtube]` and
`[app]`; next to it `profiles/<name>.toml` per profile, with `[overlay]`,
`[tips]`, `[reactions]`, `[widgets]` and `[[sources]]` (`ConfigFile` and
`ProfileFile` in `config_file.rs`):

- **Files from before profiles still load**: everything in one
  `config.toml`, the overlay's settings in `[server]`. A loose first parse
  (any TOML) tells the layouts apart; then the strict one, straight from
  the text, so errors still name the line (`OldConfigFile` spells out the
  old `[server]` rather than `flatten`ing today's into it: serde can't
  refuse unknown keys next to a flattened struct). The file becomes the
  profile "Main", unchanged on disk: headless mode never converts it, and
  the app only at its first save (`save_all`): the profile's file first,
  then a copy of the old file as `config.old.toml` (once), then the new
  `config.toml`. Stopped anywhere, `config.toml` is either still the old
  file, which loads as before, or the new one, whose profile is written.
  `profile = …` together with profile settings in `config.toml` is a hand
  edit gone wrong: an error saying where they go now.
- **Both are written profile file first**, so `config.toml` never names a
  profile whose file isn't there yet.

- **`deny_unknown_fields`** everywhere: a typo like `vidoe_id` is an error
  naming the line, instead of being silently ignored (which would turn
  "this video" into "your own broadcast").
- **Every setting has a default**, so a missing line or section is fine.
- **Sources remember their on/off switch** (`enabled = false`, only written
  when off).
- **Written with mode 0600** (only your user can read it): it contains the
  OAuth client secret. Secrets are redacted in debug output.
- **Written by the engine** after a change (see [Settings](#settings)),
  never at startup: the configured sources are added without saving, so a
  file nobody changed keeps its comments. Saving rewrites the file without
  them (a known limitation). Headless mode never writes it.
- **`theme`** (in `[overlay]`) is a built-in theme's name, a folder name in
  `themes/`, or the full path of a theme folder anywhere.

## The desktop app (`chat-app`)

### Two runtimes side by side

- **GPUI** (through gpui-kit) owns the main thread: windows, input,
  drawing, with its own executor.
- **tokio** runs the engine on two worker threads.

They only talk through `tokio::sync` channels (`EngineHandle` commands, the
`watch` status), which can be awaited from either side: awaiting only needs
a waker, not the tokio runtime. So a GPUI task can `await
engine.add_source(…)` directly. Work that needs tokio itself (the YouTube
login's local server, the keyring calls) the engine spawns on its own
runtime (`EngineHandle::youtube_login` and friends), so the windows never
need a tokio handle. The app reaches the platform crates only through
`chat-engine`, which re-exports what it needs (e.g. `EmojiMap`).

### Views and shared state

- **Windows**: the main window (status, the profile menu, theme row,
  sources, reactions and widgets at a glance, YouTube section, add form),
  the test messages window, the settings window, the Reactions window and
  the Widgets window. The side windows open once; clicking again brings
  them to the front.
- **The profile menu** (`app_view/profiles.rs`, a child module so it can
  use `AppView`'s private fields) switches, makes, renames and deletes
  profiles. A switch asks first while a profile's source is running or the
  Reactions window has unsaved changes. Before it, the settings and widgets
  windows apply what's still waiting for the typing pause (`flush`: their
  changes are sent right away, the switch after them, so they reach the
  profile they were typed in); after it, the side windows load the new
  profile's values when `AppConfig::switches` moves on (it notifies on a
  switch, and the engine publishes the new settings before the status that
  counts it).
- **The Reactions window** has the rules on the left, the selected rule's
  form on the right and the two programs' connection settings below. A
  rule's switch and Test button act at once, like a source's switch; the
  form only applies with Save, since a half-typed rule would fire with a
  half-typed action. The form keeps what it loaded (`Form`, everything as
  typed) and compares: anything else is unsaved, and picking another rule
  or closing the window asks before discarding it (the close via
  `on_window_should_close`). Save replaces the rule found by the name it
  was saved under, so a rename keeps its place. The OBS password field
  hands the password to the engine and empties itself: the window keeps
  no copy.
- **The Widgets window** lists the built-in widgets, then the widgets
  folder's (files that can't be widgets too, with the reason), and builds
  the form from the selected widget's block: no code per widget. The
  values live in the profile's settings (`ProfileSettings::widgets`, only those that fit and differ from
  the defaults, so a widget's new default applies) and go into the URL;
  nothing running reads them. Switches and choices apply at once, typed
  values after the settings window's pause (`APPLY_AFTER`), and so do the
  − and + of a number field (gpui-component's `NumberInput`, its own
  stepping switched off: `step` moves onto the grid of `min + n × step`
  as a browser does, and an empty field steps from the default); Copy URL,
  switching widgets and closing take in what's typed first. Reset (after
  a question) drops what's typed with a fresh form and removes the
  widget's table. Import is two
  steps (`prepare_import`, then `Import::save` after the replace
  question). The folder is read again whenever the window comes to the
  front, and the form only rebuilt if its widget's settings changed.
- **`Entity<AppConfig>`**: the windows' read-only view of the settings,
  shared by all windows. A GPUI entity is shared, reference-counted state
  (like `Rc<RefCell<…>>`, but GPUI hands out access and tracks changes). It
  keeps no copy: it reads the engine's (`settings()`), so no window works
  on a stale one, and a small task notifies its observers whenever they
  change, so they redraw. Windows change settings with
  `app_config::edit_settings` and sources with the engine's commands; the
  engine saves.
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
   `script-src 'none'`. Widgets get the same kind of policy: only their own
   inline scripts, by hash, so a widget page must be one file (a
   `<script src>` is refused when it's read); `connect-src` is the page's
   own address. An imported widget is still someone else's code running
   at the app's own address: it can do what any page there can, which
   today means reading chat. So a future control endpoint needs a token no
   page at that address can read (ROADMAP, Next steps 7), and the app asks
   streamers to import widgets only from people they trust.
3. **Localhost only.** The server binds `127.0.0.1`: other machines can't
   reach it. Web pages in the streamer's browser still could, so
   (`chat-server/src/local.rs`): every request whose `Host` isn't
   `127.0.0.1`/`localhost`/`[::1]` is refused (DNS rebinding), and the JSON
   API only accepts clients without an `Origin` (programs), with a localhost
   origin, OBS's local-file origin `http://absolute`, or a web site the
   streamer allowed (`approvals.rs`); not `null`, which any website can send
   from a sandboxed frame. An unknown site's opening request *waits* while
   the app asks (its question is in `Status::api_requests`, the answer goes
   back through `EngineHandle::answer_api_request`), so "Allow" lets the
   same connection through. One question per site, at most 3 open, "Deny"
   until restart, 2 minutes to answer; an RAII guard withdraws a question
   when its last waiting connection is gone. Headless mode doesn't ask.
   The API is also **off by default** and closes its clients when switched
   off.
4. **The server fetches only what chat brought.** `/avatar/<key>` looks
   the key up; it never downloads an address from the request. Registered
   are only `https` URLs on Google's image hosts from chat events; no
   redirects are followed, only raster images (no SVG) up to 1 MB are
   passed on, with `nosniff` and `script-src 'none'`.
5. **No reading outside the theme.** `/theme/…` accepts only plain path
   segments, so `../config.toml` (with the client secret) can't be fetched.
6. **Chat reactions pass no chat on.** A message only *picks* a rule; what
   an output sends comes entirely from the rule (Warudo gets the action
   name from the settings, as JSON built with serde_json, not glued
   together; OBS only the names of the configured filters and sources).
   A test sends a message with JSON and markup after the command and
   checks Warudo gets exactly the configured action.
7. **Secrets**: the YouTube refresh tokens (one per channel) and the OBS password in the OS
   keyring (`chat_core::secrets`), the config file 0600,
   written atomically (`chat_core::fs::write_private`: temp file, sync,
   rename, so a crash never leaves it half-written), redacted debug output,
   API keys sent as headers (never in URLs, which end up in logs).

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
- What can't be tested offline (real platforms, real OBS) is kept on a
  list of live checks, done by hand on real streams.

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
- **Data inside the enum variant it belongs to.** A value that only
  exists in one state (a running source's task and activity, the time of
  a failed one's next try) is a field of that variant rather than an `Option`
  next to the enum: the compiler then refuses code that reads it in any
  other state, and no `None` case has to be handled "just in case".
- **Generics over trait objects** where the set of types is known (the
  source factory, `SourceSpec`): static dispatch, no allocation, no
  object-safety limits. And a **trait object** where it isn't: a settings
  change is a `Box<dyn FnOnce(&mut Settings) + Send>`, since every closure
  has its own type.
- **`Arc`** where several owners share read-mostly data across threads
  (the hub, the server state).
- **`LazyLock`** for values computed once on first use (the overlay's CSP
  header).
- **Edition 2024's `impl Trait + use<>`**: says a returned value borrows
  nothing, so UI rows can be built in a loop.
