# 0023: Readiness gates only the first send to a terminal

Status: Accepted as a known limitation. A fix is sized below and not built.

## Context

A send that names a Claude or Codex terminal is typed into its input box with
Connecto's own Enter. Typing into a screen that is not the composer loses the
text (while the tool boots) or answers the wrong thing (a prompt). Connecto
therefore checks the live screen before the first send of a launch
(`connecto-session/src/input_ready.rs`, `relay.rs` `input_now`). The check reads
the composer at the bottom of the screen (`ComposerState` in
`connecto-adapter`): Empty, Busy, Occupied (a draft, a menu or a prompt), or
Booting.

Once the composer has been seen ready, `relay.rs` keeps the terminal ready for
that launch (`ready_attempt`). The terminal also counts as ready while
`working` is set, which the tool's own prompt hook sets for each turn.

## Decision

The check gates the first send only, and a Connecto wake only when it is
Connecto's own `/rename`. After that, later sends and wakes are typed whatever
the screen shows.

## Consequences

A later message can land:

- on an approval prompt that appeared during a turn (the `working` shortcut
  reports ready during the whole turn);
- over a draft the person has typed since the first send;
- in a slash menu the person has opened.

The screen can show these at any time after the first send, and the relay does
not look again. Senders see success for the typing, not for the answer.

Mitigations in place: the person sees the terminal and can answer; the send's
audit row records what was typed; Pause stops every write.

## What a proper fix involves

1. Read the composer before every typed send, not only the first. Drop the
   sticky `ready_attempt` in `input_now`, and keep the snapshot read on the
   relay thread, as now.
2. Stop trusting `working` as ready. Mid-turn, a prompt (approval) is Occupied,
   not Busy, so a send must wait or be refused while one is up. This needs the
   screen read during a turn, which today only happens while not working.
3. Decide the policy for a draft or a prompt that appears after a send was
   accepted: refuse with the reason (as now, after the grace), or hold until the
   composer is empty. Hold-until-empty needs a bounded queue per terminal and a
   timeout message.
4. Rerun the captured-screen tests for each tool and add captures of a prompt
   that appears mid-turn (Claude's permission prompt, Codex's approval prompt).

Size: medium to large, one to two weeks with real-tool tests. Most of the work is
steps 2 and 3, because they change how a turn's end and each queued send are
ordered, not the screen rules, which already exist for both tools.
