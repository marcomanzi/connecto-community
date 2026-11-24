# 0041: A job that asks for Antigravity runs on a stand-in when the project cannot use it

Status: Accepted (2026-11-24), built in
`crates/connecto-app/src/agentic_works/agents.rs` (`choose`, `STAND_INS`,
`availability`), `crates/connecto-app/src/agents_host.rs` (`JobAgents`),
`mod.rs` (`State::choose_agent`, the job's launch, its outcome),
`crates/connecto-app/src/main.rs` (the checker given to `Works::start`),
`crates/connecto-core/src/agentic_works.rs` (`RunningJob::agent`), the
Agentic Works view (`crates/connecto-ui/src/agentic_works.rs`), `connecto
works parallel`, and the agentic-works skill. Tested with unit tests in
`agents.rs`, `a_jobs_agents_come_from_the_saved_tools_and_finished_checks`
(`agents_host.rs`), `a_failed_jobs_reason_says_how_far_its_stand_in_came`
and the PostgreSQL integration test
`without_antigravity_a_job_runs_on_the_first_stand_in` and
`a_held_agent_look_holds_neither_an_earlier_agent_nor_a_cancel`, with
`a_jobs_codex_is_signed_in_as_its_projects_terminals_would_be`
(`agents_host.rs`) and `a_pi_host_being_started_holds_no_other_project_up`
(`model_providers.rs`), `many_claims_share_one_look_within_a_bound`
(`agents.rs`) and `a_jobs_claude_with_a_broken_provider_is_unusable_with_its_reason`
(`agents_host.rs`), `the_settings_are_read_fresh_though_the_look_is_kept`,
`a_look_that_panics_is_an_error_not_a_retry` (`agents.rs`) and
`a_change_of_tools_holds_for_the_next_job_with_looks_kept` and
`a_switch_of_profile_is_not_answered_by_a_kept_look`,
`a_look_runs_in_the_profile_it_is_given` (`agents_host.rs`) and
`a_look_is_kept_under_the_profile_it_ran_in` (`agents.rs`). Not yet tried by hand
in the app.

## Context

A job's `session_cli` names the agent that runs it. Blank, `agy` and
`antigravity` meant Antigravity, and `agy` is the queue column's default
(ADR 0032), so a program that inserts only `project_name`, `work_type` and
`prompt` gets Antigravity. On a computer or in a project where Antigravity
cannot run, every such job failed. The user wanted those jobs to run on
another agent instead, in a fixed order.

## Decision

1. **Which jobs.** A job whose `session_cli` is blank, `agy` or
   `antigravity` asks for the default agent. `agy` counts as asking for the
   default because it is the column's default: a job that named no agent
   cannot be told from one that wrote `agy`. A job that names `claude`,
   `codex`, `pi` or `opencode` runs on that agent exactly as before, with its
   own model and effort, and fails as before when it cannot run there: the
   producer chose it, and a silent switch would hide that.

2. **What "the project cannot use it" means.** The same as the new-terminal
   chooser of that project, plus two things a job cannot get past without the
   user. An agent is usable when it is:
   - allowed by the project's Tools, read from its saved settings file (the
     packages manager saves it before its view has it, and has not looked at
     a project just opened, whose view would say every tool);
   - not switched off in Settings > Agents;
   - installed and offered (a supported version), as Settings > Agents' row
     says once its background check is over;
   - for Pi, with a model provider that works for the project (a project's
     own providers host is started within the look when it is not running
     yet, its keys read in the look's counted blocking task);
   - signed in as a launch in that project checks it: Claude's providers as
     its launch asks them (`ModelProviders::claude`): through TogetherLink no
     login is needed, and a configuration that does not work (TogetherLink
     missing, the secret store unreadable) makes Claude unusable with that
     reason, so the choice goes on to Pi and OpenCode; else the tool's status
     command in the profile the project's terminals use, its own when
     Isolated (`check_auth`). Settings > Agents' sign-in is the system profile's and
     is not used.

   The candidates are looked at one at a time, in order, each on a blocking
   thread, and only until one is usable: an agent after the chosen one
   (Pi's providers, a slow sign-in check) is never looked at. Looks are
   shared by every job: one per project and agent at a time, however many
   jobs ask; a result that is not pending is kept 30 seconds; at most 8 looks
   run at once, a look counting until its work has ended, not until the jobs
   waiting for it stop waiting. Only the look (Pi's providers, the sign-in)
   is kept: the settings (the project's Tools, Settings > Agents, the agent's
   row) are read again at every claim, so a job claimed after a change of
   them never uses a look from before it. A look is kept per profile too:
   the profile the agent is run in (the system's or the project's own, as
   its settings say) and whose model providers it gets are part of its key,
   so a switch of either is looked at anew. The profile is resolved once per
   ask and given to the look (`check_auth_as` runs the status command in it),
   so the answer is always kept under the profile it was found in, even when
   the settings change while the look waits for a place. A look that panics fails the job
   ("its check failed"); it is not tried again at once. A project's Pi
   host reads its keys from the secret store with no lock held that another
   project or a look at the running hosts needs.

3. **The order.** Antigravity with the job's model and effort; otherwise the
   first usable of:
   1. Codex with GPT Terra, `gpt-6.1-terra` (the id Codex 0.167 lists in its
      models cache, and accepts: checked with `codex exec -m gpt-6.1-terra`);
   2. Claude Code with Haiku (`haiku`);
   3. Pi with the model it is configured with (no model passed);
   4. OpenCode with the model it is configured with (no model passed).

   A stand-in gets neither the job's model nor its effort: they were meant
   for Antigravity.

4. **When.** Per job, after its claim and before its prepare and setup run,
   so a job no agent can run ends at once. When the choice depends on
   something not known yet (an agent whose check is not over, Pi's providers
   still being asked) and no agent before it in the order is usable, the job
   waits, looking again every half second for up to 2 minutes (and never
   past the job's own deadline), then fails saying what was still unknown.
   A look that does not end is not waited for past that, nor past Pause,
   Stop or the job's database removed: the job ends at once. The checker is given to `Works::start`, and the agents'
   host starts before Agentic Works, so no job is ever chosen without it.

5. **Shown.** The running job says its agent and model in the Agentic Works
   view and in `connecto works parallel` ("Codex, model gpt-6.1-terra,
   standing in for Antigravity"), with how far it came: "selected" (prepare
   or setup may run), "dispatched to" (its terminal is made and starting: the
   program's checks, its login), "on" once the agent's program runs (the
   terminal past its start). The database's status line says it too when the
   job waits, is dispatched, runs and ends (until the next look at the queue
   replaces it). A stand-in's failure or timeout reason ends with "(Ran on …)"
   only once its program was seen to run; "(Dispatched to …; it had not
   started.)" when its terminal was seen to stop at its start (failed checks,
   a login); "(Dispatched to ….)" when it was only seen exited with no turn
   ended (it may have run between two looks) or its end was not seen; and
   "(Selected …; it had not started.)" before its terminal was made. The
   database's status when the job ends says the same, from the same
   evidence: "Ran job N on …", "Job N ended (Dispatched to …)", or "Job N
   ended before its agent started (…)". With no agent, the job fails
   with "No agent can run this job. Tried, in order: …", each agent with why
   it could not.

6. **A project's own agent for jobs** (added 2026-11-24). The order above
   is the project's "Agent for jobs" set to Automatic, the default. The user
   may instead choose one CLI among those the project's Tools allow, with a
   model from that CLI's list (the new-terminal chooser's: the tool's own
   list, else `ToolKind::suggested_models`; Pi's from the providers its
   terminals there get, a project's own only once their host runs: a view
   never starts one) or the CLI's default model, which every CLI has, Pi and
   OpenCode with no list too. A model not in the CLI's list is refused and
   the setting stays as it was. It is set in the Agentic Works sheet next to
   Jobs at once (shown before any database is linked, so jobs already
   waiting in a queue being linked are claimed on it), or with `connecto works agent <cli> [--model M]` /
   `connecto works agent auto` (Bypass only, like `parallel`; alone it shows
   the setting and what is offered), and kept per project in the store's
   `settings` table (`agent`, `agent_model`; NULL is Automatic and the
   default model). It is read at each job's admission, under the claims'
   lock with the number of jobs at once, and kept with the running job,
   whose agent is chosen with it; a change takes that lock for writing. So
   a job admitted before a change keeps the setting it was admitted with,
   and every job admitted after gets the new one. `connecto works agent`
   alone may be run under Pause (it only reads).
   - It applies only to jobs that ask for the default agent (blank, `agy`,
     `antigravity`). A job that names its agent keeps it and its own model
     and effort.
   - For those jobs the project's choice wins, model included: the job's
     model and effort were meant for Antigravity and are dropped (effort is
     kept only when Antigravity itself is chosen).
   - The chosen CLI is asked about once, through the same shared looks.
     Usable: the job runs there. Pending: the job waits as above. Unusable:
     the job fails at once with "This project runs its jobs on Claude, model
     sonnet (Agent for jobs, …), which cannot run this job: <why>…", never
     on a stand-in, so a project's choice is never silently overridden.
   - Shown like a stand-in: "Claude, model sonnet, the project's agent for
     jobs", in the view, `works parallel`, the status line and the failure
     reason's "(Ran on …)".
   Built in `agents::choose` (`JobAgent`), `Store::job_agent`,
   `State::job_agent_choices`, `WorksAction::JobAgent`, `works agent`, and the
   sheet's `job_agent_row`. Tested with `a_projects_chosen_agent_runs_its_unnamed_jobs`
   (the regression, failing without the change),
   `a_named_job_ignores_the_projects_agent`,
   `an_unusable_chosen_agent_fails_the_job_without_a_stand_in` (`agents.rs`),
   `the_agent_for_jobs_is_saved_per_project`,
   `works_agent_sets_and_shows_the_agent_for_jobs`,
   `a_job_keeps_the_agent_for_jobs_it_was_admitted_with` (`tests.rs`),
   `a_projects_own_pi_models_are_offered_for_its_jobs_once_loaded`
   (`agents_host.rs`), `pause_allows_only_reading_and_a_project_is_needed`
   (`connecto-session/src/bridge/works.rs`) and
   `the_agent_for_jobs_is_chosen_with_its_model_in_the_sheet`
   (`connecto-ui/src/agentic_works.rs`).

## Consequences

- A project without Antigravity runs default jobs with no change to the
  producer. Results may differ by agent; a producer that needs one agent
  names it.
- The queue's rows do not record the agent of a job that succeeded: the
  queue's columns are fixed by its validation, and `session_cli`/`model` are
  the producer's request, which a retry must see unchanged. The view and the
  status show it while it runs.
- The Terra id is fixed in the code, in one place shared with the model
  chooser (`connecto_core::terminals::CODEX_TERRA`). When Codex lists a newer
  Terra, that constant is updated.
