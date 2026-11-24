# Connecto

https://connecto.studio

A desktop app for macOS and Windows (Linux is experimental) that runs Claude Code, Codex, Pi, Antigravity CLI and OpenCode in real terminals, up to 16 per project, and lets them work together. Nothing moves between the sessions unless you send it, or an agent in one terminal uses the `connecto` command (see [Agents using other terminals](#agents-using-other-terminals)).

A companion app, **Connecto Chat** (iPhone, Android, Windows, Linux and Mac), chats with a project's agent on a paired Connecto, and can call it by voice (see [Connecto Chat](#connecto-chat)).

Connecto is released and open source under the [GNU AGPL-3.0](LICENSE). Downloads for macOS and Windows, and experimental Linux archives, are at [connecto.studio](https://connecto.studio); the source is at <https://github.com/marcomanzi/connecto-community>.

## What you need

- To run a release: a Mac (Apple silicon with macOS 11 or later, or Intel with macOS 10.15 or later), or Windows 10 version 1809 or later or Windows 11 on 64-bit Intel or AMD. The Linux archive is experimental: unpack it and run `./connecto-app`; agent setup there is incomplete.
- To build: Rust 1.95 or later, plus:
  - macOS: **libclang**, from the Xcode Command Line Tools: `xcode-select --install`.
  - Windows: the stable MSVC toolchain, Visual Studio 2022 Build Tools with the C++ workload, and a Windows 10/11 SDK (`scripts\windows\bootstrap.ps1 -CheckOnly` checks a machine).
  - Linux: `clang`, `pkg-config` and the development packages for Wayland, xkbcommon, X11 (cursor, randr, xi), OpenGL and OpenSSL (on Debian: `libwayland-dev libxkbcommon-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl-dev libssl-dev`).
- Nothing else has to be installed first: the first launch finds Claude Code, Codex, Pi, Antigravity CLI or OpenCode, or installs the ones you choose with their vendors' own installers.

## Build

```sh
cargo build --release --workspace
```

`connecto-hook` is built beside `connecto-app`; the app looks for it in its own folder.

## First launch

Open the disk image, drag **Connecto** to Applications and open it. Mac releases are signed with a Developer ID and notarized by Apple; a build you make yourself is not, and needs one extra approval the first time: System Settings → Privacy & Security → **Open Anyway**.

On Windows, unzip `connecto-<version>-windows-x86_64.zip` into `%LOCALAPPDATA%\Programs`, so the programs end up in `%LOCALAPPDATA%\Programs\Connecto` (the folder it updates itself in), and keep `connecto-app.exe`, `connecto-hook.exe` and `connecto.exe` together. The build is not signed yet, so SmartScreen asks once: **More info**, then **Run anyway**. Data is kept in `%APPDATA%\connecto`, and the menu is inside the window.

Connecto then sets itself up in a short window: it asks which agents you want (Claude Code, Codex, Pi, Antigravity CLI and OpenCode, each optional; the first two are chosen to begin with), installs a chosen one that is not on your computer from its vendor's official installer, makes its own verified copy of Claude Code and Codex from the ones already installed (Pi, Antigravity CLI and OpenCode update themselves, so they run where they are installed), and prepares its workspace folder (Connecto Home, `~/Connecto`). Then **Sign in** opens each agent's own sign-in, and you pick a folder or try a sample project. Nothing is asked of you except signing in.

The copy of each agent is made by the same code as `connecto-app provision`, which still works for scripts and for picking up a newer version of a tool by hand.

Run from a build in this repository; the same first-launch setup runs when it is needed:

```sh
target/release/connecto-app
```

`CONNECTO_DATA_DIR=/tmp/connecto-try target/release/connecto-app` gives a build its own data folder (workspace, history, audit, vault). It does **not** isolate everything: setup can still prepare `~/Connecto`, projects use your own `~/.claude` and `~/.codex` unless set to Isolated, and Antigravity always uses your `~/.gemini`.

## Projects

**File → Open Project…** opens a folder. **File → New Project** takes a folder and, optionally, a repository address to clone: the system's `git` clones it into a new folder named after the repository (never into an existing one), using your own credential helper and SSH agent; Connecto never asks for or keeps a password. Only https, http, ssh, git, `file://` and absolute local addresses are accepted. Cancel stops git and removes the half-made folder.

**Delete** sits next to Close (desktop only). It shows the full path, what is in it and any git work that would be lost, and asks you to type the folder's name. The project's terminals and background work stop first, and only then is the folder moved to the system's trash; nothing is deleted for good, and a folder with no usable trash is refused (on Windows for now, and on Linux for a folder on another disk than the home trash): remove it in your file manager then. Connecto's own data for the project goes only after the move succeeded.

Projects can be grouped in the sidebar. A project's **Tools** setting is a set of one or more CLIs (Claude, Codex, Pi, Antigravity, OpenCode), or All, the default. Every way a terminal is added or started (the window, an agent's `connecto add` or `start`, a paired machine, Agentic Works, a saved terminal brought back at launch) refuses a CLI the set leaves out, and says which ones the project allows.

## Updates

On macOS and Windows, Connecto checks for a new version at launch and then every minute, downloads it in the background, checks it (a signed feed, the file's size and SHA-256, and on a Mac the same Developer ID and Apple's notarization) and installs it by itself with one restart. It waits while a pairing, a file transfer or a package install is under way, or while you typed in the last 30 seconds; the strip at the top says what it is waiting for. After the restart, open projects and terminals come back and their conversations resume; an agent that was in the middle of a turn is told to carry on. A version that does not start within two minutes is replaced by the one you had. Both automatic checks and automatic installs can be turned off in Settings. Updates are not offered on Linux yet, and a build you make yourself has no update key and does not update itself.

## Pause and Stop

The window no longer shows the manual relay panel of the first versions (the code stays, unused): text moves between terminals when you type it there, or when an agent uses the `connecto` command.

- **Agents → Pause All Agents** (Cmd-., also the pause button in the sidebar) blocks every agent write, key press, new terminal and wake, and stops a send in progress before Enter. Running turns continue.
- **Agents → Stop Everything** pauses first, then stops every terminal. A single terminal is ended with **Close** (or **Restart**) in its menu in the sidebar.
- These controls need the window visible. While it is fully covered or minimized, sessions keep running, but the screen stops updating and no button can be clicked until you bring it back.

## Pi, and any model provider

Besides Claude Code and Codex, a terminal can run [Pi](https://pi.dev), an agent CLI that is not tied to one vendor. It is chosen at the first launch or added in **Settings > Agents**: **Install and activate** runs Pi's official installer from pi.dev. Pi needs Node.js 22.19 or newer: when it is missing or older, setup asks and then installs the official Node.js 22 into your own folder, leaving any Node.js you have alone. A `pi` you installed yourself is found where it is. Then **New Pi** (Option-Cmd-4) or `connecto add pi` opens one.

Pi uses whichever providers are set with **Set up model providers…** on its card there. Pi is offered for new terminals only once one of them works (a saved key, or a local server that answers with a chat model):

- **OpenRouter** and **Together AI**: paste an API key. The key is kept in Connecto's secret store and given to each Pi terminal in its environment when it starts (`OPENROUTER_API_KEY`, `TOGETHER_API_KEY`). It is never written into Pi's own files.
- **LM Studio**: the address of its local server (`http://localhost:1234` unless you changed it; start the server in LM Studio's Developer tab or with `lms server start`). Every chat model the server has is offered in Pi under `lmstudio/`. The address is checked when it is saved and the dialog says what answered. If nothing answers at a local address, LM Studio's own `lms` command is asked which port the server is on (it moves off 1234 when that is taken), and that address is used.

The model is chosen when the terminal is opened: the New terminal chooser has a model field for Pi (type to find one among everything those providers offer; empty is the default), and the setup dialog sets that default. An agent chooses with `connecto add pi --model <provider>/<model>` (`openrouter/...`, `together/...`, `lmstudio/...`), and gets the default without it. Inside Pi, `/model` changes it at any time. A local model that is not loaded is loaded by LM Studio at the first message. LM Studio's own helper models and models too small for Pi's prompt (under 8k tokens of context) are left out. A change of provider applies to terminals started afterwards.

A project whose Pi config is Isolated can have providers of its own: **Set up model providers…** under Pi in Project settings > Tools, with **Use model providers set for this project only** on. Its keys are kept apart from the app-wide ones, and its Pi terminals use only those. System projects use the ones in Settings > Agents.

### Claude Code with Together AI (macOS and Linux)

**Set up model providers…** on Claude's card in Settings > Agents takes a Together AI key. With one, Claude terminals start through [TogetherLink](https://docs.together.ai/docs/togetherlink) (`togetherlink claude …`), with the same Claude copy, profile, hooks and arguments as usual. The key is kept in the secret store and given in `TOGETHER_API_KEY`, never on the command line. When a key is saved and TogetherLink is missing, the dialog offers to install it with Together's own installer (which installs Bun if needed and adds `~/.local/bin` to your shell's startup file); a terminal set up for Together AI does not start without it and says so. A project whose Claude config is Isolated can have a key of its own, in Project settings > Tools. Limits: a model chosen with `--model` is not passed (TogetherLink picks its own), a key saved with `togetherlink configure` wins over Connecto's, and utility sessions (titles, memory) still use your Anthropic sign-in. Not offered on Windows.

How Pi differs from the other two:

- **Pi is not copied or signature-checked.** It is a Node.js program published without a code signature, so Connecto runs it where it is installed and only records where that is.
- **Turns are reported by an extension** Connecto loads into Pi (`pi --extension`), written into the terminal's run folder at each start. Nothing is added to your `~/.pi/agent`.
- **Pi never asks before a tool call.** Settings > Launch only decides whether a project's own `.pi` resources load without asking (`--approve`).
- There are no automations for Pi's screens.

## Antigravity CLI

A terminal can also run [Antigravity CLI](https://antigravity.google/docs/cli) (`agy`), Google's coding agent and the successor of Gemini CLI. Choose it at the first launch, or **Install and activate** it in **Settings > Agents**: Connecto runs Google's official installer (`https://antigravity.google/cli/install.sh`; `install.ps1` on Windows), which puts `agy` in `~/.local/bin`. You sign in with your Google account (or a `GEMINI_API_KEY`) in its first terminal.

How Antigravity differs:

- **It always uses your own `~/.gemini`** and sign-in: it has no way to be pointed at another folder, so there is no Isolated config for it.
- **Turns are reported by hooks** that Connecto keeps as one entry, named `connecto`, in `~/.gemini/config/hooks.json`. Your other hooks there are left as they are. Outside a Connecto terminal the entry does nothing. It stays there if you remove Connecto; delete the `connecto` entry by hand then.
- **It runs where it is installed**, as Pi does: `agy` updates itself.
- Settings > Launch: full power is `--dangerously-skip-permissions`, auto is `--mode auto` (where your account offers it), and prompts is no flag. Antigravity has no hook for its permission prompts, so a terminal waiting at one is not marked as needing you.
- It was built from Antigravity's documentation and checked only with a stand-in program: see the limits L13 to L16 in `docs/specs/agent-terminal-bridge.md`. There are no automations for its screens.

## OpenCode

[OpenCode](https://opencode.ai) (`opencode`), an open coding agent on any provider's models, is the fifth agent. Choose it at the first launch or **Install and activate** it in Settings > Agents (OpenCode's official installer; on Windows its own release from GitHub into `~\.opencode\bin`); an `opencode` you installed yourself is found where it is. Only the 1.x line is supported. **New OpenCode** (Option-Cmd-6) or `connecto add opencode` opens one; sign in inside it with `/connect`, or use its free models with no sign-in.

- **Turns are reported by a plugin** Connecto writes into the terminal's run folder and names in `OPENCODE_CONFIG_CONTENT`, for its own terminals only. Nothing is written into OpenCode's folders, and your own plugins still load.
- **Isolated** gives the project its own OpenCode settings, sign-ins and sessions (OpenCode's three XDG folders inside the project's profile).
- It runs where it is installed, since it upgrades itself. Settings > Launch: `--auto` (every permission your config does not deny), or its normal prompts. `--model` takes `provider/model`; there is no effort setting.
- An interrupted turn ends as failed, there are no automations for its screens, and project memory does not capture its turns yet.

## Agents using other terminals

Every terminal gets a `connecto` shell command, so an agent can see the other terminals, read them, type into them without approval, ask another agent something and read just the answer, and be woken by events there. The commands are `connecto list`, `send <id> [--ask]`, `result <request>`, `read <id>`, `keys <id> <key>...`, `watch <id>` and `unwatch <id>`, `add claude|codex|pi|antigravity|opencode|shell [--name <text>] [--model <model>] [--effort <level>]`, `start <id>` and `close <id>`. After `send --ask` an agent ends its turn: the reply is delivered to it automatically, in its context, labelled as another agent's words, so it does not poll `result`. `close` stops a terminal and closes it: it leaves the list and moves to the project's history, from which it can be read or continued. Each terminal's agent is told about them at launch.

- **`connecto list`** shows each terminal's id, tool, state, how much of its context window the agent has used (the CONTEXT column, a dash when the CLI does not report it), and marks such as `self`, `watched` and who a terminal belongs to. A terminal an agent started belongs to that agent: only it, and the terminals it started, can send to it, press keys in it, start it or close it.
- **`connecto agents [--roles]`** lists the coding CLIs this project can start now: ready, unusable (and why), still being checked, or not allowed by the project's Tools, with each one's version and the command that starts it. With `--roles` it picks a CLI for each role (coder, reviewer, second opinion), preferring a reviewer or second opinion on a different CLI from the coder or the asker, and saying `(same CLI)` when no other is ready. It only reads, so it works under Pause.
- **`connecto tasks <id> [--stop [task]]`** shows the background work an agent left running and for how long, and stops its background shells. The sidebar shows the same, also from a paired machine.
- **Other projects.** `connecto projects` lists the projects Connecto knows. `--project <name|path>`, given every time, lets `list`, `read`, `send` (with `--ask`), `result`, `watch` and `unwatch` reach an open project's terminals. Only talking and reading cross: `keys`, `add`, `start`, `close`, secrets and the rest are refused across projects, and so is a terminal that belongs to an agent there.
- **`connecto send-file <path>`** offers a file to the user on a paired machine, with a Download button.
- **Pause is a full brake** on agent writes, keys, add, start, close and wakes. Stop Everything still ends every terminal, and an agent cannot lift Pause or use Stop Everything.
- Agents are not rate-limited: two terminals that watch each other can keep waking each other, and each wake is a model turn on your plan. Pause stops it, and so does `unwatch`.
- The bridge is on for every CLI version. It is off for a terminal whose hooks could not be set up or whose launch checks failed, and `list` marks it `bridge off`.
- Every agent action is in the audit log with who did it and the redacted text.
- **Your own instructions stay.** The text that tells each agent about `connecto` is added beside your instructions, never in place of them, and your files are never written: Claude gets it at launch, Codex through Connecto's `SessionStart` hook as a developer message of its own (next to your configured `developer_instructions` and `AGENTS.md`), Pi as a prompt section from Connecto's extension (your `APPEND_SYSTEM.md` still loads), OpenCode in its `instructions` list, and Antigravity through its `SessionStart` hook. A Codex conversation started before 0.3.10 keeps its old behaviour when resumed.

## Secrets

Settings → Secrets is a vault for API keys, tokens, passwords and SSH keys, app-wide or for one project. Values are encrypted (AES-256-GCM) under a master key kept in the macOS Keychain, the Windows Credential Manager, or a `0600` file (on Linux, and in any build with a data folder of its own). **A vault value never enters a terminal**: no environment variable, no file. (Model provider keys for Pi and Claude via Together AI, described above, are different: they are given to those terminals in their environment.) A terminal uses a vault secret only through a use you set for it, which Connecto carries out itself:

- `connecto secret url NAME` gives a local address; web requests sent there go on to the bound `https://` address with the secret in the bound header, and the value is removed from the response.
- `connecto secret run NAME -- <program> [args]` runs one program you named, with the value in one environment variable and every vault value hidden in its output. Shells and interpreters cannot be named.
- `connecto secret ssh NAME` gives an ssh-agent socket that only that terminal can use, and the public key. macOS and Linux only: on Windows SSH keys can be kept and generated, but not used to sign.

An agent that needs a secret asks with `connecto secret request NAME --reason '…'` and a proposed use. A dialog shows the terminal, its reason and the use (which you may change), and takes the value; for an SSH key it can generate an Ed25519 pair. The agent is told the outcome and how to use the secret, never the value. Every use is logged by name, terminal and target. `connecto secret list` names the secrets a terminal can use.

The full guide, including requests from paired Macs and what the vault does not protect against, is [on the site](https://connecto.studio/docs/secrets.html).

## Requests that wait on you

A terminal's secret request and an Agentic Works job's question open as a dialog over the window. Either can be minimized to a small card in the bottom-right corner, so you can read the terminal under it (where the agent often explains what it needs); clicking the card opens it again. One request is open at a time. A new request from a terminal whose request you minimized in the last five minutes, or still have minimized, joins the stack marked new instead of covering what you are reading.

## End-of-task status

Connecto's built-in package asks every agent to end a task in which it changed something with a short status block: what was done, what was committed or pushed (branch and hash), what is left or blocked, and whether it is waiting on you. Claude reads it from `.claude/rules/connecto-status.md`, a file the package installs; the other agents get the same text at launch, through the channels above, only when the project uses the built-in package. A project's own instructions about the report format win. `CLAUDE.md` and `AGENTS.md` are never written.

## Skills & Plugins packages

A package is a bundle of skills, subagents, commands, hooks and plugin settings, kept in your Connecto Home (`packages/`) and installed into every project that uses it (**Terminal → Skills & Plugins…**). Connecto copies the files into the project's `.claude`, `.codex` and `.agents` folders for the CLIs its Tools allow, leaves files you edited alone, and records what it installed in `.connecto/packages.lock`.

The built-in package is the default. It holds team skills in which every role runs in a Connecto terminal of its own (`daily-team`, a coder and a reviewer for everyday work; `dev-team`; `brainstorm-team`), a `second-opinion` skill, and the end-of-task status rule. The team skills pick their CLIs with `connecto agents --roles`, so a reviewer or second opinion runs on a different CLI from the coder or the asker when one is ready.

### Game Dev

A second built-in package, for making games in Godot 4 with coding agents that work without the editor open: `game-design` (a one-page design and milestones checked by evidence), `godot-game` (headless import, checks, a test runner that really fails, export, recordings, 3D models from Blender Python scripts) and `game-playtest` (repeatable runs driven by a bot, judged on screenshots, video frames and logs). It is not the default: add it to a project under **Added packages**. [Roots & Ruins](https://connecto.studio/docs/game-dev.html), an isometric 3D farming game with a roguelite in its mines, was built with it.

A package can declare the tools it needs (`requires` in `package.json`, ADR 0037). Game Dev needs Godot 4.7 or a later 4.x, Blender 4.2 or later and FFmpeg 4.4 or later (from 0.3.11; earlier versions check Godot only). Connecto looks for each one where it is usually installed and reads its version, running nothing the package chooses. When one is missing or too old, the dialog shows the exact command for your system's package manager (Homebrew, winget, or Flatpak and the distribution's package manager on Linux), and only after a second yes opens a shell terminal named `Install <tool>` that types it once, where you watch it and answer any prompt. You can also install by hand and check again. The package cannot be added while a tool is missing; a project that already had it is updated, and its status says what is missing.

## Remote sessions

One Connecto can show and drive another's projects and terminals: pair two machines (**Pair a device** in Settings on one, **Add host…** in the sidebar on the other): each shows its own eight-digit code, valid for two minutes, you type each machine's code on the other, and each side names the other (ADR 0022). Or authorize an SSH key you already have and compare fingerprints. The machines connect directly, encrypted end to end, or through a relay that cannot read the traffic (`https://relay.connecto.studio` by default, or your own). Remote access is off until you turn it on, Pause applies, and remote actions are in the audit log.

From a paired machine you see the host's projects and groups, open any of its agents (and its Pi models), watch, type, scroll back and resize terminals, see and stop background work, read the history and project settings, view and approve project operations, answer its secret requests, and preview and download files. Files dropped on a remote terminal, and images pasted into it, are uploaded to the host and their paths typed. From 0.3.11, on a Mac, pasting files copied in Finder into a remote terminal asks first: Enter sends those files, Esc pastes the text.

## Connecto Chat

A Flutter companion for iPhone, Android, Windows, Linux and Mac. Start pairing on the host (**Pair a device** in Settings) and choose **Pair a host** in the app: each device shows an eight-digit code, you enter each one's code on the other and confirm the name on both. Then pick a project, its agent and a model, and chat with that project's agent in an ordinary terminal named `app-session`. You can attach files (up to four, 10 MiB together), see the agent's activity, and answer its questions and permission choices. The Windows, Linux and Mac builds are on the download page.

- **Voice calls** (iOS 26 and macOS 26 or later). Hands-free calls with the project's agent: speech is recognized on the device, the reply is spoken with a natural on-device voice, and you can talk over it. A call agrees a task with you and hands it to the project's agent; tasks agreed in a row wait on the project's **task board** (`.connecto/board.json`, kept by the agent, out of git) and run one at a time, even with the phone closed.
- **Gemini Live.** Optional, per device: with your own Gemini API key (kept in the Keychain, never sent to the host), Gemini holds the call and the project's agent is its tool. Gemini knows the project's functional memory, asks the agent when it needs to look at the code, and proposes tasks on screen; a task starts only after a plain yes the device heard. The phone talks to Google directly.
- **Siri** (iPhone). Ask Siri for a project's status, the agent's last reply, to open a project, to send the agent a message and say its reply, or to answer the question the agent is asking. Each needs the device unlocked.

## Project memory

Each project can keep a memory, off until you turn it on in **Project settings → PROJECT MEMORY**. It holds notes and memories with their sources, and agents read and add to it with `connecto memory brief|search|read|room|propose|flag|status`. While you work, a model (Claude or Codex, your choice) turns your sessions into notes; a scheduled dream consolidates them into memories, published as versions you can undo. Connecto checks everything a model proposes before storing it.

- It lives in Connecto's data folder, never in the repository, and never writes `AGENTS.md` or `CLAUDE.md`.
- Nothing runs while the app is closed. Runs are measured in characters, duration and outcome; no cost is shown.
- An agent repeating what it read from the memory never verifies it.
- What you type in a terminal is never taken as your approval. To accept, reject or correct a memory, use the **Memories** tab.

How it works, what it reads and its limits: [the project memory guide](https://connecto.studio/docs/memory.html).

**Functional memory** is a separate, plain description of what a project does: a `.functionalMemory/` folder of Markdown at the repository root (overview, architecture, features, flows, glossary), turned on under Project Settings > Project Memory. When `main` moves, a read-only utility session updates it after the branch has been still for 90 seconds, and Connecto commits the change on its own. Voice calls read it.

## Project operations and CLI improvements

An **operation** is a task a project repeats, kept as `.connecto/ops/<name>/` (`op.json` plus a `run.sh` or `recipe.md`) and run with `connecto op`. A script runs only after you approved exactly the files you saw; any later change withdraws the approval. When a prompt you type matches an operation, a line suggests it. For Claude only, an operation you switched on can intercept a short matching prompt and run instead. A discovery terminal, started only by you, looks through your past prompts for operations worth writing.

Settings → **CLI improvements** holds tasks a CLI does on itself. **Claude auto apply insights** opens a Claude terminal that runs Claude Code's `/insights` report and applies its suggestions to your Claude set-up.

## What sessions can do

There is no sandbox (ADR 0013). Unless Settings > Launch says otherwise, Claude Code, Codex and Antigravity CLI run with their own approval checks off (`--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`), and Pi never asks; all run as your user, with your files and network. **An agent can change anything your account can change.** Text on one screen can steer an agent that has that reach.

What Connecto still does:

- **It refuses your home folder as a project**, and any folder holding Connecto's data or the tools' own state (`~/.claude`, `~/.codex`, `~/.pi`, `~/.gemini`, `~/Library`).
- **A project can have its own login and profile.** By default a new project runs Claude and Codex with your own `~/.claude` and `~/.codex` (ADR 0020). Set a tool's config to **Isolated (advanced)** in Project settings and that project gets a private profile and login, under `~/Library/Application Support/connecto/projects/<id>/profiles/<tool>/` on a Mac. Antigravity has no Isolated mode.
- **The workspace is saved** in `workspace.json` in the data folder: the open projects, their order and names, the one shown, and the ten most recent. Each project keeps `terminals.json` (names, parents and the tool session ids that let a relaunch resume each conversation) and a `history/` folder with one redacted transcript per closed terminal (at most 500, 2 MiB each). An unreadable `workspace.json` is kept as `workspace.json.bad`.
- **Other sessions cannot message a Connecto Claude** (`crossSessionInbound: refuse`). Claude's own `SendMessage` and `ListAgents` are allowed, so a skill can message its subagents (after the first manual run). Those sends do not pass the bridge's audit or Pause, and a send to a refusing session can start an extra turn on the sender (L3). Claude loads the project's skills, agents and settings (`--setting-sources user,project,local`), so project hooks run alongside Connecto's. A project that sets `disableAllHooks` turns that terminal's bridge off, and one that disables bypass mode shows a warning; both are reported in `list`.
- **Processes that detach.** Stop kills the descendants it can find, but a process that detached before you clicked Stop can outlive Stop and app exit.

**Log in** starts a new, numbered profile generation for that tool in that project, and older generations stay on disk (`g<N>` folders, with the live number in `current`). A repeat login may also leave the previous Claude Keychain item behind.

## Limits worth knowing

- **Hook frames are limited to 256 KiB.** When a tool reports more, Connecto drops text in a fixed order (a long prompt is cut, the reply's text is removed) and keeps the lifecycle event, so the turn still ends, but a reply removed that way does not reach `--ask`.
- Agent sends have resource bounds rather than a size cap: a 256 KiB request frame, a bounded input and write queue per target, and 32 bridge connections at once. (The 4096-byte cap applied to the old manual relay's sends.)
- **What an agent sends is delivered as it is**, control characters removed: credential-shaped text in a `connecto send` reaches the target terminal. Only the copy kept in the audit log is redacted, and your own pane is not redacted either.
- CLI versions are not pinned: the bridge is on for every version, relying on the CLIs staying backward compatible. (Checked on Claude 2.1.318, Codex 0.166.0 and 0.167.0, Pi 1.5.2 and OpenCode 1.18.8; Antigravity CLI 1.5.11 only from its documentation and program.)

## Agentic Works

The user guide is [on the site](https://connecto.studio/docs/agentic-works.html). Choose **Terminal → Agentic Works…** in an open local project. In **Settings → Secrets**, save a PostgreSQL connection string under that project's scope and choose **Agentic Works database** as its use. Enter the secret's name in Agentic Works. **Create queue and link** creates the private `connecto_works` schema in an existing database. **Validate existing queue** checks that schema, its owner, grants, columns and project identity before linking. A project can link several databases, each under a name (`dev`, `staging`, `production`; a link made without one is `default`); one queue is linked under one name only. Remote connections require TLS with a certificate trusted by the host. An agent can also set a work up from its terminal with `connecto works` (`link`, `test-db`, `publish`, `job`, `databases`, …; `--db <name>` picks the database once there are several). The verbs that run commands, start agents or change a credential (`link`, `publish`, `setup`, `test`, `key-import`, `parallel <n>`, `agent …`) work only for an agent started in Bypass mode, and never for a job's agent or a terminal it started; any other agent is told how you do it in the sheet.

A Google Cloud SQL for PostgreSQL instance can be linked instead, without adding this Mac's IP address to the instance's authorized networks: choose **Google Cloud SQL** under **Database** and enter the instance connection name, database, user and password (or turn on IAM database sign-in). Connecto connects as Google's Cloud SQL connectors do: a service account key imported into the vault (or your `gcloud auth application-default login`) authorises it through IAM, the Cloud SQL Admin API issues a short-lived client certificate, and PostgreSQL runs inside mutual TLS to the instance's port 3307, verified against the instance's own CA and name; no proxy runs. The sheet's **How to set this up** panel gives the `gcloud` and SQL commands filled in with what you typed; the same steps are in [the guide](https://connecto.studio/docs/agentic-works.html#cloudsql).

Name a work type, choose Claude or Codex, and open its authoring session. Discuss the instructions, where to insert `{{prompt}}`, and the expected JSON Schema. Return to the menu, enter the same type, import the author's reply, review the fields and publish. For example, a `summary` definition might use `Summarize {{prompt}}` with `{"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}}`.

A work type can also have `setup` and `prepare` commands, which Connecto runs itself (not the agent) in the project root, each written as `{"argv": ["tool/setup.sh"], "timeout_s": 600}`. `argv` is never a shell line: its first item is a program on `PATH` or a path relative to the project, without `..` or control characters. `setup` runs when the type is published, and the type is published only if it succeeds; **Re-run setup** runs it again. It runs with no time limit in a shell terminal of the **agentic jobs** project (the `agentic jobs` folder of Connecto Home, opened in the **Connecto Internal** sidebar group with the Automations project), where you watch it and answer it if it asks for something; the terminal closes on success and stays open on failure. `timeout_s` (1–1800, default 600) limits `prepare`. `prepare` runs before each job with the job's `prompt` on stdin and `CONNECTO_JOB_ID`, `CONNECTO_WORK_TYPE` and `CONNECTO_JOB_DIR` (a fresh folder under `.connecto/works/`, deleted when the job ends) in its environment. It must print one JSON value of at most 256 KiB, which replaces `{{prepared}}` in the instructions; the instructions contain `{{prepared}}` exactly when `prepare` is set. A non-zero exit fails the job before any agent starts; exit 75 runs `setup` once more (within the job's 30 minutes) and retries `prepare` once. `prepare` inherits only `PATH`, `HOME`, `LANG` and `TMPDIR` and is killed with its whole process group on Stop, Pause or project close; `setup` gets your login shell's environment. Both are journalled in the audit log (exit code and duration, never output). They run with your rights, so review them before publishing.

**Queue write access grants authority to start a normal project agent.** Use the configuring PostgreSQL role for trusted producers. Connecto checks for non-owner write grants before claiming work. In the linked database, insert a job with the project's displayed name and the published work type.

```sql
INSERT INTO connecto_works.connecto_jobs (project_name, work_type, prompt)
VALUES ('My Project', 'summary', 'the README in this project');

SELECT id, status, result, reason
FROM connecto_works.connecto_jobs ORDER BY id DESC;
```

A work type is published once and serves every linked database, unless it is published for a list of them (`publish <type> --db dev,staging`).

**Which agent runs a job.** Set `session_cli` to `claude`, `codex`, `pi` or `opencode` to choose one; the job then runs there, with its optional `model` and `effort`, or fails if the project cannot run that agent. A job that leaves `session_cli` blank or names `agy` (the column's default) gets the project's **Agent for jobs**, set in the Agentic Works sheet or with `connecto works agent <cli> [--model M]`. Its default, Automatic, uses Antigravity when the project can use it (allowed by its Tools, switched on, installed and signed in), and otherwise a stand-in, the first usable of Codex, Claude Code, Pi and OpenCode: Codex runs `gpt-6.1-terra` and Claude runs `haiku`, while Pi and OpenCode use their defaults. The view says which agent a job runs on and whether it stands in; a job no agent can run fails with the reason for each.

**Several jobs at once.** **Jobs at once** (1 to 32, default 1; `connecto works parallel <n>`) lets a project run several jobs side by side, each in its own terminal and job folder; the linked databases share the places fairly. With 1, a job may treat the project's files as its own.

**Job questions.** A job's agent that misses information it cannot find itself may ask you one question at a time with `connecto works ask '<question>'` and end its turn. The question opens as a dialog (see [Requests that wait on you](#requests-that-wait-on-you)) with the project, job, work type and database; you answer or choose No answer, and the agent is woken to read the reply with `connecto works answer` and continue. However the job goes, it ends with the final JSON.

While the project is open and Connecto is running and unpaused, a worker looks at the linked databases every five seconds, and again as soon as a job ends, and claims pending rows. Each job runs in a terminal named `agentic_works_<id>_…`, which is closed when it ends; background work the agent leaves running keeps the job open until it ends. If the project is renamed, update the project name on its pending rows. A job's session follows the project's normal tool permissions. Stop, Pause, provider approval requests, provider failures and a failing `prepare` end the work. A job has 30 minutes from claim to finish. Completed rows contain the validated JSON in `result` with status `done_from_agent`. Failed and timed-out rows carry a reason. Connecto journals outcomes locally and retries delivery on reconnection. It never automatically submits an uncertain attempt again. If a connection drops during a claim commit, a committed row can stay doing until its 30-minute claim expires.

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

Design: `docs/poc/UI-RELAY-POC.md` and `docs/adr/`. Project memory: `docs/specs/project-memory.md`.

Releases are not built by CI. Pushing a version tag starts a tag watcher on a dedicated release Mac (`scripts/release/watch-tags.py`), which builds, signs and notarizes the macOS disk images, builds the Windows zip, the Linux archives and the Connecto Chat Windows build, publishes them with the signed update feed, and updates the site's downloads. A step that completed is reused if a later one (notarization, the network, the deploy) fails.

## License

Copyright (C) 2026 Marco Manzi.

Connecto is free software: you can redistribute it and/or modify it under the terms of the GNU Affero General Public License, version 3 only (`AGPL-3.0-only`), as published by the Free Software Foundation. It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See [`LICENSE`](LICENSE) for the full text.

Source: <https://github.com/marcomanzi/connecto-community>. Issues and pull requests are welcome there. Connecto is community-funded; if it helps you, you can [sustain its development](https://connecto.studio/#donate).
