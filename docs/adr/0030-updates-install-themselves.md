# 0030: Updates install themselves, and the turns they cut carry on

Status: Accepted, built in `connecto-update` (`service.rs`, `state.rs`,
`install.rs`), `connecto-session` (`terminals.rs`, `terminal_list.rs`),
`connecto-net` (`RemoteManager::restart_blocker`) and
`connecto-app/src/auto_update.rs`. Tested with unit tests; not yet tried on a
real update from the feed.

## Context

The updater checked at launch and then daily, downloaded a newer release in
the background, and waited for the person to click Update and restart; that
click waited for agent turns to end. People run agents for hours, and want
new versions to land by themselves and the agents to keep going.

A restart already restores every open project. Terminals saved `working`
start again, resume their conversation (`claude --resume <id>`,
`codex resume <id>`, ...) and are marked interrupted; they are not told to
carry on, because a crash or a quit that cut a turn says nothing about
whether the person wants it carried on. The app session (Connecto Chat) is
not started by itself at all. Each launch can pass a short first message to
a resumed conversation (the resume note, as the command's last argument).

## Decision

1. **Checks every minute.** `LOOK_EVERY` and `CHECK_EVERY_MS` are a minute
   (the first check still waits 20 s after launch). The feed is about 1 KB
   and served by Caddy with an `ETag`; the updater keeps the last feed and
   its `ETag` in memory and asks with `If-None-Match`, so an unchanged feed
   is a `304` with no body. There is no rate limit on the feed, and its
   requests are not logged (the log keeps only `.dmg`/`.zip` downloads).
2. **A newer release replaces a ready one.** With a version ready, a check
   still reads the feed; a newer version is downloaded and checked, replaces
   the ready one, and the older files are removed. While a check runs (the
   feed, then any download), the automatic install waits, so it hands over
   the newest release. The check is claimed with the ready release locked, and the automatic install decides again with that
   lock held (version, check, setting, the quit, and what blocks a restart)
   right before its hand-over, so a stale version is never handed over, and a hand-over by
   hand and an automatic one cannot both start.
3. **Installed by itself, when nothing blocks.** Setting
   "Install updates automatically", saved in `updates/state.json`
   (`auto_install`), **on by default**. Every 5 s, a ready version is handed
   over unless something blocks a restart:
   - a pairing in any phase before it ends (`Preparing` .. `Keeping`);
   - a file being sent to a peer or a chat attachment being received
     (`HostService`), or a file being read from a host;
   - a package install or a terminal's spawn (`install_gate::busy`), or a
     terminal in preflight or in its login;
   - the person typed in the window (any terminal, local or remote, or a
     text field) in the last 30 s.
   The strip and Settings say what holds it back. Agent turns do not: they
   are carried on (4).
4. **The turns an update cuts carry on, once.** The shutdown runs once
   (later callers wait for it), and first stops the automatic install
   (`Updater::stop_auto`), so no hand-over starts during a quit. Right before
   the terminals stop, if the hand-over has started (`Updater::handed_over`),
   every open project's registry (all at once, none waiting on another)
   marks the terminals a live agent is in a
   turn in right now (not background work after a turn), with the
   conversation it is in then, as `"cut_by": "update"` in `terminals.json`,
   saves, stops following the panes and stops them, so no turn ends after
   its mark. A saved `working` that a still-starting pane has not
   confirmed is an older cut and is not marked. At the next launch such a
   terminal starts (the app session too), resumes its conversation, and gets
   `continua` as its resume note. The mark is dropped and the list saved
   before any terminal starts; if that save fails, nothing is carried on, so
   it is said at most once. A terminal with no saved conversation starts but
   is not told anything; a resume that fails starts fresh with the usual
   note, not `continua`. Turns cut by a crash or a quit have no mark and stay
   as before. The same holds for Update and restart by hand (including
   Restart now); with automatic installs on, that button restarts at once,
   since the turns are carried on.
   **Asks across the update.** A lead that sent `--ask` and ended its turn
   is idle, so the rule above would leave it stopped while its helper
   carries on, and the reply would have no one to go to (the bridge keeps
   requests in memory). So at the same look, the registry asks the bridge
   for the asks typed and not answered yet (`OpenAsk`, by terminal id), and
   saves each on its target as `"owes": [{"request": "r13", "to": "<asker>"}]`
   when the target restarts: it is in a turn, or, until nothing changes,
   it waits on an ask of its own to one that restarts (a lead whose child
   waits on a grandchild). At the next launch each asker of a restored ask
   starts, resumes its conversation, and is **not** told `continua`, even if
   its own turn was cut: its helper's reply wakes it, as it would have
   without the update. The bridge opens the ask again under its old id
   (new ids come after it) once the target runs, ends it with the target's
   next turn (held while the target waits on asks of its own, as any ask
   is), and gives the reply to the asker once the asker runs (up to 180 s,
   then it stays for `connecto result`). A target that does not run within
   180 s ends the ask as exited. The marks are dropped and saved with the
   `cut_by` ones, so an ask is restored at most once.
   **Everything that was running comes back.** An update is not the
   person quitting, so the first launch after it brings back every terminal
   saved `desired: running`, not only the turns it cut and their askers:
   idle agents, their parents and children, one at its prompt with
   background work, and the app session. The launch knows it follows an
   update from the updater's pending record (`Launch::Updated`, read before
   the window opens), so this holds even when the version that quit is
   older and saved no marks. It applies to the first restore of the projects
   open at the quit only; a project opened or chosen later restores as after
   a quit, and so does a project whose setup failed at launch and is set up
   again later. An idle agent resumes its conversation with no note (a shell
   starts a new shell: its commands and jobs are not resumed); a cut turn
   still gets `continua`, and an asker of a restored ask still gets none. If
   the setup window interrupts that launch, it passes `--after-update` to the
   app it starts again, since the updater may have forgotten the update by
   then.
   Otherwise (a plain launch, a rollback, a version installed by hand) an
   idle terminal stays stopped, as before, and that is saved at launch, and
   an update's quit writes the list even when it cut nothing (and says so
   when that write fails), so a later update never brings back a terminal a
   launch left stopped. A terminal whose start failed (its preflight was
   refused) keeps `desired: running`, so an update tries it again.
5. **No restart loop.** Before an automatic hand-over the version is added
   to `auto_tried` (all are kept: one a release at most) with the time. The updater never installs
   those versions by itself again (by hand it can), even when two take
   turns, and never restarts by itself twice within 10 minutes (a clock set
   back counts as within). The attempt is also claimed with a file,
   `updates/auto-<version>.claimed`, made only if absent, so two copies of
   the app on one data folder (possible on Windows) cannot both hand it
   over. A version that does not start is rolled back and
   not offered again, as before. A failed automatic hand-over is said in the
   strip (and keeps being said) and not retried.
6. The restart is the one a stop signal makes: the window is asked to close,
   everything is shut down, the process exits, and the hand-over swaps the
   new version in and starts it.

## Consequences

- 1440 feed requests a day per running install, nearly all `304`s.
- An agent is restarted mid-turn and its tool's own resume decides what of
  the cut turn it keeps; `continua` asks it to pick up from there. A tool
  call that was running when the app quit (a build, a test) is cut, and the
  agent sees that in its conversation.
- A draft the person left in a composer, untouched for 30 s, is lost by the
  restart, as at any quit.
- The note is the tool's first message on its command line, so it goes to
  the same conversation and needs no screen check (ADR 0023 does not apply).
- A remote peer (the phone) loses its link for the restart and reconnects.
- Two copies of the app on one data folder (Windows allows it: there is no
  single-instance lock) are not made safe here, as nothing else in the data
  folder is: the claim file keeps them from handing the same version over
  twice, but they share the staging folder, the 10-minute gap, and the
  `cut_by` marks.
- The blockers are looked at right before the hand-over, not held: a
  transfer or pairing that starts between that look and the quit is cut.
  That window is the hand-over's own writes (the claim, the state, the
  pending record, starting the helper; milliseconds on a healthy disk) plus
  up to 3 s while the window is asked to close before remote connections
  are shut down.
- Background work running at the quit (a build, a watcher) is cut with its
  process; the agent comes back at its prompt, marked interrupted, and its
  tool decides what it says about the stopped task.
- The pending record lasts until the new version has been up 10 s
  (`HEALTHY_AFTER_SECS`): a relaunch by hand within those seconds restores
  as after an update too.
- A turn that ends in the moment between the look and the agent's process
  actually stopping (its descendants are listed before it is signalled) is
  told `continua` though it had finished. The agent then has nothing to
  carry on and says so: harmless, and no stop can close that window.
