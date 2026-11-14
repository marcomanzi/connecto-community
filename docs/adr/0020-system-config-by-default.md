# 0020. New projects use the system configuration

Status: Accepted. Changes the default of ADR 0013-era projects; amends the packages work.

## Decision

A new project runs Claude and Codex with the user's own `~/.claude` and `~/.codex`
(`ConfigMode::System`): the login and settings they already have, no second sign-in.
Isolated (a private profile and login per project) stays, as an advanced choice,
second in the Project Settings and labelled "Isolated (advanced)".

## Where the default lives

`ProjectSettings::default()` and `ConfigMode::default()` in `connecto-core`
(`packages.rs`). `connecto-session::project_settings::load` returns it for a project
without a settings file; `launch::uses_system_config` reads it at each launch.

## Existing projects

They keep what they had, with no silent switch:

- A settings file that names a mode keeps it, and one that omits it reads as Isolated
  (what an older file meant).
- A project that already ran but never saved settings (a folder with `profiles/`
  under `projects/<id>` and no settings file) is pinned to Isolated once, at startup
  (`pin_existing_projects`), recorded by `.config-default-system` in the data root.
  After that mark exists, a project without a file is a new project and gets System.
- A fresh data root only gets the mark.

## The store, provisioning, hooks

The CLI that runs is still Connecto's verified copy from `connecto-app provision` in
both modes; the mode only decides which configuration folder it reads. So provision
is still needed before the first launch. In System mode Connecto writes nothing
into `~/.claude` or `~/.codex`: the hooks go on each launch (Claude: a settings
file in the run folder passed with `--settings`; Codex: `-c` options), the Codex hook
trust read runs in an empty temporary `CODEX_HOME`, and transcripts are only read.
Packages are installed into the project folder, and guides into Connecto Home. What the
CLIs themselves store is what they store in the user's own terminal. The user's own
global settings apply, and `project_settings_conflicts` looks only at the project's
`.claude/settings*.json`, so a global `disableAllHooks` is not detected.
