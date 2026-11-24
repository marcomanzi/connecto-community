# Windows port: handoff

Written 2026-11-12 on macOS for an AI coding agent and the user, to be read in a
fresh Claude Code session on a Windows machine. Every code claim below was checked
against the files on the date above; line numbers are from that check and drift as
the code changes. Anything not checked is marked **unverified**.

Working directory for all commands: the repository root (the clone). Use absolute
paths when you run tools.

> **State as of 2026-11-13 (read this first; the sections below are kept as written
> and are partly history).** The workspace **compiles, passes its tests and runs on
> Windows** (x86_64, Windows 11): M1, M2 and M3 are done, and M4 is done except what
> needs GitHub, a certificate or a real SSH agent (section 7, M4, and the newest
> entry in section 10). So section 1's "does not compile for Windows", section 3
> (first steps) and the "unverified"/"never produced a working build" notes in
> sections 1 and 4 describe the start of the port, not today. The branch is
> `windows-port` (main 0.1.6 plus the port); it has not been merged. What a Mac must
> still do before merging: `cargo test --workspace` (done once on 2026-11-13 for
> 6ee6f3f; repeat for the M4 commits), the statusline goldens with the real `jq`
> (four cases were added in M4), the macOS release build, and a look at the native
> menu (built from the shared menu description since M1).

## 1. Goal and current state

Goal: Connecto (the desktop app, the `connecto-hook` helper and the `connecto` command)
runs on Windows 10 1809 or newer, with the same features as macOS where Windows
allows.

Today it does **not compile for Windows**. Reasons, by layer (details in section 4):

- Unix-only APIs in about 30 files that mention `std::os::unix`, plus the `nix` crate
  (in four crates) and the `libproc` crate (in one).
- The secret store is macOS Keychain only.
- Agent trust checks Apple code signatures.
- Shell, paths and tools are macOS-specific (`/bin/zsh`, `~/Library/...`, `scutil`,
  `open`, `defaults`).

We have not run `cargo check --workspace` for Windows yet, so the real error list is
**unverified** until the first steps in section 3 are done.

### Existing groundwork (checked)

- **`.github/workflows/release-build.yml`**, job `windows` (lines 60-70):
  `runs-on: windows-2022`, runs `scripts/release/build-windows.ps1`, uploads
  `dist/*.zip`. It is `workflow_dispatch` only, so it runs by hand. The job has **no
  Rust toolchain step**; only the macOS job does `rustup target add` (line 26). Whether
  the GitHub Windows image has the MSVC Rust toolchain preinstalled is **unverified**.
  Expect this job to fail until M1 is done.
- **`scripts/release/build-windows.ps1`** (checked): reads the version from
  `Cargo.toml` `[workspace.package]`; runs
  `cargo build --release --locked --target x86_64-pc-windows-msvc -p connecto-app -p connecto-hook`;
  copies `connecto-app.exe` and `connecto-hook.exe`; copies the hook as `connecto.exe`;
  writes `README.txt`; zips to `dist\connecto-VERSION-windows-x86_64.zip`; adds the
  zip's SHA-256 to `dist\SHA256SUMS`; prints that the build is UNSIGNED. It has never
  produced a working build (the crates do not compile for Windows yet).
- **`scripts/release/README.md`**, section "Linux and Windows are not ported". Its
  list is partly wrong (checked):
  - It says `security-framework` is a hard dependency of `connecto-net` and
    `connecto-automation`. Only **`connecto-secrets`** depends on it directly
    (`crates/connecto-secrets/Cargo.toml:13`). `connecto-net` and `connecto-automation`
    depend on `connecto-secrets` (their Cargo.toml line 13 and 17).
  - It says `libproc` is a Linux problem. `libproc` is a macOS crate (see section 4,
    M1). It is used in `connecto-session` only.
  - It says "about 25 files" use `std::os::unix`. The count is **30** files.
  - "`~/.local/bin`" and "`~/Library/Application Support`" are correct.
  - It says Windows builds are unsigned. Correct.
  The README is not changed by this document; fix it when the port starts.
- **`site/assets/releases.js`** lists only `.dmg` files (lines 13, 21, 29). No Windows
  entry exists.

## 2. Open decisions (the user must decide)

The coordinator's recommendation is marked **[recommended, not yet confirmed]**. Do
not implement a non-recommended option without asking the user.

1. **Agents native on Windows or WSL?** [recommended, not yet confirmed]: native.
   Claude Code runs natively on Windows 10 1809+ (official install page, see section
   5); native Codex support is **unverified** (see section 5). WSL would avoid the
   agent-hook and trust work in M3 but puts the agents in a Linux environment that
   Connecto must then bridge.
2. **Trust check on Windows:** Authenticode verification of the agent binaries, or
   accept agents without it at first? [recommended, not yet confirmed]: implement
   Authenticode verification (Claude's binary is signed by "Anthropic, PBC" per the
   Claude Code docs); accepting unsigned agents only as a temporary, visible step.
3. **Packaging:** unsigned zip first, or MSI/NSIS? [recommended, not yet confirmed]:
   unsigned zip first (what `build-windows.ps1` already makes). Whether to buy a
   code-signing certificate is a separate decision; without one, SmartScreen warns on
   first run.
4. **Builds on the GitHub Actions `windows-2022` runner** or cross-compiling from
   macOS? [recommended, not yet confirmed]: the Actions runner, because the MSVC
   linker and Windows SDK are needed and cross-linking them from macOS is not set up.
5. **Minimum Windows version:** 10 1809 (the ConPTY minimum). [recommended, not yet
   confirmed]. Claude Code's minimum is the same (Windows 10 1809+).

## 3. First steps on the Windows machine

Run these in a plain PowerShell in the repository root. The execution-policy flag
applies to that one process only:

1. `git status` and `git branch`. Create the branch `windows-port` from `main`
   (`git switch -c windows-port`). Do not commit or push without the user's say-so.
2. `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\bootstrap.ps1 -CheckOnly`
   It installs nothing and exits 3 if something is missing. Read its output.
3. **Untested script:** `bootstrap.ps1` was written on macOS and has never run on
   Windows. If `-CheckOnly` shows a bug, fix the script on this branch and say so in
   the commit message.
4. Run `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\bootstrap.ps1`
   (without `-CheckOnly`). It uses winget. Accept UAC prompts. If it asks for a new
   shell, open a new PowerShell and run step 2 again until it reports everything
   present.
5. Optional: `... bootstrap.ps1 -WithAgents` for the Claude Code and Codex CLIs
   (needed only for the agent work in M3).
6. In the repository root: `cargo check --workspace` (not `--all-targets`, to start).
   Save the **first 200 error lines plus the list of failing crates** into the Status
   section (section 9) under "Baseline, date". Do not commit the output as a file.
7. Confirm the toolchain: `rustc -vV` shows `host: x86_64-pc-windows-msvc`.

Record each result in the Status section.

## 4. Work list

Each item was checked in the code. Line numbers are as checked on 2026-11-12.

### 4.1 Unix-only code (M1)

- `crates/connecto-packages/build.rs:7` imports `std::os::unix::fs::PermissionsExt`
  (the build script itself does not compile on Windows).
- `std::os::unix` appears in **30 files** (`grep -rl os::unix crates`). The main
  source ones: `crates/connecto-app/src/{agent_install,agents_host,main,setup}.rs`,
  `crates/connecto-automation/src/{keystore,store}.rs`,
  `crates/connecto-hook/src/{client.rs,bin/connecto-hook.rs}`,
  `crates/connecto-net/src/{secrets,trust}.rs`,
  `crates/connecto-packages/src/{engine,fsx,install,package}.rs`,
  `crates/connecto-session/src/{bridge/server.rs,codex_trust.rs,history.rs,hook.rs,launch.rs,project_folder.rs,sweep.rs}`,
  `crates/connecto-store/src/{discover,store}.rs`. Tests in `tests/` directories
  also use it.
- `nix` (Unix system calls): `crates/connecto-pty/Cargo.toml:17`,
  `crates/connecto-session/Cargo.toml:27` and `:33`, `crates/connecto-store/Cargo.toml:15`.
  The `nix` crate does not build on Windows. It is not target-gated.
- `libproc` (macOS process info): `crates/connecto-session/Cargo.toml:17`, not
  target-gated. Used in `crates/connecto-session/src/hook.rs:20-21` and
  `sweep.rs:13-14`. Its build pulls in `bindgen` and `clang-sys` (checked in
  `Cargo.lock`); `libproc`'s build script only runs bindgen on macOS. The fix is to gate
  the dependency with `[target.'cfg(unix)'.dependencies]` or `cfg(target_os = "macos")`
  and put the macOS code behind that.
- Workspace lint: `Cargo.toml:41-42` sets `unsafe_code = "deny"` for every workspace
  crate. Clippy also denies `unwrap_used`, `expect_used` and `panic` (`Cargo.toml:44-47`).
  Windows system calls therefore need a dependency that provides a safe API. Candidates
  to check (none verified): `sysinfo` for the process tree, `tokio` named pipes for the
  socket work in M2, and a `windows-sys` based wrapper crate for job objects. Verify
  each one before choosing.
- `cfg(unix)` / `cfg(windows)` guards: only one exists in the workspace,
  `crates/connecto-ui/src/menu.rs`. Expect to add the module split yourself.
- Only one target table in the crates' Cargo files:
  `crates/connecto-ui/Cargo.toml:26` `[target.'cfg(target_os = "macos")'.dependencies]`.

### 4.2 PTY and process control (M1)

- `crates/connecto-pty/src/session.rs:83`:
  `pair.master.tty_name().ok_or(PtyError::UnknownTty)?`. portable-pty's ConPTY
  backend has no device path, so this fails on Windows. **Unverified** how portable-pty
  0.9 (`Cargo.lock`) behaves on Windows; check the crate source in the cargo registry
  (`%USERPROFILE%\.cargo\registry\src`) before changing it. The reader
  `OpenPty::tty_path` (`session.rs` ~line 88) and the places that use the path must
  be made optional.
- `crates/connecto-pty/src/session.rs:14`, `:120`, `:122`: `killpg` (process groups).
  On Windows, use a Job Object or `child.kill()` for the tree. `kill_and_reap` at
  `session.rs:142-144` already calls `child.kill()`.
- Default shell: `crates/connecto-session/src/live.rs:27` `pub const SHELL: &str = "/bin/zsh"`;
  `crates/connecto-session/src/launch.rs:883-905` (`shell_program`, `shell_program_from`:
  `$SHELL`, else `/bin/zsh`, `/bin/bash`, `/bin/sh`); `launch.rs:941` sets `SHELL`.
  Windows needs PowerShell (`pwsh` or `powershell.exe`) as the default.
- **User requirement (2026-11-12):** the app's setup on Windows must also install
  PowerShell (PowerShell 7, `pwsh`; winget ID `Microsoft.PowerShell`, **unverified**)
  when it is missing, next to the other things setup installs. Windows ships only
  Windows PowerShell 5.1 (`powershell.exe`), which stays the fallback. Interpretation
  to confirm with the user: this means the app's setup flow
  (`crates/connecto-app/src/setup.rs`), not `scripts/windows/bootstrap.ps1`.
- `crates/connecto-store/src/discover.rs:31` reads `SHELL` for the login PATH (macOS
  login shell); on Windows the PATH is the environment's PATH.
- The shell's startup flags: `crates/connecto-session/tests/bridge.rs:3027` sets
  `/bin/sh`; `crates/connecto-session/tests/real_cli.rs:70-79` run `/bin/zsh` and
  `/bin/sh` (test-only).
- Shell env: `crates/connecto-adapter/src/spec.rs:231-236` sets `HOME`, `TMPPREFIX`
  for zsh (`TMPPREFIX` is a zsh setting; Windows needs its own env).

### 4.3 Paths and macOS tools (M1)

- `HOME` is required: `crates/connecto-app/src/main.rs:51-53` (error "HOME is not set"),
  `:120`, `:587`. Use `USERPROFILE` on Windows (a small `home_dir()` helper in one module).
- Data folder: `crates/connecto-store/src/store.rs:128-132`
  `data_root` = `CONNECTO_DATA_DIR` or `home/Library/Application Support/connecto`.
  Windows: `%APPDATA%\connecto` (choice to confirm; `%LOCALAPPDATA%` is an alternative).
- Secret lock file: `crates/connecto-secrets/src/lib.rs:498-503` (`Secrets::system`):
  `$HOME/Library/Application Support/connecto/.secrets.lock`. Move to the data folder
  helper.
- `crates/connecto-session/src/project_folder.rs:93`, `:122`, `:170` and
  `crates/connecto-packages/src/home.rs:37`: a `Library` folder is treated specially
  (the macOS home folder). Check whether this is a deny-list (macOS-only) and gate it.
- `scutil`: `crates/connecto-app/src/main.rs:396` (computer name, macOS only). Windows:
  `COMPUTERNAME` environment variable.
- `open`: `crates/connecto-ui/src/setup_window.rs:453` `Command::new("open").arg(url)`.
  Windows: `cmd /c start` is unsafe with URLs; use `explorer.exe <url>` or the
  `open` crate (verify). Not `start`.
- `defaults`: `crates/connecto-ui/src/setup_window.rs:43` (macOS preferences). Find
  what it does and give Windows a registry or file equivalent, or no-op.
- Menlo font: `crates/connecto-ui/src/fonts.rs:11`
  `/System/Library/Fonts/Menlo.ttc`. Windows: Consolas or Cascadia Mono
  (`C:\Windows\Fonts`); verify the file names on the machine.
- Restart after setup: `crates/connecto-app/src/main.rs:521-563`
  `run_setup_then_restart` replaces the process with `CommandExt::exec` (line 563). On
  Windows `exec` does not exist: spawn the new process, then exit the current one.
- `crates/connecto-store/src/discover.rs:240`, `:253` (`/opt/homebrew/opt/node/bin`),
  `:262-263` (`~/.local/bin`, `/opt/homebrew/bin`), `:221` (`known_dirs`), `:28`
  (`login_path`). The fnm node path (`Library/Application Support/fnm`, line 240) is
  macOS; Windows fnm is `%APPDATA%\fnm`.

### 4.4 Secrets (M1)

- `crates/connecto-secrets/Cargo.toml:13` `security-framework = "3"` is **not**
  target-gated.
- `crates/connecto-secrets/src/lib.rs:101` `pub trait Items`; `:119` `pub struct
  KeychainItems`; `:123`, `:131`, `:136` call `security_framework::passwords::*`;
  `:146` `MemoryItems` (tests); `:487` `Secrets::new(items, lock_file)`.
- Plan: a Windows `Items` implementation over the Credential Manager (`CredWriteW`,
  `CredReadW`, `CredDeleteW`, or a crate that wraps them; **unverified** which crate
  is safe under `unsafe_code = "deny"`). Store one JSON item per service.
- **Unverified:** the brief said the Credential Manager item is limited to 2560 bytes.
  The code has no such constant (`grep` found none in `connecto-secrets`). Check the
  Windows API documentation for `CREDENTIALW.CredentialBlobSize` before relying on it.
- **State 2026-11-13 (M4):** the limit is 2560 bytes per credential
  (`credman.rs` `MAX_ITEM_BYTES`). Main's mutual pairing stores one SSH key per
  paired machine in the same document (about 500 bytes each), so only about three
  machines fitted. Fixed in `credman/parts.rs`: a document that fits stays in
  `connecto/secrets` as before; a larger one goes into up to 16 parts
  `connecto/secrets.<generation>.<n>`, the item holding a header (layout,
  generation, parts, length, FNV-1a 64). Parts first, header last, old parts
  removed after; a failed write keeps the old document; a missing or mismatched
  part is an error. Limit 40960 bytes (`MAX_VALUE_BYTES`), refused above it. An
  earlier build reads a split item as unreadable and leaves it alone. Tested in
  memory (failure at every write) and against the real Credential Manager with
  throwaway targets; pairing itself on Windows is still untried.

### 4.5 Local sockets and caller identification (M2)

- Hook socket: `crates/connecto-session/src/hook.rs:9` (imports `UnixListener`,
  `UnixStream`), `:22` (`LocalPeerPid`), `:145`/`:156` (listener), `:216`
  `accept_loop`, `:250-251` `take_frame` reads the peer pid with
  `getsockopt(&stream, LocalPeerPid)`; `:252` `descends_from(peer, config.root_pid)`
  refuses a caller who is not a descendant of the terminal's root process; `:311`
  `descends_from`.
- Bridge socket: `crates/connecto-session/src/bridge/server.rs:24` (`LocalPeerPid`
  import), `:218` `find_caller`, `:292` `getsockopt(&stream, LocalPeerPid)`, `:321`
  `find_caller` call.
- Client side: `crates/connecto-hook/src/client.rs:8` and `:165`
  (`UnixStream::connect`); `crates/connecto-hook/src/bin/connecto-hook.rs:14`, `:87`,
  `:106`.
- Plan: named pipes (`\\.\pipe\connecto-...`) with the client pid from
  `GetNamedPipeClientProcessId` and a process-tree lookup for the descendant check.
  The check's logic (`descends_from`) can stay the same; only the pid source and the
  process-tree lookup change. **TCP loopback must not be used**: it cannot identify
  the caller process, so the descendant check would not mean anything.
- `crates/connecto-session/src/hook.rs:425` uses `std::os::unix::process::parent_id`
  (test code). Windows: use the process-tree lookup.

### 4.6 Agents: trust, discovery, install, packages (M3)

- Trust: `crates/connecto-store/src/signature.rs:8` `CODESIGN = "/usr/bin/codesign"`;
  `:10-11` Mach-O magic numbers; `:25` `is_macho`; `:37` `trait` with `verify`; `:51`
  the codesign implementation; `crates/connecto-store/src/store.rs:52-55` `team_id()`
  returns `2DC432GLL2` for Codex; `:833-841` the team check. Windows: Authenticode
  (`Get-AuthenticodeSignature`, or the WinVerifyTrust API) with a publisher check; PE
  file detection instead of Mach-O. This is open decision 2.
- Codex layout: `crates/connecto-store/src/store.rs:172-178` `codex_platform()` returns
  only `x86_64-apple-darwin` or `aarch64-apple-darwin` (the `cfg!` picks macOS
  triples even on other OSes). Windows needs its own triple and `.exe` names.
- Discovery: `crates/connecto-store/src/discover.rs` (see 4.3). Windows also needs
  `.exe`, `.cmd` and `.ps1` shims from npm (`%APPDATA%\npm`). **Unverified** how the
  official Codex and Claude Code Windows installs lay out their files; check them
  after install (`where.exe claude`, `where.exe codex`).
- Installer: `crates/connecto-app/src/agent_install.rs:5` documents
  `https://claude.ai/install.sh` run with bash; `:61` `shell: "/bin/bash"`; `:150`
  `/usr/bin/curl`. Windows: the official Claude installer is
  `irm https://claude.ai/install.ps1 | iex` (code.claude.com docs, Windows tab) or
  `winget install Anthropic.ClaudeCode`. Pick one and use the same source for the
  trust check. Codex: the official README gives `npm install -g @openai/codex` and a
  PowerShell installer; whether Codex is supported natively on Windows is **unverified**.
- Builtin packages with shell scripts: `crates/connecto-packages/builtin/claude/.claude/scripts/statusline.sh`,
  `.../subagent-statusline.sh`, and the same two under `builtin/both/`. The build embeds
  them (`crates/connecto-packages/build.rs`). Adapter: `crates/connecto-adapter/src/spec.rs:8-10`
  `shell_quote` quotes for `sh`. Windows needs PowerShell versions of the four scripts
  or a Git Bash dependency. Decide with the user; the bootstrap does not install Git Bash
  unless Git for Windows is present.
- Hooks on Windows: **unverified** how Claude Code and Codex run hook commands on
  Windows (the Claude Code setup page says the Bash tool uses Git Bash and falls back to
  the PowerShell tool; it does not say which shell hooks use). Verify on the machine.

### 4.7 Remote, SSH, tests, release (M4)

- iroh: `crates/connecto-net/Cargo.toml:28` and `:43` enable iroh with `tls-ring` (ring).
  **Checked in the crate source:** ring 0.17.14 builds on x86_64 Windows from the
  pregenerated assembly in its crates.io package, so NASM and Perl are not needed. The
  MSVC toolchain and the Windows SDK are. (ring's `build.rs` uses NASM only for a git
  checkout.) Whether iroh builds on Windows as a whole is **unverified**.
- SSH agent: `crates/connecto-net/src/auth.rs:95` reads `SSH_AUTH_SOCK`; `:105`
  `ssh_agent_client_rs::Client::connect(&path)`. The Windows OpenSSH agent uses the
  named pipe `\\.\pipe\openssh-ssh-agent`. **Unverified** whether `ssh-agent-client-rs`
  (`crates/connecto-net/Cargo.toml:26`, version "1") can connect to a named pipe; if not,
  add a named-pipe path.
  **State 2026-11-13 (M4):** implemented (`auth.rs` `WINDOWS_AGENT_PIPE`, used when
  `SSH_AUTH_SOCK` is unset; `ssh-agent-client-rs` 1.1.4 opens a named pipe); a unit
  test covers the default pipe and the error for a missing agent (it names the
  "OpenSSH Authentication Agent" service), and the ignored `ssh_agent` test, run on
  the Windows machine with the service disabled, fails with that message. **Never
  tested against a real agent** (the service is disabled there and was left so).
  Manual test: administrator PowerShell
  `Set-Service ssh-agent -StartupType Manual; Start-Service ssh-agent`; as the user
  `ssh-add` (a key in `~\.ssh`); then, with `SSH_AUTH_SOCK` unset,
  `cargo test -p connecto-net --test ssh_agent -- --ignored --nocapture` (expect
  "accepted SHA256:..."). Git for Windows' `ssh-agent.exe`/`ssh-pageant` speak
  Cygwin sockets, not this pipe, so they are not a substitute.
- Tests that assume Unix: `crates/connecto-session/tests/launch.rs:35`, `:64`, `:212`
  (`#!/bin/sh` fake CLIs); `crates/connecto-session/tests/relay.rs:27`, `:30`;
  `crates/connecto-session/tests/bridge.rs:137`, `:159`; directory walks that skip
  symlinks at `crates/connecto-session/tests/launch.rs:88`, `bridge.rs:87`, `real_cli.rs:94`;
  `crates/connecto-packages/src/fsx.rs:143`, `:148` (symlink tests);
  `crates/connecto-net/tests/manager.rs` (uses `std::os::unix`);
  `crates/connecto-workspace/tests/workspace.rs` (uses `std::os::unix`). Add `#[cfg(unix)]`
  to Unix-only tests, or write Windows equivalents; do not delete tests.
- Release: `scripts/release/build-windows.ps1` and the CI job (section 1). Missing:
  installer (zip only), code signing (Authenticode; SmartScreen warns on unsigned
  files), a Windows entry in `site/assets/releases.js` (lines 13, 21, 29 list only
  `.dmg`), and a Windows release note in `scripts/release/README.md`.

### 4.8 M3 findings (verified on Windows, 2026-11-12)

Machine: Windows 11 Pro 10.0.26300 x64, Claude Code 2.1.322 (native install), Codex
0.167.2 (installed for this check with `npm.cmd install -g @openai/codex`), Git for
Windows 2.46.0, Windows PowerShell 5.1.26100, PowerShell 7.7.0 (MSIX, on PATH through
`%LOCALAPPDATA%\Microsoft\WindowsApps\pwsh.exe`), Node 20.17.0. User decisions taken
before this check: agents run **natively** (no WSL); trust = **Authenticode signature +
publisher check**; the four builtin statusline scripts get **PowerShell versions**.
Everything below was run on this machine unless it is marked **unverified**. The line
numbers in 4.6 have drifted; the ones here were checked on the same date.

#### 4.8.1 How Connecto drives the agents today (what M3 must make work)

- **Store** (`crates/connecto-store`):
  - Discovery `discover.rs:84-123`: process PATH (plus macOS login PATH), then
    `os::known_dirs`, then `os::fixed_dirs`. Windows (`discover/windows.rs:14-54`):
    no login PATH; known dirs `AppData\Roaming\npm`, `.bun\bin`, `.cargo\bin`,
    `AppData\Local\pnpm`, fnm and nvm-windows; fixed dir `~\.local\bin`; names tried
    `name.exe`, then `name.cmd`.
  - Shims: `store.rs:237-294` (`layout_problem`, `codex_layout_reason`) canonicalise the
    candidate; Claude is refused if it is a launcher (`.bat/.cmd/.ps1`, or `js/mjs/cjs`).
    Codex: `codex_release_root` (`store.rs:204-234`) takes `package = real/../..`; a
    complete release there is used directly, otherwise `package/package.json` must name
    `@openai/codex` and the release is `…/@openai/<platform>/vendor/<triple>/`.
  - Trust: `signature.rs:40-43` `trait Signature { fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String>; }`,
    implemented by `Codesign` (Unix `:70-103`: `/usr/bin/codesign --verify --strict`,
    then `codesign -dv` and the `TeamIdentifier=` line; both with `env_clear()`).
    Windows stub `:57-68` refuses every file (fails closed). `is_macho` `:18-37`.
    Team ids `store.rs:48-56` (Claude `Q6L2SF6YDW`, Codex `2DC432GLL2`).
    `verify_signatures` `store.rs:822-844` checks the entry with the team, then every
    other copied file for which `is_macho` is true (on Windows: none today).
  - Provision `store.rs:714-819`: copy into `<DATA>/cli/<tool>/<version>/`, hash, verify
    signatures, `read_version`, write `SHA256SUMS`, lock. Entries: Windows `claude.exe`,
    `bin/codex.exe` (`platform/windows.rs:20,24`). Codex copies `release_entries`
    `["bin","codex-path","codex-resources","codex-package.json"]` (`store.rs:72-77`).
    `codex_platform()` Windows (`platform/windows.rs:179-189`):
    `("x86_64-pc-windows-msvc","codex-win32-x64")` / `("aarch64-pc-windows-msvc","codex-win32-arm64")`
    — **correct** (matches the installed package, see 4.8.3).
  - `read_version` `store.rs:320-336`: `<entry> --version` with `env_clear()` and only
    `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` = a scratch folder, cwd = scratch, **no
    timeout**.
- **Adapter** (`crates/connecto-adapter`):
  - `shell_quote` `spec.rs:8-12` (POSIX `'…'`), used at `claude.rs:55,58` (Claude hook
    commands) and `codex.rs:122` (Codex hook commands).
  - Claude: `--settings <run>/claude-settings.json --setting-sources user,project,local
    --strict-mcp-config`, `--session-id`/`--resume`, `--append-system-prompt`, mode flags
    (`claude.rs:205-293`). Generated settings `claude.rs:43-85`: hooks `SessionStart`,
    `UserPromptSubmit`, `Stop`, `Notification`, each
    `{"type":"command","command":"'<hook>' claude --event <E>[ --context]"}`, plus
    `crossSessionInbound` and an `env` block repeating `CONNECTO_HOOK_SOCK`,
    `CONNECTO_HOOK_TOKEN`, `CONNECTO_BRIDGE_SOCK`, `CONNECTO_TERMINAL`, `PATH`.
  - Env: `base_env` `spec.rs:225-238` (`HOME`, `TERM`, `COLORTERM`, `TMPDIR`, `TMPPREFIX`),
    Claude `tool_env` `claude.rs:142-163` (`CLAUDE_CONFIG_DIR`, optional
    `CLAUDE_SECURESTORAGE_CONFIG_DIR`, `CLAUDE_CODE_TMPDIR`, `DISABLE_UPDATES`,
    `DISABLE_AUTOUPDATER`), Codex `codex.rs:37-49` (`CODEX_HOME`), `hook_env`
    `spec.rs:241-257`, `with_bridge_path` `spec.rs:261-272`. **Bug for Windows:**
    `spec.rs:268` joins `PATH` with a hard-coded `":"` (the session's shell launch already
    uses `platform::PATH_SEPARATOR`); the broken value also lands in the settings `env`.
  - Codex: `resume <id>`, `--no-daemon --no-alt-screen`, mode flags, `-C <project>`,
    `-c notify=["<hook>","codex"]` (argv, no shell), `hook_config` (`codex.rs:110-130`:
    `hooks.<E>=[{hooks=[{type="command",command="'<hook>' codex-hook --event <E>"}]}]`
    for `SessionStart`, `UserPromptSubmit`), `-c <trust>`, `developer_instructions`.
    `expected_hook_keys` `codex.rs:135-140` hard-codes
    `"/<session-flags>/config.toml:session_start:0:0"` — **wrong on Windows** (4.8.3).
    All of 0.167.2's flags used by Connecto exist on Windows (`--no-daemon`,
    `--no-alt-screen`, `--approve-for-me`, `--dangerously-bypass-approvals-and-sandbox`,
    `-C`).
- **Session** (`crates/connecto-session`): gate `agents_supported()`
  (`platform/windows.rs:283-289`, `Unsupported`), checked at `launch.rs:684`
  (`start_verified_copy`), `:737` (`start_resuming`), `:1001` (`start_login`), `:1041`
  (`check_auth`); `system_signin` `:1162-1187` returns `Unknown`. `start_resuming`
  `:719-837` passes the canonical (on Windows `\\?\`) project path as cwd **without**
  `platform::child_cwd` (the shell launch at `:941` does strip it) and to Codex `-C`.
  `codex_trust.rs:49-72` runs `codex -c <hook_config…> app-server --listen stdio://` with
  `env_clear()`, `CODEX_HOME`/`HOME` = scratch, `PATH=/usr/bin:/bin`, then JSON-RPC
  `initialize` / `initialized` / `hooks/list` and builds
  `hooks.state={"<key>"={trusted_hash="sha256:…"},…}` (`:39-45`); it edits no file.
  `history.rs` is Connecto's own terminal history; Claude transcripts are found by
  scanning `<profile>/projects/*/<session>.jsonl` (`utility.rs:611-622`); the folder
  encoding exists only in a test helper (`launch.rs:1206-1217`); Codex rollouts
  `<profile>/sessions/**/rollout-*-<thread>.jsonl` (`codex.rs:65-101`).
- **Packages** (`crates/connecto-packages`): variants `claude`, `codex`, `both`
  (`package.rs:22-51`), embedded by `build.rs` (`FILES` table, no platform dimension).
  `builtin/claude/.claude/settings.json:6-13` and `builtin/both/.claude/settings.json:15-22`:
  `"statusLine": {"type":"command","command":"bash \"${CLAUDE_PROJECT_DIR:-.}/.claude/scripts/statusline.sh\""}`,
  `"subagentStatusLine": {"type":"command","command":"sh \"${CLAUDE_PROJECT_DIR:-.}/.claude/scripts/subagent-statusline.sh\""}`.
  The `claude/` and `both/` copies of the two scripts are byte-identical (so: two
  scripts, four files). `statusline.sh` reads stdin with `jq` (`.model.display_name`,
  `.effort.level`, `.context_window.used_percentage`,
  `.context_window.current_usage.{input_tokens,cache_creation_input_tokens,cache_read_input_tokens}`,
  `.context_window.context_window_size`, `.cost.total_cost_usd`), formats numbers with
  `awk` (`N.NM`/`Nk`), prints one line: model (cyan) · effort (magenta) | 20-cell
  `█`/`░` bar + `NN%` (yellow <40, orange `38;5;214` from 40, red from 50) or
  `Context: no data` | `(used/max)` grey | `$X.XX` green; exit 0.
  `subagent-statusline.sh` is one `jq -c` program: for each `.tasks[]` one line
  `{"id":…,"content":"<name> · <model short> · <effort> · <label truncated to .columns> · <tokens> <pct>%"}`
  (model `claude-opus-5-5` → `Opus 5.5` after dropping `[…]` and `-YYYYMMDD`).
- **App** (`crates/connecto-app`): `AGENTS_SUPPORTED = false` on Windows
  (`platform/windows.rs:24-25`), wording `NOT_SUPPORTED_HERE` (`agent_install.rs:36-37`).
  macOS installs (`agent_install.rs:61-81,165-208,284-367`): `/usr/bin/curl` downloads
  `https://claude.ai/install.sh` (run with `/bin/bash`) or
  `https://chatgpt.com/codex/install.sh` (`/bin/sh`) into `<DATA>/run/install-<pid>`,
  runs it with `env_clear()` + `HOME`, a fixed `PATH`, `CODEX_NON_INTERACTIVE=1`, 15 min
  limit, then `find_installed` and `--version`. No npm.

#### 4.8.2 Claude Code on Windows (verified)

- **Layout.** `where.exe claude` → `C:\Users\<user>\.local\bin\claude.exe` only. It is a
  **real file** (no symlink, no launcher; 346 MB, starts with `MZ`), byte-identical
  (SHA-256) to `%USERPROFILE%\.local\share\claude\versions\2.1.322` (the versions are
  extension-less PE files; `2.1.50` is also still there). The auto-updater adds a version
  file and replaces the `claude.exe` copy; the path stays the same.
- **Signature** (`Get-AuthenticodeSignature`): `Status Valid`, `SignatureType
  Authenticode`, timestamped (DigiCert SHA256 RSA4096 Timestamp Responder 2026 1).
  Signer subject (exact):
  `CN="Anthropic, PBC", O="Anthropic, PBC", L=San Francisco, S=California, C=US, SERIALNUMBER=4860621, OID.2.5.4.15=Private Organization, OID.1.3.6.1.4.1.311.60.2.1.2=Delaware, OID.1.3.6.1.4.1.311.60.2.1.3=US`.
  Issuer `CN=DigiCert Trusted G4 Code Signing RSA4096 SHA384 2021 CA1, O="DigiCert, Inc.", C=US`,
  root `CN=DigiCert Trusted Root G4`. Thumbprint `60C2D9C5…92D1` for 2.1.322 but
  `0D7581D2…C6CB` for 2.1.50: **the certificate already rotated**, so never pin the
  thumbprint; pin the publisher: CN = `Anthropic, PBC` and O = `Anthropic, PBC`.
- **Tamper test.** A copy of `claude.exe` in the scratch folder: `Valid`; after flipping
  one byte at offset 100 000 000: `HashMismatch` (CN/O are still reported, so the check
  must require `Valid` first). Unsigned PE (`cargo-clippy.exe`, our own
  `connecto-hook.exe`): `NotSigned`. A text file named `.exe`: `UnknownError`. A missing
  file: an exception (`FileNotFoundException`).
- **Verifier invocation (works):** `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -NonInteractive -EncodedCommand <base64 of UTF-16LE script>`.
  - `-Command <text>` is **not** safe: passing a script with `"` through the Windows
    command line lost the quotes and broke the parse. `-EncodedCommand` avoids all
    quoting (script ≈ 2 KB → 2 072 base64 chars).
  - Paths go in an environment variable (`CONNECTO_VERIFY_PATHS`, newline-separated),
    never into the script text; the script uses `-LiteralPath` (a path with `[`, `]`,
    `$`, `'`, spaces and non-ASCII letters verified). Output: one JSON line per input,
    by index (`{"i":0,"status":"Valid","cn":"…","o":"…","ts":true}`), with
    `[Console]::OutputEncoding` set to UTF-8 and `$ProgressPreference='SilentlyContinue'`
    (without it Windows PowerShell writes a `#< CLIXML` progress record to stderr).
  - O is read from `$cert.SubjectName.Format($true)` (one RDN per line, `O="…"`), CN from
    `GetNameInfo(SimpleName)`. (`EnumerateRelativeDistinguishedNames` does not exist on
    .NET Framework / PowerShell 5.1.)
  - Execution policy: irrelevant for `-EncodedCommand` (no script file). Verified with
    `-ExecutionPolicy Restricted` and `AllSigned`: same result. A Group Policy that
    disables PowerShell entirely is **unverified**.
  - Environment: with a **completely empty** environment powershell.exe fails
    (`Loading managed Windows PowerShell failed with error 8009001d`); with only
    `SystemRoot` set it works. So: `env_clear()` + `SystemRoot` + the paths variable.
  - Cost: ≈1.7-1.8 s per process (first use of the module), 4.6 s for the 32 PE files
    of Codex in **one** process. One process per file would cost ≈55 s for Codex, so
    the verifier must batch.
  - Revocation checking and behaviour after the signing certificate expires (timestamp
    keeps it `Valid`, standard Authenticode) are **unverified**; concrete check: re-run
    `Get-AuthenticodeSignature` on Codex's `codex.exe` after 2026-11-12 23:02 local time
    (its certificate's NotAfter, see 4.8.3).
- **Running with a cleared environment.** `claude.exe --version` → `2.1.322 (Claude Code)`
  with an **empty** environment, with only `SystemRoot`, and with Connecto's
  `HOME`/`CLAUDE_CONFIG_DIR`/`CODEX_HOME` set (≈20 ms). So `read_version` works as is.
- **Config dir.** `CLAUDE_CONFIG_DIR` works on Windows: with a scratch dir, Claude wrote
  `.claude.json`, `backups\`, `plugins\`, `projects\`, `session-env\`, `sessions\` **into
  that dir** (not `%USERPROFILE%\.claude.json`). `claude auth status` prints the same
  JSON as on macOS (`loggedIn`, `authMethod`, `configDirectory`, `projectsDirectory`, …;
  exit 1 when logged out). Credentials: the user's default dir holds a
  `.credentials.json` file (plain file, ≈0.5 KB; contents not read); `cmdkey /list` has
  no Claude/Anthropic entry. The binary contains the strings `CredRead`/`CredWrite` and
  `CLAUDE_SECURESTORAGE_CONFIG_DIR`; whether a login under `CLAUDE_CONFIG_DIR` ever uses
  the Credential Manager, and what `CLAUDE_SECURESTORAGE_CONFIG_DIR` does on Windows, is
  **unverified** (test: `claude auth login` with a scratch `CLAUDE_CONFIG_DIR`, then
  look for `<dir>\.credentials.json` and run `cmdkey /list`).
- **Transcripts.** `<config dir>\projects\<encoded cwd>\<session id>.jsonl`. Encoding
  (verified with `…\scratchpad\m3\proj dir.x_y`): every character that is not an ASCII
  letter or digit becomes `-`, case kept, existing `-` kept:
  `C:\Users\<user>\AppData\…\m3\proj dir.x_y` → `C--Users-<user>-AppData-…-m3-proj-dir-x-y`.
  This matches the Windows test helper in `launch.rs:1206-1217`. Non-ASCII characters:
  **unverified**. Hook stdin `transcript_path` and `cwd` use backslashes.
  A process started with a `\\?\C:\…` working directory (via .NET) reported the plain
  `C:\…` cwd; still pass `child_cwd(project)` to be safe.
- **Hooks — the shell (verified with two `claude -p` runs in a scratch project; one
  without login, which already fires `SessionStart` and `UserPromptSubmit`, and one tiny
  logged-in run for `Stop`):**
  - **Shell form** (no `args`): run as `"C:\Program Files\Git\bin\bash.exe" -c "<command>"`,
    which starts `C:\Program Files\Git\usr\bin\bash.exe` (`$0=/usr/bin/bash`, bash
    5.2.26, `MSYSTEM=MINGW64`). Git Bash was found although only `Git\cmd` is on PATH;
    `where.exe bash` lists only the **WSL** `C:\Windows\System32\bash.exe` and
    `…\WindowsApps\bash.exe`, which Claude did **not** use. Inside: `SHELL` = Git
    bash path, `PATH` prefixed with Git's `mingw64\bin;usr\bin;…`, `HOME` set, and
    `CLAUDE_PROJECT_DIR` **with forward slashes** (`C:/Users/…/proj dir.x_y`), `$PWD`
    in MSYS form. A single-quoted backslash path (`'C:\Windows\…\powershell.exe' … 'a b'`)
    and a double-quoted backslash path both worked in Git Bash.
  - `"shell": "powershell"`: run as
    `pwsh.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "<command>"`
    (PowerShell 7 was preferred over 5.1 here), `CLAUDE_PROJECT_DIR` with backslashes.
  - **Exec form** (`"command": "<exe>", "args": [...]`): no shell at all, the exe is a
    **direct child of `claude.exe`**; each `args` element arrived exactly as written,
    including `arg with space` and `it's $HOME \`x\``; `${CLAUDE_PROJECT_DIR}` was
    substituted in `args` (backslash form, here followed by `/.claude/probe.ps1`).
    `CLAUDE_PROJECT_DIR` in its environment has backslashes.
  - Environment: hooks inherit the variables given to `claude.exe` (a test variable came
    through in all three forms), so `CONNECTO_HOOK_SOCK` etc. reach the helper.
  - **Ancestry:** in every form the hook process descends from the `claude.exe` that
    Connecto starts (exec form: child; shell form: `claude.exe → Git bin\bash.exe →
    usr\bin\bash.exe → hook`). The M2 descendant check therefore holds.
  - Docs (code.claude.com/docs/en/hooks, fetched 2026-11-12) agree: shell form uses
    "`sh -c` on macOS and Linux, Git Bash on Windows, or PowerShell when Git Bash isn't
    installed"; exec form "requires `command` to resolve to a real executable such as a
    `.exe`"; in PowerShell a command that is only a quoted string (e.g.
    `"\"C:\\Tools\\guard.exe\""`) is printed, not run. The docs give no minimum Claude
    version for `args` (it works on 2.1.322).
  - **Consequence:** on Windows Connecto must write its Claude hooks in **exec form**:
    `{"type":"command","command":"C:\\…\\connecto\\cli\\bin\\connecto-hook.exe","args":["claude","--event","SessionStart"]}`
    (`"--context"` appended for `UserPromptSubmit`; `["claude"]` alone without bridge
    hooks). No quoting, no path conversion, works with and without Git Bash, spaces in
    the user name are harmless. The shell-form `'path' claude …` that macOS uses would
    break when Git Bash is missing (PowerShell fallback).
- **Statusline.** Not run in `-p` mode (no statusline call was logged during the
  logged-in `-p` run). Its shell on Windows is, per the statusline docs, "Git Bash when
  Git Bash is installed, or … PowerShell when Git Bash is absent"; the docs show no
  `args`/`shell` field for `statusLine`. **Unverified on the machine** (needs an
  interactive session): which shell runs it, and whether `CLAUDE_PROJECT_DIR` is in its
  environment. Offline check done instead: the command
  `powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command '& (Join-Path $(if ($env:CLAUDE_PROJECT_DIR) { $env:CLAUDE_PROJECT_DIR } else { $PWD }) .claude/scripts/statusline.ps1)'`
  gives the same output from Git Bash `-c`, `pwsh -Command` and `powershell -Command`,
  with and without `CLAUDE_PROJECT_DIR`, for a project path with spaces and an
  apostrophe, and forwards stdin (≈265 ms per call through bash). It contains no `"`
  on purpose: double quotes inside a `-Command` argument were stripped on the way
  through the Windows command line. UTF-8 (`█░`, accented and CJK text in stdin) came
  through correctly on this machine (ACP 1252, OEM 850); scripts should still set
  `[Console]::InputEncoding`/`OutputEncoding` to UTF-8 explicitly.
  Test once in an interactive Claude session: a statusLine command that appends `$0`,
  the parent process and `CLAUDE_PROJECT_DIR` to a file.
- **Installers.** `https://claude.ai/install.ps1` (read, not run; 3.2 KB): requires
  64-bit; picks `win32-x64`/`win32-arm64`; fetches
  `https://downloads.claude.ai/claude-code-releases/latest`, then
  `<version>/manifest.json` and its SHA-256; downloads `<version>/<platform>/claude.exe`
  to `%USERPROFILE%\.claude\downloads\`, checks the SHA-256, runs `claude.exe install
  <target>` (which sets up `~\.local\bin\claude.exe` and `~\.local\share\claude\versions`),
  deletes the download. It does not change the execution policy or PATH itself.
  `winget show Anthropic.ClaudeCode`: installer type **portable**, URL
  `https://downloads.claude.ai/claude-code-releases/2.1.318/win32-x64/claude.exe` — the
  same signed file, but the winget version lags (2.1.318 vs 2.1.322 installed) and does
  not auto-update; its install location (`%LOCALAPPDATA%\Microsoft\WinGet\…`) is
  **unverified** (not installed here). Recommendation: the official `install.ps1`, run
  the macOS way (download to `<DATA>\run\install-<pid>\install-claude.ps1` with
  `%SystemRoot%\System32\curl.exe` and the same flags as macOS, then
  `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File <script>`;
  `-ExecutionPolicy` on the command line applies to that process only and changes no
  setting; a machine Group Policy can still forbid it, **unverified**). Alternative
  without PowerShell: Connecto does the same three HTTP steps itself and runs
  `claude.exe install` after its own SHA-256 **and** Authenticode check.

#### 4.8.3 Codex on Windows (verified, except where marked)

- **Support.** README (github.com/openai/codex, 2026-11-12): "Run the following on
  Windows to install Codex CLI: `powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"`",
  plus `npm install -g @openai/codex`; no support level or minimum version stated.
  `https://developers.openai.com/codex/windows` redirects to
  `https://learn.chatgpt.com/docs/windows/windows-sandbox` ("Windows sandbox"): "Use
  Codex on Windows with the native ChatGPT desktop app, the CLI, or the IDE extension";
  native PowerShell with a Windows sandbox, WSL only "when you need Linux-native
  tooling". So **native Windows is supported**; M3 is not blocked for Codex.
- **npm install** (`npm.cmd install -g @openai/codex`, prefix `%APPDATA%\npm`):
  `where.exe codex` → `%APPDATA%\npm\codex` (sh script) and `%APPDATA%\npm\codex.cmd`;
  there is also `codex.ps1`. `codex.cmd` runs `node "%dp0%\node_modules\@openai\codex\bin\codex.js"`.
  Real layout (package 0.167.2, platform package `@openai/codex-win32-x64` =
  `npm:@openai/codex@0.167.2-win32-x64`, installed **nested**):
  `%APPDATA%\npm\node_modules\@openai\codex\{package.json,bin\codex.js}` and
  `…\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\`
  with `bin\codex.exe` (368 MB), `bin\codex-code-mode-host.exe`, `codex-path\rg.exe`,
  `codex-resources\{codex-command-runner.exe,codex-windows-sandbox-setup.exe,voice\…}`
  (voice: `codex-voice-host.exe`, 26 DLLs incl. `vcruntime140.dll`, licences, JSON),
  `codex-package.json` = `{"layoutVersion":1,"version":"0.167.2","target":"x86_64-pc-windows-msvc","variant":"codex","entrypoint":"bin/codex.exe","resourcesDir":"codex-resources","pathDir":"codex-path"}`.
  Sizes: `bin` 428 MB, `codex-path` 4 MB, `codex-resources` 46 MB.
  `bin\codex.js` picks `vendor/<triple>/bin/codex.exe` and spawns it (no `execve` on
  Windows), adding `CODEX_MANAGED_PACKAGE_ROOT` and `CODEX_MANAGED_BY_NPM=1`.
  This matches the store's release model exactly: entry `bin\codex.exe`, release root
  `vendor\x86_64-pc-windows-msvc`, the four `release_entries`. **What the store must
  change:** the npm candidate on Windows is `codex.cmd`, not a symlink to `codex.js`, so
  `real/../..` is wrong; map `<dir>\codex.cmd` → `<dir>\node_modules\@openai\codex\bin\codex.js`
  (fixed npm global layout), then the existing `codex_release_root` finds
  `package/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc`.
- **Standalone installer** `https://chatgpt.com/codex/install.ps1` (read, not run;
  71 KB): downloads `codex-package-<triple>.tar.gz` and `codex-package_SHA256SUMS` from
  `https://releases.openai.com/codex` (fallback GitHub `rust-v<version>` releases),
  checks the SHA-256, extracts with `tar` into
  `%CODEX_HOME%\packages\standalone\releases\<version>-<triple>\` (CODEX_HOME defaults to
  `%USERPROFILE%\.codex`), points the junction `…\standalone\current` at it, makes
  `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` a junction to `current\bin`, and prepends
  that to the **user PATH**. It compiles C# with `Add-Type` for junction handling and
  honours `CODEX_NON_INTERACTIVE`. It checks for the same file set
  (`bin\codex.exe`, `bin\codex-code-mode-host.exe`, `codex-path\rg.exe`,
  `codex-resources\…`) and `codex-package.json`, so it is the same package layout;
  whether its `codex.exe` is byte-identical to the npm one is **unverified**.
  Discovery would need `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` in `fixed_dirs`;
  whether Rust's `fs::canonicalize` resolves the junctions to the release folder is
  **unverified** (expected yes; write a test).
- **Signatures** of all 32 PE files in the release: 30 are `Valid` with signer
  `CN="OpenAI OpCo, LLC", O="OpenAI OpCo, LLC", L=San Francisco, S=California, C=US`,
  issuer `CN=Microsoft ID Verified CS EOC CA 04, O=Microsoft Corporation, C=US`
  (Microsoft Trusted/Artifact Signing: the certificate's NotAfter is 2026-11-12 23:02,
  i.e. short-lived certificates, so a thumbprint pin is impossible), timestamped.
  `codex-resources\voice\bin\vcruntime140.dll` is `Valid`, signed by
  `CN=Microsoft Windows, O=Microsoft Corporation`. **`codex-path\rg.exe` is `NotSigned`.**
  So the entry and almost everything is Authenticode-signed by OpenAI; the trust design
  only needs a rule for `rg.exe` and the Microsoft DLL (decision D-3 below).
- **Running.** `codex.exe --version` → `codex-cli 0.167.2` with an empty environment and
  with Connecto's `HOME`/`CODEX_HOME`/`CLAUDE_CONFIG_DIR` only. With no `CODEX_HOME` it
  wrote `%USERPROFILE%\.codex\tmp\arg0-windows\…` even with `HOME` unset (it uses the
  Windows profile folder, not `HOME`); `CODEX_HOME` redirects it. When `CODEX_HOME` is
  under `%TEMP%`, Codex warns "Refusing to create helper binaries under temporary dir"
  (Connecto's dirs are under `%APPDATA%`, fine). `codex login status` → `Not logged in`
  on stderr, exit 1 (same text as the adapter parses).
- **Hooks (verified without login: `codex exec` fires `SessionStart` before the model
  call, which then fails with no credentials — no account action):**
  - Shell: hook commands run as `pwsh.exe -NoProfile -Command "<command>"` when `pwsh`
    is on PATH, else `C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -Command "<command>"`.
    With **no PATH** the hook failed (the shell or the command's program was not found);
    `SystemRoot` + `PATH=%SystemRoot%\System32;%SystemRoot%;%SystemRoot%\System32\WindowsPowerShell\v1.0`
    was enough. The hook is a descendant of `codex.exe` (`codex.exe → powershell/pwsh →
    hook`). stdin JSON: `session_id`, `transcript_path`, `cwd`, `hook_event_name`,
    `model`, `permission_mode`, `source`. Codex prepends its `tmp\arg0-windows\…` dir and
    `codex-path` to the hook's PATH.
  - **Command format for Connecto:** PowerShell syntax, single quotes only:
    `& 'C:\…\connecto\cli\bin\connecto-hook.exe' codex-hook --event SessionStart`
    (a `'` inside the path doubled as `''`). Without `&` PowerShell prints the quoted
    string instead of running it. No `"` (see the quoting loss above).
  - **Trust:** `codex app-server --listen stdio://` + `hooks/list` works on Windows with
    an environment of only `CODEX_HOME` and `HOME` (no PATH needed). The key is
    **`C:\<session-flags>\config.toml:session_start:0:0`** (backslashes, drive prefix),
    not `/<session-flags>/…`; it stayed `C:` with the working directory on `D:` or `B:`
    (CODEX_HOME was on `C:`; whether it follows CODEX_HOME's drive or the system drive is
    **unverified** — match the key by the suffix `<session-flags>\config.toml:<event>:0:0`
    or read it from `hooks/list`). Passing
    `-c "hooks.state={'C:\<session-flags>\config.toml:session_start:0:0'={trusted_hash=\"sha256:…\"}}"`
    (TOML **literal** key in single quotes, so the backslashes need no escaping) made the
    hook run; without it the hook did not run. A key in a basic `"…"` string with single
    backslashes is a TOML error and Codex refuses to start ("Error loading config.toml").
    `trustStatus` is reported as `untrusted` before. `--dangerously-bypass-hook-trust`
    exists (not to be used by Connecto).
  - `notify=["<hook>","codex"]` on Windows: **unverified** (fires at turn end, which
    needs a logged-in model turn). Test after the user logs in: one `codex exec "say ok"`
    with `-c notify=[…probe…]`.
- **Sandbox.** The Windows sandbox log in `CODEX_HOME\.sandbox\` said
  `MXC availability: true` and `Windows sandbox selection: disabled` (no `[windows]`
  config). Docs: `mxc` recommended where available (no admin setup), `elevated` (admin
  setup, `codex sandbox setup --elevated`), `unelevated` (weaker); keys
  `[windows] sandbox = "elevated"|"unelevated"`, `[features] prefer_mxc = true`
  (`codex features list`: `prefer_mxc` "under development"). How Connecto's Prompt and
  Auto (`--approve-for-me`, "workspace-write sandbox") modes behave with the selection
  disabled is **unverified** (needs login).
- **Storage.** Rollouts `<CODEX_HOME>\sessions\YYYY\MM\DD\rollout-<timestamp>-<thread>.jsonl`
  (from the hook's `transcript_path`), plus SQLite state files in `CODEX_HOME`. The
  project trust Codex asks for is a lower-cased path in `config.toml`
  ("add c:\users\…\connecto as a trusted project"); Connecto does not write project
  trust today (no change needed).
- Codex started `git.exe` (from `C:\Program Files\Git\cmd`, not on the given PATH) to
  sync its plugin marketplace from github.com at startup.

#### 4.8.4 Design for M3, per crate

- **connecto-store** (owner: store engineer; files `signature.rs`, `store.rs`,
  `platform/windows.rs`, `discover/windows.rs`, new `signature/windows.rs`):
  1. `is_pe(file)`: `MZ` at offset 0 and `PE\0\0` at the offset stored at `0x3C`
     (Windows counterpart of `is_macho`; use it in `verify_signatures` via a platform
     function `is_native_binary`).
  2. `Authenticode` (Windows implementation behind the existing `Signature` trait; keep
     the name `Codesign` exported or add a platform alias so callers in
     `connecto-app/main.rs:88,526`, `agents_host.rs:88`, `setup.rs`, `pane.rs:264` do
     not change). Add a batch method with a default (`fn verify_all(&self, files: &[(&Path, Option<&str>)]) -> Result<(), String>`
     that loops over `verify` on Unix) and implement it on Windows with **one**
     powershell.exe call: absolute path from `SystemRoot`, `-NoProfile -NonInteractive
     -EncodedCommand`, `env_clear()` + `SystemRoot` + `CONNECTO_VERIFY_PATHS`, stdout
     JSON lines by index, a timeout (e.g. 60 s, kill on expiry), fail closed on any
     missing/extra/unparseable line. Accept a file only if `status == "Valid"` and
     `cn == o == expected publisher` (exact, case-sensitive). The script that was run
     (send no empty paths; the output has one line per path, by index):

     ```powershell
     $ProgressPreference = 'SilentlyContinue'
     $ErrorActionPreference = 'Stop'
     [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
     $paths = $env:CONNECTO_VERIFY_PATHS -split "`n"
     for ($i = 0; $i -lt $paths.Count; $i++) {
       $p = $paths[$i]; if ($p -eq '') { continue }
       try {
         $s = Get-AuthenticodeSignature -LiteralPath $p
         $c = $s.SignerCertificate; $cn = $null; $o = $null
         if ($c) {
           $cn = $c.GetNameInfo([System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false)
           foreach ($l in ($c.SubjectName.Format($true) -split "`r?`n")) { if ($l -like 'O=*') { $o = $l.Substring(2).Trim('"') } }
         }
         [pscustomobject]@{ i = $i; status = [string]$s.Status; cn = $cn; o = $o; ts = [bool]$s.TimeStamperCertificate } | ConvertTo-Json -Compress
       } catch { [pscustomobject]@{ i = $i; status = 'Error'; error = $_.Exception.GetType().Name } | ConvertTo-Json -Compress }
     }
     ```
  3. Publisher constants instead of team ids on Windows (`Tool::team_id` per platform):
     Claude `"Anthropic, PBC"`, Codex `"OpenAI OpCo, LLC"`, Shell none. Per-file rule for
     Codex: `vcruntime140.dll` → `"Microsoft Corporation"` (O) / `"Microsoft Windows"`
     (CN), `codex-path\rg.exe` → per decision D-3.
  4. Discovery: map `codex.cmd` (and `codex.ps1`) in a dir to
     `<dir>\node_modules\@openai\codex\bin\codex.js` before `codex_release_root`; add
     `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin` to `fixed_dirs` (standalone installer);
     keep `~\.local\bin` for Claude (real file, no shim). winget's Claude location only
     if D-2 picks winget.
  5. `read_version`: no change needed for the environment (verified); add a timeout
     (Unix too would benefit; keep macOS behaviour if that is a rule).
- **connecto-adapter** (files `spec.rs`, `claude.rs`, `codex.rs`):
  1. `with_bridge_path`: use the platform separator (`;` on Windows). Same value in the
     settings `env`.
  2. Claude hooks on Windows in exec form (`command` = helper path, `args` =
     `["claude","--event",E]` (+`"--context"`)); macOS keeps the shell form byte for
     byte (`cfg`-split the command builder only).
  3. Codex hook command on Windows: `& '<helper with '' doubled>' codex-hook --event E[ --context]`
     (a `powershell_quote` next to `shell_quote`), still wrapped by `toml_string`.
     `expected_hook_keys` per platform (Windows:
     `C:\<session-flags>\config.toml:session_start:0:0` /
     `…:user_prompt_submit:0:0`, or better: matched by suffix in `codex_trust`), and the
     `hooks.state` value with TOML literal keys (`'…'`) on Windows.
  4. `base_env` on Windows: no `TMPPREFIX`; set `TEMP` and `TMP` (not only `TMPDIR`) to
     `<run>\tmp` so the agents' temp files stay in the session folder; keep `HOME`
     (Git Bash and Codex read it; Claude uses `CLAUDE_CONFIG_DIR`). Keep
     `CLAUDE_CODE_TMPDIR`, `DISABLE_UPDATES`, `DISABLE_AUTOUPDATER`.
     `CLAUDE_SECURESTORAGE_CONFIG_DIR`: keep the existing logic until the credentials
     test above says otherwise.
- **connecto-session** (files `platform/windows.rs`, `launch.rs`, `codex_trust.rs`):
  1. `agents_supported()` → `Ok(())` on Windows (after store and adapter land).
  2. `start_resuming`: pass `platform::child_cwd(&project)` as the PTY cwd and to the
     adapter context (Codex `-C`), so neither agent sees `\\?\`.
  3. `codex_trust`: on Windows `env_clear()` + `SystemRoot` + `CODEX_HOME` + `HOME`
     (no PATH needed; add `PATH=%SystemRoot%\System32` if in doubt); find keys by suffix;
     the app-server call itself works unchanged.
  4. The PTY environment of an agent session must contain a PATH with PowerShell
     (Codex hooks) — it already inherits `PATH`, `PATHEXT`, `SystemRoot`, `ComSpec`
     (`platform::INHERITED`).
  5. Transcript folder encoding is only used in tests; the production scan
     (`utility.rs:611-622`) already works with the Windows names.
  6. ConPTY: whether the Claude and Codex TUIs (alt screen, mouse, bracketed paste,
     resize) behave in Connecto's ConPTY terminal is **unverified**; first manual test.
- **connecto-packages** (files `build.rs`, `package.rs`, `builtin/`):
  1. Add `statusline.ps1` and `subagent-statusline.ps1` next to the `.sh` files (in
     `builtin/claude/.claude/scripts/` and `builtin/both/.claude/scripts/`, identical
     copies like the `.sh`). macOS output stays byte-identical only if the `.ps1` files
     are filtered out on macOS: give `build.rs` / `builtin()` a platform filter (e.g. a
     `*.ps1` = Windows-only and `*.sh` = Unix-only rule, or a `builtin-windows/` overlay
     folder). Decide together with D-1.
  2. `statusline.ps1` must reproduce `statusline.sh` exactly (same colours, bar, rounding
     `[math]::Round` vs `printf '%.0f'` — note `printf` rounds half to even, check `0.5`
     cases; same `-` fallbacks; same `human()` thresholds) using `ConvertFrom-Json`
     instead of `jq` and no `awk`; read stdin with `[Console]::In.ReadToEnd()`, set
     UTF-8 input/output encoding, write with `[Console]::Out.Write`, exit 0 on any error.
     `subagent-statusline.ps1`: one `ConvertTo-Json -Compress` line per task with `id`
     and `content`, same model shortening and truncation to `.columns`. Must run on
     Windows PowerShell 5.1 (no `??`, no `?:`, no `-AsHashtable`). Add golden tests that
     feed the same JSON to both scripts (on a Mac: the `.sh`; on Windows: the `.ps1`)
     and compare with stored expected output.
  3. Settings entries for Windows: see D-1.
- **connecto-app** (files `platform/windows.rs`, `agent_install.rs`, `setup.rs`,
  `agents_host.rs`):
  1. `AGENTS_SUPPORTED = true` on Windows when everything above is in.
  2. Installer sources on Windows: Claude `https://claude.ai/install.ps1`, Codex
     `https://chatgpt.com/codex/install.ps1` (same sources as macOS, so trust and
     discovery match), downloaded with `%SystemRoot%\System32\curl.exe` and run with
     `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File <script>`,
     `env_clear()` + `SystemRoot`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `TEMP`,
     `TMP`, `PATH` (System32 + PowerShell), `CODEX_NON_INTERACTIVE=1`. Note the Codex
     installer changes the **user PATH** (macOS: it writes `~/.local/bin`); say so in the
     setup wording. npm stays a supported discovery source but not the installer.
  3. Wording: replace `NOT_SUPPORTED_HERE` for agents; mention that Git for Windows is
     optional (Claude uses Git Bash for its Bash tool if present).

**Order of work.** (1) store and adapter in parallel — they touch disjoint files
(`connecto-store/**` vs `connecto-adapter/**`); packages `.ps1` scripts can also start
in parallel (`connecto-packages/builtin/**`, `build.rs`) once D-1 is decided.
(2) session (`codex_trust`, `child_cwd`, gate) after the adapter's key/quoting change,
because `codex_trust.rs` uses `expected_hook_keys`/`hook_config`. (3) app (installer,
gate, wording) last, since it switches the feature on. Manual tests at the end:
Claude session with statusline (the unverified statusline shell), Codex session after
login (notify, sandbox mode, ConPTY).

**Risks.** Authenticode via PowerShell costs ≈2 s per provisioning and depends on
Windows PowerShell being allowed (AppLocker/Constrained Language Mode **unverified**);
OpenAI's short-lived certificates make the timestamp essential; statusline via
PowerShell 5.1 costs ≈250 ms per refresh (the subagent statusline runs every tick);
the Codex installer edits the user PATH; Codex's Windows sandbox selection is disabled
by default; npm's nested platform package may be hoisted differently by other package
managers (pnpm sibling case already handled).

**Decisions for the user.**
- **D-1 statusline settings.** (a) Windows variant of the builtin `.claude/settings.json`
  with the PowerShell command above — simple, but a project shared between a Mac and a
  Windows PC gets a platform-specific committed file; or (b) keep the project file as on
  macOS and let the Windows Claude adapter put `statusLine`/`subagentStatusLine` into
  `claude-settings.json` (the `--settings` file outranks project settings for `env`,
  checked on 2.1.318; for `statusLine` **unverified**) when the project's value is the
  builtin one. Recommended: (b), after verifying the precedence.
- **D-2 Claude installer.** `install.ps1` (recommended: auto-updates, same layout as
  this machine) or winget (lags, no auto-update, other path).
- **D-3 Codex files that are not OpenAI-signed.** `codex-path\rg.exe` is unsigned and
  `vcruntime140.dll` is Microsoft-signed. Options: (a) accept `vcruntime140.dll` with
  publisher Microsoft and accept `rg.exe` unsigned, protected only by the store's own
  SHA-256 after copy (it came from the same package as the signed entry);
  (b) accept them only if their SHA-256 matches the npm/installer package metadata
  (needs the package's per-file digests, which npm does not provide; the standalone
  installer has a SHA-256 for the whole archive only); (c) do not copy `codex-path` and
  let Codex find `rg` elsewhere (behaviour **unverified**). Recommended: (a), shown to
  the user as "1 unsigned helper (ripgrep)".
- **D-4 Codex sandbox on Windows.** Leave Codex's default (selection disabled), pass
  `-c windows.sandbox=…`, or offer the elevated setup in Connecto's setup. Needs the
  logged-in test first.

## 5. Official install facts (checked on 2026-11-12)

Checked with the official pages, because `npx ctx7` was not available on the Mac
(`npx: command not found`), so the ctx7 CLI step of the project rule could not run.
Re-check with ctx7 on the Windows machine if it is available there.

- Claude Code (code.claude.com/docs, Advanced setup): Windows 10 1809+ or Windows
  Server 2019+; native install `irm https://claude.ai/install.ps1 | iex` (PowerShell) or
  `winget install Anthropic.ClaudeCode`. Git for Windows is recommended (its Git Bash
  runs the Bash tool); without it the PowerShell tool is used. Native installs
  auto-update; WinGet installs do not.
- Codex CLI (github.com/openai/codex README): installers `irm https://chatgpt.com/codex/install.ps1 | iex`
  (its command runs with `-ExecutionPolicy ByPass`, which the bootstrap does not use) and
  `npm install -g @openai/codex`. The README does **not** state Windows support or a
  minimum Windows version. **TODO:** the Windows session must confirm native Codex support
  and hook behaviour before M3 work.
- winget IDs in `scripts/windows/bootstrap.ps1` other than `Anthropic.ClaudeCode` are
  from memory: `Git.Git`, `Rustlang.Rustup`, `Microsoft.VisualStudio.2022.BuildTools`,
  `OpenJS.NodeJS.LTS`. **TODO:** confirm each with `winget show --id <id> -e`.

## 6. Native build requirements (checked in Cargo.lock and crate sources)

- Needed: rustup with `stable-x86_64-pc-windows-msvc` (rust-version 1.95 in
  `Cargo.toml:8`, edition 2024); Visual Studio 2022 Build Tools with the C++ workload
  (MSVC `cl.exe`, `link.exe`); a Windows 10/11 SDK.
- Not needed: NASM, Perl (ring's crates.io build), cmake (no `aws-lc-sys`,
  `openssl-sys`, `libsqlite3-sys` or `zstd-sys` in `Cargo.lock`), clang (only `bindgen`
  via `libproc`, which must be gated, see 4.1).
- ARM64 Windows: ring compiles C with clang there (`ring-0.17.14/build.rs:562-563`).
  Not supported by the bootstrap script yet.

## 7. Milestones and acceptance criteria

Each milestone is a set of small commits on `windows-port`.

### M1: compiles and opens a window with a working PowerShell terminal

Scope: everything in 4.1-4.4 that the app needs to start. The `connecto` command and
the agents are **disabled on Windows** in this milestone (`cfg` gate, with a message).

Acceptance:
- `cargo build --release --locked --target x86_64-pc-windows-msvc -p connecto-app` succeeds.
- `cargo test --workspace` runs on Windows with the Unix-only tests gated (section 4.7),
  and the remaining tests pass or are listed as known failures in the Status section.
- The app starts, shows its window, and opens a PowerShell terminal in a project folder
  (manual check; screenshot in the Status section).
- The data folder is `%APPDATA%\connecto` (or the decided alternative) and the app
  writes nothing under `Library`.
- The macOS build is unchanged: `cargo test --workspace` on macOS still passes (see
  "Rules").

### M2: the `connecto` command and the app are linked

Scope: section 4.5.

Acceptance:
- `connecto-hook.exe` (as `connecto.exe`) run in a PowerShell terminal of a session
  reaches the app over a named pipe.
- The app refuses a caller that is not a descendant of the terminal's root process
  (test with a second process started outside the terminal; expect refusal).
- No TCP listener is used for this.

### M3: agents (Claude Code and Codex)

Scope: section 4.6. Depends on open decisions 1 and 2 and on the hook check in 4.6.

Acceptance:
- Claude Code (native, official installer) is found by discovery, passes the trust
  check, and runs in a session.
- The statusline and hook scripts run (PowerShell versions, or Git Bash if decided).
- Codex is found and runs if the Windows support check in section 5 is positive;
  otherwise the milestone is marked **blocked** in Status.

### M4: remote, release, tests

Scope: section 4.7 and section 6.

Acceptance:
- The Windows CI job (`release-build.yml` `windows`) builds the zip on the runner
  (manual run) and uploads it.
- The SSH agent path works with the Windows OpenSSH agent, or is marked not supported.
- The Windows zip is listed on the site, with its size and SHA-256 (as done for
  macOS in commit 6959665).
- The release notes say the Windows build is unsigned, unless signing is set up.

Status of the acceptance criteria, 2026-11-13: M1, M2, M3 met on Windows (see
section 10; the macOS test suite passed on a Mac for 6ee6f3f). M4:
- CI job: prepared (Rust target step), **not run** on GitHub. The same script built
  a working zip on a Windows 11 PC.
- SSH agent: implemented and unit-tested; **not tested against a real agent**
  (section 4.7 has the manual test). Not marked "not supported".
- Site: ready (the Windows texts appear once `releases.js` lists the zip), **not
  listed** yet: the zip must be built with the relay token and uploaded first
  (`scripts/release/README.md`, "Publishing a build on the site").
- Release notes: done (`scripts/release/README.md`, README.txt in the zip, the
  site's install page): unsigned, SmartScreen.

## 8. Rules for the port

- Work on the `windows-port` branch. Do not commit to `main`.
- Never break macOS. Every platform-specific change goes behind `cfg(unix)`,
  `cfg(windows)` or `cfg(target_os = ...)`, or into a small per-platform module.
  Macros and gated items must produce the same behaviour on macOS.
- Keep `unsafe_code = "deny"` (`Cargo.toml:41-42`). Use Windows APIs only through
  dependencies that expose safe interfaces.
- Prefer small per-platform helper modules over scattered `cfg` blocks (one module
  for paths, one for the secret store, one for process trees, one for the pipe server).
- macOS must be re-verified before merging: push the branch so the existing macOS CI
  build runs, or run the macOS build and tests on the Mac. The release workflow's macOS
  jobs do not run on push (manual only), so run them by hand.
- Commit in small steps. Each commit message says which section of this document it
  implements.
- Keep the Status section below current (done, in progress, blocked; date-stamped) so
  any later session can resume.

## 9. Reviews

No Codex reviewer terminals are available in a plain Claude Code session. Use the
project's `dev-team` skill with Claude reviewers as the fallback. Before merging a
milestone, get a review of the diff that includes the macOS-safety check in section 8.

## 10. Status

Update this section at each session. Newest entries at the top of each list.

- 2026-11-13 (Windows session, M4: release, CI, docs): **M4 done on this machine;
  the CI job, signing and a real SSH agent are untested.** Small commits on
  `windows-port`, not pushed (the coordinator pushes).
  - Status line (after the Mac's 6ee6f3f): all stored cases passed, but the twin
    read every `a // b` as "b on an error", which the Mac run disproved (jq 1.7.1
    empties the whole line). `statusline.ps1` (both copies) now throws for the
    effort, used, token, size and cost paths and keeps the `?` reading for the
    model only; new cases `effort_string`, `usage_number`, `cost_number`,
    `model_array` (derived here; **a Mac run with jq confirms them**).
  - `build-windows.ps1` run for real (Windows PowerShell 5.1 with redirected
    output, and PowerShell 7). Fixed: SHA256SUMS ended in CRLF (now LF, no BOM,
    sorted, no duplicates; `sha256sum -c` in Git Bash: OK), zip entries had `\`
    under 5.1 (now `/` via System.IO.Compression), cargo's stderr stopped the
    script under 5.1 with redirected output, `CARGO_TARGET_DIR` ignored, README.txt
    UTF-8 with BOM and outdated (now ASCII CRLF, Windows-specific), a missing Rust
    target is named. Built **without** a relay token (`CONNECTO_NO_RELAY_TOKEN=1`:
    none is on this machine), so that zip is for testing only:
    `dist\connecto-0.1.6-windows-x86_64.zip`, 15356726 bytes. Unzipped into a
    fresh folder and started with a fresh `CONNECTO_DATA_DIR` and
    `CONNECTO_SKIP_SETUP=1`: window "Connecto" up, `cli\bin\connecto-hook.exe` and
    `connecto.exe` installed (same bytes as in the zip), closed cleanly.
  - `release-build.yml` `windows`: `rustup target add x86_64-pc-windows-msvc`
    before the build (as the macOS job; no caching there, none here); the relay
    secret was already passed. **Run on GitHub on 2026-11-13** (run 43433217398 on
    `windows-port` at 44f420b, inputs `only=windows`, `no_relay_token=true`): the job
    passed on its first run in about 16 minutes; the artifact
    `connecto-0.1.6-windows-x86_64.zip` (15978268 bytes) matches the `checksums` job's
    SHA256SUMS, holds the three executables and README.txt, and its app started on
    Windows 11 and installed its helper. Two inputs were added for this: `only`
    (one platform per run; default `all`) and `no_relay_token` (test build; default
    off). **The repository has no Actions secrets**: a release build from GitHub needs
    the secret `CONNECTO_RELAY_TOKEN` (macOS jobs too); without it only
    `no_relay_token` test builds work. There is no test CI at all
    (recommendation in `scripts/release/README.md`).
  - Credential Manager limit (the known defect of the previous entry): fixed by
    splitting the document across credentials (section 4.4). 10 unit tests and the
    real Credential Manager test (throwaway targets) pass.
  - SSH agent: unchanged code; state and manual test in 4.7. The ignored
    `ssh_agent` test fails here as expected (service disabled, left disabled).
  - `scripts/release/README.md` rewritten for Windows and Linux (section 1's
    corrections), with the site publishing steps. Site: `assets/downloads.js`
    renders the cards and shows the Windows texts (landing page, install page
    "On Windows") once `releases.js` has a Windows entry; nothing Windows is
    listed yet. Checked with `node --check` and the renderer under Node with a
    fake Windows entry; no browser run.
  - **How the Windows zip gets to the site:** from the Mac, with a zip built with
    the relay token (the CI artifact `windows-x86_64`, or `build-windows.ps1` on a
    Windows machine with the token), checked with `shasum -a 256`, uploaded to
    `downloads/`, one entry in `site/assets/releases.js`
    (`scripts/release/README.md`, "Publishing a build on the site").
  - Verified on Windows: `cargo check --workspace --all-targets` and `cargo clippy
    --workspace --all-targets` zero warnings; `cargo test --workspace` 1392
    passed, 0 failed, 8 ignored; macOS `cargo clippy --workspace --all-targets
    --target aarch64-apple-darwin` (scratch copy) zero warnings.
  - Untested: the CI `windows` job on GitHub; signing (none; Authenticode would
    need a certificate and `signtool`, described in the release README); a real
    SSH agent; pairing on Windows end to end; a release zip with the relay token;
    the site in a browser; the new statusline cases against jq.
  - Follow-ups: exe icon and manifest (none yet); a Job Object so terminating a
    terminal ends its process tree (`PtySession::terminate()` kills the shell
    only); the Codex `SKILL.md` files of the built-in packages still show bash
    examples; real `install.ps1` runs on a clean machine (only fake scripts were
    run); Codex sandbox mode D-4 (open); the `rg.exe` exception D-3 (revisit when
    Codex signs it); same-second pid reuse in the caller check
    (`process_tree::started_by` compares start times at second resolution); ARM64
    Windows; test CI for macOS and Windows.

- 2026-11-13 (Windows session): **`windows-port` rebased onto `main` 85a7b04 (0.1.6).**
  Backup of the old branch: `windows-port-pre-rebase` (9d53a9c). Not pushed.
  - Conflicts (13 files) resolved so both sides stay: connecto-store
    `discover.rs` (main's `is_launcher`/`run_path` kept; the system folders
    moved to `discover/{unix,windows}.rs`; on Windows a `.cmd`/`.bat`/`.ps1`
    is a launcher too), `signature.rs` (main's `check`/`Failure`/retries kept
    for macOS unchanged; the port's `verify_all` became `check_all` +
    `checks_together`, so Windows still checks all files in one PowerShell run
    and retries the batch; Authenticode answers "Error" are `Incomplete`, the
    rest `Invalid`), `store.rs` (main's fall-through `provision_found`,
    re-copy on a refusal and `claude_layout_reason` kept; Claude must be a
    native program, Mach-O on macOS, PE on Windows; copies stay held on
    Windows across the re-copy), `lib.rs`; connecto-adapter `claude.rs`,
    `codex.rs`, `lib.rs` (imports); connecto-session `launch.rs` (main's
    sign-in folder per check + `for_child` paths), `hook.rs` (main's ACK after
    the log boundary, over the port's `local_socket` streams); connecto-app
    `agent_install.rs`, `agents_host.rs`, `setup.rs` (main's `runnable_entry`,
    `provision_found`, new wording through `THIS_COMPUTER`/`THE_COMPUTER`);
    connecto-ui `setup_window.rs` (`home_dir` per platform). `Cargo.lock` is
    main's plus the port's Windows dependencies.
  - Ported from main to Windows: new connecto-net `staging.rs` (owner-only
    mode is a no-op on Windows, as `trust::write_private`); Keychain wording in
    the new pairing/relay texts (`connecto_secrets::STORE_NAME`,
    `platform::KEY_STORE`); main's tests made to build on Windows (Unix-only
    ones gated, Windows twins for wrapper skipping, refused/unfinished
    signature checks, the hook log boundary over a named pipe, the shared
    trust reads); `subagent-statusline.ps1` rewritten for main's right-aligned
    rows, its goldens re-derived (Python reading of the jq program) plus six
    new cases and a running-time test. The secret caching of 85a7b04 lives in
    the shared `Secrets` layer, so the Credential Manager gets it unchanged.
  - **Codex readiness on Windows is fixed by main's new composer reading**
    (a footer with ` · ` is enough): the real Codex 0.167.2 now reads `Ready`
    at its composer (`windows_agents`).
  - Verified on Windows: `cargo check`/`clippy --workspace --all-targets`
    zero warnings; `cargo test --workspace` 1381 passed, 0 failed, 8 ignored;
    `windows_agents` (real Claude 2.1.323 and Codex 0.167.2) 4 passed;
    macOS `clippy --target aarch64-apple-darwin` (scratch copy) zero warnings;
    release build of `connecto-app` and `connecto-hook` succeeds.
  - Unverified: the macOS test suite (type-check only); the new subagent
    goldens against the real `jq`; pairing on Windows (the Credential Manager
    item holds 2560 bytes, and every paired machine adds a key to it, so only
    a few machines fit); the built-in relay token in a Windows release build.

- 2026-11-12 (same Windows session, M3 integration): **M3 code complete; agents are on
  on Windows. Not yet tried by a person in the app. Nothing committed.**
  - Decisions taken: **D-1** (b) the PowerShell status-line twins are injected through
    Connecto's `--settings` file while the project's value is the builtin one;
    **D-2** the official installers `https://claude.ai/install.ps1` and
    `https://chatgpt.com/codex/install.ps1`; **D-3** (a) Codex's `codex-path\rg.exe`
    accepted unsigned as the single, switchable exception
    (`connecto-store` `ACCEPT_UNSIGNED_CODEX_RG`), the Microsoft runtime DLLs accepted
    as signed by Microsoft; **D-4** (Codex sandbox mode) still **open**: Codex's default
    (selection disabled) is left as it is.
  - connecto-session: `codex_trust` uses the adapter's new API (`hook_trust_state`,
    keys matched with any drive letter), Windows env `CODEX_HOME`, `HOME`,
    `SystemRoot` only (`platform::codex_trust_env`; Unix unchanged); `agents_supported()`
    is `Ok` on Windows; every path in `LaunchCtx`/`LoginCtx`/`HookCtx` and the PTY cwd
    are passed without `\\?\` (`launch::for_child` = `platform::child_cwd`, identity on
    Unix); the Claude secure-storage identity of a new profile is written in that form;
    status checks and the trust read run with `CREATE_NO_WINDOW`
    (`platform::no_console`, no-op on Unix).
  - connecto-store: `read_version` without a console window; the Authenticode check
    normalises `/` to `\` before comparing PowerShell's answer (the store joins
    `bin/codex.exe`; every Codex provisioning failed with "answered for another file"
    before this fix).
  - connecto-app: `AGENTS_SUPPORTED` and `NOT_SUPPORTED_HERE` removed (agents on both
    platforms, setup and Settings > Agents as on macOS); the `Shared` signature wrapper
    forwards `verify_all`; versions and sign-in are asked of `Store::program_of` on
    Windows (never npm's `codex.cmd`); installer per platform in `platform/{unix,windows}.rs`
    (macOS code moved unchanged): Windows downloads with `%SystemRoot%\System32\curl.exe`
    (same flags), saves `<data>\run\install-<pid>\install-<tool>.ps1`, runs Windows
    PowerShell by absolute path `-NoProfile -NonInteractive -ExecutionPolicy Bypass -File`,
    no window, built environment (`OS`, `SystemRoot`, `USERPROFILE`/`APPDATA`/`LOCALAPPDATA`
    under the home discovery searches, `TEMP` in the work folder, system `PATH`,
    `CODEX_NON_INTERACTIVE=1`), 15 min limit, log tail on failure, then discovery +
    `--version`; the store's Authenticode provisioning follows exactly as for a
    pre-existing install. The Codex card says its installer adds its folder to PATH.
  - connecto-ui: the empty-window text is the common one again.
  - Verified on this machine (real Claude Code 2.1.322 and npm Codex 0.167.2, temp data
    folder, per-project profiles, local fake model, no login):
    `crates/connecto-session/tests/windows_agents.rs` (ignored; run with
    `cargo test -p connecto-session --features test-endpoints --test windows_agents -- --ignored --test-threads=1 --nocapture`):
    discovery → Authenticode → copy (Claude ≈32 s, Codex ≈49 s in a debug build) →
    relay terminal in ConPTY → first-run screens → a turn: Claude's exec-form hooks and
    Codex's PowerShell hooks + `notify` reach the app over the named pipe (state
    Starting/Idle, `finished_turns` +1, no refused hook caller); Claude's transcript
    folder is `C--…-work-proj-dir-x-y`; status commands say logged out in a fresh
    profile; both logged-out TUIs draw their first screen, redraw after a resize and end
    on terminate. **Codex `notify` at turn end works on Windows.** Codex's TUI runs
    `SessionStart` with the first turn, not at start-up. Status-line twins: the exact
    generated command gives identical output through Git Bash `-c`, `pwsh -Command` and
    Windows PowerShell `-Command`, with and without a UTF-8 BOM on stdin.
  - Unverified: a real login (`claude auth login` / `codex login` in a profile; whether
    Claude writes `<profile>\.credentials.json`; `CLAUDE_SECURESTORAGE_CONFIG_DIR` on
    Windows); the real `install.ps1` runs (not run on purpose; tested with fake scripts);
    statusline precedence of `--settings` in a live session; Codex sandbox (D-4);
    mouse in both TUIs; the macOS test suite (type-check only).
  - Found: Codex 0.167.2 on Windows shows no `? for shortcuts` footer, so the Codex
    readiness check (`codex.rs` `readiness`) stays NotReady; held sends rely on the
    `SessionStart` + 1 s rule (`input_ready.rs`), which on Windows comes only with the
    first turn. Not changed (shared code); check the footer on macOS with 0.167.2.
  - The default project mode is **System** (the user's own `~\.claude`, `~\.codex`):
    on Windows the CLIs find those by `USERPROFILE`, which Connecto passes through, so it
    works in the app; tests must use per-project profiles (one early test run asked the
    user's real Claude `auth status`, read-only).

- 2026-11-12 (same Windows session, M3 research): **facts and design in 4.8; no Rust
  code changed.** Native Codex is supported (M3 not blocked). Claude hooks must use exec
  form on Windows; Codex hooks run through PowerShell; Codex hook-trust keys are
  `C:\<session-flags>\…` on Windows. Codex 0.167.2 was installed with npm for the check
  (`%APPDATA%\npm`). Side effects left on the machine: a transcript folder
  `C--Users-…-scratchpad-m3-proj-dir-x-y` in the user's `~\.claude\projects` from the one
  logged-in `claude -p` test (safe to delete). Open: decisions D-1…D-4 (4.8.4) and the
  **unverified** items in 4.8 (statusline shell in an interactive session, Codex
  `notify` and sandbox after login, ConPTY behaviour of both TUIs).

- 2026-11-12 (same Windows session, M2): **M2 code complete, not yet tried by hand in
  the app.** Nothing committed.
  - Hook and bridge servers serve named pipes on Windows
    (`connecto-session/src/local_socket/windows.rs`), through `interprocess` 2.4.8
    (tokio flavour; new Windows-only dependencies `interprocess`, `tokio`, `widestring`
    in connecto-session). Caller pid: `client_process_id()`
    (`GetNamedPipeClientProcessId`); it must name a running process that started no
    later than the accept (`process_tree::started_by`), then the unchanged descendant
    check. Pipes: `FILE_FLAG_FIRST_PIPE_INSTANCE` (a taken name fails),
    `PIPE_REJECT_REMOTE_CLIENTS`, a DACL for this user and SYSTEM only, names
    `\\.\pipe\connecto-hook-<32 hex>` / `connecto-bridge-<32 hex>`
    (`platform::endpoint`). Time-outs cancel reads and writes; shutdown connects to
    the pipe to end the accept.
  - In a PowerShell terminal `connecto list|send|read|keys|add shell|start|close` reach
    the bridge, and a process outside the terminal's tree gets `not_a_terminal`
    (tests in `connecto-session/tests/windows_session.rs`). Agents stay off (M3).
  - Helper update: `connecto-store` replaces a running `connecto-hook.exe` /
    `connecto.exe` by renaming the old one aside (`.<name>.old-<pid>-<n>`), removed by
    a later refresh (`platform::replace`, `sweep_replaced`; Unix unchanged: `rename`).

- 2026-11-12 (same Windows session, later): **M1 code complete, waiting for the user's
  manual test and for a macOS run.** Nothing committed yet (user's say-so needed).
  - **User's manual test (2026-11-12): the app works** (setup, PowerShell terminal).
    User requirement from that test: Windows must show a top menu inside the window,
    because there is no system menu bar as on macOS. **Done** as an M1 follow-up:
    in-window menu bar in connecto-ui (`menu/bar.rs`; a native menu would need `unsafe`).
    `menu.rs` now holds one menu description used by both platforms; the macOS native
    menu is built from it (`menu/macos.rs`), so **a Mac must compare the native menu with
    the previous build** before merging. The seven menu-only commands are reachable, so
    clippy is at zero warnings; connecto-ui has 334 tests. Not verified by screenshot:
    a menu in its open state (the user checks by hand). Also fixed: the empty-screen
    wording on Windows and the missing `＋` glyph in the sidebar hint (the glyph is
    probably missing on macOS too; unchanged there). The follow-up items below about the
    menu, the wording and the sidebar band are therefore closed.
  - Every crate ported. On Windows: `cargo check` and `cargo clippy --workspace
    --all-targets` pass (only connecto-ui's dead-code warning for seven menu-only
    commands); `cargo test --workspace` exit 0, 1158 passed, 0 failed, 6 ignored
    (network, token or real SSH agent);
    `cargo build --release --locked --target x86_64-pc-windows-msvc -p connecto-app -p connecto-hook`
    succeeds. Run one build at a time: rustc ran out of memory with three in parallel.
  - Smoke test (release exe, `CONNECTO_DATA_DIR` set to a temp folder): the Welcome
    screen and, with `CONNECTO_SKIP_SETUP=1`, the main window draw and close cleanly.
    Not yet verified by a person: setup past Welcome, restart after setup, opening a
    project, typing in the PowerShell terminal, AltGr characters on non-US layouts.
  - macOS: `cargo clippy --workspace --all-targets --target aarch64-apple-darwin` passes
    from Windows, in a scratch copy with a patched `libproc` (empty build script) and a
    stub C compiler for `ring`. This is a type-check only. **No macOS test has run**;
    run `cargo test --workspace` on a Mac or the macOS CI before merging.
  - Layout: each crate has a small `platform` module (`unix.rs` / `windows.rs`, plus
    `macos.rs` where needed) holding the moved Unix code and the Windows counterpart.
  - Disabled on Windows until later milestones, each with a clear message: agent
    terminals and agent install (M3; the store's signature check fails closed), the hook
    and bridge servers (M2; `connecto-session/src/local_socket/windows.rs` is the stub to
    fill in). The hook client already speaks named pipes (`\\.\pipe\<name>`).
  - Answers to the doc's unverified items: iroh builds and its loopback tests pass on
    Windows; `ssh-agent-client-rs` supports the Windows agent pipe (implemented, untested
    against a real agent: the service is disabled on this machine); the Credential Manager
    limit is 2560 bytes per item (Microsoft docs), oversize writes are refused;
    winget `Microsoft.PowerShell` confirmed (7.7.0, MSIX, per-user,
    `%LOCALAPPDATA%\Microsoft\WindowsApps\pwsh.exe`).
  - Setup on Windows: agents shown as not supported; optional "PowerShell 7" step via
    winget when `pwsh` is missing (never blocks; real install not yet exercised).
  - `.gitattributes` added: builtin package files and `*.sh` are LF on every platform
    (a Windows checkout embedded CRLF scripts otherwise).
  - New dependencies: `sysinfo` 0.40 (non-macOS process tree, Windows process-alive),
    `keyring-core` 1 + `windows-native-keyring-store` 1.1 (Windows), `getrandom` 0.3 and
    `rfd` 0.17 (already in the lock), `tokio` as a Windows dev-dependency of connecto-hook.
  - Known gaps and follow-ups:
    - Seven commands are reachable only from the macOS menu; Windows has no menu
      (user decision: in-window menu bar or a native menu through a safe wrapper).
    - The statusline scripts need `bash`, `jq` and `awk`; Git for Windows has no `jq`
      (input for the M3 scripts decision).
    - `PtySession::terminate()` kills only the shell; its children end when the session
      is dropped. A Job Object would close the gap.
    - Replacing a running `connecto-hook.exe` / `connecto.exe` fails while in use (M2).
    - M2 lead: `interprocess` 2.4.8 has a safe `client_process_id()` for named pipes
      (unverified here); pipe names need a `connecto-` prefix.
    - connecto-ui wording: "Connecto starts Claude and Codex in it" on the empty screen,
      and a missing glyph in the "Open one with ▯" hint; wasted band at the top of the
      sidebar. No exe icon or manifest yet.
    - `scripts/release/README.md` corrections from section 1 still to do.
- 2026-11-12 (Windows session, Windows 11 Pro 10.0.26300, x86_64): machine set up,
  baseline collected. Nothing committed yet.
  - Branch `windows-port` already existed and was checked out; tree clean at start.
  - `bootstrap.ps1 -CheckOnly`: ran, exit 3 (Build Tools, Windows SDK, rustup, rustc
    missing; confirmed by hand). Two script bugs fixed on this branch:
    1. `Get-NativeOutput` returned a one-line result as a string, so `$out[0]` was the
       first character (`present: g`). Now returns `,@($out)`.
    2. Items missing before an install stayed in the missing list after a successful
       install, so the install run ended `STILL MISSING ...` / exit 1 although everything
       was installed. Missing items are now recorded after the install attempt
       (`Add-MissingUnless`).
  - `bootstrap.ps1` (install): installed Visual Studio Build Tools 17.14.45 (MSVC
    14.44.35207), Windows SDK 10.0.26100.0, rustup 1.29.3. Re-run in both modes: exit 0.
    `--includeRecommended` did bring the Windows SDK.
  - winget IDs: all five confirmed with `winget show --id <id> -e` (section 5 TODO done).
  - Toolchain: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, `host: x86_64-pc-windows-msvc`.
  - Already on the machine: Git 2.46.0, Node.js (`C:\Program Files\nodejs`), Claude Code
    native install (`C:\Users\<user>\.local\bin\claude.exe`). Not present: `codex`, `pwsh`
    (only Windows PowerShell 5.1), so the default shell must fall back to `powershell.exe`.
  - **Baseline, 2026-11-12:** `cargo check --workspace --keep-going`, exit 101, 23 errors.
    Cargo stops at the first failing layer, so `connecto-session`, `connecto-net`,
    `connecto-automation`, `connecto-secrets`, `connecto-ui`, `connecto-workspace` and
    `connecto-app` were **not reached**; their errors are still unknown. All third-party
    crates that were reached compiled (including `ring`, `wgpu`, `egui`, `iroh-base`,
    `iroh-relay`, `portable-pty`) except the two macOS-only ones below.
    Failing crates and their errors:
    - `libproc` 0.14.11 (build script): `E0601` no `main` in `build.rs` on Windows.
      Dependency of `connecto-session`; must be target-gated (4.1).
    - `core-foundation` 0.10.1 (lib): `std::os::unix` and `libc::PATH_MAX` missing.
      Pulled in by `security-framework` from `connecto-secrets`; must be target-gated (4.4).
    - `connecto-packages` (build script), 2 errors: `build.rs` uses
      `std::os::unix::fs::PermissionsExt` (4.1).
    - `connecto-hook` (lib), 1 error: `client.rs:8` `std::os::unix::net::UnixStream`.
    - `connecto-pty` (lib), 4 errors, all in `session.rs`: `:14` `nix::sys::signal`,
      `:15` `nix::unistd`, `:83` no `tty_name` on `MasterPty`, `:118` no
      `process_group_leader` on `MasterPty`. This answers the 4.2 question: portable-pty
      0.9 does not have these two methods on Windows.
    - `connecto-store` (lib), 13 errors: `discover.rs:12` `std::os::unix::process::CommandExt`,
      `:18` `nix::sys::signal`, `:19` `nix::unistd`, `:66` `Command::process_group`;
      `store.rs:7` `PermissionsExt`, `:400-402` `nix::{errno, sys::signal, unistd}`,
      `:615` `std::os::unix::fs::symlink`, `:308`/`:421`/`:797` `Permissions::from_mode`,
      `:810` `Permissions::mode`.
    Note: the `nix` crate itself builds on Windows as an empty crate; the errors come
    from its missing modules, not from its build.
  - Open decisions 1-5 (section 2): still **unanswered**. M1 proceeds on the recommended
    options (native, `%APPDATA%\connecto`); none of the others affects M1.
- 2026-11-12: Handoff written on macOS (`docs/windows-port.md`,
  `scripts/windows/bootstrap.ps1`). Not run on Windows. Status: **not started**.
  - Baseline `cargo check --workspace` on Windows: **not yet collected**.
  - `bootstrap.ps1`: **untested** on Windows.
  - Open decisions 1-5 (section 2): **unanswered** by the user.
  - Open items: Codex Windows support (section 5), hook shell on Windows (4.6),
    `ssh-agent-client-rs` named pipe (4.7), Credential Manager size limit (4.4).
