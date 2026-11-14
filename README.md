# Connecto

https://connecto.studio

A desktop app for macOS and Windows that runs Claude Code and Codex in real terminals, up to 16 per project, and lets them work together. Nothing moves between the sessions unless you send it, or an agent in one terminal uses the `connecto` command (see Agents using other terminals).

Connecto is released and open source under the [GNU AGPL-3.0](LICENSE). Downloads for macOS and Windows are at [connecto.studio](https://connecto.studio); the source is at <https://github.com/marcomanzi/connecto-community>.

## What you need

- macOS, with Rust 1.95 or later.
- **libclang**, needed to build. The Xcode Command Line Tools provide it: `xcode-select --install`.
- Claude Code and Codex already installed, with `claude` and `codex` in `~/.local/bin`.

## Build

```sh
cargo build --release --workspace
```

`connecto-hook` is built beside `connecto-app`; the app looks for it in its own folder.

## First launch

Open the disk image, drag **Connecto** to Applications and open it. A build that is not signed yet needs one extra approval the first time: System Settings → Privacy & Security → **Open Anyway**.

Connecto then sets itself up in a short window: it asks which agents you want (Claude Code, Codex or both), makes its own verified copy of each from the one already on your Mac, and prepares its workspace folder (Connecto Home, `~/Connecto`). Then **Sign in** opens each agent's own sign-in, and you pick a folder or try a sample project. Nothing is asked of you except signing in.

The copy of each agent is made by the same code as `connecto-app provision`, which still works for scripts and for picking up a newer version of a tool by hand.

Run from a build in this repository (needs `claude` and `codex` already installed, in `~/.local/bin` or Homebrew's folders):

```sh
target/release/connecto-app
```

To try a build without touching your real data, give it a folder of its own: `CONNECTO_DATA_DIR=/tmp/connecto-try target/release/connecto-app`.

## Using the relay

Type a message, or click **Use** on a captured reply, pick a destination, and click **Send**. The preview shows exactly what will be sent. Every hop needs its own click.

- A target that is busy or may not be ready asks for a second click, and says what that risks.
- A session's reply cannot be sent back to that same session. Text you typed can go to either. To replace a reply you clicked **Use** on with your own text, clear the box first.
- `written, unconfirmed` is not success. A delivery ends `completed` only when the target's own signal says so, and `uncertain` otherwise. Nothing is retried automatically.
- `completed` means the target session reported it, which a process inside that session could fake. It never triggers anything.
- **End delivery** closes a delivery that will never finish, for example after you interrupt its turn. It sends nothing to the session.
- **Pause** blocks new deliveries and stops one in progress before Enter. Running turns continue.
- **Stop** ends a session. **Stop all** pauses first, then stops everything.
- Closing the app ends any open delivery `uncertain`. `kill -9` cannot: the audit log then has a delivery with an intent and no outcome.
- **Pause** and **Stop** need the window visible. While it is fully covered or minimized, sessions and deliveries keep running, but the screen stops updating and no button can be clicked until you bring it back.

## Agents using other terminals

Every terminal gets a `connecto` shell command, so an agent can see the other terminals, read them, type into them without approval, ask another agent something and read just the answer, and be woken by events there. The commands are `connecto list`, `send <id> [--ask]`, `result <request>`, `read <id>`, `keys <id> <key>...`, `watch <id>` and `unwatch <id>`, `add claude|codex [--name <text>]`, `start <id>` and `close <id>`. After `send --ask` an agent ends its turn: the reply is delivered to it automatically, in its context, labelled as another agent's words, so it does not poll `result`. `close` stops a terminal and closes it: it leaves the list and moves to the project's history, from which it can be read or continued. Each terminal's agent is told about them at launch.

- **Pause is a full brake** on agent writes, keys, add, start, close and wakes. Stop all still ends every terminal, and an agent cannot lift Pause or use Stop all.
- Agents are not rate-limited: two terminals that watch each other can keep waking each other, and each wake is a model turn on your plan. Pause stops it, and so does `unwatch`.
- Terminals on an unverified CLI version have no bridge and say so.
- Every agent action is in the audit log with who did it and the redacted text.

## What sessions can do

There is no sandbox (ADR 0013). Claude Code and Codex always run with their own approval checks off (`--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`), as your user, with your files and network. **An agent can change anything your account can change.** Text on one screen can steer an agent that has that reach.

What Connecto still does:

- **It refuses your home folder as a project**, and any folder holding Connecto's data or the tools' own state (`~/.claude`, `~/.codex`, `~/Library`).
- **Each project has its own login and profile** for each tool, under `~/Library/Application Support/connecto/projects/<id>/profiles/<tool>/`. Only `USER`, `LOGNAME`, `PATH` and `LANG` are inherited into a terminal.
- **The workspace is saved** in `workspace.json` in the data folder: the open projects, their order and names, the one shown, and the ten most recent. Each project keeps `terminals.json` (names, parents and the tool session ids that let a relaunch resume each conversation) and a `history/` folder with one redacted transcript per closed terminal (at most 500, 2 MiB each). An unreadable `workspace.json` is kept as `workspace.json.bad`.
- **Other sessions cannot message a Connecto Claude** (`crossSessionInbound: refuse`). Claude's own `SendMessage` and `ListAgents` are allowed, so a skill can message its subagents (after the first manual run). Those sends do not pass the bridge's audit or Pause, and a send to a refusing session can start an extra turn on the sender (L3). Claude loads the project's skills, agents and settings (`--setting-sources user,project,local`), so project hooks run alongside Connecto's. A project that sets `disableAllHooks` turns that terminal's bridge off, and one that disables bypass mode shows a warning; both are reported in `list`.
- **Processes that detach.** Stop kills the descendants it can find, but a process that detached before you clicked Stop can outlive Stop and app exit.

**Log in** starts a new, numbered profile generation for that tool in that project, and older generations stay on disk (`g<N>` folders, with the live number in `current`). A repeat login may also leave the previous Claude Keychain item behind.

## Limits worth knowing

- **Hook frames are limited to 256 KiB.** A longer reply loses its end-of-turn signal, so the delivery stays open or ends `uncertain`, never `completed`. Use **End delivery** to close it.
- A delivery's body is limited to 4096 bytes after redaction. Longer is denied, not truncated.
- Redaction applies to relay messages, audit and evidence. Your own pane is not redacted.
- CLI versions are not pinned: the bridge is on for every Claude and Codex version, relying on the CLIs staying backward compatible. (Checked on Claude 2.1.318 and Codex 0.166.0.)

## Audit log

Every delivery is recorded in `~/Library/Application Support/connecto/audit/`, each record chained to the previous one by hash. Check a file with:

```sh
target/release/connecto-app audit verify <file>
```

The check finds an edited record. **It cannot detect deletion of the end of the file.**

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --no-fail-fast
```

Tests that need the installed CLIs are ignored by default. They use a local fake model, with no login and no spend:

```sh
cargo test -p connecto-session --features test-endpoints --test real_cli -- --ignored --test-threads=1
```

**Test seams are compiled out of the default build:**

| Feature, on `connecto-app` | Provides |
| - | - |
| `test-endpoints` | `--fake-model <url>`, a local fake model, with a permanent red FAKE MODEL banner |
| `ui-test-driver` | `CONNECTO_UI_SCRIPT` replays raw window events, `CONNECTO_UI_PICK` skips the folder panel, and `CONNECTO_UI_POS` sets where the window opens, as `<x>,<y>` in points |
| `frame-capture` | `CONNECTO_METRICS`, `CONNECTO_SHOT_DIR`, and `--pane <command>` to run plain shells |

`CONNECTO_DATA_DIR` overrides the data folder, so tests stay out of the real one. Screenshots show session content, so keep them away from real sessions.

Design decisions: `docs/adr/`.

## License

Copyright (C) 2026 Marco Manzi.

Connecto is free software: you can redistribute it and/or modify it under the terms of the GNU Affero General Public License, version 3 only (`AGPL-3.0-only`), as published by the Free Software Foundation. It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See [`LICENSE`](LICENSE) for the full text.

Source: <https://github.com/marcomanzi/connecto-community>. Issues and pull requests are welcome there. Connecto is community-funded; if it helps you, you can [sustain its development](https://connecto.studio/#donate).
