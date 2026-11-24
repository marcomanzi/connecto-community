# 0028: Calls in Connecto Chat use the device's own speech, and the project's agent answers

Status: Accepted, first slice built: `connecto-flutter-app/packages/connecto_voice`
(Swift, iOS and macOS 26 or later) and `connecto-flutter-app/lib/call.dart`.
Turn-taking is tested with a fake voice; recognition, echo cancellation and
speech have not been tried on a device yet.

## Context

People want to talk to a project's agent hands-free, like a phone call. Two
shapes were possible:

- **A realtime speech-to-speech model** (OpenAI Realtime, Gemini Live). These
  give sub-second turns, but they answer from their own model, not the
  project's agent. The agent would only be reachable as a tool that model
  calls. They also need a provider key on the phone. The app has no secret
  store, and the vault (ADR 0027) never hands a value to a client. They are
  paid per audio minute as well, and audio leaves the device.
- **The device's speech around the agent the project already runs.** Speech
  recognition sends each turn as an ordinary chat message (the existing `send`
  op), and speech synthesis reads the reply aloud. On Apple platforms all of
  it runs on the device.

The agent's turns take seconds to minutes. A call with it is a hands-free
conversation, not a sub-second phone call, whichever voice is in front.

## Decision

1. **On-device pipeline, agent as the brain.** `SpeechAnalyzer` with
   `SpeechTranscriber` (`DictationTranscriber` where that is unavailable)
   produces the text, the host's `send` op delivers it, and the `chat` op's
   reply is spoken by `AVSpeechSynthesizer`. The Rust peer and the host are
   unchanged. There are no keys, no new network paths and no per-minute cost.
2. **One voice-processing engine.** Synthesized speech is written to buffers
   (`AVSpeechSynthesizer.write`) and played through an `AVAudioPlayerNode` on
   the same `AVAudioEngine` whose input has voice processing on. Echo
   cancellation then removes the reply from the microphone, so the person can
   talk over it (barge-in). The app also ignores recognized words that are
   part of what is being spoken, and for two seconds after. It needs two
   words before it stops speech. A route change (a headset in or out) taps
   the microphone again. An interruption (a phone call) ends the call.
3. **End of turn by silence** (1.1 s after the last new words), decided
   in Dart, so it is testable. Each result carries its audio range. Only a
   result that adds words and reaches further into the audio restarts the
   silence: the recognizer finalizing or re-punctuating a long utterance
   does not hold the turn open. A late final for audio already sent is
   dropped. When the turn is sent, a short two-note cue plays through the
   echo-cancelled output, so the wait is not silent. The reply is looked for only among
   messages added after the history seen at send, so an identical earlier
   prompt is never answered from its old reply. Spoken commands: "stop" while the agent works
   sends the existing `stop` op, and "hang up" ends the call. Nothing else is
   sent while the agent works. Choice dialogs are not answered by voice;
   the call says a choice is waiting on screen. Text questions are answered
   like any message.
4. **Foundation Models only condense.** The on-device model (about 3B
   parameters, a 4K to 8K token window shared by prompt and answer) is weak at
   world knowledge and code. That rules it out as the brain. It does turn a
   long markdown reply into two or three spoken sentences well. Replies of
   320 characters or less after markdown is stripped are spoken as they are.
   Longer ones are condensed when `SystemLanguageModel.default.isAvailable`.
   Otherwise the call speaks the first two sentences and "The rest is in the
   chat." The reply is cut to about half the model's `contextSize`. A
   condensing session is prewarmed when the call starts and after each use.
   The agent's last message usually lands before its turn is marked over,
   so a reply seen while the agent still runs is condensed then, and used
   if it has not changed when the turn ends.
5. **Polling the chat.** The `chat` op reads the history the peer already
   holds, with no network round trip, so it is read every 200 ms while a
   reply is awaited (and once right after the send), and every 750 ms
   otherwise.
6. **The person picks the call language.** The call page lists every
   locale either transcriber supports, by name, with whether its model is
   on the device. The choice is kept in `UserDefaults` and used by later
   calls. Picking one between turns restarts recognition in it. A missing
   model is downloaded through `AssetInventory`, with progress shown. When
   the app already holds `maximumReservedLocales`, the oldest is released.
   Speech uses the best installed voice for that language. Better voices
   are downloaded by the person in Settings, since apps cannot fetch them.
   The language also drives everything said: the on-device intake and
   condenser are told to answer in it, the agent is asked to write in it,
   and the fixed phrases (`CallWords`) exist in English and Italian, with
   English for any other language. Spoken commands and agreement words are
   understood in both ("stop", "basta", "fermati"; "hang up", "riattacca",
   "chiudi la chiamata"; "sì, procedi", "va bene").
7. **Apple 26 or later only.** The call button shows only where the plugin
   reports support. Other platforms and older OS versions keep the chat as it
   is.

## Update: a voice session is one task

A call is now a voice session for one task, separate from the project's app
session:

1. **A conversation on the device.** What the person says goes to
   Foundation Models first, not to the agent. The model holds a short,
   natural conversation in the call language, one or two sentences a turn,
   one question at a time. It never proposes a task: it cannot see the code.
   Its answer (`IntakeAnswer`, guided generation) is `next` (talk or check)
   then `say`; `next` comes first, so a check is acted on before the rest is
   written (the stream is cut there).
2. **Checked with the project, on hold.** Once the request is nearly clear,
   or the next question needs the code, the call says "Okay, checking"
   ("Ok, controllo") and goes on hold: a soft two-note tone every few
   seconds, skipped while anything is said. The app session is sent
   `validationPrompt`: check the request against the code, change nothing,
   start nothing, and answer in a strict form, either `TASK` with Title,
   Goal, Scope and Done, or `QUESTIONS` with at most two, written in the
   call language. Since 2026-11-23 it reads as far as it needs to judge
   well, and it may hand a deeper check to a read-only helper terminal
   (`analysisHelperRules`, as ADR 0034's `ask_project`). In that case its
   turn ends on a `Checking.` note. The call keeps waiting for the answer
   that follows, and reads it past the `[connecto notice]` prompt that
   wakes the agent (`repliesAfter(throughHandoffs:)`). Meanwhile it says it
   is still working, for up to
   `helperCheckLimit` (10 minutes). A request only to find out or check
   something is answered in plain words, never proposed as a task.
   Questions are spoken and the conversation goes on. A task
   is proposed only when its goal, scope and how to tell it is done are all
   given (`parseValidation`, which keeps the values as written); the call
   says it in the agent's own words (its `Say:` line, a natural paraphrase
   ending with whether to start; the fields only when it gives none) and
   asks to start. The task that starts keeps the fields exactly (ADR 0034). Anything
   said while on hold is noted and the request is checked again once the
   answer comes, since that answer predates it. "Stop" calls the check
   off at once: its answer, even one already in, is dropped. The host stops
   only a running agent, so the stop is sent once the check shows as
   running and retried by the polls for up to 20 s, to that session, only
   while that check's prompt is the person's latest message there, and
   only until it answers; a check whose send failed is not stopped. When its answer was already in, nothing
   is stopped. A yes counts only for a
   proposal that was there when the person began saying it.
   Without the on-device model every turn is checked with the project.
3. **Hand-off.** The call page shows the proposed task with a Start task
   button. A turn that is only a yes ("yes", "go ahead", "sì, procedi", "va
   bene") starts it too; anything else ("start later", "sì, ma aspetta")
   drops the proposal and goes back into the conversation. On agreement the
   call says the work is starting, and the app session is sent the task with
   how to run it (`taskPrompt`). It doesn't do the work itself: it starts a
   Claude terminal on Opus for it, then a Codex terminal to review, and
   reports back in the call language. Its final report begins with the
   English words "Task complete.". The desktop app is unchanged; only the
   messages are. Since ADR 0029 the task goes onto the project's task
   board: it carries an id, waits in the queue if another task runs (the
   agent answers "Queued [id]." and the voice session ends), and is run as
   the project's `howToRun` says (by default Opus to code, Codex to review).
4. **Until complete.** After the hand-off the call stays on hold. Turns go
   to the app session as before. Its replies are spoken, including ones it
   sends on its own when its terminals finish. A message that begins "Task
   complete." for this task (with its id, ADR 0029: another task's report
   does not count; even one that lands with an update in the same poll) is
   spoken, with the marker said in the call language, and ends the voice
   session. New task starts the
   next one.
5. **A new app session.** The chat's New session action closes the app
   session on the host, with the terminals it started, after a
   confirmation (the `clear` op). The agent chooser then offers a fresh one.

The settings (the gear) hold the relay, the call language and the
installed version.

## Consequences

- Privacy: audio stays on the device. Only the recognized text goes to the
  host, as a chat message the person could have typed. Condensing is on the
  device. Nothing logs what is heard or said.
- Latency of the conversation: every turn is acknowledged at once by the
  cue (or, for a check, the spoken hold phrase, said while the message goes
  out). The intake and condensing sessions are prewarmed in the call
  language when the call starts and after each use; answers are capped at
  80 tokens (intake) and 120 (condensing). What remains: the 1.1 s of
  silence that ends a turn, the on-device model's answer (a few hundred ms
  to about a second, spoken only once complete: `say` is not streamed to
  speech), and a check, which is a full agent turn on the project (seconds,
  more if it reads much code). The hold tone covers that wait.
- Latency once the task runs: about 1.1 s of silence to end a turn, plus the host round trip,
  plus the agent's own turn, plus up to 200 ms to notice it is over. Then
  condensing (often already done, else up to about a second), with first
  speech about 100 ms later: synthesis is played buffer by buffer. Not yet
  done: streaming the condensed text to speak its first sentence early. Long agent turns are acknowledged every
  30 s ("Still working.").
- The app adds microphone and speech-recognition usage strings. It adds the
  `audio` background mode on iOS, so a call goes on with the screen locked,
  and audio input on macOS.
- A realtime model can be added later behind the same `VoiceEngine` seam if
  sub-second chit-chat matters. Its key would have to be brokered by the host,
  and the vault forbids returning values.

## Update: turn-taking like a colleague

A fixed 1.1 s of silence cut people off mid-thought, two recognized words
were needed before speech stopped, and "aspetta un attimo, devo fare una
cosa" was taken as a request. Now, still all on the device:

1. **End of turn by how it sounds.** The plugin follows the level of the
   echo-cancelled microphone (20 ms frames, a slowly followed room floor,
   hysteresis) and emits `activity` when the person starts and stops. At each
   stop it runs Smart Turn v3.2 (Pipecat, BSD-2-Clause, 23 languages
   including Italian; bundled as a compiled Core ML model, about 17 MB, a few
   ms per run) on the last 8 s of the turn and emits `turn` with the
   probability that the turn is over and where the pause began, on the
   recognizer's clock. The app ends a turn that sounded finished after
   400 ms, gives one that sounded unfinished 2.8 s, and one that trails off
   on an open word ("e", "che", "però", "and", "the") 2.8 s too. Words that
   arrive late for audio before the pause are the recognizer catching up and
   keep the judgement. Without an estimate (model not loaded) the old 1.1 s
   applies.
2. **Talking over speech.** A voice over speech holds it (the player
   pauses); real words then stop it, one being enough once held. Listener
   words ("mh", "sì", "ok", "certo") let it go on after 1.2 s. "Aspetta" or
   "wait" alone stops it and the call just listens. Echo is now a run of the
   spoken words, not any substring.
3. **Going on with a turn.** When the person goes on while the on-device
   answer is still being worked out, or cuts it short within 2.5 s of its
   start, the answer is dropped and both parts go to the model as one turn.
4. **Pause.** "Aspetta un attimo che devo fare una cosa", "torno subito",
   "dammi un minuto", "give me a minute" (or a short "un attimo") pause the
   call: it says so once, the hold tone and "Still working" stop, replies and
   a completion report are kept. Talk in the room is ignored, except a whole
   sentence in the first 4 s, which means they went on. "Eccomi", "ci sono",
   "I'm back" (or the Resume button) end it: what was kept is said, else the
   last thing said, as a reminder; words after "eccomi" are a turn.

The thresholds are first guesses from synthetic audio; they need trying on
the phone with real voices and rooms.

## Update: patient turns for a slow agent

On the iPhone, calls still cut people off mid-sentence. The 400 ms after a
single estimate of 0.5 or more ended many thinking pauses. The person
speaking again did not cancel it: only a later recognizer partial could save
the turn. And what they said after a cutoff was dropped, or answered "still
working" and lost. The agent takes seconds to minutes to answer, so a second
more of waiting costs nothing, while a cutoff sends half an instruction. Now:

1. **Voice holds the turn.** The person's voice coming back (`activity`)
   while a turn waits to end re-arms the patient wait at once, well before
   any words. It does so at most three times without new words, so a noisy
   room still lets the turn end.
2. **Bands, slower.** An estimate of 0.8 or more ends the turn after 1.2 s.
   From 0.5 to 0.8, or with no estimate, the wait is 2 s. Under 0.5, or on an
   open word, it is 3 s. All of these are constructor parameters, for tuning
   on the phone.
3. **More open words.** "è", "non", "da", "su", "dei", "sono", "ho", the
   clitics, "questo", "quel" and fillers such as "eh", "beh", "mmm" in
   Italian. "Is", "are", "on", "at", "from", "my", "this" in English.
4. **Sending on purpose.** A turn that ends on "vai" or "fatto" ("go
   ahead", "done", "send") goes after 400 ms, without that word, unless the
   sentence needs it ("ho fatto", "when it's done"). A tap on the transcript
   or the status circle sends at once. While the silence runs, a bar shows it
   running out.
5. **The rest of a cut sentence is kept.** A result that begins in audio
   already sent, reaches well past it and has more words is the recognizer
   reporting the segment again. The words after the sent ones are kept as the
   next turn. In the conversation, that turn is joined to the one cut short
   as before.
6. **Follow-ups.** What is said while the agent works is answered "Noted"
   and sent to it as one message once its turn is over. A task report that
   comes first does not drop it.

None of this has been measured on a device yet: the next step is a small
corpus of Italian dictation with thinking pauses, and false cutoffs per ten
minutes.

## Update: the Mac's model for phones without one

Foundation Models needs Apple Intelligence hardware: an iPhone 14 has none,
so its calls asked the project from the first turn and cut long replies to
two sentences. Now clarify and condense go down a chain: Foundation Models
on the device, then the Mac's model through the peer link, then (later) a
small model on the phone, then the old behaviour. The prompts moved to Dart
(`lib/assistant.dart`), word for word, so every model answers alike; the
intake answer is JSON for models without guided generation.

- **Wire.** `Complete` (tag 41: messages, max tokens, temperature, and the
  peer's wait) on a stream of its own, answered by `Completed` (42). A host
  from before it answers `Denied(Protocol)`, read as "none to offer"; an
  older phone never asks.
- **Host.** It forwards to an OpenAI-compatible server on this machine only
  (LM Studio by default), set in `remote/assistant.json`
  (`{"model": "…", "url": "…"}`; off without a model). The address must be
  loopback: nothing is opened, and the host proxies nowhere else. Two at a
  time, each given no longer than its peer waits; the guard is asked (pause,
  audit `assistant`); reasoning is turned off and stripped.
- **Phone.** A deadline per request: 2.5 s to answer in the conversation,
  6 s to condense. The link's health (each answer's share of its deadline,
  smoothed, and failures) decides whether to ask at all: slow, skip 30 s; a
  failure, skip 15 s doubling to 5 min; none set up or too old, skip 10 min.
  A one-token warm-up at call start loads the model and is not timed.

## Update: a model on the phone, the last fallback

Offline, or on a link the health check skips, the chain's last model runs on
the phone: Qwen3.5 2B instruct (Q4_K_S GGUF, 1.3 GB) with llama.cpp on Metal,
through llama.swift pinned to 2.10442.0 (llama.cpp b10442, the last build
whose XCFramework still has an iOS Simulator slice).

- It is off until the person turns on "On-phone assistant (offline)" in
  Settings. Only then does it download, on Wi-Fi only (waiting for it), to
  Application Support, excluded from backup, checked by size and SHA-256. Not
  on first need: 1.3 GB over cellular in a call is a bad surprise, and a
  download started then would not help that call. A failed download waits for
  the person to turn it on again; off deletes it.
- It loads at call start when ready, unloads after 3 minutes unused and on
  memory warnings. One request at a time, with the deadline checked between
  tokens; it is given longer than the Mac (10 s to answer, 20 s to condense),
  being the last resort.
- The call shows the download as its notice; Settings shows progress, the
  Wi-Fi wait, errors and the size.

## Update: a natural voice that keeps up

On an iPhone 14 the natural voice paused mid-sentence (synthesis slower than
real time) and switched voices mid-call.

- **Measured.** Each natural utterance reports its speed (audio seconds per
  second of synthesis) and time to first audio; "Show voice timings" in
  Settings shows them on the call screen, with turn sent → reply ready →
  first sound.
- **Faster.** A PocketTTS session is made ahead of each utterance, so its
  voice prefill is off the critical path. The first call per language
  measures GPU fp16, GPU int8 and Neural Engine placement (the Mimi decoder
  stays on the CPU), keeps the fastest and remembers it; each engine has its
  own folder and the losers' language packs are deleted. Under 1.1× real
  time the device uses the system voice, and Settings says so. The on-phone
  model gives way while speech is being computed.
- **No stutter.** Audio is held back before it plays, by the measured speed
  and the longest the text could last; past 1.5 s, or below real time, the
  reply goes sentence by sentence and pauses only between sentences.
- **Voices.** Settings offers the language's own voice, Vera, Anna, Eve,
  Alba or the system voice, remembered per language. A call keeps the voice
  it began with.
- **Steadier.** Every language samples at temperature 0.3, the default
  Kyutai's shipped configs now use (kyutai-labs/pocket-tts#324), instead of
  the 0.7 FluidAudio still applies to all packs but English. Kyutai's
  Italian figures (fewer word errors, a slightly better quality score) are
  for italian_24l, not the 6-layer pack used here, and do not cover speed;
  neither has been heard or timed on a device yet. The fixed phrases are
  shorter ("Okay, checking", "Ok, controllo").

## Update: Jev judges whether a turn is finished, and whether it fits

Smart Turn hears how something is said, not what: "controlla i test nella
cartella native" said with a falling voice, then a pause, still ended the
turn half-way. And everything the microphone caught went to the agent, also
a word to someone in the room. When the Mac has a TypeSafe Jev key (the one
automations use, in Connecto's settings; the phone never holds it), the call
asks Jev, in one request, whether the words so far are a finished turn and
whether they fit the call. Jev only decides when to stop listening and
whether to ask first: the words go to the agent as before.

1. **A turn check over the peer link.** The phone sends `TurnCheck` (the
   transcript, the call's context and how long it waits) on a stream of its
   own, as it does a completion; the Mac answers `TurnChecked` with two
   percents (finished; fits, when a context came), or says it cannot judge
   (no key, or a host from before it answers `Denied(Protocol)`). The
   context is the project, the task being shaped or worked on, the call's
   conversation, the agent's open question and what was last said to the
   person, as whole lines, the latest that fit in 4 KB. A transcript over
   4 KB is not checked at all: neither is ever cut, because only the Mac
   redacts them and a cut could split a secret. The Mac redacts secrets as
   automations do and asks two `noul` questions of `jev-1.14.0` (one without
   a context), once (never again on 429 or 529), as a request that is over
   by the phone's wait (1.2 s from the phone, at most 2 s). It runs on the
   peer's stream, so the four turn-check slots bound the requests really in
   flight. The guard allows it as it does completions, so a pause refuses
   it. An answer that is not a probability, for either question, is a
   failed check.
2. **Asked at each pause, not while talking.** When the person's voice stops
   (`activity`), the words are left 150 ms to settle and Jev is asked once
   per distinct text. It is not asked while the call is asked to wait
   ("dammi un minuto": side talk and the way back), nor about a call
   command, and an answer about an earlier turn is dropped even for the same
   words. A turn sent with a word ("…, vai") is asked about at once, without
   that word, for the fit only: it is never held, and goes on time when the
   answer is late. Words the recognizer revises for the same audio are
   judged afresh without restarting the silence; when the old words' hold
   had already outlasted the silence, the revised turn ends on Jev's answer
   about it (held again if cut off, asked about if it does not fit), or at
   the latest when an answer could still come (about 1.65 s). Whenever the words are dropped (the turn sent, a
   language switch, the task started) Jev's verdicts and hold go with them.
3. **Cut off: the call keeps listening.** Under 0.3 the turn is taken for
   cut off, and the call listens on: the person may have stopped to do
   something else. Its silence running out does not end it; new words are
   judged again, a tap sends it, and muting pauses it. Call commands ("hang
   up", "stop", "aspetta", "dammi un minuto") are never held: Jev took
   "aspetta un attimo" for cut off, and the page does not show them a hold
   either. A safety bound of 2 minutes remains, so
   a call left open with half a sentence does not stay silent forever (the
   agent's reports wait while the person is mid-sentence). At the bound
   nothing is sent: the call says where the person stopped and asks whether
   to send it or go on. Estimates and voice holds never restart the bound;
   new words judged cut off again, and unmuting, do. The page counts down
   to it while it is the longer wait.
4. **Not for the call: the call asks first.** Under 0.4 for fits (said to
   someone in the room, or making no sense here), the turn ends as usual,
   but before anything is sent the call says what it heard and asks whether
   to send it. This holds for a tap to send and a word that sends too, when
   Jev's answer is in (without one they go as before); call commands are
   never asked about. A low fit comes before "cut off": words that are both
   are asked about as not for the call. The answer: a yes sends the words
   as they were (with the proposal they were said under); "no", "lascia
   stare" lets them go; more words that do not fit leave the question open
   and send nothing; anything else is what the person meant and goes in
   their place (after words only left half-way, it goes on with them). The
   question stands only once it was delivered: its playback finished, or
   was heard at least through the quoted words. An answer that comes while
   it still plays (said over it, or a tap) waits for how it went; a
   question the voice failed to say, or cut before then, asked nothing, and
   the words are let go. And it stands only while it is the last thing the
   call said: once the call says anything else (the agent's reply or question, an update), the
   words are let go and an answer is taken for what was said last. A
   language switch or the task starting lets them go too. The questions
   never say "yes" or "sì": the answer would be taken for their echo.
5. **Without a key nothing changes.** The first "cannot judge" turns the
   checks off for the call. A late or failed check leaves the wait Smart
   Turn and the open words gave and sends as before: a finished sentence
   goes as fast as it did.
6. **Muting pauses a turn.** Muting used to drop the words being said. Now
   they are kept and the silence stops; unmuting starts the patient wait
   again, so a muted pause never sends half a sentence, and the person goes
   on where they left off. Muted, the device feeds the recognizer no audio,
   so what it still reports is for words said before muting (a final after
   a partial, or words that come in late): it is kept for the turn, with no
   silence, no Jev and nothing sent until unmuted. This holds with or
   without a key.

Measured from this Mac against Jev on 16 Italian and English sentences,
finished and cut off, with and without punctuation, asked alone: every one
right at 0.3, cut-off ones at 0.04 to 0.07, finished ones at 0.58 to 0.92;
200 to 290 ms a request. With a context and both questions, on 15 turns in
three calls (an answer to the call's question, the same cut off, a word to
someone at home, nonsense, "yes", "stop", a follow-up), every fit right at
0.4: 0.97 to 0.99 for words meant for the call, 0.03 to 0.34 for the others;
cut-off ones at 0.07 to 0.08; 210 to 340 ms a request. The relay adds its
round trip on the phone, which has not been timed on a device yet.

## Update: Gemini Live, and proposals in natural words

ADR 0034 adds calls through Gemini Live, opt-in with the person's own key,
with the on-device call as the default and the fallback. It also changes
how every call proposes a task: in natural words (the project's agent
writes a `Say:` line for on-device calls; Gemini paraphrases on its own),
not the fields read out. The task that starts keeps the fields exactly.

## Update: the on-device call knows the project

The call reads the project's functional memory (ADR 0033, the
`functional_memory` op) when it starts, without waiting for it, with the
loader Gemini Live calls use (`lib/functional_memory.dart`).

- The intake's models get it in their instructions, each as much as fits:
  the Mac's model (through the relay) all of it, up to the same 48 KiB as
  Gemini (UTF-8 bytes: the peer link takes 64 KiB per message); the phone's own models only README.md and overview.md, at most
  3,000 characters. Foundation Models gets that excerpt in the prompt (its
  prewarmed session keeps its instructions) and cuts it again to a quarter
  of its context (`contextSize / 4` tokens at 3 characters each, as the
  condenser measures); the llama model gets it inside its system message,
  which stays under `PhoneModel.maxChars` so the answer format, last, is
  never cut off. The instructions say the memory is for informed
  questions: how the code is built is still for the project to check.
- The check (`validationPrompt`) tells the project's agent to start from
  `.functionalMemory/` and read the code only where it does not answer:
  cheaper and faster.
- No memory, or a host without the op: every prompt is exactly as before.

## Update: several tasks in one call (2026-11-23)

A call is no longer one task. Once the agent has answered a task's start
(its "Queued [id]." or a sentence that it started), the task is on the
board and the call goes back to the conversation: what the person says
next is a new request, checked and proposed as the first was, and a yes
starts it after the earlier ones. The call page lists the tasks it started
with where each stands.

- "Queued" no longer ends the call. The reports of every task the call
  started are said; the call ends on its own only when the last of them is
  complete or failed and the person has not taken up anything new since
  the last start. Otherwise the person hangs up.
- The next request starts a fresh conversation: the one that shaped the
  task before is not part of its check.
- What is said while a start is answered still goes to the agent as a
  follow-up. After it, a turn goes to the agent only when it asked: a
  blocked task's question, which `taskPrompt` has begin with
  `Question [id].` (any reply or message of its own, for a task of this
  call; the marker is not said), or the host's question in words. Other
  questions ("Anything else?") leave the turn to the conversation. A
  message Connecto put in the agent's terminal (`[connecto notice]`,
  `[connecto from …]`) is not taken for one the person typed.
- One reader (`_readHistory`) reads the history once, through one cursor,
  and hands each message to what it is for: a report or `Question [id].`
  about a task of the call's is set aside to be said in order (another
  task's is dropped); what follows the outstanding prompt is its answer;
  what the agent says on its own (after Connecto woke it, or after an
  answered prompt) is said once the call has a task; what answers a
  message typed on screen or a called-off check is not the call's. The
  agent's last message is read only once its turn is over. Each prompt is
  registered as outstanding before it goes out, with the task it is about
  (a start, or an answer to that task's question; none for a check), so a
  poll that sees it before the send returns still reads its answer. Only a
  report or question about that task acknowledges it; another task's, or
  anything during a check, is said on its own. A report about the task
  being started hands it off wherever it lands. A host question that
  comes with an acknowledgement is still said.
- A question the call says (a blocked task's, or the host's in words)
  comes before a pending proposal: the next turn answers it, and it stays
  the open question until its answer is actually sent. Saying it un-hears
  the proposal, so a later yes first has the proposal read back, and only
  a yes after that starts it. A blocked task's question un-hears the
  proposal as it begins to be said, and is the open question only once it
  was heard to the end; cut short or never heard, it is said again (up to
  three times) before anything answers it. A check that gets
  no answer of its own still ends on a stopped session, a host question or
  the timeouts.
- A start whose outcome is unknown is retried with the same board id.
