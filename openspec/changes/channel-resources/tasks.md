# Tasks

## 1. Configuration (`door`, `slack`)

- [x] 1.1 Parse `SLACK_CHANNELS` and each channel's id and speakers, refusing a start with none, a bad or repeated name, a missing id or no speakers, and refusing the retired `SLACK_CHANNEL`, `SLACK_ALLOWED_USERS` and `ROC_INBOX_SPACE` by name (The host declares the channels)
- [x] 1.2 Refuse an undeclared channel name as `no-channel` on every resource, without calling Slack (The host declares the channels)

## 2. Threads and reactions (`slack`)

- [x] 2.1 Serve threads as `urn:slack:channel:{name}:thread:{key}`, with keys unique per channel in the existing map (A thread is named by a key the caller chooses)
- [ ] 2.2 Open, reply, read and refuse unknown keys per channel (Writing to a new key opens the thread)
- [ ] 2.3 Post a top-level message on a write to `urn:slack:channel:{name}` (Writing to a channel posts a new message)
- [ ] 2.4 Serve reactions as `urn:slack:channel:{name}:thread:{key}:reaction:{reaction}` (A reaction marks a thread's opening message)
- [ ] 2.5 Serve existing `threads.tsv` lines under the declared channel whose id they carry (Threads opened before channels keep their keys)

## 3. Inbox (`slack`, `door`)

- [ ] 3.1 One inbox per channel at `urn:slack:channel:{name}:inbox`, filtered by that channel's speakers (Only allowed people, in the configured channel, are delivered)
- [ ] 3.2 Mint an `m-` key for a speaker's top-level message and record it as a thread (A speaker's top-level message opens a thread)

## 4. Capabilities (`slack`)

- [ ] 4.1 Require `urn:cap:slack:channel:{name}:{read,post,react}` on every verb, including the inbox, and remove the global capabilities (Capabilities gate the verbs)

## 5. Vectors, docs and deploy

- [ ] 5.1 Update `openspec/vectors.json` for every changed and new scenario, the README runbook (two channels, the new `door.env` keys, mounting `urn:slack:channel:`), and descriptions (`Meta`) for every new resource
- [ ] 5.2 Human only, Michael on Tamariz, after this change's PRs merge: in one sitting, rewrite `~/.config/ikigai-slack/door.env` for `gonk-chat`, restart `roc-slack-door`, and switch Dax's kickoff (`~/gonk-da/.gonk/dax-kickoff.md`) to `urn:sd:slack:channel:gonk-chat:thread:<key>` and `urn:sd:slack:channel:gonk-chat:inbox`; check that one thread reply round-trips
- [ ] 5.3 Archive the change into `openspec/specs/`
