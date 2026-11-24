# 0039: A project's Agentic Works runs several jobs at once

Status: Accepted (2026-11-23), built in
`crates/connecto-core/src/agentic_works.rs` (`MAX_PARALLEL`, `RunningJob`,
`WorksView::parallel` and `running`, `WorksAction::Parallel`),
`crates/connecto-app/src/agentic_works/mod.rs` (the project's cycle,
`fill`, `take`, `run_job`, `next_database`, the shared setup),
`db.rs` (the claim's cap), `store.rs` (the `settings` table and the journal
keyed per job), `cli.rs` (`connecto works parallel`),
`crates/connecto-session/src/bridge/works.rs` (what Pause allows),
`crates/connecto-session/src/launch.rs` (Antigravity's shared files) and
`crates/connecto-ui/src/agentic_works.rs` (the sheet). Tested with unit
tests and the PostgreSQL integration test
`ten_jobs_run_at_once_shared_by_the_databases`; the tests of ADR 0032 and
0036 run unchanged at one job at a time. Not yet tried by hand in the app.

## Context

ADR 0036 §4 kept one job at a time per project, every database in turn, and
left running several at once "for the user to decide", because agents would
then share the project folder. Many work types have jobs that are
independent of one another, each reading its own input and writing its own
answer; for those, one job at a time makes a long queue needlessly slow.

One job at a time was enforced in three places: the cycle (one task per
project, holding the project's lock file while its job ran), the queue
(a unique index allowing one row `doing`, and a claim taking a job only when
none was doing) and the local journal (one row per project and database).

## Decision

1. **A setting per project.** `parallel`, from 1 to 32, 1 when never set
   (the store's `settings` table; a project without a row runs one job at a
   time exactly as before). The sheet's "Jobs at once" and
   `connecto works parallel <n>` set it (Bypass agents only, like `link`;
   `connecto works parallel` alone reads it, also under Pause). Lowering it
   stops no running job: no new one starts until fewer run. A change is
   made while no claim is being admitted (it takes the lock claims are
   admitted under), and each claim reads the number and counts the jobs
   running only once admitted, after connecting: once a lower number is
   acknowledged, no claim takes a job by the old one. Whether a job was
   admitted with several at once (its own folder, the prompt's note) is
   fixed at admission and goes with the job. A job admitted with one at a
   time was told of no other job and may use the project's files as its
   own: while it runs no other job is admitted, even after the number is
   raised.

   A claim takes nothing across a Pause: the pause epoch is checked when
   the claim is admitted and again inside the claim's transaction once a
   job is chosen; when it changed (even if the user has resumed since), the
   transaction is rolled back and the job stays waiting for the next look.
   Taken with the old epoch, it would fail before dispatch.
2. **The cycle keeps the places filled.** A project's cycle cleans up what
   an earlier run left (terminals, dropped outcomes, job folders), as
   before, and then stays while its jobs run: it looks at the databases
   again as soon as a job ends, or every 5 seconds, and ends once nothing
   runs and nothing waits. Each job runs in its own task: its prepare
   command, its agent terminal, its outcome. Every job still runs inside
   its project's cycle, so the clean-up at the start never touches a
   running job, and Pause, Stop and closing the project end every job, the
   cycle waiting for them.
3. **The databases share the places fairly.** A look first writes back the
   outcomes waiting (of jobs no longer running), then takes jobs one by one
   while places are free: each from the database running the fewest of the
   project's jobs, the next in turn after the one that gave the last job
   among equals (`next_database`). A database with nothing waiting, or
   that fails, is left for the rest of that look. With one place this is
   ADR 0036's turn exactly. Places are not taken back from a database that
   filled them while the others were empty: as its jobs end, the others,
   running fewer, get the free places first.
4. **The queue takes several jobs.** The claim takes a job only while the
   queue has fewer rows `doing` than the project's setting (with 1, "none
   doing", as before), under the same advisory lock, now with `FOR UPDATE
   SKIP LOCKED`, in a `READ COMMITTED` transaction whatever the role's
   default isolation, so the count after the lock sees every claim
   committed before it. A queue made before (its `one_active_work` index
   allows one row doing) has that index replaced before the first claim
   with a setting above 1 (the connecting role owns the queue, ADR 0032):
   only when it is exactly that index on `connecto_jobs`, an index of that
   name on another table is left alone. The replacement, `doing_work`, on
   `claimed_at` where the row is doing (new queues are made with it, so
   finding expired and counting active jobs never scans finished history),
   is built first, outside any claim, with `CREATE INDEX CONCURRENTLY` and
   its own limits (5 minutes for the statement and for its locks: it waits
   for producers' open transactions); one left invalid by an interrupted
   build is dropped and built again. Only once it is valid is the old index
   dropped, in a short transaction with a claim's limits; when that cannot
   have its lock, the next look tries again.

   The change runs in a task of its own per queue, on its own connection:
   the project's cycle waits for it two seconds at most, then serves that
   queue one job at a time and the others fully until it is done. Closing
   Connecto ends the task and drops its connection; the build is run with
   `client_connection_check_interval = 1s` (PostgreSQL 14 and later), so the
   server stops it at once. Several Connectos (Macs) sharing a queue change
   it one at a time: the whole change holds a session advisory lock, and a
   Connecto that cannot have it leaves the queue for that look, never taking
   another's half-built (still invalid) index for an interrupted one.
   A change is started only while its database is linked, checked and
   registered (with its task) under the lock a removal takes, so a removal
   either finds it or it sees the database gone. Removing a database calls
   off its queue's change first: the statement under way is stopped with
   that connection's own cancellation key (its server process and the
   secret only it was given), sent to the address it was made to (for Cloud
   SQL, the same route and TLS), never by process id over a connection
   resolved anew. A cancellation sent proves nothing (a proxy may take it in
   and drop it): the change's own statement is kept waited for, and only its
   ending says the server stopped it. The removal waits for that; when it
   does not come in time (the server does not answer, or the cancellation
   was lost), the removal fails, the database stays linked, and the change
   stays tracked while the cancellation is sent again every few seconds. A
   change whose connection was lost (no answer from the server, which may
   still be running it) is over only once a fresh connection to the same
   queue (its identity checked) can take the session lock the change holds;
   until then it stays tracked, a removal fails, and it is told to stop. The old index
   is dropped only after checking, just before, that the database is still
   linked and the change not called off. An older Connecto still takes one job at a time from
   such a queue: its claim checks that none is doing.

   The count is the queue's: two Connectos sharing one queue (two Macs)
   each claim up to their own setting of that one count, so with unequal
   settings the Mac with the lower one takes nothing while the other's
   jobs fill it. The project's number of jobs at once across several
   databases is each Mac's own: it does not add up the jobs another Mac
   runs from other databases.
5. **A journal row per job.** The journal is keyed by the job's token
   (migrated in one transaction from a row per database). Each job's row is
   written before dispatch, gets its terminal, then its outcome, and is
   cleared once written back. Every outcome goes back to the database its
   job came from, by its own task right after the job; when that fails it
   stays journalled and the next look writes it back. A job lets go of
   nothing (its place, its folder, its journal row) before its terminal is
   confirmed closed: a close that fails is tried again every second; when
   Connecto stops first, the journal row stays and the next start closes
   the terminal. Restart recovery is
   unchanged: every journalled terminal is closed and every outcome written
   back ("Connecto stopped before recording a final response" for jobs that
   were running).
6. **Each job keeps its own 30 minutes** from its claim: its prepare, setup
   and agent end at its own deadline, and the queue marks a job doing for
   30 minutes timed out at the next claim, as before. A job waiting for
   the answer to its question keeps its place and its clock (ADR 0040).
7. **A folder per job.** With a setting above 1, every job gets
   `<project>/.connecto/works/<id>-<token>` (as prepare already did), its
   prepare command gets it as `CONNECTO_JOB_DIR` with `CONNECTO_JOB_ID` and
   `CONNECTO_WORK_TYPE`, and its agent is told in its prompt that other
   jobs run at the same time in the project folder and that this folder is
   its own. The folder is removed when the job ends.
8. **Setup is shared.** A job whose prepare asks for setup again (exit 75)
   while another job's setup of the same type and command runs waits for
   that one and takes its own result (kept with that run, whatever runs
   next), instead of failing. While a setup with another command runs (a
   revision being published), the job waits for it and then runs its own.
   A setup its own job stopped (Pause, that job's database removed, its
   deadline, Connecto closing) records no failure, and a job waiting for it
   runs setup itself, within its own deadline.
9. **Removing a database** (`unlink`) is refused while any job of it runs or
   waits to be written back; `--force` drops every such outcome and stops
   every such running job, as for one job before. A dropped job's journal
   row stays until its own task has closed its terminal, even if the
   database is linked again meanwhile.
10. **Status.** The view lists the jobs running (database, id, type, since);
    the sheet shows them under the databases, and `connecto works show` and
    `databases` print the setting and the running jobs.
11. **Antigravity's shared files.** Antigravity reads its hooks and its
    settings from one file each in the user's home, which every Antigravity
    launch updates. Concurrent launches now update them one at a time, each
    through its own temporary file, so ten jobs starting together neither
    fail nor leave a file half written.

## What a project must make safe itself

Connecto isolates what it runs, not what the project's own commands and
agents do. With a setting above 1:

- The prepare command must write only into `CONNECTO_JOB_DIR` (and read the
  job from stdin), never to fixed paths in the project such as `tmp/`,
  `out.mp4` or a shared cache, unless it locks them; two prepares run at
  the same time.
- Setup must be safe to run while other jobs' prepares and agents run (it
  runs at most once at a time per type).
- The work type's instructions should keep the agent's files in its job
  folder, and say what in the project it may change: several agents work in
  the same project folder and git checkout at once. Work that edits the
  project's own files should stay at 1.
- External limits (an API's rate limits or quotas, GPU or memory for video
  work, the database's connection limit: each job uses one connection while
  its outcome is written back, and each look one per database) must fit the
  number of jobs at once.

## Consequences

- A project runs up to 32 jobs at once from all its databases; ten jobs of
  a slow work type take about as long as one.
- With the setting at 1 nothing changes, except that a queue made from now
  on has no `one_active_work` index (the claim enforces the same).
- A long job no longer delays the other databases while places are free.
- A database that fails to connect still costs up to 20 s per look, during
  which no free place is filled.
