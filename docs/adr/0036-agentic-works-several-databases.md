# 0036: A project's Agentic Works takes jobs from several named databases

Status: Accepted (2026-11-23), built in
`crates/connecto-core/src/agentic_works.rs` (names, `LinkedDatabase`,
`DbStatus`, the actions), `crates/connecto-app/src/agentic_works/store.rs`
(the `databases` and `journal` tables and the migration),
`crates/connecto-app/src/agentic_works/mod.rs` (the cycle over databases,
link and unlink, publishing for some databases), `cli.rs` (`--db`,
`databases`, `unlink`), `crates/connecto-session/src/bridge/works.rs` (what
Pause allows) and `crates/connecto-ui/src/agentic_works.rs` (the sheet).
Tested with unit tests and one PostgreSQL integration test
(`every_linked_database_is_served_and_one_failing_stops_none`); not yet
tried by hand in the app.

## Context

Agentic Works (a PostgreSQL queue whose rows start an agent in the project,
see the `agentic-works` skill) linked one database per Connecto project. A
product with several databases (one per environment, for example) needed
one Connecto project per database, and every work type's instructions
published again in each.

Work type definitions were already kept by Connecto (the local store), not
in the database: only the link, its queue identity and the outcome journal
were one per project.

## Decision

1. **Named databases.** A project links any number of databases, each under
   a name (`dev`, `staging`, `production`: letters, digits, `-`, `_`, up to
   32; never `all`, which means every database). Each has its own connection, a connection string secret or Cloud SQL
   with a key or the gcloud login, and its own password in the vault. One
   queue (its `connecto_works.project` identity) is linked under one name
   only. A Cloud SQL password kept by a link is used for its own instance and
   user only, as before; two links on one instance and user may share it,
   and it is deleted only when no link uses it any more.
2. **Migration.** The store's `links` and `outcomes` tables (one row per
   project) become `databases` and `journal` (one row per project and
   database), the old rows named `default`, in one transaction when the
   store opens. A link made without a name is `default`. An older Connecto
   opening a migrated store sees no link.
3. **Work types are published once.** A type serves every linked database,
   including those linked later, unless it is published for a list of them
   (`publish <type> --db dev,staging`; `--db all` widens it again; a
   republish without `--db` keeps the list; a blank `--db` or a blank name
   in it is refused). A definition handed to the user for review (an agent
   not in Bypass) keeps the databases asked for, and the sheet shows them
   before publishing. A job reads its definition and its databases from one
   row, so a republish never pairs one version's commands with another's
   databases. A job whose type is not
   published for its database fails with that reason, as a job of an
   unpublished type always has.
4. **One job at a time per project, every database in turn** (superseded
   by ADR 0039: a project may run several jobs at once, the databases
   sharing the places; with the setting at 1 this section still holds). Each cycle
   looks at the databases starting after the one that gave the last job and
   runs the first job found, so a busy database never starves another. The
   journal holds one outcome per database, written back to the database the
   job came from (its queue identity is checked as before). A cycle lists
   the databases once; just before claiming, and before writing an outcome
   back, it checks the database is still linked with the same queue, under
   a lock removal takes, so a database removed meanwhile gives nothing. A database that
   does not connect within 20 s, or fails, gets a failing status and is
   skipped: the others are served in the same cycle, and its own undelivered
   outcome waits without blocking them.
5. **Status.** Each database's last answer (ok, the job taken or why it
   failed, and when) is kept in memory and shown by the sheet and
   `connecto works databases` / `show`.
6. **Commands.** `--db <name>` names the database for `link`, `test-db`,
   `test`, `job` and `unlink`; it may be left out while the project has one
   database. `test-db` alone tests each linked database. `unlink --db <name>`
   (Bypass agents only, like `link`) forgets the link and leaves the queue in
   the database; it is refused while a job of that database runs or waits to
   be written back, unless `--force`. Forced, the outcome is dropped (the
   database marks the job timed out after 30 minutes) but its journal row
   stays, marked, until its terminal is closed: a running job is stopped and
   its terminal closed, as after any job. Under Pause, `databases` and
   `test-db [--db <name>]` are allowed: they only reach linked databases.

## Consequences

- One Connecto project serves every environment of a product with one set of
  work types; no instructions are repeated per database.
- Jobs of different databases never run at the same time in one project, so
  a long job in one database delays the others, as a long job delayed the
  next one before. Running one job per database at once would remove that
  delay, but agents would then share the project folder concurrently; that
  is left for the user to decide. (ADR 0039 lets each project choose.)
- A failing database costs at most one connection timeout per cycle.
- Remote projects do not show Agentic Works (the sheet needs a local
  project), so nothing changes there.
