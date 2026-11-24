# 0026: The dream schedule runs only while the app is open, once per local date

Status: Accepted, implemented in `connecto-memory/src/scheduler.rs`. Spec
`docs/specs/project-memory.md` §7. The 30 s tick has not been observed in a
running app; the rules are tested with a fake clock.

## Context

The dream cycle consolidates a project's memory at a time the user picks
(default 02:30 on weekdays). Connecto has no scheduler: `connecto-automation`
reacts to screens, it does not plan in time (spec §2.3). A daemon that works
while the app is closed or the machine sleeps is out of scope for this version
(spec §1.2), and nothing may run model sessions behind the user's back.

## Decision

1. One scheduler thread in the app, a tick every 30 s, plus a check at start and
   when a wall-clock jump (more than 60 s against the monotonic clock) shows the
   machine woke from sleep. No timers in the OS,
   no launch agent, no work while the app is closed.
2. A schedule is per project: weekdays, local time `HH:MM`, maximum duration,
   and a minimum condition (default: at least one event or annotation after the
   last cutoff). When the minimum is not met, the date is settled as done with
   no run.
3. Idempotency key `(schedule, local date)`: at most one run per date. A local
   hour that a daylight-saving change repeats or skips still gives exactly one run
   for that date. Local date and time are compared as calendar values, never as
   instants. A run that ends published, with nothing to change, or **failed**
   counts as that date's run.
4. Catch-up: when the app opens and one or more due dates have no run, **one**
   cumulative run starts, and the missed dates are recorded as caught up by it.
   Due dates go back at most 31 days and never before memory was turned on
   (the first time the scheduler saw the schedule). A run held because the CLI
   does not trust the folder leaves its date not done and is tried again
   5 minutes later.
5. A run found `in_corso` at start is marked `interrotta`, its staging is
   discarded, and catch-up starts it again once. A run that had already
   published (a `publication` row exists) but stopped before its own books were
   written is completed by `scheduler::reconcile` instead, from a pending record
   written before publishing.
6. Pause and Stop Everything: no run starts while paused; a running dream checks
   the pause, the pause epoch, cancellation and its time limit (counted from the
   start of the run) inside every staging transaction and inside the
   publication, up to the commit, and does not publish if any of them closed;
   Stop Everything cancels memory runs (added with this work, spec §2.4) and
   nothing restarts until the user resumes. Closing the app cancels runs; the store stays
   consistent because publishing is one transaction (ADR 0024).
7. The UI says next to the time that nothing runs while the app is closed.

## Consequences

- A user who closes Connecto every evening gets the dream at the next start,
  not at night. That is visible, and the freshness and backlog indicators show
  it.
- The scheduler is plain code over a clock trait, so daylight-saving changes,
  restarts and missed days are tested with a fake clock (spec §11.2).
- A future daemon would replace the trigger, not the run: the run, the key and
  the catch-up rule stay.

**Reverse if** users need dreams while the app is closed; that is the daemon in
spec §1.2 and needs its own ADR.
