---
title: Privacy policy
permalink: /privacy/
---

# Privacy policy

_Effective 2026-10-20._

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

- your channel's name, to confirm which account you logged in with;
- your live broadcasts (their ids, status and scheduled start time), to
  find the stream to follow;
- the live chat of your broadcasts, or of a video whose id you enter:
  messages, and for each message the author's public display name, channel
  id, profile picture link and badges, plus the details of Super Chats,
  Super Stickers, memberships and gifts.

From YouTube's own chat page, the one anyone can open in a browser, without
logging in and without cookies (only while **Learn YouTube emoji** is on, as
it is unless you switch it off): the codes and picture links of the
channel emoji used in a chat you read. The page also contains that chat's
recent messages; chat-aggregator reads past them and keeps only the emoji.

From Twitch: the public chat of the channels you enter, read anonymously
without any login.

From Picarto, without any login, the way Picarto's website does for a
logged-out visitor: the public info of the channels you enter (whether
they're live, their id), an anonymous chat token for them, and their public
chat, with each message's author name, user id, name colour, profile
picture link and badges. To show Picarto's own emotes and emoji names, the
program also downloads the script of Picarto's website once each time a
Picarto chat starts, and reads the emote lists from it.

## What is stored, where, and for how long

- **Your YouTube login** (a refresh token) is stored in your operating
  system's credential store (Keychain on macOS, Credential Manager on
  Windows, Secret Service on Linux). If none is available, it's stored in a
  file in chat-aggregator's settings folder that only your user account can
  read. It stays there until you log out or revoke access (see below).
- **Your settings** (for example your Google project's client id and
  secret, an optional API key, channel names, overlay theme, a copy of your
  custom emoji list) are stored in chat-aggregator's settings folder on your
  computer.
- **Learned emoji** (codes like `:_hypeWave:` and their picture links) are
  stored per channel in the `youtube-emojis` folder in the settings folder,
  so they work right away next time. They stay until you delete that
  folder.
- **A log file** (`chat-aggregator.log`, and the one from the previous
  start as `chat-aggregator.old.log`) in the settings folder records what
  the program did: which sources started, connection problems, errors. It
  can include the channel names and video ids you entered, but no chat
  messages, passwords or login tokens. It's replaced each time the program
  starts and never sent anywhere; you can attach it to a bug report if you
  choose.
- **Chat messages are not saved.** They're kept in memory while the program
  runs (the most recent ones, 20 by default, so a reloaded overlay isn't
  empty) and are gone when you close it.
- **Profile pictures** of YouTube chatters shown in the overlay are kept in
  memory only: for at most 24 hours, and never written to disk. They're
  gone when you close the program.

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
- The program connects only to the platforms themselves: Google/YouTube (for
  login, chat, chatters' profile pictures and, through YouTube's chat page,
  channel emoji), Twitch (for chat) and Picarto (for chat, channel info and
  the emote lists in its website's script). It
  downloads each YouTube profile picture shown in the overlay once from
  YouTube's image server and hands it to your streaming software itself.
  Emote and gift images in the overlay are loaded from Twitch's,
  YouTube's and Picarto's image servers by your streaming software, and so
  are Picarto chatters' profile pictures.
- Your YouTube API usage is recorded by Google in **your own** Google Cloud
  project.

## Removing access and data

- Log out in chat-aggregator (**Log out** in the app's YouTube section,
  `--youtube-logout` in the command-line version): this deletes the stored
  login from your computer.
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
