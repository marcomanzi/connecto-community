# 0040: An Agentic Works job can ask the user a question

Status: Accepted (2026-11-24), built in
`crates/connecto-core/src/agentic_works.rs` (`JobQuestion`,
`WorksView::questions`, `WorksControl::questions` and `answer`),
`crates/connecto-app/src/agentic_works/questions.rs` (each job's question
and reply), `mod.rs` (`ASK_NOTE` in the job's prompt, `State::ask`,
`job_answer`, the job's loop), `cli.rs` (`connecto works ask` and
`answer`), `crates/connecto-hook/src/client.rs` (the question on stdin),
`crates/connecto-ui/src/job_question.rs` and `app.rs` (the dialog),
`crates/connecto-session/src/relay.rs` (numbered replies), and the
agentic-works skill. Tested with unit tests and the PostgreSQL integration
test `a_job_missing_data_asks_the_user_and_ends_with_the_answer`, and the
window test `a_jobs_question_holds_the_window_until_answered`,
`an_answer_given_while_the_asking_turn_is_on_counts_only_once_read` and
`reading_the_view_while_a_job_asks_never_deadlocks`,
`a_reply_kept_while_background_work_runs_does_not_end_the_job` and
`a_single_ask_before_the_terminal_is_journalled_is_heard` and
`interim_json_then_a_silent_final_stop_fails_the_job`. Not yet
tried by hand in the app.

## Context

ADR 0032 assumed a job always has what it needs: its agent gets the
instructions, the prompt and the prepared JSON, and its first turn's final
reply must be the JSON the schema describes. An agent that hits a problem
or misses a piece of data can only guess or fail, and a job whose agent
asks the user anything (a permission, a question on screen) fails at once
("The provider requires user input").

What Connecto had for an agent to ask the user and get an answer back:

- `connecto secret request`: a dialog over the window, answered by the
  user; the bridge then tells the asking terminal with a `[connecto
  notice]` (`AskReply`), typed as its next prompt when it is idle, or kept
  in its inbox while it works. A notice holds 120 bytes at most.
- `connecto send --ask` between agents, answered with `connecto result`.
- Questions on a terminal's screen (`connecto_core::question`) and a
  blocked task's question in the app session's chat, which a voice call on
  the phone lists and answers (ADR 0034). Neither is a notification: the
  phone sees them only during a call, and they belong to a terminal's
  screen or the app session, not to a job.
- Siri requests notify the phone through the push gateway, but only for a
  Siri request's own outcome.

There is no system notification on the Mac and no push to the phone for
anything else.

## Decision

1. **The prompt says it.** Every job's prompt, after the schema, says: if
   the agent hits a problem or misses information about this job that it
   cannot find itself, it may ask the user one question at a time with
   `connecto works ask '<question>'` (or on stdin), then end its turn
   without a final answer, not wait or poll; a `[connecto notice]` wakes it
   when the reply is in, `connecto works answer` prints the reply, and it
   continues; however the job goes, it ends with the final JSON and nothing
   else.
2. **Only the job's own agent asks.** `ask` and `answer` are for the
   terminal the journal names for a running job (not a terminal it
   started, nor the user's), so the question is always that job's. The
   job's terminal is journalled just after its agent starts, so an agent
   that asks at once may come first: a terminal Connecto made for a job
   waits for the journal to name it, 10 seconds at most (`ATTACH_WAIT`),
   before `ask` or `answer` is refused; a terminal a job's agent started
   waits too, and is then refused. The
   question and its reply are bound to the terminal's attempt as the bridge
   found the asker in it (`WorksCaller::attempt`): a restart of that
   terminal in place keeps its id but reads nothing. One question at a time
   per job; asking again drops the last reply, so `answer` never gives an
   earlier question's reply for the new one. Up to 4000 characters, the
   answer up to 16000.
3. **The user sees it as the secret requests are seen.** The question is
   shown in a dialog over the window, above everything until answered (a
   secret request waiting goes first): the project, the job, its work type
   and database, the terminal, when it was asked and by when it must be
   answered, and the agent's words. The user types an answer and sends it,
   or chooses No answer. Agentic Works wakes the window when a question
   comes or goes. The project's `WorksView` carries its questions too, and
   the Agentic Works notice says a job asked. The phone has no path for
   this (see Context): it gets nothing in this decision.
4. **The reply reaches that job's session through the bridge.** The answer
   (or the words saying none will come) is kept with the job. The job's
   own loop hands it over only when its agent is idle (turn ended, no
   background work): it calls the `AskReply` the bridge gave with the
   `ask` command, so the bridge types a `[connecto notice]` as the agent's
   next prompt. The notice only says the reply is in (it would not hold an
   answer); `connecto works answer` prints it, as `connecto result` does
   for a request. The hand-over goes by the terminal list's view, which may
   lag (a turn under way may still show idle): it decides only when the
   notice is sent, never which turn ends the job.
5. **The job's end is bound to the agent reading the reply.** When the
   agent first runs `works answer`, Connecto records, from the terminal's
   own counters (the number of the last reply it kept,
   `Relay::terminal_reply_numbered`, counted under the same lock as the
   reply, and the turns it ended, `Relay::terminal_finished_turns`), where
   the terminal stood: the turn that read is under way. Once a job has
   asked, no turn ends it, valid JSON included (a guess, even one the turn
   that asked ends with after the user already answered), until that read;
   then only the reply of a turn the terminal counted as ended after the
   read is the job's answer. The terminal binds each counted turn to the
   reply it ended with, under one lock with the count
   (`Relay::terminal_last_turn`): what the turn's last stop kept, or none
   when that stop kept none; a turn's beginning clears what an earlier stop
   kept, and so does a stop that failed (`StopFailure`, `Out::TurnFailed`,
   even with another prompt's reply kept while the turn stayed on). A turn is counted only once its background work is over, so a
   reply kept at a `Stop` that lists running tasks (interim JSON) is the
   turn's only if no later stop of it replaced it, even with nothing,
   whatever the list's view shows. A counted turn after the read that ended
   with no reply of its own (or one kept before the read) fails the job with
   "Provider returned no final assistant text." An agent that never reads
   its reply ends at the job's deadline (`timeout`). With no question,
   nothing changes: a turn that ends ends the job, valid JSON or not.
6. **The 30 minutes include the wait.** The time limit is not stopped
   while a job waits: the queue itself expires a job `doing` for 30 minutes
   (ADR 0032), and another Connecto sharing the queue would time it out, so
   the job's own deadline cannot move. A question waits for an answer until
   5 minutes before the job's deadline (`ANSWER_MARGIN`); one cannot be
   asked later than that.
7. **No answer still ends with JSON.** When the user chooses No answer, or
   the question's time is over, the agent is told no answer will come, not
   to ask again, and to end now with its final JSON, saying what was missing
   if the schema allows it: it has the last 5 minutes for that. A job whose
   agent still does not end in time times out as before.
8. **A question goes with its job.** When the job's run ends (its JSON, a
   failure, Pause, Stop, its database removed, its deadline), its question
   and reply are dropped and the job is closed to questions in one step,
   before its terminal is closed; an `ask` checks that the job runs and
   registers its question under the same lock, so none is kept for a run
   that has ended, however long closing the terminal takes. Lock order: the
   questions' lock, then the running jobs'; nothing takes the questions'
   while holding the running jobs' (the view takes each in a statement of
   its own). The mark goes
   once the job has left the running jobs. The dialog closes; while it is
   open the window is modal (no typing, no file dropped on a terminal). Nothing is kept on disk:
   a Connecto restart ends every running job (ADR 0032), and its question
   with it. Several jobs at once (ADR 0039) each have their own question;
   the dialog shows the oldest, and says how many more wait.

## Consequences

- A job that misses a piece of data can finish with the right result
  instead of a guess or a failure, if the user is at the Mac in time.
- Work types should give their schema a way to report a failure or a
  missing piece of data, since a job whose question is not answered still
  ends with JSON.
- A job that runs unattended loses up to 25 minutes waiting on a question
  nobody sees; its place in "jobs at once" stays taken meanwhile.
- The answer is the user's words given to an agent running with the
  project's launch mode; Connecto shows the question as the agent's own
  words, unchecked, as for secret requests.
- Phone notifications for questions are left for later: they would need
  a push path for something other than a Siri request.
