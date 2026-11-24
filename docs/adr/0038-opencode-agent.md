# 0038: OpenCode is a fifth agent, hooked by a plugin named in its config variable, isolated with the XDG folders

Status: Accepted, built in `connecto-adapter/src/opencode.rs` and
`opencode_plugin.mjs` (launch, login, status, the plugin), `connecto-hook`
(`connecto-hook opencode`), `connecto-session` (launch, isolation, models),
`connecto-store` (runs in place), `connecto-app` (install, setup, Settings >
Agents, Agentic Works), `connecto-ui` (chooser, menu, Cmd+Alt+6, project
Tools and config, Settings > Launch), `connecto-net` and the phone app (wire
byte 6, the agent list), and the built-in package (version 7).

Verified with the real program, 1.18.8 and 1.19.6 (what the installer gives
today): `connecto-session/tests/real_opencode.rs` runs it under Connecto's
launch path, in an isolated project, against a stand-in model
(`tests/fixtures/mock_openai.py`), and checks the start, the prompt, the
session id, the reply, the bridge instructions in the system prompt, the
database in the project's profile, a turn at work on the screen, a
permission prompt, the agent's shell seeing the user's XDG folders, and a
resume. `agent_install::tests::the_real_opencode_installer_works_into_an_isolated_home`
runs the real installer. Both are ignored by default (they need OpenCode, or
the network). Not tried: the Connecto app's own window, a `/connect` sign-in,
Windows.

## Context

The user wanted OpenCode as one more CLI, on par with Claude, Codex, Pi and
Antigravity: installed by Connecto, a terminal kind of its own, login done
inside the CLI, an isolated mode per project, the skill packages, the hooks
and the state, and `connecto add opencode`.

Facts that shaped the decision, observed on 1.18.8:

- **Two product lines.** `curl -fsSL https://opencode.ai/install | bash`
  installs the newest **1.x** release (npm `opencode-ai`) into
  `~/.opencode/bin/opencode`. opencode.ai's documentation now describes
  OpenCode **2** (`@opencode/cli`, `opencode.ai/v2/install`), whose plugins
  have a different API ("V1 plugin implementations do not run in V2").
- **No hook commands, but plugins.** OpenCode runs JavaScript plugins in its
  own Bun runtime, from `plugin(s)/` folders or from the config's `plugin`
  list, and `OPENCODE_CONFIG_CONTENT` is merged over the user's own
  configuration: a plugin named there loads beside the user's plugins.
  The `permission.ask` hook never fires in 1.18.8; the `permission.asked`
  event does.
- **The XDG folders hold everything.** Config in `~/.config/opencode`,
  sign-ins (`auth.json`) and sessions (`opencode.db`, SQLite) in
  `~/.local/share/opencode`, recent models in `~/.local/state/opencode`, on
  macOS too. Setting `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and
  `XDG_STATE_HOME` moves all of them.
- **A session exists only after the first prompt.** The TUI starts with no
  session; `session.created` fires when the first prompt is sent.
- **Logins are per provider.** `opencode providers login` (alias `auth
  login`) or `/connect` in the TUI adds one provider's credential beside the
  others. OpenCode Zen's free models and keys in the environment work with no
  login at all.

## Decision

1. **A plugin in the run folder, named in `OPENCODE_CONFIG_CONTENT`.** Each
   launch writes `connecto-opencode.mjs` (with a `CONFIG` line: the helper,
   the inbox switch, the resumed session, the shell variables) and the bridge
   instructions into the run folder, and sets
   `OPENCODE_CONFIG_CONTENT={"plugin":["file://…"],"instructions":["…"]}`.
   OpenCode decodes the plugin's `file:` URL, so the path is percent-encoded
   (a `#` in a folder name would otherwise start a fragment and the plugin
   would never load).
   Nothing is written into OpenCode's folders, the user's plugins still load,
   and a user's own `OPENCODE_CONFIG_CONTENT` is replaced for Connecto's
   terminals only. The instructions use OpenCode's own `instructions` list,
   not an experimental hook.

2. **The plugin speaks Claude's hook payloads.** It runs `connecto-hook
   opencode --event <name>` (the same path as Pi's extension) with
   `SessionStart`, `UserPromptSubmit` (from `chat.message`), `Stop` with the
   last assistant text (on `session.status` idle / `session.idle`),
   `StopFailure` (from `session.error` or an assistant error; an abort is
   `interrupted`), and `Notification` for `permission.asked` and
   `question.asked`. Subagent sessions (those with a `parentID`) are left
   out, except their permission prompts, which the user must answer too.
   Inbox notices join the prompt as a synthetic text part, which OpenCode
   gives the model and does not draw.

3. **Ready at once, the session named with its first prompt.** The plugin
   reports `SessionStart` with an empty id when OpenCode loads it, so the
   terminal is ready (an empty id is never resumed). When a prompt is
   accepted in a session Connecto was not told yet (the first one, or after
   `/new`), it reports `SessionNamed` with that id: a `ToolEvent` of its own
   that only records the id a restart resumes. A second `SessionStart` would
   taint the input ledger (the prompt's acknowledgement would turn
   uncertain), and waiting for the turn's end would leave a quit during the
   first turn, a permission dialog included, with nothing to resume. A
   resumed terminal (`--session <id>`) names its id in the first report.

4. **No log is followed.** The sessions are in SQLite; the plugin reports
   every end itself. So an interrupted turn ends as a failed one
   (`interrupted`), not as Connecto's Interrupted, and project memory does not
   capture OpenCode turns yet.

5. **Isolated = the three XDG folders in the profile.** With the project's
   OpenCode config Isolated, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and
   `XDG_STATE_HOME` point at `config/`, `data/` and `state/` of the
   project's Connecto profile, so it has its own settings, sign-ins and
   sessions. The cache stays the user's (no secrets, and it holds large
   downloads). The plugin's `shell.env` hook gives the agent's shell commands
   the user's own values back (empty when unset, which every XDG reader takes
   as unset), so `git`, `gh` and the rest find their settings. System default
   (the default for new projects, as for Pi) leaves the variables alone.
   An OpenCode launch, login and status check keep the three XDG variables of
   Connecto's own environment (the other agents get none), so System default
   finds sign-ins and settings in custom XDG folders, and the model list
   reads `$XDG_STATE_HOME/opencode/model.json`.
   `~/.claude/skills` and `~/.claude/CLAUDE.md` are still read from `HOME`,
   as in every other mode.

6. **Login is OpenCode's own, and never starts a profile over.** Connecto's
   login runs `opencode providers login`; inside a terminal it is `/connect`.
   Status asks `--version` only: an OpenCode that runs is ready, as Pi is,
   since it works without any provider. An isolated login always runs in the
   current profile generation: rotating to a fresh one, as a Claude login
   does, would drop the other providers and the sessions.

7. **Installed from opencode.ai, run in place.** The setup and Settings >
   Agents run `https://opencode.ai/install` with `bash` and
   `--no-modify-path` (Connecto finds `~/.opencode/bin` itself and leaves the
   shell's startup file alone). The store records where it is and makes no
   copy: OpenCode upgrades itself there. Windows has no PowerShell installer:
   there Connecto does what the script does on a Windows with a shell, the
   newest `v1.x` release from GitHub's list (never 2.x, a draft or a
   pre-release; up to ten pages of 100 are looked through, as 2.x releases
   fill the first ones), its `opencode-windows-x64.zip` (`arm64` on ARM,
   `x64-baseline` on a processor without AVX2) over HTTPS
   with the `curl.exe` Windows ships, unpacked with its `tar.exe`, and
   `opencode.exe` put whole in `~\.opencode\bin`, where discovery looks.

   Only the 1.x line is taken. The store refuses to record an OpenCode
   whose major version is not 1 (`StoreError::Unsupported`, which Setup and
   Settings > Agents show with the command that installs 1.x) and drops its
   older record of that same install. Wherever readiness is told, the
   program itself is asked, never the record, and its version is read by one
   rule everywhere (`connecto_store::version_text`: stdout when it names a
   version, else stderr): the store's probe, the check before each start, Setup's
   "Already set up" and its last screen, and Settings > Agents, which then
   shows the install with why and does not offer it. So one that upgraded
   itself to 2.x in place is refused with the same words instead of starting
   without its plugin.

8. **Launch modes.** Full power is `--auto` (every permission the config does
   not deny is granted). OpenCode has no reviewing mode, so Settings offers
   only that and its normal prompts; a saved Auto means its prompts. There is
   no effort flag in the TUI: `--effort` is refused for OpenCode. `--model`
   takes `provider/model`; the chooser offers the user's favorite and recent
   models from `model.json`, then OpenCode's default.

9. **Skills and rules.** OpenCode reads `.agents/skills`, `.claude/skills` and
   `.opencode/skills`, and `AGENTS.md` (or `CLAUDE.md`). A project of OpenCode
   alone gets the one-agent (`codex`) variant of the built-in package, whose
   teams start `connecto add opencode`; the both-variant leaves OpenCode out
   of its table, as it does Pi.

## Consequences

- OpenCode 2 is not supported. When the installer moves to 2.x, the plugin
  must be rewritten for `@opencode/plugin` (`ctx.event.subscribe`,
  `ctx.session.hook("prompt")`, …) and the config key becomes `plugins`.
- An interrupt ends an ask as failed, not interrupted.
- The screen is read from the prompt box as measured on 1.18.8 (the `┃` rows
  closed by `╹▀`, `esc interrupt` under it while a turn works). The grey
  placeholder is told by its whole shape (`Ask anything... "<suggestion>"`,
  with one `…` on 1.19.6)
  and only under the home screen's logo, since its colour does not reach the
  screen reader: a draft that starts with the same words is a draft. A
  redesign of OpenCode's TUI would make a send wait (Booting) until it is
  measured again; the keys that edit a draft are not measured.
- Remote peers. A host grants `FEATURE_OPENCODE` (4) when asked; a client
  asks for it, and on a host that does not grant it (one from before
  OpenCode reads byte 6 as Claude) never asks for OpenCode's models, never
  adds an OpenCode terminal (`TOO_OLD_FOR_OPENCODE`), and the chooser leaves
  OpenCode out (`TerminalsControl::offers`). An old client still reads byte
  6 in a list as Claude, for display only. A `set-settings` from a peer
  that does not name `opencode_config` is its own ask
  (`SetSettingsBeforeOpenCode`) and reaches the packages worker as its own
  command, which keeps the project's OpenCode config as it is when the write
  is applied, after every write queued before it: never a value read
  earlier, never the default.
- Windows: the release install is covered by unit tests and run for real on
  macOS with the macOS archive (the same steps); it has not been run on
  Windows.
