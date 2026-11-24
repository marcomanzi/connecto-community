# 0042: New project from a git repository, and Delete Project

Status: Accepted (2026-11-24), built in
`crates/connecto-core/src/project_files.rs` (`Cloning`, `DeletePlan`,
`ProjectTask`), `crates/connecto-core/src/workspace.rs` (`clone_project`,
`plan_delete`, `delete_project`), `crates/connecto-workspace/src/project_files.rs`
(transport policy, staging and publishing a clone, refusals, identity,
git warnings, the trash), `crates/connecto-workspace/src/lib.rs`
(reservations, the deletion on the worker and the teardown thread, the
`linked` flag) and `crates/connecto-ui/src/project_files.rs` (the New
Project and Delete Project dialogs). Tested with unit tests in those
modules, `crates/connecto-workspace/tests/project_files.rs` (a local bare
repository, a server that never answers, a test folder standing in for the
Trash) and the window tests in `crates/connecto-ui/src/tests/project_files.rs`.
The real macOS Trash and the freedesktop trash in a real home folder are
not exercised by tests (tests never touch the person's Trash); Windows is
not built here.

## Context

A project could only be opened from a folder that already existed, and
removing one meant closing it and deleting its folder by hand. People asked
for both from Connecto: start a project from a repository, and delete a
project together with its folder. Deleting a folder is the most dangerous
thing Connecto can do on the person's disk, and a clone writes into a
folder the person picked.

## Decision

**Clone.** File > New Project takes a folder and, optionally, a repository
address. The clone goes into a new folder named after the repository inside
the chosen one (as `git clone <url>` run there would), never into an
existing folder.

- The system's `git` runs it, with `GIT_TERMINAL_PROMPT=0`: the person's
  credential helper and SSH agent answer; Connecto never asks for or keeps
  a password.
- Only https, http, ssh (`ssh://` or `user@host:path`, IPv6 hosts
  included), git, `file://` and absolute local paths are taken; remote
  helpers (`name::address`), other schemes, relative paths and any user
  info in an http(s) address are refused before git runs, and git itself
  is run with `protocol.allow=never` plus those protocols.
- The destination (the chosen folder's real path and the repository's
  name) is checked by the rules for a project's folder before git runs:
  a repository named `.codex` cloned into the home folder is refused
  before anything is written.
- git writes into a private staging folder beside the target, made by
  Connecto alone (exclusive create; identity read from the opened folder).
  Only that folder is ever removed, and only while it is still the one
  made. The finished checkout is published with a rename that never
  replaces anything (`renameat_with` + NOREPLACE: `renameatx_np` on macOS,
  `renameat2` on Linux; .NET `Directory.Move` on Windows), and checked
  before it is opened. If the target appeared meanwhile, the clone is kept
  in the staging folder and the message says where.
- Cancel stops git and what it started (its process group) and removes the
  staging folder; no half-made project is left. Closing Connecto cancels
  every clone under way and waits for that cleanup. The clone's hold on
  its destination is released by a guard, however the clone ends (a
  thread that never started included), and a staging folder that could
  not be removed is named in the message.

**Delete.** Next to Close (sidebar menu and File menu; desktop only, since
Close is not offered remotely). The person sees the full path, what is in
it, where it goes and any git work that would be lost, and types the
folder's name exactly. Then the project closes, its terminals, background
work and relay stop, and only if all of that stopped does the folder go to
the system's trash. Recents and Connecto's data for the project (terminal
list, history, profiles) go only after the move succeeded.

- **Trash only.** Nothing is ever deleted for good. A folder with no
  usable trash is refused and the person removes it in their file manager:
  Linux folders on another disk than the home trash, and Windows for now
  (Windows does not promise a folder can be put back from the Recycle Bin,
  and checking needs unsafe code).
- **Refused folders**: one opened through a link anywhere in its path
  (remembered in the saved workspace and the recents, and detected at
  restore when a saved folder became a link while Connecto was closed), a
  volume root (its device differs from its parent's), the home folder and
  anything holding it, system trees (`/System`, `/Library`, `/usr`, `/var`,
  `/private`, `/opt`, `/etc`, … as prefixes), the home folder's own folders
  and settings, anything holding Connecto's data, Connecto Home, the
  Automations folder, the source checkout or the running app, and anything
  holding another project (open or recent; rechecked against the live list
  when the deletion starts).
- **One directory, not a path.** When the confirmation is shown the
  workspace opens the folder and its parent and keeps them open (by the
  plan's token) until the deletion ends or the dialog closes; their device
  and inode are read from those handles, so they cannot pass to another
  folder meanwhile. `delete_project` takes the plan and refuses when the
  folder at the path is not that one, or the plan was given back. While a deletion runs its folder is reserved: nothing
  inside opens, and no clone goes into it; clone and deletion holds never
  overlap.
- **Git warnings**: uncommitted changes (only the project's subtree when it
  is inside a larger repository), unpushed commits, a detached HEAD with
  commits on no branch, stashes, no remote, bare repositories, and "git
  could not check" whenever a check failed, never taken for clean.

## The residual window

The move to the trash goes by path, as the Finder's `trashItemAtURL` and
the freedesktop rename do; that is what keeps "Put Back" pointing at the
original place, so the folder is not first renamed into a private staging
spot. Everything the move needs (the name in the trash, the `.trashinfo`
record and its date, computed in Rust) is made ready first; then the path
is checked (no link on the way, its real path unchanged), then the parent's
identity, and last the folder's own identity, against the held handles;
then the move is the only call left: one `renameat2` on Linux, one
Foundation call (`trashItemAtURL`) on macOS, whose own system calls Connecto
does not control. A folder swapped in within that call, after every process
of the project has stopped, would be moved instead. Even then
nothing is erased: the moved item is checked after the move, and the error
names what is in the Trash and where, and says the confirmed folder was not
moved.

## Consequences

- A new project from a repository needs git on the machine; the message
  says so when it is missing.
- People on Windows remove project folders themselves for now.
- A project opened through a link can be closed but not deleted from
  Connecto.
