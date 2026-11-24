---
name: agentic-works
description: Set up, fix or test this project's Agentic Works (Connecto's PostgreSQL job queue that agents answer with JSON) using the `connecto works` command. Use when the user asks to configure an agentic work, link a database or Google Cloud SQL to the project, publish or change a work type, or check why jobs don't run.
---

# Agentic Works

Agentic Works turns this project into a job queue: a program inserts a row in `connecto_works.connecto_jobs` (project name, work type, prompt), Connecto opens an agent terminal in the project, and the agent's final JSON answer, checked against the work type's schema, goes back into the row. You set it all up with `connecto works`, without the Agentic Works sheet. It only works on the project of the terminal you run it in.

`link`, `unlink`, `publish`, `setup`, `test` and `key-import` run commands, start agents or replace a credential with the user's rights, so only an agent Connecto started in Bypass mode may use them, and never a job's agent or a terminal it started. If you were started in Auto or Prompt mode, they are refused. `publish` then tells you to end your turn with the definition JSON as your whole reply, for the user to import and publish in the sheet; say so to the user first. Leave linking, setup, test jobs and key imports to them in Terminal → Agentic Works….

Start with `connecto works show`: it lists the linked databases and how each last answered, which work types are published and for which databases, which secrets exist (names only) and the last outcome. `connecto works help` lists every command.

## Several databases (dev, staging, production)

One project can take jobs from several databases, each linked under a name, so one set of work types serves every environment. Do not make one Connecto project per database.

- Link each with `--db <name>` (letters, digits, `-`, `_`; not `all`): `connecto works link --db staging --secret STAGING_DB --create`. Each has its own connection (connection string or Cloud SQL, key or gcloud login) and its own secret or password. The first database linked without `--db` is named `default`; a project linked before named databases has that one.
- Work types are published once and serve every linked database, including those linked later. `publish <type> --db dev,staging` limits a type to those; `--db all` widens it again; republishing without `--db` keeps the type's databases. If `publish` hands the definition to the user, the databases you named go with it. A job of a type not published for its database fails with that reason.
- Connecto runs one job at a time in the project by default, looking at each database in turn, and writes every outcome back to the database its job came from. A database that does not answer is marked failing and skipped: the others keep running.
- To run several jobs at once, `connecto works parallel <n>` (1 to 32; Bypass only, like `link`; `connecto works parallel` alone shows it). The databases share the places: each free place goes to the database running the fewest jobs. Each job has its own claim, its own 30 minutes, its own agent terminal and its own folder (`CONNECTO_JOB_DIR`, also named in the agent's prompt). Before raising it, make the work type safe to run several times at once: see "Several jobs at once" below.
- `connecto works databases` lists them and how each last answered. `test-db` tests them all (or one with `--db`); `test`, `job` and `link` need `--db <name>` once there are several. `connecto works unlink --db <name>` stops taking jobs from one (its queue stays in the database); `--force` also stops a job of it that is running and drops an outcome still waiting to be written back to it.

## 1. The database

Secret values never pass through you. When a connection string or password is missing, ask the user for it and end your turn until the notice says they answered:

```
connecto secret request WORKS_DB --agentic-works --reason 'PostgreSQL connection string for the Agentic Works queue'
```

PostgreSQL by connection string (the role must be able to create a schema; a remote host needs TLS with a trusted certificate):

```
connecto works test-db --secret WORKS_DB          # each step, changes nothing
connecto works link --secret WORKS_DB --create    # new database: makes the queue
connecto works link --secret WORKS_DB             # queue already there: validates it
connecto works link --db production --secret PROD_DB --create   # another database
```

Google Cloud SQL: `connecto works guide --cloudsql <project:region:instance> --database <DB> --user <USER>` prints the gcloud and SQL steps (service account, key, database role, API) and the `link` command that fits. If `gcloud` is installed and signed in, you may run them yourself, but ask the user before creating accounts, keys or roles in their Google project. Then:

```
connecto works key-import ~/Downloads/connecto-<project>.json   # prints the key's secret name
connecto works test-db --cloudsql <project:region:instance> --database <DB> --user <USER>
connecto works link    --cloudsql <project:region:instance> --database <DB> --user <USER> --create
```

The first `test-db` or `link` for a user and instance makes Connecto ask the user for that password itself: end your turn, and run the command again when the notice says they answered. Use `--adc` instead of a key for the user's own `gcloud auth application-default login`, `--iam` to sign in with the service account's IAM user (no password), and `--ip private|psc` for a private address. After importing a key, tell the user they can delete the downloaded file.

When `test-db` fails, its last line says why: fix that (role grants, TLS, API enabled, instance name) and test again before linking.

## 2. The work type

Agree with the user on what the work does, what the prompt will contain and what JSON they need back. Write the definition as one JSON object, check it, then publish it:

```
connecto works check <<'EOF'
{
  "instructions": "Read {{prompt}} and summarize it in three sentences for a new developer.",
  "schema": {"type": "object", "required": ["summary"], "properties": {"summary": {"type": "string"}}}
}
EOF
connecto works publish summary --file work-summary.json
```

- `instructions` must contain `{{prompt}}`. Connecto adds the schema and asks for one JSON value itself.
- `schema` is a JSON Schema of the result.
- Optional `setup` (`{"argv": ["tool/setup.sh"]}`) runs at publish, in a shell terminal of the agentic jobs project the user can answer; the type is published only if it succeeds.
- Optional `prepare` (`{"argv": [".venv/bin/python", "tool/prepare.py"], "timeout_s": 600}`) runs before each job, reads the prompt on stdin and prints one JSON value that replaces `{{prepared}}`; the instructions contain `{{prepared}}` exactly when there is a prepare. It gets `CONNECTO_JOB_DIR`, a fresh folder for its files, and exits 75 to ask for setup again.
- `argv` is a list, never a shell line: a program on PATH or a path inside the project, no absolute paths, no `..`.

### Several jobs at once

With `parallel` above 1, several prepares and agents of the project run at the same time in the same project folder. Connecto gives each job its own folder and runs a type's setup once at a time (a job asking for it waits for the running one), but the project's own scripts must be safe too:

- `prepare` writes only into `CONNECTO_JOB_DIR` (with `CONNECTO_JOB_ID` and `CONNECTO_WORK_TYPE` in its environment), never to fixed paths such as `tmp/` or `out.mp4` or a shared cache it does not lock.
- `setup` must be safe while other jobs' prepares and agents run.
- The instructions should tell the agent what in the project it may change; a work type that edits project files should stay at one job at a time.
- Rate limits, quotas, memory or GPU (video work), and the database's connection limit must allow that many jobs.

These commands run with the user's rights, outside agent permissions. Make the scripts they name part of the project, keep them small, and tell the user what they do.

To change a type, publish it again under the same name. `connecto works show summary` prints what is published. `connecto works setup summary` runs its setup again.

### When a job's agent is stuck

A job's agent always ends with the JSON the schema describes. If it hits a problem or misses information about its job, it can ask the user one question at a time: `connecto works ask '<question>'` (or the question on stdin), then it ends its turn without a final answer. The user sees the question in Connecto with the project, work type, database and job, and answers it or chooses No answer. A `[connecto notice]` then wakes the agent; `connecto works answer` prints the reply, and the agent continues and ends with its JSON. Connecto adds these instructions to every job's prompt.

- The 30 minutes of the job include the wait. A question waits for an answer until 5 minutes before the job's deadline; then the agent is told no answer came and has those minutes to end with its JSON. A question cannot be asked in those last 5 minutes.
- So the JSON can still say what went wrong, give the schema a way to report a failure or a missing piece of data (for example an optional `error` field), and say in the instructions when to use it.
- Jobs that run unattended should not depend on answers: a job whose question nobody sees ends with the failure result its JSON gives.
- Only the job's own agent can ask or read the answer; you, setting it up, cannot.

## 3. Test it end to end

```
connecto works test summary --prompt 'the README in this project' --cli claude [--db dev]
connecto works job <id> [--db dev]
```

`test` queues a real job and prints its id. Connecto claims it within a few seconds while the project is open and nothing is paused, and runs it in a terminal named `agentic_works_<id>_…`; the job fails if that agent needs an approval, so the project's launch mode must not prompt. A job can take minutes: check `job <id>` once in a while, not in a tight loop. `status` is `done_from_agent` with `result` holding the JSON, or `failed`/`timeout` with a `reason`.

When it works, give the user the SQL their own programs use, in each database:

```sql
INSERT INTO connecto_works.connecto_jobs (project_name, work_type, prompt)
VALUES ('<project name from `works show`>', 'summary', '...') RETURNING id;
SELECT status, result, reason FROM connecto_works.connecto_jobs WHERE id = <id>;
```

A job without `session_cli` (or with `agy` or `antigravity`) runs on Antigravity. When the project cannot use Antigravity (not installed, signed out, deactivated in Settings > Agents, or off in the project's Tools), it runs on the first usable of: Codex with `gpt-6.1-terra`, Claude with Haiku, Pi, OpenCode (these two with their own default model). `works parallel` shows which agent runs each job; with none usable the job fails with a reason listing each one tried. A job that names `claude`, `codex`, `pi` or `opencode` always runs on that agent.

That order is the project's **Agent for jobs** set to Automatic (the default). The user can instead choose one CLI the project's Tools allow, and its model, for the jobs that name no agent: in Terminal → Agentic Works… next to Jobs at once, or `connecto works agent claude --model sonnet` (no `--model`: the CLI's default model; a model must be one `connecto works agent` lists for that CLI; `connecto works agent auto` goes back; Bypass only, like `parallel`; `connecto works agent` alone shows it and the CLIs and models offered, also under Pause). A change holds for jobs admitted after it; a job already admitted keeps the agent it was admitted with. Those jobs then run on that CLI and model (the job's own model and effort were meant for Antigravity and are dropped, except effort when Antigravity itself is chosen), with no stand-in: when it cannot run them, they fail saying why. A job that names its agent keeps it, and its own model.
