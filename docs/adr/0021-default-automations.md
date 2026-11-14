# 0021. Automations that come with a new install

Status: Accepted.

## Decision

Six automations ship inside the app as data (`connecto-automation/defaults/<slug>.json`),
each answering a first-run screen of Claude Code or Codex: the welcome screen, the
theme picker, the folder trust prompts, the bypass warning, the Chrome prompt. They
were taken from the author's own automations and reviewed for anything private (no
paths, names, keys or URLs). They arrive **switched off**: an automation sends a
screen to a model and types keys on its own, so it needs the Jev key and the user's
say-so.

## How they are seeded (`defaults.rs`)

Once per default, when the engine starts with an automations folder and when the
folder is changed. A record of what was seeded is kept in
`<data>/automations/defaults.json`.

- An automation with the same folder name, or for the same agent with the same
  trigger phrases, is the user's own (this is how a migrated install is recognized):
  nothing is written; it is recorded as theirs and never updated.
- A default that was written and later deleted stays deleted.
- One that is byte-identical to what was written follows a newer shipped text; one the
  user edited or switched on is never overwritten.

## Not shipped

The author's per-project registry automations ("Reum", "Accept the first-run theme",
"Role before each prompt") belong to one project and one person, and the two old
data-folder stubs have no trigger.
