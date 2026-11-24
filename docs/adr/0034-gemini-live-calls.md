# 0034: Calls can go through Gemini Live, with the project's agent as a tool

Status: Accepted, first slice built: `connecto-flutter-app/lib/live_call.dart`,
the Gemini Live section in Settings, and `LiveAudio.swift` in
`packages/connecto_voice`. The protocol, tool calls and the confirmation
gate are tested with a fake socket; nothing has been tried against Google's
servers or on a device yet.

## Context

A call (ADR 0028) is a conversation on the device: Foundation Models (or the
Mac's model, or a model on the phone) asks questions, the project's agent
checks the request against the code, and a task is proposed and started on
a yes. Turns are slow by design: 1.2 to 3 s of silence ends one, and the
on-device model knows nothing of the project, so most real questions wait
for a check.

ADR 0028 set realtime speech-to-speech models aside: they answer from their
own model, need a provider key, and are paid per minute. Two things have
changed. Gemini 3.8 Live (`gemini-3.8-live`) runs tools asynchronously
(`NON_BLOCKING`, with the result spoken `WHEN_IDLE`), so it can keep talking
while the project's agent works for minutes. And the project now describes
what it does in its functional memory (ADR 0033), so a model that cannot
see the code can still talk about the features and ask informed questions.

Some people would pay for a call that feels like a phone call. Others will
not send their audio to Google. Both stay possible.

## Decision

1. **Opt-in, per device.** Settings has a Gemini Live section: an API key
   field, a switch, and a test. The key is kept in the Keychain (generic
   password, `connecto.gemini-live`, this device only, after first unlock;
   on macOS the data-protection keychain when the build is entitled to it,
   else the login keychain), never in preferences, never shown back, never
   logged. The switch is a preference and only works with a key. The test
   opens a real Live session and waits for `setupComplete`, so the key, the
   model and the quota are all tried. Without a key, or with the switch
   off, calls are the on-device calls of ADR 0028, unchanged.
2. **The phone talks to Google directly.** It opens the Live WebSocket
   (`v1beta` `BidiGenerateContent`, the key in the `key` query parameter as
   Google documents it). Every error text that could carry the URL passes
   through `redactKey`, and failures are told by cause, not by message
   ("Gemini refused the API key.", "Gemini's quota is used up for now.",
   "Gemini can't be reached."). The host is not involved and the vault
   (ADR 0027) stays as it is: the key is the person's, on their phone.
3. **Same audio path.** `LiveAudio` runs its own `AVAudioEngine` with voice
   processing on the input, as on-device calls do: echo cancellation removes
   Gemini's voice from the microphone, so the person can talk over it. The
   microphone goes out as 16 kHz 16-bit mono PCM; Gemini's 24 kHz PCM is
   played on a player node of the same engine. Server VAD ends turns, set
   patient (`END_SENSITIVITY_LOW`, 900 ms); a barge-in (`interrupted`)
   drops what is queued to play. Muting stops the audio and sends
   `audioStreamEnd`. A route change taps the microphone again; an
   interruption ends the call.
4. **What Gemini knows.** Its system instructions carry the project's
   functional memory (the host's `functional_memory` op, ADR 0033), each
   file as `## <path>` then its text, up to 48 KB. A host without the op, or
   a project without a memory, gives none, and Gemini is told to ask the
   project instead. The instructions say how to behave: a colleague on the
   phone, one question at a time, the call language unless the person
   speaks another, never making up how the code works.
5. **Three tools, all `NON_BLOCKING`, results `WHEN_IDLE`.**
   - `ask_project(question)`: the project's agent answers it after reading
     the code, as the on-device check does: change nothing, start no work
     (`askPrompt`, sent through the same `send`/`chat` ops). It needs no
     confirmation. Gemini says it is checking and keeps talking; the answer
     is spoken when Gemini is idle. A call Gemini cancels gets no late
     answer. The answer has no length cap: Gemini says what matters.
     (2026-11-23, the user's decision.) An analysis that needs more than a
     quick look goes to one read-only helper terminal the agent starts in
     its own project (`analysisHelperRules`). The helper may also ask
     another project (ADR 0035). A person asking to "launch an agent to
     check" something gets the same: an analysis, with no confirmation.
     Each ask carries a check id (`newCheckId`). The agent's turn then ends
     on a `Checking [id].` note, which is the tool's answer. From the end
     of that reply (`ProjectLink.ask`), the controller watches the chat for
     a later `Findings [id].` message and passes it to Gemini as a
     "Project update". Only an id still waited for counts, so findings
     without an id are never passed on. A check is waited for from before
     its ask goes out, so findings read before the reply still count. A
     task started meanwhile keeps the watch's place. A check Gemini calls
     off is dropped, before or after the handoff, and its findings are not
     passed on. Messages are routed
     by the prompt they answer: an answer to an ask prompt never becomes an
     update, and other messages do only while a task runs. Coding work
     still waits for `propose_task` and a yes.
   - `propose_task(title, goal, scope, done)`: shown on screen with a Start
     button, and read back by the phone itself (update 2026-11-23, below:
     Gemini no longer says it).
   - `start_task()`: starts the proposal through the existing start path
     (`taskPrompt`, the task board of ADR 0029), with the fields exactly as
     proposed. Gemini's arguments cannot change them.
6. **Starting needs the person's yes, heard by the device.** Gemini is
   told to start only on a clear yes, and the app does not take its word
   for it, nor its transcripts: Google sends input transcription
   independently of the other messages, so a late "yes" from before the
   proposal, or a "yes" whose ", but change the scope" comes later, cannot
   be placed. The answer is heard by the device's own recognizer
   (`SpeechAnalyzer`, as on-device calls use, on the same echo-cancelled
   microphone), whose results come in the microphone's order:
   - It starts listening only once the proposal was said. Superseded
     2026-11-23: no longer a turn of Gemini's, but the phone's own
     read-back of the frozen proposal, played out whole ("Answers and tasks
     are read back by the phone", below).
   - Each listening is a session with its own id. Whenever Gemini speaks
     again the session ends, and a fresh one starts once its audio played
     out; results of an ended session, even ones that come late, are
     dropped. The answer is what the person says after Gemini last
     stopped.
   - A recognizer that fails or ends on its own reports it; what it heard
     is dropped (it can no longer hear a correction), and only the Start
     button confirms that proposal.
   - `start_task` counts it only when it is a plain yes (`agrees`, the
     words on-device calls accept), nothing is still being recognized, and
     nothing new came for 0.7 s; it waits up to 3 s for that, since the
     answer can still be coming when the tool call arrives. "Yes" then
     "but change the scope" is not a yes.
   - Where the recognizer cannot run (no model for the language), a spoken
     yes never counts: Gemini is told to ask for a tap on Start.

   Otherwise `start_task` answers that nothing started and Gemini is to ask
   again. A tap on Start confirms too. After the wait the app checks again
   that the call is open, the call was not cancelled, the proposal is the
   same one, and no start (a tap meanwhile) is under way: one task is sent
   at most. The board id stays with the proposal, so a start whose outcome
   is unknown (the message went out, its answer was lost) is retried with
   the same id, which the agent recognizes (ADR 0029). Analysis never needs
   any of this.
7. **After the start.** The agent's own messages (its "Queued", "Task
   complete" and "Task failed" reports, ADR 0029) are read every few seconds
   and passed to Gemini as "Project update: …", to tell the person.
   Update 2026-11-23: a call can start several tasks, one after another.
   Once `start_task` answered, `propose_task` takes a new proposal (a new
   board id, the yes gate armed again for it); only while a start is under
   way is it refused. Gemini is told to ask whether there is anything else.
   The reports of every task started in the call are passed on until each
   is complete or failed; other tasks' reports are not. An outcome is taken
   in one place (`_reconcile`), whichever reader sees it first. Delivery to
   Gemini: `start_task` answers with its own task's messages and those
   about no task; everything else about the call's tasks goes through the
   report watcher. A message about the task that follows its own start
   prompt is claimed by whichever reader takes it first, so it reaches
   Gemini once. `start_task` waits for its own answer (a message about its
   task or about none), not just any reply: another task's report that
   comes first does not answer it. The watcher reads one poll at a time,
   across restarts of its timer.
8. **Long calls.** Session resumption and context-window compression are
   on. When the server closes the connection (after `goAway`, or a drop),
   the call reopens it with the last resumable handle, up to three times.
   A connection whose setup is not answered within 10 s counts as dropped,
   so a stalled socket never leaves the microphone open on "Connecting".
9. **Failures fall back to the device.** A bad key, no network, quota, or
   a session that cannot be resumed ends the Live call with the cause on
   screen and a button for the on-device call. That call says first that
   Gemini is not available ("Gemini non è disponibile, continuo sul
   telefono"), then greets as usual.

## Answers and tasks are read back by the phone (2026-11-23)

The terminals' questions are answered in the call: a permission (run a
command), a menu, a question that wants words, on any agent terminal the
phone is granted (the app session, a coder, a reviewer), and a blocked
task's "Question [id]." in the app session's chat. Since this types into
terminals, every step fails closed. And since a yes must be to what will
happen, task proposals are now read back the same way, by the phone, not
by Gemini (this supersedes "Gemini says a proposal in its own words",
below, for Gemini Live calls).

1. **The phone reads it back, not Gemini.** `propose_task` and
   `propose_answer` freeze the proposal, show it, and the phone says it
   with its own voice (`LiveAudio.say`: `AVSpeechSynthesizer`, on the same
   echo-cancelled player as Gemini): a task's title, goal, scope and done
   criterion; an answer's terminal, what it asks whole (its operation, for
   a permission; refused past 600 characters) and the exact choice or
   words. Gemini is told to stay silent and is not played over it. Only a
   read-back that played out whole counts: every piece of the synthesis
   converted (none skipped), nothing flushed it while synthesized or
   played, no newer one replaced it, its synthesis did not stall (30 s).
   Only then does the device's recognizer listen. Talked over, or followed
   by any of Gemini's speech (another task described, an unrelated
   question), it no longer counts. `start_task` and `confirm_answer` then
   act only on a plain yes heard after it; a plain no drops an answer;
   anything else needs a new read-back. A Live call has its own, narrow
   vocabulary (`liveYes`, `liveNo`): the whole answer must be one of a few
   unambiguous phrases in the call's language ("yes", "go ahead",
   "confirm"; "sì", "procedi", "confermo"; "no", "annulla"). Reactions that
   may only acknowledge what was read ("great", "ok", "ottimo", "va bene",
   "perfetto") are neither. A
   start that fails reads the same task (its board id kept, for a retry)
   back again. One thing waits for a yes at a time.
2. **The host reads and writes.** The phone mirrors only the app session,
   so two project asks were added on the host
   (`connecto_net::voice_answers`). `questions` lists the dialogs at each
   granted terminal's live bottom (`poll_capture` without `flush`: a
   synchronized redraw still held back is waited for, never read half
   drawn; soft-wrapped rows are joined into their line) and the chat's
   question. Each has its kind, its options with which are tap-only, what
   it asks (`prompt`), an id from its text (the selection and the whole
   field left out; selection marks dropped only on the choices' rows, so a
   command keeps every character) and an occurrence. For a dialog the
   occurrence is a token the host issues for that instance
   (`voice_answers::Dialogs`), bound to its exact operation and to the
   input and output generations the host last saw it at. Any change the
   host did not see ends it: other input (a key on the Mac, Connecto
   typing), a restart, or any output drawn since (the dialog could have
   gone and come back between two looks; a watcher looks every 100 ms
   while tokens are out). One answer claims it before its first key
   (re-checking the live screen); another is refused, and while it is
   claimed or submitted and still shown no other token is issued for it.
   Nothing drawn after the claim is taken for the instance until the
   answer writes a key, and then only the change that key was to draw
   (the next choice selected, the field holding exactly the words).
   Each write carries the input and output generations the answer last
   saw, and both are checked with the enqueue: the input count in the
   common input gate, the screen held (no output taken in) from the
   comparison through the enqueue. Enter spends it. For the chat the
   occurrence is the revision.
   `question-answer` names the terminal's wire id, the id, the occurrence,
   and an option or words. Every write (each arrow, the paste, Enter) is a
   conditional write (`TerminalSession::send_input_if`): the input
   generation is compared, the bytes enqueued and the count moved under
   the one input gate every writer of the session goes through (the
   window, peers, the relay, Connecto), never held back while Connecto
   types; and, for the agent's own input line, it holds exactly the
   expected text both as typed (the draft tracker) and as drawn. Drawn
   means proven: the whole text on the input box's first row and nothing
   typed on any of its other rows (Claude's and Codex's adapters read the
   box to its end; other tools' are not read that far, so nothing is
   proven there). A collapsed paste names a count, not the text, and
   proves nothing. So only what fits on one row of the input line is typed
   by voice at all: a longer text is refused before anything is typed.
   The agent's own protocol replies to the terminal do not count as input. A choice moves one arrow at a time, Enter only
   once it is selected; words only into a field known empty (only the
   whole placeholder is empty), Enter only once it shows exactly them;
   chat words only while the app session is between turns and its chat
   ends on that message at that revision, on an input line empty, Enter
   only on it holding exactly them.
3. **The peer's grants, through delivery.** Only terminals in the peer's
   latest list are read or written. Each write checks that terminal's
   grant with the peer's listing held through the write, and each piece of
   a list of questions is sent with the listing held from the check of
   every terminal it carries to the send.
4. **The call's own prompts are conditional too.** A task to start and a
   question to analyse go to the app session only through the host's
   `send-prompt` (`voice_answers::send_prompt`): only while it is between
   turns, asks nothing (no dialog, no text question) and its input line is
   empty, as typed and as drawn; Enter only on it holding exactly the
   prompt, nothing having come up meanwhile. A question that comes after
   the confirmation never gets the board prompt as its answer. A Mac from
   before that ask cannot make sure of it: the call does not type there.
   The call's own prompts (a task's board prompt, an `ask_project`
   question, the on-device call's turns) are several lines long: drawn
   wrapped or collapsed into a placeholder, they cannot be read back. What
   Enter submits is known by construction instead (decision of
   2026-11-23): (1) the app session is between turns with no dialog and no
   question, its input line empty as typed and as drawn, at input
   generation G and screen generation S; (2) the paste goes in only at G
   and S, moving the input to G+1; (3) Enter goes in only at exactly G+1
   (nobody else wrote since our own paste: no key on the Mac, no other
   answer, no Connecto typing), with the line holding exactly the prompt
   as the draft tracker follows it, after a fresh look found no dialog,
   no question and the chat unmoved, the screen held at that look through
   the enqueue (the paste's own redraw is allowed). The line was empty and
   only our paste went in, so it holds exactly the prompt, whether drawn
   in full, wrapped or collapsed. Accepted: the one case left is the agent
   itself putting an old draft back into its input line between the paste
   and Enter, with no dialog and no input. The strict read-back stays for
   answers to dialogs and to blocked tasks' questions, whose paste and
   Enter also carry the screen generation of the look just before each.
   "Empty", for any conditional write, is strict: the tool says so, nothing
   typed (not faint) on the box's first row (a line shaped like the hint
   but not drawn faint is someone's text), and every other row of the box
   read and empty. The box is read from its geometry, columns kept, never
   from glyphs typed text can contain: Claude's lies between its two rules
   (rows of `─` from the first column, nearly as wide as the screen, the
   top one perhaps naming the session), its first row starting with `❯`
   at the first column and the others indented; Codex's is its last row
   starting with `›` at the first column, the indented rows under it, down
   to its footer, found from the screen's bottom up: hint rows, the one
   status row naming the folder, and the blank row above it (a typed line
   shaped like the footer, with no blank row under it, is ambiguous). A
   footer so verified also makes the composer ready, with or without a `·`
   in its status row; a turn at work above it still reads `Busy`, and
   text typed on any later row of the box reads `Occupied` (for the relay's
   readiness too), even with its first row empty. A typed `❯`, `›` or `────` sits
   indented, so it is read as text; anything else (a marker or a rule
   where the box's own cannot be, no end found) is ambiguous and proves
   nothing. A refused conditional write leaves the draft tracker as it
   was, so the retry after a late redraw finds it whole.

   Pi's, OpenCode's and Antigravity's input boxes are not read beyond
   their first row. There a call's own prompt (which, before this work,
   went by the plain send) rests on the input generations alone: the
   draft tracker says the line is empty and the tool draws no draft and
   no turn at work (a `Busy` reading vetoes it, whatever the host's
   activity says), the paste at G, Enter at exactly G+1 with no dialog.
   Chat questions there are offered for an answer on screen, not by voice. A dialog's
   answer and a typed answer stay strict there: tap only.

   A task's "Question [id]." stays pending until a report of the agent's
   begins with that task's marker ("Task complete [id]", "Task failed
   [id]", "Queued [id]", or the Italian forms; a report that only mentions
   it does not), a newer question of that task takes its place, a prompt
   answers it by name, or a prompt right after it answers it (as before;
   one that says it is a new request does not). Its occurrence is the
   message that asked it, by its absolute place among everything the chat
   ever held (not its index in the kept history, which repeats once the
   oldest messages are dropped), so turns about other tasks leave it pending,
   listed and unchanged (a proposal for it is not dropped). Its answer is
   routed by name, "Answer to Question [<id>]: …": the phone puts the lead
   in the proposal, reads it back and sends it whole, and the host types
   only text that begins with that very task's lead.

   A call's prompt is not held back while a task's "Question [id]." waits
   in the chat: a new task or an `ask_project` question is a separate
   request by design (a call starts several tasks), and refusing would
   stop every new task while any task is blocked; the call already makes
   the person's next turn the question's answer once it was heard. Each
   such prompt says so itself instead ("This is a new task, id …" / "This
   question": "a new request, not an answer to any "Question [<id>]." you
   asked; such a question still waits for its own answer"), so the agent
   cannot take it for that answer.
5. **Known layouts only.** By voice: Claude's Bash permission with its
   command recovered between the dotted rules, Codex's command permission
   with its whole `$` command (up to its choices, found from the footer up
   and separated from it by a blank row: a command with numbered rows of
   its own is not cut there; an unclear boundary is tap-only), each with
   only the known one-shot answers;
   Codex's question form's numbered choices; Codex's and Pi's text fields
   with no choices. Choices must be read whole (the footer is only the
   dialog's last row), not wrapped, without broad words. Anything else, of
   any provider (OpenCode, Antigravity, Claude's other dialogs), is shown
   and tapped. The host checks this at the write, whatever the phone asked.
   A Mac from before these asks: shown, nothing by voice.
6. **Handles per occurrence.** Gemini names a question by a handle bound
   to the terminal and the occurrence. A question that goes away drops a
   proposed answer to it at once, card and all; back again, it is new.
   Both readers of the questions (the list tool and the watcher) go through
   one update: a new one interrupts what waits for a yes.

"Approving tasks by voice" is the propose/start flow; a task queued on the
board needs no approval of its own (ADR 0029 has no such status), so none
was added.

## Proposals are paraphrased, in both kinds of call

Reading Title, Goal, Scope and Done aloud with their labels sounds like a
form. Now:

- Gemini says a proposal in its own words. Superseded 2026-11-23 for
  Gemini Live calls: the phone reads the frozen proposal back itself
  (above). On-device calls are unchanged.
- On-device calls ask the project's agent, in `validationPrompt`, for a
  `Say:` line with the TASK form: how it would propose the task out loud,
  two or three natural sentences, ending by asking whether to start, never
  with the word "yes" or "sì" (the answer would be taken for its echo).
  `proposalSpeech` says that line, adding "Shall I start?" when it does not
  end with a question. An agent that gives none gets the fields read as
  before.
- Either way, the task that starts keeps the fields exactly as written.
  The paraphrase is never sent to the agent.

This updates ADR 0028, Update "a voice session is one task", point 2: the
call no longer reads out the goal, what changes and how to tell it is done.

## Consequences

- Audio of the call goes to Google, under the person's own key and Google's
  terms for it (on the free tier Google may use it to improve its
  products). Settings says audio goes to Google and is billed to the key.
  The transcript stays on the phone. The project's code does not go to
  Google; only the agent's answers, the functional memory and what the
  person says do.
- Cost: per audio minute, billed to the key. With proactive audio always on
  in 3.8, input is billed while the call listens.
- Latency: Gemini answers in about a second and keeps talking while the
  agent works. The agent's answers take as long as before.
- The on-device pipeline stays the default and the fallback, so a call
  never depends on Google.

## Not verified yet

- Answers by voice: the arrow-key navigation and the 40 ms read between
  keys have run only against fixtures and fakes, not against Claude's,
  Codex's or Pi's real dialogs on a Mac; nor has the phone's read-back
  (`AVSpeechSynthesizer` through the live player) on a device. The
  occurrence (rows above a dialog) assumes they stay still while it waits:
  a line that keeps changing above it would make every answer refused. OpenCode's
  and Antigravity's dialogs have no fixture yet and may not be recognised
  as questions.

- Nothing has run against Google's Live API: the message shapes follow
  Google's Live API reference and tools guide (read 2026-11-22). Two points
  there were unclear: `scheduling` is put inside the function response, as
  the tools guide's example has it; and whether a function response for a
  call made before a resumption is still accepted after it.
- The close codes and reasons for a bad key and for quota are not
  documented; the failures are told by matching their text.
- Echo cancellation with Gemini's voice, barge-in, and the confirmation
  gate with real speech (in Italian too) have not been tried on a phone:
  in particular how fast the on-device recognizer starts after the
  read-back, and whether its final result for a short "sì" comes within
  the 3 s a start waits.
- The `functional_memory` op is new (ADR 0033); a host from before it
  gives no memory.
