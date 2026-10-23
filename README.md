# ROC-slack-door

A module for [ikigai](https://github.com/ikigai-rs), maintained by Michael Carducci. It isn't
part of the ikigai-rs project; parts that prove themselves will be offered there.

Slack conversations as resources, the resource-oriented (ROC) way. A program names a
conversation, writes to it, reads it back, and finds people's replies waiting in an inbox; it
does all of that with the same five verbs it uses for everything else. Channel ids, message
timestamps, Socket Mode envelopes, retries, and rate limits stay behind the names, so a program
written against them can move to another chat system by swapping the door underneath it.

> **Status: built, not yet run against Slack.** The first change,
> [`slack-resources`](openspec/changes/archive/2026-10-19-slack-resources/proposal.md), says what
> this does. The library, the door, and the Unix socket are in `main`; the live connection
> and the first mount from another kernel are still to be checked by hand.

## The resources (proposed)

| Name | Verbs | What it is |
|---|---|---|
| `urn:slack:thread:{key}` | source, sink, exists | A conversation, named by a key the caller chooses. The first write opens it as a new message in the channel; later writes reply in the thread. Reading returns the conversation as JSON, with no Slack ids in it. |
| `urn:slack:thread:{key}:reaction:{name}` | sink, delete | A reaction on the thread's opening message (`white_check_mark`, for example). |
| an inbox space ([ikigai-intray](https://crates.io/crates/ikigai-intray)) | the space's own | Where replies and reactions from allowed people land, tagged with the thread's key. A message that starts a new conversation lands with no key. |

Three capabilities gate the verbs: `urn:cap:slack:read`, `urn:cap:slack:post`, and
`urn:cap:slack:react`. A host can let one caller read and another post.

The specs say exactly what each resource does:
[threads](openspec/specs/threads/spec.md),
[reactions](openspec/specs/reactions/spec.md),
[inbox](openspec/specs/inbox/spec.md), and
[configuration](openspec/specs/configuration/spec.md).

## Two pieces

- **`artifacts/slack`** is the library crate, `roc-slack-door`. It holds the space, its endpoints, and their
  descriptions, written against a `SlackTransport` trait; every scenario runs against a fake
  transport, with no network.
- **`artifacts/door`** is the binary, the program `roc-slack-door` (package
  `roc-slack-door-server`). It holds the Socket Mode connection (outbound only, so the
  machine running it needs no public URL), reads its configuration, writes the inbox, and serves
  the space over a Unix socket or QUIC for another kernel to mount.

The tokens, the workspace, the channel, the allowed people, and the bot's name belong to
whoever runs the door; they arrive as configuration. Nothing in this repository names one.

## Running the door

Every value below is a placeholder. Use your own, and keep the tokens out of the repository,
out of the shell history, and out of any file another user can read.

### 1. Create the Slack app

Create a Slack app from a manifest in your workspace, with these settings:

```yaml
display_information:
  name: <bot name>
features:
  bot_user:
    display_name: <bot name>
oauth_config:
  scopes:
    bot:
      - chat:write
      - groups:history
      - reactions:read
      - reactions:write
settings:
  event_subscriptions:
    bot_events:
      - message.groups
      - reaction_added
  socket_mode_enabled: true
```

Install the app to the workspace. Then make a private channel for the door, and invite the bot
to it (`/invite @<bot name>`).

### 2. Collect the two tokens and the ids

- **Bot token** (`xoxb-…`): the app's *OAuth & Permissions* page, after installing.
- **App-level token** (`xapp-…`): *Basic Information*, *App-Level Tokens*, with the
  `connections:write` scope.
- **Channel id** (`C0000000000`): the channel's details, at the bottom.
- **Allowed people** (`U0000000000`, comma separated): each person's profile, *More*, *Copy member ID*.

### 3. Configure the door

Write `KEY=VALUE` lines to a file only you can read (`chmod 600`):

```sh
SLACK_BOT_TOKEN=xoxb-<your bot token>
SLACK_APP_TOKEN=xapp-<your app-level token>
SLACK_CHANNEL=C0000000000
SLACK_ALLOWED_USERS=U0000000000,U0000000001
ROC_STATE_DIR=/home/<you>/.local/state/roc-slack-door
ROC_INBOX_SPACE=<inbox name>
```

Environment variables of the same names override the file. The door refuses to start and names
whichever of the six are missing.

### 4. Run it as a user service

Build the binary (`cargo build --release -p roc-slack-door-server`), then put it under systemd:

```ini
# ~/.config/systemd/user/roc-slack-door.service
[Unit]
Description=roc-slack-door: Slack conversations as ikigai resources

[Service]
ExecStart=%h/src/ROC-slack-door/target/release/roc-slack-door --config %h/.config/roc-slack-door/door.env
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now roc-slack-door
journalctl --user -u roc-slack-door -f
```

The door writes its socket to `door.sock` in the state directory. The log should say
`serving …/door.sock`; a `connection lost` line with `invalid_auth` means a token is wrong.

### 5. Mount it from another kernel

Another ikigai kernel mounts the socket once, under an alias prefix:

```sh
ikigai --mount "urn:sd:=<state dir>/door.sock"
```

Then it names the resources through the alias: `urn:sd:slack:thread:<key>` for a thread and
`urn:sd:space:<inbox name>` for the inbox, and `urn:sd:slack:thread:<key>:reaction:<name>` for
a reaction. One alias mount, because `--mount` strips its prefix (`urn:slack:thread:x` would
reach the door as `urn:thread:x`, which it does not serve) and `urn:space:` is also served
locally, so a mount there would never reach the door.

Only the machine that runs the door should hold the socket; the socket is not a network
address, and it is not exposed beyond that machine.

## The spec is the tests

What this does is written in `openspec/`. Every `#### Scenario:` there has an entry in
`openspec/vectors.json`, the tests run every entry, and `tests/test_specs_and_vectors.py` fails
when a scenario has no vector or a vector names no scenario. The documentation can't drift from
the code without CI saying so.

A change in behavior starts as a folder under `openspec/changes/`: its scenarios first, then
their vectors, then the code. When the change is done, it is archived into `openspec/specs/`.

## Working on it

[`AGENTS.md`](AGENTS.md) holds the rules every person and agent here works by. Work is claimed
on a [gonk](https://github.com/ikigai-rs/ikigai-gonk) ledger before anyone writes; every change
goes through a pull request with green CI; every commit uses
[Arlo's Commit Notation](https://github.com/RefactoringCombos/ArlosCommitNotation) and is a
microcommit. Pull requests are merged with a merge commit or rebased, never squashed, so the
microcommits reach `main` intact.

```sh
python3 scripts/test.py             # every test, as CI runs them
python3 tools/gonk.py next          # what is ready on this repository's ledger
```

The repository was generated by
[ikigai-gonk-starter](https://github.com/ikigai-rs/ikigai-gonk-starter)'s `tools/new.py`.

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
