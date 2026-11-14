# Download and first steps

chat-aggregator runs on **Windows** (10 or newer) and **Linux**. There is
no installer yet: you download an archive, unpack it and start the program.

## Download

Get the newest build from the
[Releases page](https://github.com/iSneeze/chat-aggregator/releases): the
`.zip` for Windows, the `.tar.gz` for Linux. Unpack it anywhere you like.

### Windows

- Start `chat-aggregator.exe`.
- Windows SmartScreen warns about the file ("Windows protected your PC"),
  because it isn't signed yet: **More info → Run anyway**.
- If the app doesn't open at all, its log file says why (see
  [Reporting a problem](#reporting-a-problem)).

### Linux

- Start `./chat-aggregator`.
- It needs a graphics driver with Vulkan (Mesa or your GPU vendor's),
  Wayland or X11, and libxkbcommon. Desktop installs normally have all
  three, on distributions from about 2022 on.
- **NixOS:** the downloaded program doesn't run there. Use the flake
  instead: `nix run github:iSneeze/chat-aggregator` (or
  `nix profile install github:iSneeze/chat-aggregator`).

## First steps

1. **Add a source.** In the main window under **Add a source**, pick the
   platform, type your channel name and press **Add**. A light next to
   each source shows how it's doing, with a line of text saying why.
   - Twitch and Picarto only need the channel name.
   - RPlay needs a widget link from your RPlay studio: open **Dashboard
     Widget**, click **Generate Widget URLs**, then **Show** next to
     **Stream Chat**, and copy that link. Keep it to yourself: the key in
     it opens your channel's live tools, not only the chat (see
     [Privacy](privacy.md)).
   - YouTube needs a one-time setup first:
     [Connecting YouTube](youtube-setup.md).
   - StreamElements and Streamlabs tips need a token from their dashboard;
     the app says which one.
   - **Demo** needs nothing: it sends sample messages of every kind.
2. **Add the overlay to OBS.** Under **Overlay**, press **Copy** to copy
   its address (`http://127.0.0.1:7878/`). In OBS, add a **Browser
   Source** and paste it into the **URL** field. 450×800 is a good size to
   start with.
3. **Try it.** **Test messages…** sends messages by hand, so you can see a
   donation or a membership on stream without waiting for one.

From here:

- [Overlay themes](themes.md): pick a built-in look or make your own.
- [Widgets](widgets.md): chat shown other ways, like emotes floating up
  the screen.
- **Reactions** in the main window: let chat trigger things in Warudo or
  OBS.

## Where your things are kept

| | Windows | Linux |
|---|---------|-------|
| Settings, themes, widgets, log | `%APPDATA%\chat-aggregator\config\` | `~/.config/chat-aggregator/` |
| YouTube login, tokens, OBS password | Windows Credential Manager | your desktop's keyring (GNOME Keyring, KWallet, …) |

The app opens the settings folder for you: **⚙ Settings → General →
Settings folder → Open folder**.

On Linux without a keyring, logins are kept in a file in the settings
folder that only your user can read. What the app stores and sends is
described in the [privacy policy](privacy.md).

## Reporting a problem

The app writes a log of each start to `chat-aggregator.log` in the
settings folder (the start before that is kept as
`chat-aggregator.old.log`). Windows builds have no console window, so this
file is where errors end up.

Open an [issue](https://github.com/iSneeze/chat-aggregator/issues), say
what you did and what happened, and attach `chat-aggregator.log` (and
`chat-aggregator.old.log` if you restarted the app after the problem). The
log names your channels, so have a look at it before posting it in public.

## Licenses

Each archive contains `THIRD-PARTY-LICENSES.html`: the licenses of the
open-source projects the app is built from. chat-aggregator itself is
under the
[MIT license](https://github.com/iSneeze/chat-aggregator/blob/master/LICENSE).
