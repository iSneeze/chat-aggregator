---
title: Privacy policy
permalink: /privacy/
---

# Privacy policy

_Effective 2026-11-22._

chat-aggregator is a program that runs **on your own computer**. It has no
servers, no user accounts and no analytics. The developers never receive
any of your data. This page describes what the program does with data on
your computer.

## YouTube API Services

chat-aggregator uses **YouTube API Services**. By connecting YouTube you
agree to be bound by the
[YouTube Terms of Service](https://www.youtube.com/t/terms). Google's
handling of data is described in the
[Google Privacy Policy](https://policies.google.com/privacy).

You connect YouTube with your own Google Cloud project and log in with
Google. chat-aggregator asks only for the permission **"View your YouTube
account"** (`youtube.readonly`), which is read-only: it can't post, delete
or change anything on your channel.

## What data is accessed

From YouTube, with your permission:

- your channel's name and id, to confirm which channel you logged in with
  and to keep each channel's login apart;
- your live broadcasts (their ids, status and scheduled start time), to
  find the stream to follow;
- the live chat of your broadcasts, or of a video whose id you enter:
  messages, and for each message the author's public display name, channel
  id, profile picture link and badges, plus the details of Super Chats,
  Super Stickers, memberships and gifts.

From YouTube's own chat page, the one anyone can open in a browser, without
logging in and without cookies (only while **Learn YouTube emoji** is on, as
it is unless you switch it off): the codes and picture links of the channel
emoji and custom member badges used in a chat you read (keyed by each
member's public channel id so their badge picture can be matched to their
messages). The page also contains that chat's recent messages;
chat-aggregator reads past them and keeps only the emoji and member badge
picture links.

From Twitch: the public chat of the channels you enter, read anonymously
without any login, plus the public global and channel chat badge artwork for
those channels (fetched anonymously from Twitch's public web endpoint).

From Picarto, without any login, the way Picarto's website does for a
logged-out visitor: the public info of the channels you enter (whether
they're live, their id), an anonymous chat token for them, and their public
chat, with each message's author name, user id, name colour, profile
picture link and badges. To show Picarto's own emotes and emoji names, the
program also downloads the script of Picarto's website once each time a
Picarto chat starts, and reads the emote lists from it.

From RPlay, only if you add it as a source, with the key in the widget link
you enter, the way RPlay's own OBS widgets use it (as your channel): your
channel's state (whether it's live, its chat server, its banned words, its
access settings and the ids of its managers), a chat token, and your
channel's chat, with each message's author name and user id, plus
donations and missions. Donations don't carry the sender's name, so it's
looked up on RPlay (a public lookup, without the key). Most chatters have
no profile picture, so chat-aggregator asks RPlay's image server once per
chatter (and again every hour) whether theirs exists, without downloading
it; not for chatters who hid their profile. RPlay's answer about your
channel also contains your stream key; chat-aggregator doesn't keep it.
The emote lists (RPlay's free set and your channel's own) are downloaded
too.

From StreamElements, only if you add it as a source, with the Overlay Token
you enter: your channel's activity feed. That feed also carries your
platforms' own events (follows, subscriptions, Super Chats); chat-aggregator
keeps only the tips from it: each tipper's name, profile picture link,
amount and message.

From Streamlabs, only if you add it as a source, with the Socket API Token
you enter: your account's live events. Streamlabs also sends your alert
and tip page settings, your donation totals and your platforms' own events
(follows, subscriptions, Super Chats) on that connection; chat-aggregator
keeps only the tips: each tipper's name, amount, currency and message.

## What is stored, where, and for how long

- **Your YouTube logins** (a refresh token per channel you connect) are
  stored in your operating system's credential store (Keychain on macOS,
  Credential Manager on Windows, Secret Service on Linux), each under its
  channel's id. If none is available, they're stored in files in
  chat-aggregator's settings folder that only your user account can read.
  Each stays there until you log out of that channel or revoke access (see
  below). The connected channels' ids and names are written to the
  settings file, so the app knows which logins it has; the logins
  themselves never are.
- **Your OBS WebSocket password**, if you store one for chat reactions
  (in the Reactions window), is kept the same way as the YouTube login
  (credential store, or a private file without one) until you remove it
  there. It is never written to the settings file.
- **Your StreamElements Overlay Token**, if you add StreamElements as a
  source, is kept the same way, under your StreamElements Account ID, until
  you remove that source (which deletes it). It is never written to the
  settings file; the Account ID is.
- **Your Streamlabs Socket API Token**, if you add Streamlabs as a source,
  is kept the same way, under the name you gave that account in the app,
  until you remove that source (which deletes it). It is never written to
  the settings file; the name is.
- **Your RPlay widget key**, if you add RPlay as a source: of the widget
  link you paste, only the key in it is kept, the same way, under the name
  you gave that channel in the app, until you remove that source (which
  deletes it). It is never written to the settings file; the name is.
  That key is powerful: RPlay lets whoever has it use your channel's live
  tools (chat moderation, the stream's title and access settings, donation
  settings and history). chat-aggregator only uses it to read your chat and
  your channel's state.
- A copied profile shares its tip and RPlay sources, and with them their
  tokens: a token is deleted once no source of any profile uses it any more
  (removing the last such source, or deleting the last profile with one).
- **Your settings** (for example your Google project's client id and
  secret, an optional API key, channel names, overlay theme, a copy of your
  custom emoji list) are stored in chat-aggregator's settings folder on your
  computer: what all profiles share in `config.toml`, and each profile's
  sources and settings in its own file in the `profiles` folder, until you
  delete that profile in the app. Settings from a version before profiles
  are kept as they were in `config.old.toml`, for going back; delete it
  whenever you like.
- **Widgets you import** (web pages that show chat their own way, see
  [Widgets](widgets.md)) are copied into the `widgets` folder in the
  settings folder, and the settings you pick for widgets are stored in the
  profile's file, until you remove the widget in the app.
- **The Timer widget** remembers where it is (when it started, its
  pauses, the time added, the ids of the chat messages that added it, and
  a label set with `!timer desc`)
  in your streaming software's browser storage on your computer, so it
  carries on after a reload. It stays in your streaming software's
  browser data until that's deleted. Switch off "Keep running through
  reloads" and it stores nothing.
- **Learned emoji and member badges** (codes like `:_hypeWave:`, member
  channel ids, and their picture links) are stored per channel in the
  `youtube-emojis` folder in the settings folder, so they work right away
  next time. They stay until you delete that folder.
- **A log file** (`chat-aggregator.log`, and the one from the previous
  start as `chat-aggregator.old.log`) in the settings folder records what
  the program did: which sources started, connection problems, errors. It
  can include the channel names and video ids you entered, but no chat
  messages, passwords or login tokens. For RPlay it also notes your
  channel's access settings, which kinds of events and messages its chat
  sends and the names of their fields (not what's in them), why a chatter
  was disconnected (a ban or a kick, not who) and how many emotes it has,
  to help improve RPlay support. It's replaced each time the program
  starts and never sent anywhere; you can attach it to a bug report if you
  choose.
- **RPlay test builds only** (made by hand for testing RPlay, their
  archive's name ends in `-rplay-capture`; the releases don't do this):
  RPlay's chat as it arrives, names and messages included, in
  `rplay-capture.jsonl` in the settings folder, up to 50 MB. Never sent
  anywhere: a tester sends it to the developer by hand. Delete it when
  you're done testing.
- **Chat messages are not saved.** They're kept in memory while the program
  runs (the most recent ones, 20 by default, so a reloaded overlay isn't
  empty) and are gone when you close it.
- **Profile pictures and custom member badge images** of YouTube chatters
  shown in the overlay are kept in memory only: for at most 24 hours, and
  never written to disk. They're gone when you close the program.

## What is shared, and with whom

- **Nothing is sent to the developers or any third party.**
- chat-aggregator shows the chat in an overlay served on your own computer
  (`127.0.0.1`); other computers can't reach it. Programs running on your
  computer can read the same chat through its local API, if you switch it
  on (it's off by default), and so can pages served from your own computer
  and OBS browser sources. A web site (e.g. a browser game) can only after
  you allowed it: chat-aggregator asks you the first time one connects, and
  remembers the sites you allowed in its settings (you can remove them under
  ⚙ Settings → Connection). That chat can include members-only and unlisted
  YouTube broadcasts, if you read those, and a site you allow can tell from
  it which channel is yours.
- Apart from chat reactions (below), the program connects only to the
  platforms themselves: Google/YouTube (for login, chat, chatters' profile
  pictures and, through YouTube's chat page, channel emoji and custom member
  badges), Twitch (for chat and public badge artwork), Picarto (for chat,
  channel info and the emote lists in its website's script), RPlay (for chat,
  your channel's state, the emote lists, donors' names and whether a chatter
  has a profile picture, with your widget key; the chat itself runs on
  Amazon's IVS chat service, at the address RPlay names), StreamElements (for
  tips, with your Overlay Token) and Streamlabs (for tips, with your Socket
  API Token). It downloads each YouTube profile picture and custom member
  badge shown in the overlay once from YouTube's image server and hands it to
  your streaming software itself. Emote, badge and gift images in the overlay
  are loaded from Twitch's, YouTube's, Picarto's and RPlay's image servers by
  your streaming software, and so are Picarto's and RPlay's chatters' and
  StreamElements tippers' profile pictures (from StreamElements' image
  server).
- **Widgets** get the chat through the same local API (switched on for
  them), like any page served from your own computer. Your streaming
  software loads the pictures and fonts a widget asks for from the web: the
  built-in Live Emoji and Danmaku only load emote pictures, from the
  platforms' image servers, and the Timer nothing at all. A widget you
  import comes from someone else, and could pass on
  what it shows to the servers it loads things from: import widgets only
  from people you trust.
- **Chat reactions** (only if you set up rules for them): chat-aggregator
  connects to the programs your rules name, at the address you enter (on
  your own computer unless you enter another one): Warudo, OBS. It sends
  them only what your rules say (an action name; which OBS filter or
  source to switch, and the stored OBS password to log in), never chat
  messages or chatter names.
- Your YouTube API usage is recorded by Google in **your own** Google Cloud
  project.

## Removing access and data

- Log out in chat-aggregator (**Log out** next to the channel in the app's
  YouTube section, `--youtube-logout <channel id>` in the command-line
  version): this deletes that channel's stored login from your computer,
  and the app takes the channel off its list.
- Remove the StreamElements source in chat-aggregator: this deletes the
  stored Overlay Token from your computer. To make a token useless
  everywhere, create a new one on the StreamElements dashboard.
- Remove a Streamlabs source in chat-aggregator: this deletes its stored
  Socket API Token from your computer. To make a token useless everywhere,
  generate a new one on the Streamlabs dashboard (Settings → API Settings →
  API Tokens).
- Remove an RPlay source in chat-aggregator: this deletes its stored key
  from your computer. To make the key useless everywhere, click **Reset all
  widget URLs** in your RPlay studio (Dashboard Widget); RPlay's own
  widgets then need their new links in OBS too.
- Revoke chat-aggregator's access at any time in your Google Account, under
  third-party connections:
  [myaccount.google.com/connections](https://myaccount.google.com/connections).
- To remove everything, delete chat-aggregator's settings folder (for
  example `~/.config/chat-aggregator` on Linux, `%APPDATA%\chat-aggregator`
  on Windows).

## This website

This website is hosted on GitHub Pages and sets no cookies of its own.
GitHub may log visits as described in the
[GitHub Privacy Statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement).

## Changes and contact

Changes to this policy are published on this page; its full history is
visible in the
[source repository](https://github.com/iSneeze/chat-aggregator/commits/master/docs/privacy.md).
Questions: open an issue at
[github.com/iSneeze/chat-aggregator/issues](https://github.com/iSneeze/chat-aggregator/issues).
