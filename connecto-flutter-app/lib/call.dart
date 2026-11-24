import 'dart:async';
import 'dart:math';

import 'package:connecto_voice/connecto_voice.dart';

import 'package:flutter/material.dart';

import 'assistant.dart';
import 'board.dart';
import 'functional_memory.dart';
import 'peer.dart';
import 'settings.dart';
import 'theme.dart';
import 'turn_judge.dart';
import 'widgets.dart';

/// Where a call stands, as the person hears it.
enum CallPhase {
  connecting,
  listening,

  /// Working out, on the device, what to say next.
  thinking,
  sending,

  /// Waiting on the project's agent: the call is on hold.
  working,
  speaking,
  ended,
}

/// The fixed phrases a call speaks, in its language.
class CallWords {
  const CallWords({
    required this.greet,
    required this.goAhead,
    required this.stillWorking,
    required this.stopping,
    required this.couldNotStop,
    required this.notReady,
    required this.choiceWaiting,
    required this.needsChoice,
    required this.agentAsks,
    required this.notThrough,
    required this.sessionStopped,
    required this.done,
    required this.noReply,
    required this.checking,
    required this.noted,
    required this.starting,
    required this.proposal,
    required this.scope,
    required this.doneWhen,
    required this.shallIStart,
    required this.taskComplete,
    required this.queued,
    required this.taskFailed,
    required this.restInChat,
    required this.codeInChat,
    required this.someCode,
    required this.aLink,
    required this.paused,
    required this.back,
    required this.recap,
    required this.forMe,
    required this.goOnOrSend,
    required this.leftOut,
  });
  final String greet, goAhead, stillWorking, stopping, couldNotStop;
  final String notReady, choiceWaiting, needsChoice, agentAsks, notThrough;
  final String sessionStopped, done, noReply, checking, noted, starting;
  final String proposal, scope, doneWhen, shallIStart, taskComplete, queued;
  final String taskFailed;
  final String restInChat, codeInChat, someCode, aLink;

  /// Said when the person asks to wait, when they are back, and before what
  /// was last said to them, as a reminder.
  final String paused, back, recap;

  /// Asked before sending words that did not seem meant for the call, and
  /// words left half-way for a long while; `{said}` is what was heard. And
  /// said when the person lets them go. The questions never say "yes" or
  /// "sì" themselves: the answer would be taken for their echo.
  final String forMe, goOnOrSend, leftOut;

  static const en = CallWords(
    greet: 'What would you like done?',
    goAhead: 'Go ahead.',
    stillWorking: 'Still working.',
    stopping: 'Okay, stopping it.',
    couldNotStop: "I couldn't stop it.",
    notReady: "The project's session isn't ready. Check it on screen.",
    choiceWaiting: 'The agent is waiting for a choice on screen.',
    needsChoice: 'It needs a choice on screen.',
    agentAsks: 'The agent asks:',
    notThrough: "That didn't go through.",
    sessionStopped: 'The session stopped.',
    done: 'Done.',
    noReply: "I didn't hear back. Check the chat.",
    checking: 'Okay, checking.',
    noted: "Got it, I'll pass it on.",
    starting: "Great, I'm on it. I'll keep you posted.",
    proposal: "Here's the task:",
    scope: 'What changes:',
    doneWhen: "It's done when:",
    shallIStart: 'Shall I start?',
    taskComplete: 'Task complete.',
    queued: 'Queued.',
    taskFailed: 'Task failed.',
    restInChat: 'The rest is in the chat.',
    codeInChat: 'There is code in the chat.',
    someCode: 'some code',
    aLink: 'a link',
    paused: "Sure, take your time. Say I'm back when you're ready.",
    back: "Okay, I'm here.",
    recap: 'Where we were:',
    forMe:
        'Sorry, was that for me? I heard: "{said}". '
        'Shall I send it, or what did you mean?',
    goOnOrSend:
        'You stopped at: "{said}". Shall I send it, or do you want to go on?',
    leftOut: "Okay, I'll leave it.",
  );

  static const it = CallWords(
    greet: 'Ciao, cosa c\'è da fare?',
    goAhead: 'Dimmi pure.',
    stillWorking: 'Ci sto ancora lavorando.',
    stopping: 'Ok, lo fermo.',
    couldNotStop: 'Non sono riuscito a fermarlo.',
    notReady: 'La sessione del progetto non è pronta. Controlla sullo schermo.',
    choiceWaiting: "L'agente aspetta una scelta sullo schermo.",
    needsChoice: 'Serve una scelta sullo schermo.',
    agentAsks: "L'agente chiede:",
    notThrough: 'Il messaggio non è partito.',
    sessionStopped: 'La sessione si è fermata.',
    done: 'Fatto.',
    noReply: 'Non ho avuto risposta. Controlla la chat.',
    checking: 'Ok, controllo.',
    noted: 'Ok, lo aggiungo.',
    starting: 'Perfetto, ci penso io. Ti aggiorno.',
    proposal: 'Ecco il compito:',
    scope: 'Cosa cambia:',
    doneWhen: 'Sarà finito quando:',
    shallIStart: 'Procedo?',
    taskComplete: 'Compito completato.',
    queued: 'In coda.',
    taskFailed: 'Compito non riuscito.',
    restInChat: 'Il resto è nella chat.',
    codeInChat: "C'è del codice nella chat.",
    someCode: 'del codice',
    aLink: 'un link',
    paused: 'Certo, fai con calma. Quando ci sei, dimmi eccomi.',
    back: 'Ok, ci sono.',
    recap: 'Dicevamo:',
    forMe:
        'Scusa, dicevi a me? Ho sentito: "{said}". '
        'Lo mando, o cosa intendevi?',
    goOnOrSend: 'Ti sei fermato a: "{said}". Lo mando, o vuoi continuare?',
    leftOut: 'Ok, lascio stare.',
  );

  /// The phrases for [locale] (`it-IT`, `it`); English for any other.
  static CallWords of(String locale) => languageCode(locale) == 'it' ? it : en;
}

/// The language part of a locale identifier: `it-IT` is `it`.
String languageCode(String locale) {
  final code = locale.split(RegExp('[-_]')).first.toLowerCase();
  return code.isEmpty ? 'en' : code;
}

/// The English name of a language code, for messages to the agent.
String languageNameOf(String code) =>
    const {
      'en': 'English',
      'it': 'Italian',
      'es': 'Spanish',
      'fr': 'French',
      'de': 'German',
      'pt': 'Portuguese',
      'nl': 'Dutch',
      'ja': 'Japanese',
      'zh': 'Chinese',
    }[code] ??
    'the language with code "$code"';

/// A task the voice session worked out with the person and the project;
/// it goes to the project's agent once they agree.
class TaskDraft {
  final String title, goal, scope, done;

  /// How the proposal is said aloud: the agent's own natural paraphrase,
  /// not the fields read out (empty when it gave none). Never sent: the
  /// task that starts keeps the fields exactly.
  final String spoken;
  const TaskDraft({
    required this.title,
    required this.goal,
    required this.scope,
    required this.done,
    this.spoken = '',
  });

  /// The task as shown and sent.
  String get task => '$goal\nScope: $scope\nDone when: $done';
}

/// What the project's agent answered to a validation: a task ready to
/// propose, or questions for the person.
sealed class Validation {
  const Validation();
}

class Proposed extends Validation {
  final TaskDraft draft;
  const Proposed(this.draft);
}

class Questions extends Validation {
  final List<String> questions;
  const Questions(this.questions);
}

/// How the project's agent answers a call's question or check more deeply
/// than a quick look: it hands the analysis to one read-only helper terminal
/// of its own project, which may also ask another project (ADR 0035). Never
/// coding work: that still waits for the person's yes to a proposed task.
const analysisHelperRules =
    "Answer a quick lookup yourself. When the answer needs more than that (many files, a why or how question, or something in another project), start one read-only analysis terminal in this project, on the agent you run on: `connecto add <claude, codex, pi, antigravity or opencode, as you are> --name check-<topic>`. Ask it with `connecto send <id> --ask`, saying exactly what to find out and that it must change nothing and start no work. For another project it uses `--project <name>` on list, read and send --ask; if no terminal there is open to everyone, say so plainly. When it has answered, close it with `connecto close <id>`. Starting it needs no confirmation: it is an analysis, as is a request to \"launch an agent to check\" something.";

/// The start of the message that ends the agent's turn when it handed a
/// call's question or check to a helper terminal: its answer comes in a
/// later turn ([analysisHelperRules]).
const checkingMarker = 'Checking.';

/// [message] says a helper terminal is checking: the answer comes later.
/// It may carry the check's id, `Checking [c-1a2b].`.
bool isCheckingNote(String message) => checkMarker(message, 'checking') != null;

/// The id in a check's [word] marker at the start of [message]
/// (`Checking [c-1a2b].`, `Findings [c-1a2b].`): '' for a marker without
/// one, null when [message] does not begin with that marker.
String? checkMarker(String message, String word) {
  final m = RegExp(
    '^[\\s*_#>`-]*$word\\s*(?:[\\[(]\\s*([a-z0-9-]+)\\s*[\\])])?\\s*[.:]',
    caseSensitive: false,
  ).firstMatch(message);
  return m == null ? null : (m[1] ?? '').toLowerCase();
}

/// [text] is a notice Connecto typed into the agent's terminal to wake it
/// (an ask's answer arrived), not the person's words.
bool isConnectoNotice(String text) =>
    unwrapPaste(text).trimLeft().startsWith('[connecto notice]');

/// [text] is a message Connecto put in the agent's terminal (a notice, or
/// another terminal's answer to an ask), not words the person typed.
bool isFromConnecto(String text) {
  final t = unwrapPaste(text).trimLeft();
  return t.startsWith('[connecto notice]') || t.startsWith('[connecto from ');
}

/// What the project's agent is sent to check a request against the code,
/// without starting any work; it answers in a form [parseValidation] reads.
/// A request only to find out or check something is answered in plain words
/// instead, which the call says as they are.
/// With [memory] (the project has a functional memory), it starts from that
/// before reading code: cheaper and faster.
String validationPrompt(
  List<(bool, String)> turns,
  String language, {
  bool memory = false,
}) {
  final name = languageNameOf(languageCode(language));
  final talk = [
    for (final (mine, text) in turns) '${mine ? 'Person' : 'Call'}: $text',
  ].join('\n');
  final read = memory
      ? "Start from the project's functional memory in .functionalMemory/ (README.md first, then the file it points you to): it says what the project does and where. Read the code where the memory does not answer, as far as you need to judge well that the request is clear and fits the project."
      : "Read the project's code as far as you need to judge well that the request is clear and fits the project.";
  return """A request taken by voice in Connecto Chat, to check before any work starts. $read Don't change anything and don't begin the work.

$analysisHelperRules

If you start a helper, end this turn with one line that begins with "$checkingMarker" and says what it is checking. When it answers, close it and reply as below.

If the person only wants to find out or check something, with nothing to change, don't propose a task: answer in $name in a few short plain sentences, in neither form.

Otherwise answer in exactly one of these two forms, with nothing before or after it. Keep the first line and the field names in English; write everything else in $name, in short plain sentences that are read aloud.

TASK
Title: <at most six words>
Goal: <what the person wants, one sentence>
Scope: <what changes and where, one or two sentences>
Done: <how to tell it is done, one sentence>
Say: <how you would propose this task out loud to the person, in your own words: two or three natural sentences, not the fields read out, ending by asking whether to start it; never with the word "yes" or "sì">

QUESTIONS
1. <one short question the person can answer by voice>
2. <a second one, only if needed>

Use TASK only when the goal, the scope and how to tell it is done are all clear. Otherwise ask at most two questions.

The call so far:
$talk""";
}

/// The agent's answer to [validationPrompt]; null when it is in neither
/// form. A task is proposed only when its goal, scope and how to tell it is
/// done are all given: otherwise its questions, if any, are asked instead.
Validation? parseValidation(String reply) {
  // Only the formatting around markers, labels and whole lines goes: the
  // values keep their identifiers and operators (`user_id`, `count > 5`).
  final lines = [
    for (final raw in reply.split('\n'))
      if (raw
              .replaceFirst(
                RegExp(r'^\s*(#{1,6}\s+|>\s*)?([-*+•]|\d+[.)])?\s+'),
                '',
              )
              .replaceFirstMapped(RegExp(r'^(\*\*|__)(.*)\1$'), (m) => m[2]!)
              .trim()
          case final l when l.isNotEmpty)
        l,
  ];
  final bare = [
    for (final l in lines)
      l.replaceAll(RegExp(r'[*_#`]'), '').trim().toUpperCase(),
  ];
  int marker(Set<String> names) => bare.indexWhere(
    (l) => names.contains(l.replaceAll(RegExp(r'[:.]$'), '')),
  );
  final taskAt = marker({'TASK', 'COMPITO'});
  final questionsAt = marker({'QUESTIONS', 'DOMANDE'});
  if (taskAt >= 0 && (questionsAt < 0 || taskAt < questionsAt)) {
    String field(Set<String> names) {
      for (final l in lines.skip(taskAt + 1)) {
        final m = RegExp(
          r'^(\*\*|__)?\s*([\p{L} ]+?)\s*(?:\1\s*:|:\s*\1)\s*(.*)$',
          unicode: true,
        ).firstMatch(l);
        if (m != null && names.contains(m[2]!.toLowerCase())) {
          return m[3]!.trim();
        }
      }
      return '';
    }

    final d = TaskDraft(
      title: field({'title', 'titolo'}),
      goal: field({'goal', 'obiettivo'}),
      scope: field({'scope', 'ambito'}),
      done: field({'done', 'fatto', 'finito', 'completato'}),
      spoken: field({'say', 'spoken', 'dire'}),
    );
    if (d.goal.isNotEmpty && d.scope.isNotEmpty && d.done.isNotEmpty) {
      return Proposed(
        d.title.isEmpty
            ? TaskDraft(
                title: _titleOf(d.goal),
                goal: d.goal,
                scope: d.scope,
                done: d.done,
                spoken: d.spoken,
              )
            : d,
      );
    }
  }
  if (questionsAt >= 0) {
    final end = taskAt > questionsAt ? taskAt : lines.length;
    final asked = [
      for (final l in lines.sublist(questionsAt + 1, end).take(2))
        l.replaceFirstMapped(RegExp(r'^(\*\*|__)(.*)\1$'), (m) => m[2]!),
    ];
    if (asked.isNotEmpty) return Questions(asked);
  }
  return null;
}

String _titleOf(String words) {
  final first = firstSentences(words.trim(), 1);
  return first.length <= 60 ? first : '${first.substring(0, 57)}…';
}

/// [s] ending as a sentence.
String _sentence(String s) {
  final t = s.trim();
  return t.isEmpty || RegExp(r'[.!?]$').hasMatch(t) ? t : '$t.';
}

/// How a call proposes [d] out loud, ending on the request to confirm: the
/// agent's paraphrase when it gave one (natural speech, not the fields),
/// else the fields with their labels. Either way the task that starts keeps
/// the fields as written.
String proposalSpeech(TaskDraft d, CallWords w) {
  final spoken = speakable(d.spoken, w).trim();
  if (spoken.isNotEmpty) {
    return spoken.endsWith('?')
        ? spoken
        : '${_sentence(spoken)} ${w.shallIStart}';
  }
  return '${w.proposal} ${_sentence(d.goal)} ${w.scope} ${_sentence(d.scope)} '
      '${w.doneWhen} ${_sentence(d.done)} ${w.shallIStart}';
}

/// How [taskPrompt] begins: a prompt that starts a task.
const taskPromptLead = 'A task agreed by voice in Connecto Chat';

/// The line a call's own prompt carries so the agent never takes it for the
/// answer to a question it asked about a task (ADR 0034): [what] says what
/// the request is.
String newRequestNote(String what) =>
    '$what: a new request, not an answer to any "Question [<id>]." you asked. '
    'Such a question still waits for its own answer.';

/// What the project's agent is sent for an agreed task: it puts it on the
/// project's task board ([boardRules]) under [id], and runs the board one
/// task at a time, each as the board's `howToRun` says (by default a coder
/// on the strongest CLI the project allows and a reviewer on another,
/// [defaultHowToRun]). Its messages about a
/// task begin with a marker tagged with the task's id ("Queued [id].",
/// "Task complete [id].", "Task failed [id]."), so a voice session knows
/// where its own task stands whatever else is on the board.
String taskPrompt(TaskDraft d, {required String id, String language = 'en'}) {
  final name = languageNameOf(languageCode(language));
  return """$taskPromptLead, for the project's task board. Don't do the work in this terminal: you keep the board, hand each task out and keep me posted.

${newRequestNote('This is a new task, id $id')}

$boardRules

How your turns work: your last message of a turn is what I see, and it is read aloud; a turn ends with it. So make the board changes and start any terminal (with `connecto send <id> --ask`) first, and end the turn with the message. When a terminal you asked answers, you get a new turn.

1. If the board has no howToRun, or its howToRun is exactly the earlier default instructions at the end of this message, set it to the default instructions at the end of this message, word for word. Any other howToRun is the project's own: keep it.
2. If id $id is already on the board, don't add it again or start anything for it. Reply with where it stands, beginning with its marker: "Queued [$id]." if it is queued, "Task complete [$id]." if it is done, "Task failed [$id]." if it failed, or one sentence saying it is running or blocked.
3. Otherwise add it to the end of the board as queued, with its id, title, goal, scope and done exactly as given.
4. If no task is running or blocked, start the first queued task (step 6). It may be an older task, not this one.
5. Then reply. If this task did not start (another task is running or blocked, or older tasks are queued before it), begin with "Queued [$id]." and say in one sentence in $name which task runs now and this one's place in line. If it started, say so in one sentence.
6. Starting a task: read the board again (its order may have changed), set the task to running, and run it exactly as the board's howToRun says at that moment. Every task you start, now or later, is run that way.
7. When a task is done as howToRun says: set it to done with a one-line note, start the next queued task (step 6) if there is one, then reply with one message that begins with "Task complete [<its id>]." followed by two or three sentences in $name on what changed, and one on the task that started next, if any.
8. When a task fails: set it to failed with a one-line note, start the next queued task (step 6) if there is one, then reply with one message that begins with "Task failed [<its id>]." followed by one sentence in $name on why, and one on the task that started next, if any.
9. If you are blocked or need a decision: set the task to blocked, then ask me in a message that begins with "Question [<its id>]." and wait. Once I answer, set it to running again and go on.
10. Keep your messages short, in plain sentences and in $name: they are read aloud. Write each marker in English at the very start of its message, and use task ids only in markers.

Default instructions:
----------
$defaultHowToRun
----------

Earlier default instructions (replace them only when the board has exactly this text):
----------
$previousDefaultHowToRun
----------

Id: $id
Task: ${d.title}
Goal: ${d.goal}
Scope: ${d.scope}
Done when: ${d.done}""";
}

/// A task id after a marker: `[t-k3x9]` or `(t-k3x9)`.
const _taskIdPattern = r'(?:\s*[\[(](t-[a-z0-9]+)[\])])?';

/// Where a task ended up, as the agent reports it.
enum TaskOutcome { complete, queued, failed }

/// What the agent's messages follow: the outstanding prompt ([reply]), a
/// prompt of this call's already answered or Connecto waking the agent
/// ([own]: it speaks on its own), or a message typed on screen or a check
/// called off ([none]: not this call's).
enum _After { reply, own, none }

/// A message read to be said: a report ([outcome]) or question about the
/// task [task], or, without a task, the agent speaking on its own.
/// [start]: the report answers that task's start.
class _Heard {
  _Heard(this.text, {this.task, this.outcome, this.start = false});
  final String text;
  final String? task;
  final TaskOutcome? outcome;
  final bool start;

  /// For a question: how often it was said again, not heard whole.
  int tries = 0;
}

/// How often a blocked task's question is said again when it was not heard
/// whole; after that it is left to the chat on screen.
const _questionTries = 3;

/// Why the call asks before sending what was said: it was left half-way
/// for a long while, or did not seem meant for the call.
enum _Doubt { unfinished, notForCall }

/// Words the call asked about before sending them, with the proposal and
/// check they were said under, and whether they were left half-way (an
/// answer goes on with them) or seemed not for the call (an answer replaces
/// them). A yes sends them; "no" lets them go.
///
/// An answer counts only once the question was delivered: its playback
/// ([asked]) finished, or was heard at least through the quoted words
/// ([quoteEnd]). An answer that comes first waits for that result; a
/// question that failed or was cut before then asked nothing. And it counts
/// only while the question is the last thing the call said ([utterance]).
class _Unsure {
  _Unsure({
    required this.text,
    required this.draft,
    required this.check,
    required this.unfinished,
    required this.utterance,
    required this.asked,
    required this.quoteEnd,
  });
  final String text;
  final TaskDraft? draft;
  final int? check;
  final bool unfinished;
  final int utterance;
  final Future<Spoken> asked;
  final int quoteEnd;

  /// Whether [spoken] (how the question went) delivered it.
  bool deliveredBy(Spoken spoken) =>
      spoken.completed || spoken.heard.length >= quoteEnd;
}

/// A report marker at the start of a message, as a statement: "Task
/// complete?" is not one. Group 1 is the words, group 2 the task's id.
final _reportMarker = RegExp(
  r'^[\s*_#>]*(task complete|attività completata|compito completato'
  r'|task failed|compito non riuscito|attività non riuscita|queued|in coda)'
  '$_taskIdPattern'
  r'(?=\s*([.!:*_]|$))',
  caseSensitive: false,
);

/// The report [message] begins with, and the task id it names (null when
/// it names none); null when it is not a report.
(TaskOutcome, String?)? taskMarker(String message) {
  final m = _reportMarker.firstMatch(message);
  if (m == null) return null;
  final words = m[1]!.toLowerCase();
  final outcome = words.contains('complet')
      ? TaskOutcome.complete
      : words.contains('fail') || words.contains('non riuscit')
      ? TaskOutcome.failed
      : TaskOutcome.queued;
  return (outcome, m[2]?.toLowerCase());
}

/// A question the agent asks about a task it runs (it is blocked), with
/// the task's id: `Question [t-k3x9].`.
final _questionMarker = RegExp(
  r'^[\s*_#>]*question\s*[\[(](t-[a-z0-9]+)[\])][.!:]?[*_]*\s*',
  caseSensitive: false,
);

/// The id of the task [message] asks about; null when it asks nothing.
String? questionTask(String message) =>
    _questionMarker.firstMatch(message)?[1]?.toLowerCase();

/// The agent's message reporting that the task is over.
bool isTaskComplete(String message) =>
    taskMarker(message)?.$1 == TaskOutcome.complete;

/// What [message] reports about [task] by its id; null when it reports
/// nothing about it. Found wherever it is in the history.
TaskOutcome? taggedOutcome(String message, String task) =>
    switch (taskMarker(message)) {
      (final outcome, final id?) when id == task.toLowerCase() => outcome,
      _ => null,
    };

/// A report naming another task than [task]: not this call's to say.
bool isOtherTasksReport(String message, String? task) =>
    task != null &&
    switch (taskMarker(message)) {
      (_, final id?) => id != task.toLowerCase(),
      _ => false,
    };

/// What the agent's [replies] to this call report: an untagged "Task
/// complete." (as before task ids), or, for a call that knows no id, any
/// tagged report. Tagged reports for a known id are found by
/// [taggedOutcome] instead.
TaskOutcome? replyOutcome(List<String> replies, {String? task}) {
  for (final r in replies) {
    switch (taskMarker(r)) {
      case (TaskOutcome.complete, null):
        return TaskOutcome.complete;
      case (final outcome, final id?)
          when task == null || id == task.toLowerCase():
        return outcome;
      default:
    }
  }
  return null;
}

/// Whether any one of the agent's [messages] reports [task] complete: a
/// report can arrive after an update in one poll. Another task's report
/// does not count.
bool reportsComplete(List<String> messages, {String? task}) =>
    replyOutcome(messages, task: task) == TaskOutcome.complete;

/// Whether any one of [messages] says [task] waits in the board's queue.
bool reportsQueued(List<String> messages, {String? task}) =>
    replyOutcome(messages, task: task) == TaskOutcome.queued;

/// [message] with its English report marker said in the call language, and
/// task ids left out: they are for the app, not the ear.
String localizeComplete(String message, CallWords w) {
  String marker(String words, String said) =>
      message = message.replaceFirstMapped(
        RegExp(
          '^([\\s*_]*)$words$_taskIdPattern[.!][*_]*',
          caseSensitive: false,
        ),
        (m) => '${m[1]}$said',
      );
  marker('task complete', w.taskComplete);
  marker('task failed', w.taskFailed);
  marker('queued', w.queued);
  message = message.replaceFirst(_questionMarker, '');
  return message.replaceAll(
    RegExp(r'\s*[\[(]t-[a-z0-9]+[\])]', caseSensitive: false),
    '',
  );
}

/// A reply as it should sound: no markdown, code or addresses read out.
String speakable(String reply, [CallWords w = CallWords.en]) {
  var codeSaid = false;
  var text = reply.replaceAllMapped(RegExp(r'```[\s\S]*?(```|$)'), (_) {
    if (codeSaid) return '\n';
    codeSaid = true;
    return '\n${w.codeInChat}\n';
  });
  text = text
      // Inline code: short names are read, longer snippets are not.
      .replaceAllMapped(
        RegExp(r'`([^`\n]*)`'),
        (m) => m[1]!.length <= 40 ? m[1]! : w.someCode,
      )
      .replaceAllMapped(RegExp(r'!?\[([^\]]*)\]\([^)]*\)'), (m) => m[1]!)
      .replaceAll(RegExp(r'https?://\S+'), w.aLink)
      .replaceAll(RegExp(r'^\s*\|.*$', multiLine: true), '')
      .replaceAll(RegExp(r'^\s*([-*_=]\s*){3,}$', multiLine: true), '')
      .replaceAll(RegExp(r'^\s{0,3}#{1,6}\s+', multiLine: true), '')
      .replaceAll(RegExp(r'^\s*>\s?', multiLine: true), '')
      .replaceAll(RegExp(r'^\s*[-*+•]\s+', multiLine: true), '')
      .replaceAll(RegExp(r'(\*\*|__|~~)'), '')
      .replaceAllMapped(
        RegExp(r'(^|\W)[*_]([^*_\n]+)[*_](?=\W|$)'),
        (m) => '${m[1]}${m[2]}',
      );
  final sentences = [
    for (final line in text.split('\n'))
      if (line.trim().isNotEmpty) line.trim(),
  ];
  return [
    for (final s in sentences) RegExp(r'[.!?:;,]$').hasMatch(s) ? s : '$s.',
  ].join(' ').replaceAll(RegExp(r'\s+'), ' ').trim();
}

/// The first [count] sentences of [text].
String firstSentences(String text, int count) {
  final parts = text.split(RegExp(r'(?<=[.!?])\s+'));
  return parts.take(count).join(' ');
}

/// The last sentence of [text].
String _lastSentence(String text) =>
    text.trim().split(RegExp(r'(?<=[.!?])\s+')).last;

/// What is said for [reply]: short replies as they are, long ones condensed
/// on the device when it can, else their opening and a pointer to the chat.
Future<String> spokenReply(
  String reply,
  Future<String?> Function(String) condense, {
  int shortEnough = 320,
  CallWords words = CallWords.en,
}) async {
  final text = speakable(reply, words);
  if (text.length <= shortEnough) return text;
  final condensed = await condense(text);
  if (condensed != null && condensed.trim().isNotEmpty) {
    return speakable(condensed, words);
  }
  return '${firstSentences(text, 2)} ${words.restInChat}';
}

String _plain(String s) => s
    .toLowerCase()
    .replaceAll(RegExp(r"[^\p{L}\p{N}' ]", unicode: true), ' ')
    .replaceAll(RegExp(r'\s+'), ' ')
    .trim();

/// Spoken commands, in English and Italian; only a whole turn counts.
const _hangUp = {
  'hang up',
  'end call',
  'end the call',
  'goodbye',
  'riattacca',
  'chiudi',
  'chiudi la chiamata',
  'chiudi chiamata',
  'termina la chiamata',
  'metti giù',
  'arrivederci',
};
const _stop = {
  'stop',
  'cancel',
  'stop it',
  'stop working',
  'interrupt',
  'basta',
  'fermati',
  'ferma',
  'fermo',
  'fermalo',
  'annulla',
  'interrompi',
};

/// A whole turn that ends the call.
bool isHangUp(String text) => _hangUp.contains(_plain(text));

/// A whole turn that stops the agent.
bool isStop(String text) => _stop.contains(_plain(text));

/// Words a listener says to show they follow, not to take the turn: said
/// over speech, they leave it going.
const _backchannelWords = {
  'mh',
  'mhm',
  'mm',
  'mmh',
  'mmm',
  'hm',
  'hmm',
  'uh',
  'ah',
  'eh',
  'oh',
  'ok',
  'okay',
  'yes',
  'yeah',
  'yep',
  'right',
  'sure',
  'true',
  'cool',
  'sì',
  'si',
  'certo',
  'giusto',
  'vero',
  'esatto',
  'capito',
  'chiaro',
  'ottimo',
  'perfetto',
  'bene',
  'benissimo',
  'già',
};
const _backchannelPhrases = {'uh huh', 'got it', 'i see', 'ho capito'};

/// A whole turn that only says "I'm following" ("mh", "sì sì", "ok").
bool isBackchannel(String text) {
  final plain = ' ${_plain(text)} ';
  var rest = plain;
  for (final p in _backchannelPhrases) {
    rest = rest.replaceAll(' $p ', ' ');
  }
  final words = rest.trim().split(' ').where((w) => w.isNotEmpty);
  if (plain.trim().isEmpty) return false;
  return words.every(_backchannelWords.contains);
}

/// A whole turn that asks to hold on a second, to say something: speech
/// stops and the call listens, without answering.
const _holdOn = {
  'aspetta',
  'aspetta aspetta',
  'no aspetta',
  'aspetti',
  'no no',
  'no no no',
  'wait',
  'wait wait',
  'no wait',
  'hold on',
  'hang on',
};
bool isHoldOn(String text) => _holdOn.contains(_plain(text));

/// Phrases that mean the person steps away for a while, wherever they come
/// in a turn ("aspetta un attimo che devo fare una cosa e poi continuiamo").
const _awayPhrases = {
  'torno subito',
  'arrivo subito',
  'devo fare una cosa',
  "devo fare un'altra cosa",
  'faccio una cosa',
  'dammi un minuto',
  'dammi un attimo',
  'dammi un secondo',
  'dammi un momento',
  'dammi due minuti',
  'dammi cinque minuti',
  'aspettami',
  'mi aspetti',
  'facciamo una pausa',
  'poi continuiamo',
  'poi riprendiamo',
  'be right back',
  'give me a minute',
  'give me a moment',
  'give me a second',
  'give me a sec',
  'bear with me',
  'wait for me',
  'take a break',
  "let's take a break",
  'then we continue',
  "i'll be back",
};

/// Asking for a moment, which is a pause only as a short turn of its own:
/// "aspetta un secondo, il file è sbagliato" goes on with the request.
const _momentPhrases = {
  'un attimo',
  'un attimino',
  'un momento',
  'un momentino',
  'un secondo',
  'un minuto',
  'pausa',
  'one moment',
  'a moment',
  'one sec',
  'one second',
  'a second',
  'just a sec',
  'a minute',
  'pause',
};

/// A turn that asks the call to wait while the person does something else.
bool isPause(String text) {
  final plain = ' ${_plain(text)} ';
  final count = plain.trim().split(' ').length;
  if (count <= 24 && _awayPhrases.any((p) => plain.contains(' $p '))) {
    return true;
  }
  return count <= 5 && _momentPhrases.any((p) => plain.contains(' $p '));
}

/// Phrases that say the person is back after a pause.
const _backPhrases = [
  'eccomi qua',
  'eccomi qui',
  'eccomi',
  'rieccomi',
  'eccoci',
  'ci sono',
  'sono qui',
  'sono tornato',
  'sono tornata',
  'sono di nuovo qui',
  'dove eravamo rimasti',
  'dove eravamo',
  'possiamo continuare',
  'possiamo riprendere',
  'continuiamo',
  'riprendiamo',
  "i'm back",
  'im back',
  'i am back',
  "i'm here",
  'where were we',
  "let's continue",
  "let's go on",
  'we can continue',
  'back',
];

/// Small words that may come before "I'm back" without changing it.
const _backLeads = {'ok', 'okay', 'allora', 'bene', 'sì', 'si', 'yes', 'so'};

/// When [text] says the person is back, what they said after it (empty
/// when nothing); null otherwise. Only the start of a turn counts: talk with
/// someone else in the room does not resume the call.
String? resumeRest(String text) {
  final words = _plain(text).split(' ').where((w) => w.isNotEmpty).toList();
  var at = 0;
  while (at < words.length - 1 && at < 2 && _backLeads.contains(words[at])) {
    at++;
  }
  final rest = words.sublist(at).join(' ');
  for (final p in _backPhrases) {
    if (rest == p || rest.startsWith('$p ')) {
      return rest.substring(p.length).trim();
    }
  }
  return null;
}

/// Words that leave a sentence open when it ends on them ("vorrei che il
/// login e"): the person is still thinking, not done.
const _openEndings = {
  'e',
  'ed',
  'o',
  'ma',
  'però',
  'che',
  'quindi',
  'allora',
  'cioè',
  'tipo',
  'perché',
  'se',
  'di',
  'del',
  'della',
  'a',
  'al',
  'alla',
  'con',
  'per',
  'in',
  'nel',
  'nella',
  'il',
  'lo',
  'la',
  'i',
  'gli',
  'le',
  'un',
  'uno',
  'una',
  'quando',
  'come',
  'dove',
  'mentre',
  'oppure',
  'anche',
  'poi',
  'insomma',
  'praticamente',
  'diciamo',
  'magari',
  'ehm',
  'em',
  'uhm',
  // Where Italian speakers stop to think: "il problema è", "non", "da".
  'è',
  'e\'',
  'non',
  'né',
  'da',
  'dal',
  'dallo',
  'dalla',
  'dai',
  'dagli',
  'dalle',
  'su',
  'sul',
  'sullo',
  'sulla',
  'sui',
  'sugli',
  'sulle',
  'tra',
  'fra',
  'dei',
  'degli',
  'delle',
  'dello',
  'ai',
  'agli',
  'alle',
  'allo',
  'nei',
  'negli',
  'nelle',
  'nello',
  'col',
  'coi',
  'sono',
  'ho',
  'hai',
  'ha',
  'abbiamo',
  'mi',
  'ti',
  'ci',
  'si',
  'vi',
  'ne',
  'questo',
  'questa',
  'questi',
  'queste',
  'quel',
  'quello',
  'quella',
  'quei',
  'quelle',
  'cosa',
  'sia',
  'dunque',
  'comunque',
  'ovvero',
  'ossia',
  'siccome',
  'finché',
  'affinché',
  'eh',
  'ehh',
  'eeh',
  'mh',
  'mm',
  'mmh',
  'mmm',
  'beh',
  'boh',
  'and',
  'or',
  'but',
  'so',
  'because',
  'that',
  'the',
  'an',
  'to',
  'of',
  'with',
  'for',
  'if',
  'when',
  'like',
  'um',
  'uh',
  'er',
  'which',
  'then',
  'also',
  'maybe',
  'basically',
  'is',
  'are',
  'was',
  'were',
  'on',
  'at',
  'from',
  'by',
  'into',
  'about',
  'than',
  'as',
  'my',
  'your',
  'our',
  'their',
  'this',
  'these',
  'those',
  'erm',
  'hmm',
  'well',
};

/// Words that end a turn on purpose ("controlla i test, vai"), and the words
/// that, just before one, make it part of the sentence instead ("ho fatto",
/// "when it's done", "dove vai").
const _sendWords = <String, Set<String>>{
  'vai': {
    'non',
    'ci',
    'ne',
    'te',
    'tu',
    'se',
    'dove',
    'come',
    'quando',
    'che',
    'chi',
    'mi',
    'ti',
    'lo',
    'la',
    'ma',
  },
  'fatto': {
    'ho',
    'hai',
    'ha',
    'abbiamo',
    'avete',
    'hanno',
    'avevo',
    'aveva',
    'avrei',
    'avrà',
    'è',
    "e'",
    'sono',
    'sei',
    'siamo',
    'siete',
    'stato',
    'stata',
    'stati',
    'state',
    'già',
    'non',
    'ben',
    'mal',
    'bel',
    'il',
    'lo',
    'un',
    'del',
    'di',
    'che',
    'mai',
    'appena',
    'viene',
    'venga',
    'sia',
    'era',
    'fatto',
    'questo',
    'quello',
    'tutto',
    'ancora',
  },
  'done': {
    'is',
    'are',
    'was',
    'were',
    'be',
    'been',
    'being',
    'get',
    'gets',
    'got',
    'getting',
    "it's",
    'its',
    "i'm",
    'im',
    "we're",
    "you're",
    "they're",
    'not',
    'have',
    'has',
    'had',
    'well',
    'already',
    'all',
    'never',
    'almost',
    'nearly',
    'half',
    'until',
    'till',
    'when',
    'the',
    'job',
    'it',
    'that',
    'this',
    'you',
    'we',
    'they',
    'i',
  },
  'send': {
    'to',
    'and',
    'then',
    'please',
    'will',
    'can',
    'should',
    'could',
    'would',
    "i'll",
    "don't",
    'not',
    'must',
    'or',
    'also',
    'you',
    'we',
    'i',
  },
  'go ahead': {
    'to',
    'can',
    'will',
    'should',
    'could',
    'would',
    'let',
    'it',
    'them',
    'not',
    "don't",
    'and',
    'we',
    'you',
    'i',
    'they',
    'might',
  },
};

/// [text] without the word at its end that sends it on purpose ("…, vai",
/// "…, done"), or null when it does not end on one. It counts only when the
/// recognizer set it apart: after punctuation (a pause), or, when
/// [separate], as a result of its own. A send word alone, or one the sentence
/// needs ("ho fatto", "when it's done", "press send"), does not count.
String? withoutSendWord(String text, {bool separate = false}) {
  final tokens = text.trim().split(RegExp(r'\s+'));
  final plain = [for (final t in tokens) _plain(t)];
  // Trailing punctuation-only tokens ("—", "…") are not words.
  var end = plain.length;
  while (end > 0 && plain[end - 1].isEmpty) {
    end--;
  }
  for (final MapEntry(key: phrase, value: needs) in _sendWords.entries) {
    final words = phrase.split(' ');
    final at = end - words.length;
    if (at < 1) continue;
    var matches = true;
    for (var i = 0; i < words.length && matches; i++) {
      matches = plain[at + i] == words[i];
    }
    if (!matches) continue;
    // The word before, ignoring tokens that are only punctuation.
    var before = at - 1;
    while (before >= 0 && plain[before].isEmpty) {
      before--;
    }
    if (before < 0) continue;
    final previous = plain[before].split(' ').last;
    if (needs.contains(previous)) continue;
    // Positive evidence of a pause before it: punctuation after the word
    // before, or a token of punctuation alone in between.
    final apart =
        before < at - 1 || RegExp(r'[,.;:!?…–—-]$').hasMatch(tokens[before]);
    if (!separate && !apart) continue;
    final rest = tokens
        .sublist(0, at)
        .join(' ')
        .replaceAll(RegExp(r'[\s,;:–—-]+$'), '')
        .trim();
    if (_plain(rest).isEmpty) continue;
    return rest;
  }
  return null;
}

/// Whether [text] is a send word alone ("vai", "done").
bool _isSendWord(String text) => _sendWords.containsKey(_words(text).join(' '));

/// Whether [text] trails off on a word that leaves it open.
bool trailsOff(String text) {
  final words = _plain(text).split(' ');
  return words.isNotEmpty && _openEndings.contains(words.last);
}

/// The words of [text] as they are compared: lower case, no punctuation.
List<String> _words(String text) =>
    _plain(text).split(' ').where((w) => w.isNotEmpty).toList();

/// What [text] says after the words [sent], when it reports them again and
/// goes on (the recognizer gives the whole segment, from its start); null
/// when it says nothing more. Words it revised are counted, not matched.
String? _wordsAfter(List<String> sent, String text) {
  if (sent.isEmpty) return null;
  final tokens = text.trim().split(RegExp(r'\s+'));
  var seen = 0;
  for (final (i, token) in tokens.indexed) {
    seen += _words(token).length;
    if (seen >= sent.length) {
      final rest = tokens.sublist(i + 1).join(' ').trim();
      return _words(rest).isEmpty ? null : rest;
    }
  }
  return null;
}

/// Whether [heard] is the speech's own echo: a run of words in [spoken], or
/// mostly its words as the recognizer mishears them through the speaker
/// (an ending changed, a word lost or added). At least [echoShare] of
/// heard's longer words must be in [spoken]: short ones ("il", "the") are in
/// any reply, so they do not count. Only for a phrase of three such words
/// or more: a short answer ("sì, procedi" to "Procedo?") must match exactly,
/// or the person's own reply would be taken for echo.
bool _echoOf(String heard, String spoken) {
  final h = _plain(heard), s = _plain(spoken);
  if (h.isEmpty) return false;
  if (' $s '.contains(' $h ')) return true;
  final said = _words(s).toSet();
  final content = [
    for (final w in _words(h))
      if (w.length >= 3) w,
  ];
  if (content.length < 3) return false;
  final echoed = content.where((w) => said.any((t) => _sameWord(w, t))).length;
  return echoed >= content.length * echoShare;
}

/// The share of heard words that must be in the speech for [_echoOf].
const echoShare = 0.6;

/// Whether [a] and [b] are one word as a recognizer may give it: the same,
/// or long words with the same stem ("controllato", "controllata").
bool _sameWord(String a, String b) =>
    a == b ||
    (a.length >= 5 && b.length >= 5 && a.substring(0, 4) == b.substring(0, 4));

/// Words that agree to a proposed task, in English and Italian.
const _agreeWords = {
  'yes',
  'yeah',
  'yep',
  'ok',
  'okay',
  'go',
  'start',
  'confirm',
  'confirmed',
  'correct',
  'perfect',
  'sure',
  'great',
  'sì',
  'si',
  'vai',
  'procedi',
  'inizia',
  'conferma',
  'confermo',
  'perfetto',
  'esatto',
  'certo',
  'certamente',
  'giusto',
  'benissimo',
  'ottimo',
  'avanti',
  'parti',
  'partiamo',
  'andiamo',
  'assolutamente',
  'd\'accordo',
};
const _agreePhrases = {
  'go ahead',
  'do it',
  'sounds good',
  "that's right",
  'start it',
  "let's go",
  'va bene',
  'va benissimo',
  'mi va bene',
  'per me va bene',
  'ok vai',
  'sì vai',
  'si vai',
  'fallo',
  'vai pure',
  'procedi pure',
};

/// Words that may go with a yes without changing it.
const _agreeFillers = {'please', 'thanks', 'grazie', 'pure'};

/// Letting go of words the call asked about ("no", "lascia stare"): the
/// whole turn says so.
bool dismisses(String text) => _dismiss.contains(_plain(text));

const _dismiss = {
  'no',
  'no no',
  'nope',
  'no thanks',
  'no thank you',
  'never mind',
  'nevermind',
  'forget it',
  'leave it',
  'not for you',
  'no not for you',
  "that wasn't for you",
  "it wasn't for you",
  'no grazie',
  'niente',
  'no niente',
  'lascia stare',
  'no lascia stare',
  'lascia perdere',
  'no lascia perdere',
  'lasciamo stare',
  'lascia',
  'non era per te',
  'no non era per te',
};

/// A spoken command for the call itself: never held for Jev, never asked
/// about.
bool isCallCommand(String text) =>
    isHangUp(text) || isStop(text) || isHoldOn(text) || isPause(text);

/// [said] as the call quotes it back: whole when short, else its last words.
String quoted(String said, {int words = 25}) {
  final all = said.trim().split(RegExp(r'\s+'));
  return all.length <= words
      ? said.trim()
      : '… ${all.sublist(all.length - words).join(' ')}';
}

/// A plain yes to the task on offer: the whole turn is agreement (a phrase,
/// agreement words, fillers). "Start later" or "sì, ma aspetta" is not.
bool agrees(String text) {
  var plain = _plain(text);
  if (plain.isEmpty) return false;
  // Longest phrases first, so "va benissimo" is not read as "va bene".
  final phrases = _agreePhrases.toList()
    ..sort((a, b) => b.length.compareTo(a.length));
  var agreed = false;
  for (final p in phrases) {
    final next = ' $plain '.replaceAll(' $p ', ' ');
    if (next.trim() != plain) {
      agreed = true;
      plain = next.trim();
    }
  }
  for (final w in plain.split(' ').where((w) => w.isNotEmpty)) {
    if (_agreeWords.contains(w)) {
      agreed = true;
    } else if (!_agreeFillers.contains(w)) {
      return false;
    }
  }
  return agreed;
}

/// When the last turn went out, its reply was ready and was first heard,
/// and how the natural voice did: what the call screen shows when asked to
/// (Settings, "Show voice timings").
class CallTimings {
  CallTimings({DateTime Function()? now}) : _now = now ?? DateTime.now;
  final DateTime Function() _now;

  bool shown = false;
  DateTime? _sent, _ready, _heard;

  /// The natural voice's last utterance.
  VoiceTiming? last;

  void turnSent() {
    _sent = _now();
    _ready = _heard = null;
  }

  /// Text to say is ready: the first after the turn counts.
  void replyReady() {
    if (_sent != null && _ready == null) _ready = _now();
  }

  void firstSound() {
    if (_ready != null && _heard == null) _heard = _now();
  }

  void measured(VoiceTiming timing) => last = timing;

  /// Turn sent → reply text ready.
  Duration? get toReply =>
      _sent == null || _ready == null ? null : _ready!.difference(_sent!);

  /// Reply text ready → first sound.
  Duration? get toSound =>
      _ready == null || _heard == null ? null : _heard!.difference(_ready!);

  String _s(Duration d) => '${(d.inMilliseconds / 1000).toStringAsFixed(2)} s';

  /// The readout: what is known so far.
  List<String> get lines => [
    if (toReply case final r?)
      'Turn → reply ${_s(r)}${toSound == null ? '' : ' → first sound ${_s(toSound!)}'}',
    if (last case final t?)
      'Voice ${t.voice} · ${t.engine} · ${t.speed.toStringAsFixed(2)}× real time'
          '${t.firstAudio == null ? '' : ' · first audio ${_s(t.firstAudio!)}'}',
  ];
}

/// Runs one voice session, which is one task. First it holds a short,
/// natural conversation with the person, on the device ([intake]). Once the
/// request is nearly clear, or needs the code, the call goes on hold and the
/// project's agent checks it without starting work ([validating]): it asks
/// questions, which the call speaks, or gives a well-defined task, which the
/// call proposes ([draft]). Once the person agrees (a tap or a spoken yes) the
/// task goes to the agent, which hands it to new terminals, and the call stays
/// on hold, speaking its updates, until it reports the task complete. The
/// person can talk over speech to cut it short, and can talk while on hold.
/// Everything said is in the call's [language].
class CallController extends ChangeNotifier {
  CallController({
    required this.call,
    required this.voice,
    required this.target,
    this.endOfTurn = const Duration(milliseconds: 2000),
    this.quickTurn = const Duration(milliseconds: 1200),
    this.patientTurn = const Duration(milliseconds: 3000),
    this.sentOnPurpose = const Duration(milliseconds: 400),
    this.sureDone = 0.8,
    this.maybeDone = 0.5,
    this.voiceHolds = 3,
    this.resumeWindow = const Duration(milliseconds: 2500),
    this.bargeWait = const Duration(milliseconds: 1200),
    this.pauseGrace = const Duration(seconds: 4),
    this.poll = const Duration(milliseconds: 750),
    this.pollWorking = const Duration(milliseconds: 200),
    this.stillWorking = const Duration(seconds: 30),
    this.helperCheckLimit = const Duration(minutes: 10),
    this.voiceLimit = const Duration(seconds: 8),
    this.unfinishedTurn = const Duration(minutes: 2),
    this.cutOff = 0.3,
    this.offCall = 0.4,
    this.projectName,
    this.intro,
    this.judgeSettle = const Duration(milliseconds: 150),
    Assistant? assistant,
    TurnJudge? judge,
  }) : _assistantGiven = assistant,
       _judgeGiven = judge;

  final Assistant? _assistantGiven;
  final TurnJudge? _judgeGiven;

  /// Whether what the person said so far is a finished turn, judged by the
  /// Mac's Jev when it has a key ([TurnJudge]); else never asked.
  late final TurnJudge judge =
      _judgeGiven ?? TurnJudge(call, '${target['host']}');

  /// Who answers in the conversation and shortens long replies: the
  /// device's model, else the Mac's through the relay, else none.
  late final Assistant assistant =
      _assistantGiven ??
      Assistant(
        voice: voice,
        mac: RemoteModel(call, '${target['host']}'),
        phone: PhoneModel(voice),
      );

  final Future<dynamic> Function(String, Data) call;
  final VoiceEngine voice;

  /// The host and project.
  final Data target;

  /// Silence after speech that ends the person's turn when nothing tells
  /// more ([TurnEstimate]), or when it was unsure ([maybeDone] up to
  /// [sureDone]).
  final Duration endOfTurn;

  /// The silence that ends a turn that sounded finished (at least
  /// [sureDone]), and the one a turn that sounded unfinished (under
  /// [maybeDone]) or trails off ("vorrei che il login e…") is given; the
  /// person's voice coming back also holds the turn for [patientTurn]. The
  /// agent takes minutes to answer, so waiting costs little and cutting
  /// someone off costs a lot.
  final Duration quickTurn, patientTurn;

  /// The silence after a turn that ends on a word that sends it ("…, vai",
  /// "…, done").
  final Duration sentOnPurpose;

  /// Where a [TurnEstimate] counts as finished, and as unsure.
  final double sureDone, maybeDone;

  /// How often the person's voice alone ([VoiceActivity], no new words) holds
  /// the turn open again: a bound, so noise cannot keep it open forever.
  final int voiceHolds;

  /// How long after the call begins its answer the person can still go on
  /// with what they were saying: the answer is dropped and their words are
  /// taken together.
  final Duration resumeWindow;

  /// How long speech held for the person's voice ([VoiceActivity]) waits for
  /// real words before it goes on.
  final Duration bargeWait;

  /// Just after a pause starts, a full sentence still goes on with the call:
  /// "un attimo" then more words was not a pause.
  final Duration pauseGrace;

  /// How often the chat is read: [poll] while listening, [pollWorking]
  /// while a reply is awaited. Reading it is local (the peer keeps the
  /// host's history), so the fast rate costs no network traffic.
  final Duration poll, pollWorking;

  /// How often a long agent turn is acknowledged out loud.
  final Duration stillWorking;

  /// How long a check handed to a helper terminal ([isCheckingNote]) is
  /// waited for before the call stops waiting and says so.
  final Duration helperCheckLimit;

  /// How long the person's voice alone ([VoiceActivity], no words) holds
  /// back what the call has to say: a bound, so a noisy room cannot keep a
  /// report from being said.
  final Duration voiceLimit;

  /// How long a turn Jev judged cut off half-way ([cutOff]) is listened to
  /// for: the person stopped mid-sentence, and may be busy with something
  /// else for a while, so the call keeps listening. It is a safety bound,
  /// not a wait: at its end nothing is sent, the call asks whether to send
  /// what it heard or go on ([CallWords.goOnOrSend]). New words judged cut
  /// off again, or unmuting, start it again.
  final Duration unfinishedTurn;

  /// Jev's probability that the words fit the call, under which the call
  /// asks before sending them ([CallWords.forMe]): said to someone in the
  /// room, or making no sense here.
  final double offCall;

  /// The project's name, for Jev's context.
  final String? projectName;

  /// Said before the greeting: why this call is on the device (Gemini Live
  /// failed, ADR 0034).
  final String? intro;

  /// Jev's probability of a finished turn under which the turn is taken
  /// for cut off. Higher ones change nothing: the turn ends as without Jev.
  final double cutOff;

  /// How long a pause's words are left to settle before Jev is asked: the
  /// recognizer's words for the audio before the pause come a little late.
  final Duration judgeSettle;

  CallPhase phase = CallPhase.connecting;
  bool muted = false;

  /// The call's language, a locale such as `it-IT`: what the person speaks,
  /// what is said to them and what the agent is asked to write in.
  String language = 'en';
  CallWords get words => CallWords.of(language);
  String get _code => languageCode(language);

  /// What the person is saying now, then what they said last.
  String heard = '';

  /// What was last said to the person.
  String said = '';
  String? notice, problem;

  /// The conversation that works out the task: `(fromPerson, text)`.
  final intake = <(bool, String)>[];

  /// The task the project proposed, once it is well defined.
  TaskDraft? draft;

  /// The project's agent is checking the request; no work has started.
  bool validating = false;

  /// The task went to the project's agent; [completed] once it reported back,
  /// [queued] once it said the task waits behind a running one, [failed]
  /// once it reported the task failed.
  bool started = false, completed = false, queued = false, failed = false;

  /// The started task's id on the project's board: the last one started.
  String? taskId;

  /// The board id given to a proposal at its first start: kept with it.
  (TaskDraft, String)? _startId;

  /// The tasks started in this call, in order, with their board ids. Once
  /// the agent answered a start, the task is on the board and the call
  /// goes on: the person can work out and start the next one.
  final tasks = <(TaskDraft, String)>[];

  /// What the agent reported for each of [tasks], by id: queued, or over.
  final outcomes = <String, TaskOutcome>{};

  /// [tasks] whose report (complete or failed) is still to come.
  Iterable<String> get _pending => [
    for (final (_, id) in tasks)
      if (outcomes[id] != TaskOutcome.complete &&
          outcomes[id] != TaskOutcome.failed)
        id,
  ];

  /// The person took up something new since the last task was handed off:
  /// a report then is said but does not end the call.
  bool _talkedSinceHandOff = false;

  /// The agent asked the person something on its own (a task it runs is
  /// blocked): their next turn is its answer.
  String? _askedTask;

  /// How far the history has been read: each message is read once, by
  /// [_readHistory], and handed to what it is for. Null until the first
  /// read, which only notes where the call began.
  List? _read;

  /// What the agent's messages are read under: the message from the
  /// person's side they follow ([_After]), and for [_After.reply] the
  /// prompt it answers.
  _After _after = _After.own;

  /// The outstanding send's answer as read so far, and whether a report or
  /// question of this call's answered it (said through [_heard]).
  final _replyParts = <String>[];
  bool _replyAnswered = false;

  /// The agent's last message while it still writes it: read once its
  /// turn is over, condensed meanwhile when it is the reply.
  String? _writing;

  /// Prompts this call sent ([_plain]), and those of checks called off:
  /// what the agent says after one of those is not a reply to anything.
  final _ourPrompts = <String>{}, _calledOff = <String>{};

  /// What was read to be said, in the order it came: reports and questions
  /// about this call's tasks, and the agent's own messages.
  final _heard = <_Heard>[];

  /// The person asked the call to wait ("aspetta un attimo, devo fare una
  /// cosa"): nothing is said until they are back, and what the agent sends
  /// meanwhile is kept for then.
  bool paused = false;

  /// Just paused: a whole sentence now still goes on ([pauseGrace]).
  bool _pauseFresh = false;

  /// What was to be said while paused, with what goes with each: said, in
  /// order, once the person is back.
  final _keptWhilePaused = <_Kept>[];

  /// The agent reported the task over while the call was paused: the call
  /// ends once that is said.
  bool _endWhenBack = false;

  /// What the call last said that matters (an answer, a question, a
  /// reply), repeated when the person is back.
  String _recap = '';

  /// Waiting on the project's agent: a soft tone plays between what is said.
  bool get onHold =>
      !ended &&
      !paused &&
      (validating ||
          (started && !completed) ||
          (!_talkedSinceHandOff &&
              _pending.any((id) => outcomes[id] != TaskOutcome.queued)));

  /// The person added to the request while it was being checked.
  bool _addedWhileChecking = false;

  /// Bumped by every check, and when one is called off: a send or answer
  /// for an older one is dropped.
  int _checkId = 0;

  /// A called-off check still to be stopped on the host, which stops only
  /// a running agent: retried by the polls until it runs, or for a while.
  bool _stopPending = false, _stopInFlight = false;
  DateTime _stopUntil = DateTime(0);

  /// The session, prompt and history before it of the called-off check:
  /// the stop goes only to that session, and only while that check has not
  /// answered.
  Object? _stopSession;
  String _stopPrompt = '';
  List _stopBefore = const [];
  int? _stopCheck;

  /// The prompt of the check being sent, before the host has it.
  String _checkPrompt = '';

  /// The proposal and the check as they were when the turn being said
  /// began: a yes counts only for a proposal the person had heard.
  TaskDraft? _turnDraft;
  int? _turnCheck;

  /// The agent's question the call is about to say (it waits for the
  /// person to stop talking): words said before it are not its answer.
  /// And the one it said last: only that one is answered.
  String? _questionDue, _questionSaid;

  /// Bumped by every intake turn; an answer to an older one is dropped.
  int _intakeTurn = 0;

  /// Bumped when the person revises the request (a new intake turn, not a
  /// pause or a "wait"): what the project said about the older one is not
  /// said any more.
  int _revisions = 0;

  /// The intake answer being worked out: called off when a newer turn (or
  /// the person talking over it) makes it useless, so the model stops.
  Cancellation? _clarifying;

  /// The proposal as it was said (once it begins to be said), and the
  /// proposal last heard to the end: a yes counts only for one the person
  /// heard whole.
  (TaskDraft, String)? _proposal;
  TaskDraft? _heardDraft;

  /// The person's voice is on ([VoiceActivity]), until it stops or for
  /// [voiceLimit] at most.
  bool _voiceOn = false;
  Timer? _voiceTimer;

  /// The hold tone as last set on the device; null: not set yet.
  bool? _holdOn;

  Data? _chat;
  String _settled = '', _volatile = '';
  String? _sent, _speaking;

  /// The task the outstanding prompt is about ([_send]).
  String? _sentAbout;

  /// The history when the turn was sent; the reply is looked for only in
  /// what came after it.
  List _before = const [];
  bool _sawRunning = false, _polling = false, _acting = false, _closed = false;
  bool _disposed = false;
  DateTime? _workingSince, _lastNudge;

  /// The reply as last seen while the agent still ran, and its spoken form
  /// being prepared (condensed) meanwhile, so the end of the turn does not
  /// wait for it.
  String? _preparedFor;
  Future<String>? _prepared;
  bool _preparing = false;

  /// The last speech, and until when its echo may still be recognized.
  String _echo = '';
  DateTime _echoUntil = DateTime(0);

  /// Bumped by every [_say]; only the latest one moves the phase on.
  int _utterance = 0;

  /// Audio (seconds into the call) the current turn reaches, and up to where
  /// turns were already acted on: a late final for that audio is dropped.
  double? _turnEnd;
  double _consumed = -1;
  Timer? _silence, _poller;
  StreamSubscription<VoiceEvent>? _events;

  /// The last [TurnEstimate] for the turn being said: how finished it
  /// sounded, and where its pause began. Words that arrive late for audio
  /// before that pause are the recognizer catching up, not more speech.
  double? _soundedDone;
  double? _pauseAt;

  /// How often the person's voice held the turn since its last new words.
  int _voiceHeld = 0;

  /// The person's voice stopped in the turn being said (not [paused], the
  /// call asked to wait): Jev is asked about it then, not while they talk.
  bool _voicePaused = false;
  Timer? _judgeTimer;

  /// Bumped as each turn ends: an answer about an earlier turn is dropped,
  /// even for the very same words.
  int _judgeTurn = 0;

  /// Jev's last answer, for this text (plain), and the text being asked.
  String? _judgedText, _judging;
  double? _judged, _judgedFits;

  /// The call's question about words it did not send ([_Unsure]).
  _Unsure? _unsure;

  /// The turn being said is rechecked by Jev after a revision released a
  /// hold: it ends on Jev's answer, or on its timer.
  bool _rechecking = false;

  /// The one extension Jev gave the words it judged cut off: while it runs
  /// the turn does not end on silence, and when it runs out it ends. Holds,
  /// estimates and new arms never make it longer; only new words, judged
  /// cut off again, earn a new one.
  Timer? _jevBound;

  /// The silence ran out while [_jevBound] still held the turn.
  bool _heldByJev = false;

  /// Time since [_jevBound] began, for the countdown the page shows.
  final _jevClock = Stopwatch();

  /// The wait the turn being said ends after, while it runs, and a count
  /// bumped each time it starts again: the call page shows it running out.
  Duration? turnWait;
  int turnArmed = 0;

  /// The recognizer's segments in the turn being said, by where their audio
  /// starts (ms; null: not given), with the words each last reported in
  /// full; and the same for the segments already sent. The recognizer may
  /// report a sent segment again with the rest of the sentence: only the
  /// words that segment had are taken off.
  final _turnSegments = <int?, List<String>>{};
  final _sentSegments = <int?, List<String>>{};

  /// The last result taken into the turn being said, as it was heard.
  String _lastPiece = '';

  /// The outcome of a task report that came while the person still had
  /// follow-ups for the agent: the call ends once those are through.
  TaskOutcome? _outcomeAfterFollowUps;

  /// What the person said for the agent while it worked: sent as one
  /// message once it is idle.
  final _followUps = <String>[];

  /// Speech is held because the person's voice was heard over it; it goes
  /// on after [bargeWait] unless real words come.
  bool _held = false;
  Timer? _holdTimer;

  /// The person's last turn in [intake] whose answer has not been heard
  /// yet: what they say next goes with it. Set while the on-device answer is
  /// worked out, and kept when that answer is cut short just after it
  /// began.
  int? _openTurn;
  bool _answerBegun = false, _answerFresh = false, _answerCut = false;

  /// Ends [_answerFresh] and [_pauseFresh].
  Timer? _freshTimer, _pauseTimer;

  bool get ended => phase == CallPhase.ended;

  /// Whether this call speaks with the natural voice, fixed at its first
  /// words; null before.
  bool? _natural;

  /// The natural voice's speed here, latest first: how much audio is held
  /// back before it speaks ([speechPlan]).
  double _speed = 1;

  /// How fast the last turn was answered and spoken, for the call screen.
  final timings = CallTimings();

  /// The language can change between turns, not while a turn is on its way.
  bool get canSwitchLanguage =>
      phase == CallPhase.listening || phase == CallPhase.speaking;

  Future<String?> _condense(String text) =>
      assistant.condense(text, language: _code);

  /// Restarts recognition in [id] (null: the device's language), downloading
  /// its model first if needed. Later calls use it too.
  Future<void> switchLanguage(String? id) async {
    if (_closed || !canSwitchLanguage) return;
    _silence?.cancel();
    _silence = turnWait = null;
    _forgetJudgement();
    // A question about words in the old language is moot.
    _unsure = null;
    _settled = _volatile = heard = '';
    _turnSegments.clear();
    _sentSegments.clear();
    _turnEnd = null;
    // The new recognizer counts audio from zero again.
    _consumed = -1;
    _utterance++;
    _speaking = null;
    // Another language, another voice: chosen again at the next words.
    _natural = null;
    phase = CallPhase.connecting;
    notifyListeners();
    try {
      await voice.stopSpeaking();
      await voice.setLanguage(id);
      language = id ?? await _deviceLanguage() ?? language;
      await voice.start();
    } catch (e) {
      if (!_closed) problem = e.toString();
      await hangUp();
      return;
    }
    if (_closed) {
      await voice.stop();
      return;
    }
    notice = null;
    // The restarted voice has no hold tone yet.
    _holdOn = null;
    await _say(words.goAhead);
  }

  /// The language calls use now, as the device reports it; also how fast
  /// the natural voice is here, and whether its timings are shown.
  Future<String?> _deviceLanguage() async {
    try {
      final l = await voice.languages();
      if (l.speed case final speed? when speed > 0) _speed = speed;
      timings.shown = l.showTimings;
      return l.current.isEmpty ? null : l.current;
    } catch (_) {
      return null;
    }
  }

  Future<void> start() async {
    _events = voice.events.listen(_onEvent);
    // What the project does, for the conversation's models and the check;
    // not waited for: the first turn is seconds away.
    unawaited(_knowProject());
    try {
      await Future.wait([
        _refresh(),
        _deviceLanguage().then((l) => language = l ?? language),
      ]);
      // Hung up while the chat was fetched: never open the microphone.
      if (_closed) return;
      await voice.start();
    } catch (e) {
      if (!_closed) problem = e.toString();
      await hangUp();
      return;
    }
    if (_closed) {
      // Hung up while the microphone opened: close it again.
      await voice.stop();
      return;
    }
    _holdOn = null;
    // The Mac's model loads, and the link is measured, before a turn needs it.
    if (assistant.mac case final RemoteModel mac) unawaited(mac.prewarm());
    if (assistant.phone is PhoneModel) {
      unawaited(voice.prewarmPhoneModel());
      unawaited(_watchPhoneModel());
    }
    // One tick at the fast rate; while listening only every few ticks.
    final tick = pollWorking < poll ? pollWorking : poll;
    final idleEvery = max(
      1,
      (poll.inMicroseconds / tick.inMicroseconds).round(),
    );
    var ticks = 0;
    _poller = Timer.periodic(tick, (_) {
      final waiting = phase == CallPhase.working || phase == CallPhase.sending;
      if (waiting || ++ticks % idleEvery == 0) _refresh();
    });
    await _say(intro == null ? words.greet : '$intro ${words.greet}');
  }

  /// The project's functional memory, given to [assistant]; a host without
  /// it (or too old to say) leaves the call as it was.
  Future<void> _knowProject() async {
    final (memory, truncated) = await functionalMemory(call, target);
    if (memory.isNotEmpty && !_closed) {
      assistant.know(memory, truncated: truncated);
    }
  }

  /// While the on-phone assistant downloads, the call says how far along.
  Future<void> _watchPhoneModel() async {
    String? shown;
    while (!_closed) {
      final state = await voice.phoneModel();
      final progress = state.progress;
      if (!state.on || state.ready || progress == null) break;
      // Only over its own notice, never over another.
      if (notice == null || notice == shown) {
        notice = shown = state.waiting
            ? 'The on-phone assistant downloads on Wi-Fi.'
            : 'Downloading the on-phone assistant… ${(progress * 100).round()}%';
        notifyListeners();
      }
      await Future<void>.delayed(const Duration(seconds: 1));
    }
    if (shown != null && notice == shown && !_closed) {
      notice = null;
      notifyListeners();
    }
  }

  /// Muting pauses the turn being said, not ends it: its words are kept
  /// and its silence stops, and starts again (patiently) once unmuted.
  Future<void> toggleMute() async {
    muted = !muted;
    _voicePaused = _heldByJev = false;
    _judgeTimer?.cancel();
    if (muted) {
      _silence?.cancel();
      _silence = turnWait = null;
    } else if (!_closed && '$_settled$_volatile'.trim().isNotEmpty) {
      // Back from muting, words left half-way are listened to afresh.
      final text = '$_settled $_volatile';
      if (_cutOffNow(text) && _mayHold(text)) _startJevBound();
      _armSilence(hold: true);
    }
    notifyListeners();
    await voice.mute(muted);
  }

  Future<void> hangUp() async {
    if (_closed) return;
    _closed = true;
    _silence?.cancel();
    _judgeTimer?.cancel();
    _jevBound?.cancel();
    turnWait = null;
    _clarifying?.cancel();
    _voiceTimer?.cancel();
    _holdTimer?.cancel();
    _freshTimer?.cancel();
    _pauseTimer?.cancel();
    _poller?.cancel();
    phase = CallPhase.ended;
    notifyListeners();
    unawaited(_events?.cancel());
    try {
      await voice.stop();
    } catch (_) {
      // Already stopped.
    }
  }

  @override
  void notifyListeners() {
    // The hold tone follows the state, wherever it changed.
    final hold = onHold;
    if (hold != _holdOn && (!_closed || _holdOn == true)) {
      _holdOn = hold;
      unawaited(voice.hold(hold).catchError((_) {}));
    }
    if (!_disposed) super.notifyListeners();
  }

  @override
  void dispose() {
    _disposed = true;
    hangUp();
    super.dispose();
  }

  void _onEvent(VoiceEvent event) {
    if (_closed) return;
    switch (event) {
      case FirstSound():
        timings.firstSound();
      case VoiceTiming():
        if (event.speed > 0) _speed = event.speed;
        timings.measured(event);
      case Listening():
        if (phase == CallPhase.connecting) phase = CallPhase.listening;
      case VoiceStatus(:final text):
        notice = text.isEmpty ? null : text;
      case VoiceDownload(:final fraction):
        notice = fraction == null || fraction >= 1
            ? 'Downloading the speech model…'
            : 'Downloading the speech model… ${(fraction * 100).round()}%';
      case VoiceFailed(:final message):
        problem = message;
        hangUp();
        return;
      case Heard(text: final said, :final isFinal, :final start, :final end):
        var text = said;
        // While muted, no audio reaches the recognizer: what it reports
        // then is for words said before muting. It is kept for the turn,
        // which waits ([toggleMute]): no silence, no Jev, nothing sent.
        if (text.trim().isEmpty) return;
        // A result that begins in audio already acted on is a revision of a
        // sent turn (a final can end later than the partial that was sent)
        // unless it reaches well past it with more words: the person went on
        // after the turn was sent, and the recognizer reports the whole
        // segment. Those words are kept as a turn of their own.
        var segment = start == null ? null : (start * 1000).round();
        if ((start ?? end) case final from? when from < _consumed - 0.05) {
          final sent = end != null && end > _consumed + 0.25
              ? _sentSegment(segment)
              : null;
          final rest = sent == null ? null : _wordsAfter(sent.$2, text);
          if (sent == null || rest == null) return;
          segment = sent.$1;
          text = rest;
        } else if (start == null && end != null && end <= _consumed + 0.05) {
          return;
        }
        // Echo cancellation removes most speech from the microphone; what
        // leaks through is recognized as the speech's own words and ignored.
        final words = _plain(text);
        final command = isStop(words) || isHangUp(words) || isHoldOn(words);
        if (!command &&
            (_speaking != null || DateTime.now().isBefore(_echoUntil)) &&
            _echoOf(words, _echo)) {
          return;
        }
        // Words while the answer is worked out: it will not be said (they
        // are going on), so its model stops now.
        if (!muted && phase == CallPhase.thinking) _clarifying?.cancel();
        if (!muted && _speaking != null) {
          // "Mh", "sì", "ok" over the reply say the person follows: it goes
          // on. Real words stop it: one once their voice held it, else two.
          // So over anything said, while the agent works too.
          if (!command &&
              (isBackchannel(words) ||
                  words.split(' ').length < (_held ? 1 : 2))) {
            return;
          }
          _bargeIn();
        }
        if (_settled.isEmpty && _volatile.isEmpty) {
          _turnDraft = draft;
          _turnCheck = validating ? _checkId : null;
        }
        final before = _plain('$_settled $_volatile');
        final reached = _turnEnd;
        if (end != null) _turnEnd = max(_turnEnd ?? end, end);
        _turnSegments[segment] = _words(said);
        _lastPiece = text;
        if (isFinal) {
          _settled = '$_settled $text'.trim();
          _volatile = '';
        } else {
          _volatile = text;
        }
        heard = '$_settled $_volatile'.trim();
        notice = null;
        // Silence counts from the last new words. A final that only settles
        // (or re-punctuates) what was already heard, or revises audio already
        // covered, is not new speech: the recognizer finalizing a long
        // utterance would otherwise keep the turn open.
        final newAudio = end == null || reached == null || end > reached + 0.05;
        final newWords = _plain(heard) != before;
        if (newWords) _voiceHeld = 0;
        if (!muted && (_silence == null || (newWords && newAudio))) {
          // Words for audio before the pause just judged are the recognizer
          // catching up: the judgement stands. Later ones are more speech.
          final pause = _pauseAt;
          final catchingUp =
              pause != null && end != null && end <= pause + 0.15;
          if (!catchingUp) _soundedDone = _pauseAt = null;
          _armSilence();
        } else if (!muted && newWords) {
          // The recognizer revising words it already reported (same audio):
          // not new speech, so the silence goes on, but Jev's verdict was
          // for the old words. The new ones are judged; a hold that only
          // they kept, once the silence has run out, is let go.
          _askJudgeSoon();
          if (_heldByJev && !_jevHolds) {
            // The silence ran out long ago: the revised words end the turn
            // once Jev has judged them (cut off, side talk), or within the
            // time an answer may take.
            _heldByJev = false;
            _rechecking = true;
            final recheck = judgeSettle + judge.deadline + TurnJudge.grace;
            _silence?.cancel();
            _silence = Timer(recheck, _silenceOver);
            turnWait = recheck;
            turnArmed++;
          }
        }
      case VoiceActivity(:final speaking):
        if (muted) return;
        _voiceOn = speaking;
        _voiceTimer?.cancel();
        if (speaking) _voiceTimer = Timer(voiceLimit, () => _voiceOn = false);
        _voicePaused = !speaking;
        if (!speaking) {
          _askJudgeSoon();
          return;
        }
        _judgeTimer?.cancel();
        // The person's voice over speech: hold it to hear them out. Words
        // decide whether it stops ([Heard]) or goes on ([bargeWait]).
        if (_speaking != null && !_held) {
          _held = true;
          unawaited(voice.pauseSpeaking().catchError((_) {}));
          _holdTimer?.cancel();
          _holdTimer = Timer(bargeWait, _letSpeechGoOn);
          if (_silence == null) return;
        }
        // Their voice again while a turn waits to end: they are going on.
        // It arrives well before the recognizer's words, so the turn is held
        // open at once, for the patient wait; the words then set it as
        // usual. Bounded, so a noisy room still lets the turn end.
        if (_silence != null && _voiceHeld < voiceHolds) {
          _voiceHeld++;
          _soundedDone = _pauseAt = null;
          _armSilence(hold: true);
          notifyListeners();
        }
        return;
      case TurnEstimate(:final complete, :final at):
        if (muted || _settled.isEmpty && _volatile.isEmpty) return;
        _soundedDone = complete;
        _pauseAt = at;
        // Once the voice holds are spent with no new words, a pause in the
        // noise no longer starts the wait again: the turn ends on time.
        if (_voiceHeld >= voiceHolds) return;
        _armSilence();
        notifyListeners();
        return;
    }
    notifyListeners();
  }

  /// (Re)starts the silence that ends the turn being said: at once when it
  /// ends on a word that sends it, short when it sounded finished, long when
  /// it sounded unfinished, trails off on an open word or the person's voice
  /// came back ([hold]), else [endOfTurn].
  void _armSilence({bool hold = false}) {
    final text = '$_settled $_volatile';
    final done = _soundedDone;
    final onPurpose = _onPurpose(text) != null;
    final wait = hold
        ? patientTurn
        : onPurpose
        ? sentOnPurpose
        : trailsOff(text) || (done != null && done < maybeDone)
        ? patientTurn
        : done != null && done >= sureDone
        ? quickTurn
        : endOfTurn;
    _heldByJev = _rechecking = false;
    _askJudgeSoon();
    _silence?.cancel();
    _silence = Timer(wait, _silenceOver);
    // The page counts down to what really ends the turn: Jev's hold, while
    // it is longer than this silence.
    final held = _jevHolds ? unfinishedTurn - _jevClock.elapsed : Duration.zero;
    turnWait = held > wait ? held : wait;
    turnArmed++;
  }

  /// The silence ran out: the turn ends, unless Jev's extension still
  /// holds it, which then ends it ([_jevBoundOver]).
  void _silenceOver() {
    _rechecking = false;
    if (_jevHolds) {
      _heldByJev = true;
      return;
    }
    _turnEnded();
  }

  void _jevBoundOver() {
    if (!_heldByJev || muted || _closed || _silence == null) return;
    _turnEnded();
    notifyListeners();
  }

  /// Whether Jev's extension holds the turn being said: it is running, and
  /// the words are the ones it judged cut off.
  bool get _jevHolds {
    final text = '$_settled $_volatile';
    return (_jevBound?.isActive ?? false) && _mayHold(text) && _cutOffNow(text);
  }

  /// Whether Jev's "cut off" may hold [text]: never a turn the person sent
  /// with a word, nor a call command, nor while muted or asked to wait.
  bool _mayHold(String text) =>
      !muted && !paused && _onPurpose(text) == null && !isCallCommand(text);

  void _startJevBound() {
    _jevBound?.cancel();
    _jevBound = Timer(unfinishedTurn, _jevBoundOver);
    _jevClock
      ..reset()
      ..start();
  }

  /// The call so far, for Jev to tell whether the person's words fit it:
  /// the project, the task, the conversation, what the agent asked and
  /// what was last said to the person. Whole pieces only.
  String? _judgeContext() {
    final d = draft;
    final project = projectName ?? '${target['project'] ?? ''}';
    return TurnJudge.contextOf(
      [
        if (project.isNotEmpty) 'Project: $project',
        if (d != null)
          started
              ? 'Task the agent is working on: ${d.title}. ${d.goal}'
              : 'Task being shaped: ${d.title}. ${d.goal}',
        if (started && d == null) 'The agent is working on the request.',
      ],
      [
        'Conversation so far:',
        for (final (person, text) in intake)
          '${person ? 'Person' : 'Assistant'}: $text',
        if (_questionSaid case final q?) 'The agent asked: $q',
        if (said.isNotEmpty) 'Last said to the person: $said',
      ],
    );
  }

  /// Whether Jev judged [text], as it is now, cut off half-way.
  bool _cutOffNow(String text) {
    final judged = _judged;
    return judged != null && judged < cutOff && _judgedText == _plain(text);
  }

  /// Whether Jev may be asked about [text]: not while the call is asked to
  /// wait ([paused]: side talk, and "eccomi"), nor for a turn sent on
  /// purpose, which goes at once.
  bool _mayJudge(String text) =>
      !paused && !muted && !_closed && judge.available && !isCallCommand(text);

  /// During a pause, asks Jev about the words so far once they settle,
  /// unless it was already asked about them.
  void _askJudgeSoon() {
    final text = '$_settled $_volatile';
    if (!_voicePaused || !_mayJudge(text)) return;
    final key = _plain(text);
    if (key.isEmpty || key == _judgedText || key == _judging) return;
    _judgeTimer?.cancel();
    // Sent with a word, it goes in a moment: asked at once, for the fit.
    final soon = _onPurpose(text) != null ? Duration.zero : judgeSettle;
    _judgeTimer = Timer(soon, _askJudge);
  }

  Future<void> _askJudge() async {
    final text = '$_settled $_volatile'.trim();
    final key = _plain(text);
    if (_silence == null || !_mayJudge(text) || key.isEmpty) return;
    if (key == _judging) return;
    final turn = _judgeTurn;
    _judging = key;
    // Without the word that sends it: that is what would go.
    final scores = await judge.judge(
      _onPurpose(text) ?? text,
      context: _judgeContext() ?? '',
    );
    // The turn ended meanwhile, or newer words are being asked about.
    if (turn != _judgeTurn || _judging != key || _closed) return;
    _judging = null;
    if (scores == null) return;
    final complete = scores.complete;
    _judgedText = key;
    _judged = complete;
    _judgedFits = scores.fits;
    // Still these words, still waiting to end: a cut-off turn is held for
    // [unfinishedTurn] from now, once. A finished one keeps its wait.
    final same = _silence != null && _plain('$_settled $_volatile') == key;
    if (complete < cutOff && same && _mayHold(text)) {
      _startJevBound();
      turnWait = unfinishedTurn;
      turnArmed++;
      notifyListeners();
    }
    // A revision was being rechecked: it ends now on this answer (or is
    // held, cut off), not at the end of the recheck's timer.
    if (_rechecking && same) {
      _rechecking = false;
      _silence?.cancel();
      _silenceOver();
      notifyListeners();
    }
  }

  /// [text] without the send word it ends on, when the person sent it on
  /// purpose; null otherwise.
  String? _onPurpose(String text) =>
      withoutSendWord(text, separate: _isSendWord(_lastPiece));

  /// The sent segment a result starting at [segment] (ms) reports again:
  /// the one starting there, or for a result with no start, the latest.
  (int?, List<String>)? _sentSegment(int? segment) {
    if (_sentSegments.isEmpty) return null;
    if (segment == null) {
      final last = _sentSegments.entries.last;
      return (last.key, last.value);
    }
    for (final MapEntry(:key, :value) in _sentSegments.entries) {
      if (key != null && (key - segment).abs() <= 100) return (key, value);
    }
    return null;
  }

  /// Whether a turn is waiting to end: [sendNow] sends it.
  bool get canSendNow => _silence != null && !muted && !_closed;

  /// Sends the turn being said now, without waiting for the silence (a tap
  /// on the call page).
  void sendNow() {
    if (!canSendNow) return;
    _silence?.cancel();
    _turnEnded(byHand: true);
    notifyListeners();
  }

  /// Real words over the reply: it stops, and the turn is the person's.
  void _bargeIn() {
    _releaseHold();
    // While the agent works, the call stays on it.
    if (phase == CallPhase.speaking) phase = CallPhase.listening;
    unawaited(voice.stopSpeaking().catchError((_) {}));
    // Cut just after it began: the person had not finished.
    if (_openTurn != null && _answerFresh) _answerCut = true;
  }

  /// Held speech heard no real words: it goes on.
  void _letSpeechGoOn() {
    if (!_held) return;
    _releaseHold();
    if (!_closed && _speaking != null) {
      unawaited(voice.resumeSpeaking().catchError((_) {}));
    }
  }

  void _releaseHold() {
    _held = false;
    _holdTimer?.cancel();
    _holdTimer = null;
  }

  /// The words being said are gone (sent, or dropped): Jev's answers,
  /// asks and hold for them go too, so the next words are judged afresh.
  void _forgetJudgement() {
    _voicePaused = _heldByJev = _rechecking = false;
    _judgeTimer?.cancel();
    _jevBound?.cancel();
    _jevBound = null;
    _judgeTurn++;
    _judgedText = _judging = null;
    _judged = _judgedFits = null;
  }

  /// The turn ends. [byHand]: the person tapped to send it, so it is not
  /// asked about.
  void _turnEnded({bool byHand = false}) {
    final heardNow = '$_settled $_volatile'.trim();
    // Jev's verdict on exactly these words, taken before it is forgotten.
    final cut = _cutOffNow(heardNow);
    final fits = _judgedText == _plain(heardNow) ? _judgedFits : null;
    timings.turnSent();
    _silence = null;
    turnWait = null;
    _soundedDone = _pauseAt = null;
    _voiceHeld = 0;
    _forgetJudgement();
    unawaited(voice.endTurn().catchError((_) {}));
    final said = '$_settled $_volatile'.trim();
    _settled = _volatile = '';
    if (_turnEnd case final end?) _consumed = max(_consumed, end);
    _turnEnd = null;
    // Its segments as the recognizer has them, to know them again if it
    // reports one once more with the rest of the sentence. Only the latest
    // few can still go on.
    for (final MapEntry(:key, :value) in _turnSegments.entries) {
      _sentSegments.remove(key);
      _sentSegments[key] = value;
    }
    while (_sentSegments.length > 8) {
      _sentSegments.remove(_sentSegments.keys.first);
    }
    _turnSegments.clear();
    // "…, vai": the word that sent it is not part of the message.
    final onPurpose = _onPurpose(said);
    final message = onPurpose ?? said;
    _lastPiece = '';
    if (said.isEmpty || _closed) return;
    // Asked about first: words that did not seem meant for the call, even
    // sent by hand or with a word (Jev's known verdict; without one they go
    // as before), and words left half-way whose hold ran out (not when sent
    // by hand or with a word). Never a call command. Not fitting comes
    // first: a reply then replaces the words, it does not go on with them.
    final doubt = isCallCommand(message)
        ? null
        : fits != null && fits < offCall
        ? _Doubt.notForCall
        : cut && !byHand && onPurpose == null
        ? _Doubt.unfinished
        : null;
    _handle(message, _turnDraft, _turnCheck, doubt);
  }

  /// Acts on a turn; [draftThen] and [checkThen] are the proposal and check
  /// as they were when it began.
  Future<void> _handle(
    String text, [
    TaskDraft? draftThen,
    int? checkThen,
    _Doubt? doubt,
  ]) async {
    if (isHangUp(text)) {
      await hangUp();
      return;
    }
    if (paused) {
      final rest = resumeRest(text);
      final soon = _pauseFresh;
      if (rest != null) {
        await _resume(rest);
      } else if (soon && _plain(text).split(' ').length >= 3) {
        // Right after "un attimo" a whole sentence: they went on.
        await _resume(text);
      } else {
        // Talk with someone else in the room, or to themselves.
        notice = 'Paused. Say "${_backHint()}" to go on.';
        notifyListeners();
      }
      return;
    }
    if (isHoldOn(text)) {
      // "Aspetta": the person wants to say something. Speech already
      // stopped; any answer still being worked out is dropped, and what
      // they say next goes with their last turn.
      if (_openTurn != null) _answerCut = true;
      _supersedeIntake();
      if (phase == CallPhase.thinking) phase = CallPhase.listening;
      notifyListeners();
      return;
    }
    if (isPause(text)) {
      await _pause();
      return;
    }
    if (!started && isStop(text) && (validating || checkThen != null)) {
      await _callOffCheck(draftThen);
      return;
    }
    // The call asked about the last words: a yes sends them as they were,
    // "no" lets them go, more side talk leaves the question open (nothing
    // goes), and anything else is what the person meant (after words left
    // half-way, it goes on with them). An answer that comes while the
    // question still plays waits for how it went: undelivered, or no longer
    // the last thing said, the question is over, the words are let go and
    // this turn is taken as it is.
    var kept = _unsure;
    if (kept != null) {
      final spoken = await kept.asked;
      if (_closed) return;
      if (!identical(_unsure, kept)) {
        // Another turn settled it meanwhile.
        kept = null;
      } else if (!kept.deliveredBy(spoken) || kept.utterance != _utterance) {
        _unsure = kept = null;
      }
    }
    if (kept != null) {
      if (agrees(text)) {
        _unsure = null;
        await _handle(kept.text, kept.draft, kept.check);
        return;
      }
      if (dismisses(text)) {
        _unsure = null;
        await _say(words.leftOut);
        return;
      }
      if (doubt == _Doubt.notForCall) {
        notice = 'Waiting for your answer.';
        notifyListeners();
        return;
      }
      _unsure = null;
      if (kept.unfinished) text = '${kept.text} $text';
    } else if (doubt != null) {
      final template = doubt == _Doubt.unfinished
          ? words.goOnOrSend
          : words.forMe;
      final quote = quoted(text);
      final at = template.indexOf('{said}');
      final ask = template.replaceFirst('{said}', quote);
      // Heard through the quoted words means up to where they end in the
      // question, and the character that closes them: found from where they
      // were put, never by looking for quotes, which the words may hold.
      final after = at + quote.length;
      final quoteEnd = at < 0
          ? ask.length
          : after < ask.length && ask[after] == '"'
          ? after + 1
          : after;
      final asking = _say(ask);
      // [_say] has taken its turn: the question is this utterance.
      final question = _unsure = _Unsure(
        text: text,
        draft: draftThen,
        check: checkThen,
        unfinished: doubt == _Doubt.unfinished,
        utterance: _utterance,
        asked: asking,
        quoteEnd: quoteEnd,
      );
      final spoken = await asking;
      // A question that failed, or was cut before it was heard through
      // what it quotes, asked nothing: a later "yes" must not send.
      if (!question.deliveredBy(spoken) && identical(_unsure, question)) {
        _unsure = null;
      }
      return;
    }
    final chat = _chat;
    final status = chat?['status'] as String?;
    final waiting = phase == CallPhase.working || phase == CallPhase.sending;
    if (waiting || (started && status == 'Running')) {
      if (isStop(text) && status == 'Running') {
        try {
          await call('stop', {...target, 'id': chat?['id']});
          await _say(words.stopping);
        } catch (e) {
          problem = e.toString();
          await _say(words.couldNotStop);
        }
      } else if (validating) {
        // On hold while the project checks: what is added goes with it.
        intake.add((true, text));
        _addedWhileChecking = true;
        await _say(words.noted);
      } else {
        // The agent can't take a message while it works: it gets it as a
        // follow-up once its turn is over. Often it is the rest of a
        // sentence that was cut short.
        _followUps.add(text);
        await _say(words.noted);
      }
      return;
    }
    if (!started && !_answersAgent) {
      // The proposal was there when the turn began, and was heard to the end.
      final current = draft != null && identical(draft, draftThen);
      final heardIt = current && identical(_heardDraft, draft);
      if (heardIt && agrees(text)) {
        await startTask();
      } else if (current && agrees(text) && identical(_proposal?.$1, draft)) {
        // A yes to a proposal cut short: it is said again, whole, and a yes
        // after that agrees.
        await _proposeAgain();
      } else if (draft != null && !heardIt && agrees(text)) {
        // A yes said before the proposal came: it is being said now, and
        // agreeing takes a yes after it.
        return;
      } else {
        // Anything else revises the request: a later yes must not start
        // the task as it was.
        draft = null;
        await _converse(_goOnWith(text));
      }
      return;
    }
    // The agent asks something the call is about to say: these words were
    // said before it, so they are not its answer. They go to the agent once
    // the question is answered.
    final asked = chat?['question'] as String?;
    final unheard =
        asked != null &&
        chat?['textAnswer'] == true &&
        (_questionDue != null ||
            (_questionSaid != null && asked != _questionSaid));
    // What this turn answers: the blocked task's question, if one was asked.
    // It stays the open question until its answer is sent.
    final about = _askedTask ?? taskId;
    if (unheard) {
      _followUps.add(text);
      if (_questionDue != null) {
        // A question is about to be said (the host's latest, as of then).
        await _say(words.noted);
      } else {
        // The host asks something else than the call said: that is asked
        // now, and the answer after it is its answer.
        _questionDue = asked;
        await _deliver('', now: _asking);
      }
      return;
    }
    if (await _send(text, about: about) && _askedTask == about) {
      _askedTask = null;
    }
  }

  /// A turn after a task was handed off answers the agent, not the
  /// conversation, when it asked something: in a message of its own (a
  /// blocked task) or as the host's question in words. Not while a new
  /// task is being shaped.
  bool get _answersAgent {
    if (tasks.isEmpty || validating) return false;
    // A blocked task's question, said after anything else the call asked.
    if (_askedTask != null) return true;
    final chat = _chat;
    final question = chat?['question'] as String?;
    if (question == null || chat?['textAnswer'] != true) return false;
    // The host's question in words, once the call said it (or is about
    // to), comes before a proposal; with none open, always.
    return draft == null || _questionSaid == question || _questionDue != null;
  }

  /// The call speaks a question that interrupts a proposal: a yes from now
  /// on answers it, and the proposal needs reading back, and a yes after
  /// that, before it can start.
  void _interruptProposal() {
    _heardDraft = null;
  }

  /// One turn of the conversation, on the device: an answer at once, or,
  /// once the request is nearly clear, a check with the project.
  Future<void> _converse(String text) async {
    if (tasks.isNotEmpty) _talkedSinceHandOff = true;
    _revisions++;
    intake.add((true, text));
    _openTurn = intake.length - 1;
    _answerBegun = _answerFresh = _answerCut = false;
    final cancel = _supersedeIntake();
    final mine = _intakeTurn;
    phase = CallPhase.thinking;
    notifyListeners();
    // Heard at once, before the model answers.
    unawaited(voice.cue().catchError((_) {}));
    Intake? answer;
    try {
      answer = await assistant.clarify(intake, language: _code, cancel: cancel);
    } catch (_) {
      // Taken as no model.
    }
    // The person spoke again meanwhile: that turn answers instead. One
    // still being said will be taken with this one ([_goOnWith]).
    if (_closed || mine != _intakeTurn) return;
    // Their voice, before any words: wait to hear whether they go on.
    while (!_closed &&
        !muted &&
        _voiceOn &&
        !cancel.cancelled &&
        mine == _intakeTurn &&
        _settled.isEmpty &&
        _volatile.isEmpty) {
      await Future<void>.delayed(_quietCheck);
    }
    if (_closed || mine != _intakeTurn) return;
    if (_settled.isNotEmpty || _volatile.isNotEmpty || cancel.cancelled) {
      _answerCut = true;
      if (phase == CallPhase.thinking) phase = CallPhase.listening;
      notifyListeners();
      return;
    }
    _clarifying = null;
    final say = answer == null ? '' : speakable(answer.say, words);
    if (answer == null || answer.check || say.isEmpty) {
      // Without the on-device model the project helps from the first turn.
      _openTurn = null;
      await _check();
      return;
    }
    intake.add((false, say));
    final at = intake.length - 1;
    phase = CallPhase.listening;
    _answerBegun = _answerFresh = true;
    _freshTimer?.cancel();
    _freshTimer = Timer(resumeWindow, () => _answerFresh = false);
    _recap = say;
    _keepHeard(at, say, await _say(say));
  }

  /// A new intake turn: the answer being worked out for the last one, if
  /// any, is called off. Returns the new turn's own.
  Cancellation _supersedeIntake() {
    _clarifying?.cancel();
    _intakeTurn++;
    return _clarifying = Cancellation();
  }

  /// An answer kept in [intake] at [at] as [said], once spoken: when the
  /// person cut it short, the history keeps only what they heard of it.
  void _keepHeard(int at, String said, Spoken spoken) {
    if (spoken.completed) return;
    if (at >= intake.length || intake[at] != (false, said)) return;
    intake[at] = (false, heardBeforeCut(said, spoken.heard));
  }

  /// Says the proposal again, whole: the last time it was cut short.
  Future<void> _proposeAgain() async {
    final (d, speech) = _proposal ?? (null, '');
    if (d == null || !identical(d, draft)) return;
    var at = -1;
    await _deliver(
      speech,
      still: () => identical(d, draft),
      commit: () {
        intake.add((false, speech));
        at = intake.length - 1;
      },
      heard: (spoken) => _keepHeard(at, speech, spoken),
    );
  }

  /// [text], or, when the person was still going on with their last turn
  /// (its answer was not heard: still being worked out, or cut short just
  /// after it began), that turn and [text] together, in its place.
  String _goOnWith(String text) {
    final open = _openTurn;
    final goesOn =
        open != null &&
        open == intake.length - 1 - (intake.last.$1 ? 0 : 1) &&
        intake[open].$1 &&
        (!_answerBegun || _answerCut);
    _openTurn = null;
    if (!goesOn) return text;
    final joined = '${intake[open].$2} $text';
    intake.removeRange(open, intake.length);
    return joined;
  }

  /// The person asked the call to wait: it says so once and keeps quiet
  /// until they are back.
  Future<void> _pause() async {
    paused = true;
    _pauseFresh = true;
    _pauseTimer?.cancel();
    _pauseTimer = Timer(pauseGrace, () => _pauseFresh = false);
    // An answer still being worked out is dropped.
    _supersedeIntake();
    _openTurn = null;
    if (phase == CallPhase.thinking) phase = CallPhase.listening;
    notifyListeners();
    await _say(words.paused);
  }

  /// Ends the pause, from a spoken "eccomi" ([rest] is what followed it) or
  /// the Resume button.
  Future<void> resume() => _resume('');

  Future<void> _resume(String rest) async {
    if (!paused || _closed) return;
    paused = false;
    notice = null;
    notifyListeners();
    if (_keptWhilePaused.isNotEmpty) {
      final kept = [..._keptWhilePaused];
      _keptWhilePaused.clear();
      // In order, each with what goes with it; a new pause keeps the rest.
      for (final k in kept) {
        await _deliver(
          k.text,
          still: k.still,
          commit: k.commit,
          heard: k.heard,
          now: k.now,
        );
      }
      if (_endWhenBack) await _complete();
      return;
    }
    if (rest.split(' ').where((w) => w.isNotEmpty).length >= 2) {
      await _handle(rest, draft, validating ? _checkId : null);
      return;
    }
    // Back with nothing more: where things stood, as a reminder.
    final recap = _recap.length <= 200 ? _recap : _lastSentence(_recap);
    await _say(
      recap.isEmpty ? words.back : '${words.back} ${words.recap} $recap',
    );
  }

  String _backHint() => languageCode(language) == 'it' ? 'eccomi' : "I'm back";

  /// Calls off the check at once: its answer, even one already in, is
  /// dropped, and the agent is stopped as soon as the host can.
  Future<void> _callOffCheck(TaskDraft? draftThen) async {
    // Its answer already came: only that is dropped, nothing runs to stop.
    final awaited = validating;
    validating = _addedWhileChecking = false;
    _stopCheck = _checkId++;
    _stopSession = _chat?['id'];
    _stopPrompt = _sent ?? _checkPrompt;
    _stopBefore = _before;
    // What the agent says after it answers nothing any more.
    _calledOff.add(_plain(_stopPrompt));
    _sent = null;
    if (!identical(draft, draftThen)) draft = null;
    if (phase == CallPhase.working || phase == CallPhase.sending) {
      phase = CallPhase.listening;
    }
    _stopPending = awaited;
    _stopUntil = DateTime.now().add(const Duration(seconds: 20));
    notifyListeners();
    if (awaited && _chat?['status'] == 'Running') await _stopCalledOff();
    await _say(words.stopping);
  }

  /// Stops the agent running a called-off check; retried by the polls.
  Future<void> _stopCalledOff() async {
    if (_stopInFlight || !_calledOffIsCurrent(_chat)) return;
    _stopInFlight = true;
    try {
      await call('stop', {...target, 'id': _stopSession});
      _stopPending = false;
    } catch (_) {
      // The next poll that sees it running tries again.
    } finally {
      _stopInFlight = false;
    }
  }

  /// The called-off check is the session's current turn: its prompt is the
  /// latest message from the person. Anything else is not stopped.
  bool _calledOffIsCurrent(Data? chat) {
    if (chat == null || chat['id'] != _stopSession || _stopPrompt.isEmpty) {
      return false;
    }
    final messages = chat['messages'] as List? ?? const [];
    for (final m in messages.reversed) {
      if ((m as List)[0] == true) {
        return _plain('${m[1]}') == _plain(_stopPrompt);
      }
    }
    return false;
  }

  /// Puts the call on hold and asks the project's agent to check the
  /// request against the code, without starting work.
  Future<void> _check({bool announce = true}) async {
    if (_closed) return;
    _openTurn = null;
    _checkId++;
    validating = true;
    _addedWhileChecking = false;
    final prompt = validationPrompt(
      intake,
      language,
      memory: assistant.knowsProject,
    );
    // Said while the message goes out: neither waits for the other.
    final speech = announce ? _say(words.checking) : Future.value();
    final mine = _checkId;
    if (!await _send(prompt, cue: !announce) && mine == _checkId) {
      validating = false;
      // A re-check starts while the last one is still awaited.
      _sent = null;
      if (phase == CallPhase.working || phase == CallPhase.sending) {
        phase = CallPhase.listening;
      }
      notifyListeners();
    }
    await speech;
  }

  /// The project's answer to a check: its questions are asked, a
  /// well-defined task is proposed.
  Future<void> _validated(String reply) async {
    validating = false;
    // What the call says now is what the next turn answers.
    _askedTask = null;
    // The check's answer: taken, once.
    _sent = null;
    if (_addedWhileChecking) {
      // The answer predates what the person added: check again with it.
      _addedWhileChecking = false;
      _sent = null;
      await _check(announce: false);
      return;
    }
    final String say;
    TaskDraft? proposed;
    switch (parseValidation(reply)) {
      case Proposed(draft: final d):
        draft = proposed = d;
        say = proposalSpeech(d, words);
      case Questions(:final questions):
        say = speakable(questions.join('\n'), words);
      case null:
        say = await spokenReply(reply, _condense, words: words);
    }
    // Said once the person stops talking, unless what they said meanwhile
    // revised the request: then it is not said at all. A pause keeps it
    // for when they are back.
    final revision = _revisions;
    var at = -1;
    await _deliver(
      say,
      still: () =>
          revision == _revisions &&
          (proposed == null || identical(proposed, draft)),
      commit: () {
        intake.add((false, say));
        at = intake.length - 1;
        if (draft case final d? when identical(d, proposed)) {
          _proposal = (d, say);
        }
      },
      heard: (spoken) => _keepHeard(at, say, spoken),
    );
  }

  /// Hands the agreed [draft] to the project's agent.
  Future<void> startTask() async {
    final d = draft;
    if (d == null || started || validating || _closed) return;
    _silence?.cancel();
    _silence = turnWait = null;
    _forgetJudgement();
    // The task starts: a question about other words is moot.
    _unsure = null;
    _settled = _volatile = '';
    started = true;
    final before = taskId;
    // A start whose outcome is unknown (sent, its answer lost) is retried
    // with the same id, which the agent recognizes.
    final id = taskId = switch (_startId) {
      (final draft, final id) when identical(draft, d) => id,
      _ => newTaskId(),
    };
    _startId = (d, id);
    notifyListeners();
    final speech = _say(words.starting);
    if (await _send(
      taskPrompt(d, id: id, language: language),
      cue: false,
      about: id,
    )) {
      tasks.add((d, id));
    } else {
      started = false;
      taskId = before;
    }
    notifyListeners();
    await speech;
  }

  /// Sends [text] to the project's agent and waits for its reply; false when
  /// it could not be sent.
  ///
  /// The prompt is registered as outstanding before it goes out, with the
  /// task it is about ([about]: a start, or an answer to the agent about a
  /// task; null for a check): a poll that sees it in the history first
  /// reads what follows as its answer, and only that task's report or
  /// question acknowledges it.
  Future<bool> _send(String text, {bool cue = true, String? about}) async {
    final check = validating ? _checkId : null;
    final chat = _chat;
    final status = chat?['status'] as String?;
    final question = chat?['question'] as String?;
    final textAnswer = chat?['textAnswer'] == true;
    if (chat?['chatReady'] != true ||
        !(status == 'Ready' || status == 'Needs an answer')) {
      await _say(words.notReady);
      return false;
    }
    if (question != null && !textAnswer) {
      await _say(words.choiceWaiting);
      return false;
    }
    // The session is idle: a called-off check is over, nothing to stop.
    _stopPending = false;
    if (check != null) _checkPrompt = text;
    phase = CallPhase.sending;
    problem = null;
    // What came before is read first: what follows the prompt answers it.
    if (chat != null) _readHistory(chat);
    _before = chat?['messages'] as List? ?? const [];
    _ourPrompts.add(_plain(text));
    _replyParts.clear();
    _replyAnswered = false;
    _writing = null;
    _preparedFor = _prepared = null;
    _sent = text;
    _sentAbout = check != null ? null : about;
    notifyListeners();
    // Heard at once, so the wait for the agent is not silent.
    if (cue) unawaited(voice.cue().catchError((_) {}));
    try {
      // The call's own prompt (the agent asks nothing): typed by the host
      // only into an idle session's empty input line, Enter only right
      // after it with nothing else typed (`sendPrompt`, ADR 0034). An answer
      // to the agent's text question goes as before.
      await call(question == null ? 'sendPrompt' : 'send', {
        ...target,
        'id': chat?['id'],
        'text': text,
        'question': ?question,
      });
    } catch (e) {
      // Hung up, or the check was called off, meanwhile: nothing to undo,
      // and a check that never reached the host has nothing to stop.
      if (check != null && check == _stopCheck) _stopPending = false;
      // Never out: nothing answers it.
      if (identical(_sent, text)) _sent = null;
      if (_closed || (check != null && check != _checkId)) return false;
      problem = e.toString();
      phase = CallPhase.listening;
      notifyListeners();
      await _say(words.notThrough);
      return false;
    }
    // Hung up, or the check was called off, while it went out.
    if (_closed || (check != null && check != _checkId)) return true;
    _sawRunning = false;
    _workingSince = _lastNudge = DateTime.now();
    phase = CallPhase.working;
    notifyListeners();
    // A quick agent may already be done; don't wait for the next tick.
    unawaited(_refresh());
    return true;
  }

  /// Reads the chat, then acts on it. While a reply is said, or waits for
  /// the person to stop talking, the chat is still read (its status, a
  /// question) but acted on once that is done.
  Future<void> _refresh() async {
    if (_polling || _closed) return;
    _polling = true;
    final Data chat;
    try {
      chat = await call('chat', target) as Data;
    } catch (e) {
      // Shown, not fatal: the link to the host can come back.
      problem = e.toString();
      notifyListeners();
      return;
    } finally {
      _polling = false;
    }
    if (_closed) return;
    _chat = chat;
    if (_acting) return;
    _acting = true;
    try {
      if (_stopPending) {
        final answered = repliesAfter(
          _stopBefore,
          chat['messages'] as List? ?? const [],
          _stopPrompt,
        ).isNotEmpty;
        // Retired once late, in another session, or once the check is over.
        if (DateTime.now().isAfter(_stopUntil) ||
            chat['id'] != _stopSession ||
            (answered && chat['status'] != 'Running')) {
          _stopPending = false;
        } else if (chat['status'] == 'Running') {
          await _stopCalledOff();
        }
      }
      _readHistory(chat);
      if (await _sayHeard()) return;
      // A report waited on the person's follow-ups; they went through and
      // the agent answered: the call ends now.
      if (_outcomeAfterFollowUps != null &&
          !_stillForTheAgent &&
          _sent == null &&
          _speaking == null &&
          phase == CallPhase.listening &&
          chat['status'] == 'Ready' &&
          chat['question'] == null) {
        await _finishAfterFollowUps();
        return;
      }
      if (phase == CallPhase.working) {
        await _await(chat);
      } else if (_followUps.isNotEmpty &&
          !paused &&
          phase == CallPhase.listening &&
          _speaking == null &&
          chat['status'] == 'Ready' &&
          chat['question'] == null) {
        await _sendFollowUps();
      }
    } catch (e) {
      problem = e.toString();
      notifyListeners();
    } finally {
      _acting = false;
    }
  }

  /// While the agent works: speaks its reply once its turn is over.
  Future<void> _await(Data chat) async {
    final status = chat['status'] as String?;
    if (status == 'Running') {
      _sawRunning = true;
      final now = DateTime.now();
      if (now.difference(_lastNudge!) >= stillWorking) {
        _lastNudge = now;
        // Not awaited: the poll goes on while it is said. Never over the
        // person: then this one is skipped.
        if (!paused && !_personTalking) unawaited(_say(words.stillWorking));
      }
      // A check's answer is parsed, not condensed.
      if (!validating) _prepare([..._replyParts, ?_writing]);
      return;
    }
    // Its answer as read ([_readHistory]): reports and questions about
    // this call's tasks are not part of it, and are said on their own.
    final replies = List.of(_replyParts);
    final reply = replies.isEmpty ? null : replies.join('\n\n');
    final question = chat['question'] as String?;
    final waited = DateTime.now().difference(_workingSince!);
    // A question to answer in words: until it is said, what the person
    // says is not its answer, whatever comes with it.
    if (question != null && chat['textAnswer'] == true) _questionDue = question;
    if (reply != null && validating) {
      // The check went to a helper terminal: the answer comes in a later
      // turn, with this note before it. Until then (and no longer than
      // [helperCheckLimit]) the call keeps waiting, saying so now and then.
      if (isCheckingNote(replies.last)) {
        if (waited < helperCheckLimit) {
          final now = DateTime.now();
          if (now.difference(_lastNudge!) >= stillWorking) {
            _lastNudge = now;
            if (!paused && !_personTalking) {
              unawaited(_say(words.stillWorking));
            }
          }
          return;
        }
        validating = false;
        _sent = null;
        await _deliver(words.noReply);
        return;
      }
      await _validated(replies.where((r) => !isCheckingNote(r)).join('\n\n'));
      return;
    }
    final wasChecking = validating;
    if (reply != null) {
      final ready = reply == _preparedFor ? _prepared : null;
      _preparedFor = _prepared = null;
      var spoken =
          await (ready ??
              spokenReply(_localized(replies), _condense, words: words));
      // The reply to what was sent: taken, once (a turn sent while it is
      // said has its own).
      _sent = null;
      if (question != null && chat['textAnswer'] != true) {
        await _deliver('$spoken ${words.needsChoice}');
      } else if (question == null || chat['textAnswer'] != true) {
        await _deliver(spoken);
      } else {
        // The reply, then the host's question in an utterance of its own:
        // the question counts as said only once it is, so words that cut
        // the reply short are not its answer. A reply that says it already
        // counts once heard to the end.
        final says = _plain(spoken).contains(_plain(question));
        var whole = false;
        await _deliver(spoken, heard: (s) => whole = s.completed);
        if (says && whole && _chat?['question'] == question) {
          _questionSaid = question;
          _questionDue = null;
          _interruptProposal();
        } else {
          await _deliver('', now: _asking);
        }
      }
      await _ended(replies);
      return;
    }
    if (_replyAnswered && !validating) {
      // Answered by a report or a question, already said. A question of
      // the host's is still said: what the person says next answers it.
      _sent = null;
      if (phase == CallPhase.working) phase = CallPhase.listening;
      notifyListeners();
      if (question != null) {
        if (chat['textAnswer'] == true) {
          await _deliver('', now: _asking);
        } else {
          await _deliver(words.choiceWaiting);
        }
      }
      return;
    }
    if (status == 'Stopped' || status == 'Failed') {
      validating = false;
      _sent = null;
      await _deliver(words.sessionStopped);
    } else if (question != null) {
      validating = false;
      _sent = null;
      if (chat['textAnswer'] == true) {
        await _deliver('', now: _asking);
      } else {
        await _deliver(words.choiceWaiting);
      }
    } else if (_sawRunning && waited > const Duration(seconds: 5)) {
      validating = false;
      _sent = null;
      await _deliver(wasChecking ? words.noReply : words.done);
    } else if (waited > const Duration(seconds: 20)) {
      validating = false;
      _sent = null;
      // The turn was missed between polls, or never started.
      await _deliver(words.noReply);
    }
  }

  /// The host's question in words as it is now, noted as the one said as
  /// it begins to be said; nothing when it has none any more.
  String _asking() {
    final chat = _chat;
    final question = chat?['question'] as String?;
    _questionDue = null;
    if (question == null || chat?['textAnswer'] != true) return '';
    _questionSaid = question;
    _interruptProposal();
    return '${words.agentAsks} ${speakable(question, words)}';
  }

  /// Sends what the person said while the agent worked, as one message.
  Future<void> _sendFollowUps() async {
    final text = _followUps.join(' ');
    _followUps.clear();
    // Not retried: a send that fails says so. A report that came meanwhile
    // then ends the call as it would have.
    if (!await _send(text, about: taskId)) await _finishAfterFollowUps();
  }

  /// The person has words not yet sent: follow-ups, or a turn being said.
  bool get _stillForTheAgent => _followUps.isNotEmpty || _silence != null;

  /// Ends the call for a report that waited on the person's follow-ups.
  Future<void> _finishAfterFollowUps() async {
    final outcome = _outcomeAfterFollowUps;
    if (outcome == null || _stillForTheAgent) return;
    _outcomeAfterFollowUps = null;
    await _finish(outcome);
  }

  /// Starts condensing [reply] while the agent still runs: its last message
  /// usually lands before its turn is marked over. One at a time; a reply
  /// that grows meanwhile is prepared again on a later poll.
  void _prepare(List<String> replies) {
    if (replies.isEmpty) return;
    final reply = replies.join('\n\n');
    if (reply == _preparedFor || _preparing) return;
    _preparing = true;
    _preparedFor = reply;
    _prepared = spokenReply(
      _localized(replies),
      _condense,
      words: words,
    ).whenComplete(() => _preparing = false);
  }

  /// Reads the history once, from where it was read last: each message is
  /// handed to what it is for. A report or question about a task of this
  /// call's goes to [_heard] (another task's is dropped); what follows the
  /// outstanding prompt is its answer ([_replyParts]); what the agent says
  /// on its own (woken by a terminal, or after an answered prompt) is
  /// heard too once the call has a task; what answers a message typed on
  /// screen, or a check called off, is not this call's. The agent's last
  /// message is read only once its turn is over: it may still be written.
  void _readHistory(Data chat) {
    final now = chat['messages'] as List? ?? const [];
    final read = _read;
    if (read == null) {
      _read = now;
      return;
    }
    final added = newMessages(read, now);
    var take = added.length;
    _writing = null;
    if (chat['status'] == 'Running' &&
        take > 0 &&
        (added.last as List)[0] != true) {
      take--;
      final last = '${(added.last as List)[1]}';
      if (_after == _After.reply &&
          taskMarker(last)?.$2 == null &&
          questionTask(last) == null) {
        _writing = last;
      }
    }
    for (final m in added.take(take)) {
      _readMessage((m as List)[0] == true, '${m[1]}');
    }
    _read = now.sublist(0, now.length - added.length + take);
  }

  void _readMessage(bool fromPerson, String text) {
    final sent = _sent;
    if (fromPerson) {
      final plain = _plain(unwrapPaste(text));
      if (sent != null && plain == _plain(sent)) {
        _after = _After.reply;
      } else if (isFromConnecto(text)) {
        // A check handed to a helper is answered after the notice that
        // wakes the agent with the helper's answer: followed through it.
        final handedOff =
            _after == _After.reply &&
            validating &&
            _replyParts.isNotEmpty &&
            isCheckingNote(_replyParts.last);
        if (!handedOff) _after = _After.own;
      } else if (_calledOff.contains(plain)) {
        _after = _After.none;
      } else if (_ourPrompts.contains(plain)) {
        _after = _After.own;
      } else {
        // Typed on screen: what follows answers that, not this call.
        _after = _After.none;
      }
      return;
    }
    if (text.trim().isEmpty) return;
    // The outstanding prompt's answer only while it is outstanding.
    if (_after == _After.reply && sent == null) _after = _After.own;
    final reply = _after == _After.reply;
    final ids = _callTasks;
    final (outcome, reportId) = switch (taskMarker(text)) {
      (final o, final id?) => (o, id),
      _ => (null, null),
    };
    final asks = questionTask(text);
    final about = reportId ?? asks;
    if (about != null) {
      if (!ids.contains(about)) return;
      _heard.add(
        _Heard(
          text,
          task: about,
          outcome: outcome,
          start: outcome != null && started && about == taskId,
        ),
      );
      // Only the task the prompt is about answers it: another task's
      // report, or anything during a check, is said on its own.
      if (reply && about == _sentAbout) _replyAnswered = true;
      return;
    }
    if (reply) {
      _replyParts.add(text);
    } else if (_after == _After.own && (started || tasks.isNotEmpty)) {
      _heard.add(_Heard(text));
    }
  }

  /// Says what [_readHistory] set aside, in order, once no check is under
  /// way (its answer comes first). A report hands off the task whose start
  /// it answers, or settles an earlier one, and may end the call; a
  /// question makes the next turn its answer. True when anything was said.
  Future<bool> _sayHeard() async {
    if (validating || _heard.isEmpty || _closed) return false;
    while (_heard.isNotEmpty && !validating && !_closed) {
      final h = _heard.removeAt(0);
      final id = h.task;
      if (id == null) {
        // The agent's own messages, together.
        final own = [h.text];
        while (_heard.isNotEmpty && _heard.first.task == null) {
          own.add(_heard.removeAt(0).text);
        }
        await _deliver(
          await spokenReply(_localized(own), _condense, words: words),
        );
        await _ended(own);
        continue;
      }
      final say = await spokenReply(
        localizeComplete(h.text, words),
        _condense,
        words: words,
      );
      final outcome = h.outcome;
      if (outcome == null) {
        // A blocked task's question: a pending proposal's yes is off from
        // when it begins to be said. It is the open question once heard to
        // the end; cut short (or never heard), it is said again first, and
        // nothing said meanwhile answers it.
        var again = false;
        await _deliver(
          say,
          commit: _interruptProposal,
          heard: (spoken) {
            if (spoken.completed) {
              _askedTask = id;
            } else if (h.tries < _questionTries && !_closed) {
              h.tries++;
              _heard.insert(0, h);
              again = true;
            }
          },
        );
        // Said again on a later poll, once the person is through.
        if (again) return true;
        continue;
      }
      await _deliver(say);
      if (h.start) {
        _handOff(outcome);
      } else {
        outcomes[id] = outcome;
      }
      await _finish(outcome);
    }
    return true;
  }

  /// What the agent's [replies] say about the last task started: the
  /// answer to its start hands it off ([_handOff]); a report that it is
  /// over may end the call ([_finish]).
  Future<void> _ended(List<String> replies) async {
    final outcome = replyOutcome(replies, task: taskId);
    if (started) {
      _handOff(outcome);
    } else if (outcome != null && taskId != null) {
      outcomes[taskId!] = outcome;
    }
    if (outcome != null) await _finish(outcome);
  }

  /// The agent answered the start of the task: it is on the board, queued
  /// or running, and the call goes on. The person can propose another.
  void _handOff(TaskOutcome? outcome) {
    final id = taskId;
    if (!started || id == null) return;
    started = false;
    draft = null;
    _proposal = null;
    _heardDraft = null;
    _talkedSinceHandOff = false;
    // The next request starts afresh: the conversation that shaped this
    // task is not part of it.
    intake.clear();
    _openTurn = null;
    if (outcome != null) outcomes[id] = outcome;
    notifyListeners();
  }

  /// The ids of the tasks this call started, and the one being started.
  Set<String> get _callTasks => {
    for (final (_, id) in tasks) id,
    if (started && taskId != null) taskId!,
  };

  /// A report ends the call only when it is the last one this call waits
  /// for and the person has not taken up anything new meanwhile: a task
  /// that is queued, still running or being shaped keeps the call on.
  bool _mayEnd(TaskOutcome outcome) =>
      outcome != TaskOutcome.queued &&
      _pending.isEmpty &&
      !validating &&
      draft == null &&
      !_talkedSinceHandOff;

  Future<void> _finish(TaskOutcome outcome) async {
    if (!_mayEnd(outcome)) return;
    // The person still has something for the agent, acknowledged, still
    // being said, or sent while the report was spoken: it goes first, and
    // the call ends once the agent answered it.
    if (_stillForTheAgent ||
        _sent != null ||
        phase == CallPhase.sending ||
        phase == CallPhase.working) {
      _outcomeAfterFollowUps = outcome;
      return;
    }
    queued = outcome == TaskOutcome.queued;
    failed = outcome == TaskOutcome.failed;
    // Paused: the report waits to be heard, and the call with it.
    if (paused) {
      _endWhenBack = true;
      return;
    }
    await _complete();
  }

  /// [messages] joined as they are spoken, each completion marker in the
  /// call language.
  String _localized(List<String> messages) =>
      [for (final m in messages) localizeComplete(m, words)].join('\n\n');

  /// The agent reported the task complete (or queued): the voice session is
  /// over.
  Future<void> _complete() async {
    if (_closed) return;
    completed = !queued && !failed;
    await hangUp();
  }

  /// Says [text] once the person has stopped talking (their voice and any
  /// words not yet sent), never over them; while they talk, the agent's
  /// turn is over and what they say is a new turn. [still] says whether
  /// it is still to be said after that wait; [commit] runs as it begins to
  /// be said, and [heard] once it was, with how it went. Paused, all of it
  /// waits for the person to be back.
  Future<void> _deliver(
    String text, {
    bool Function()? still,
    void Function()? commit,
    void Function(Spoken)? heard,
    String Function()? now,
  }) async {
    // Hung up while it was prepared (condensed): the call stays ended.
    if (_closed) return;
    _recap = text;
    final kept = (
      text: text,
      still: still,
      commit: commit,
      heard: heard,
      now: now,
    );
    if (paused) {
      // Kept for when the person is back.
      _keptWhilePaused.add(kept);
      if (phase == CallPhase.working || phase == CallPhase.sending) {
        phase = CallPhase.listening;
      }
      notifyListeners();
      return;
    }
    if (_personTalking) {
      if (phase == CallPhase.working || phase == CallPhase.sending) {
        phase = CallPhase.listening;
        notifyListeners();
      }
      await _untilQuiet();
      if (_closed) return;
      // Paused meanwhile: kept, and looked at again when they are back.
      if (paused) {
        _keptWhilePaused.add(kept);
        notifyListeners();
        return;
      }
    }
    // Still to be said, after the wait or kept through a pause?
    if (still != null && !still()) return;
    // What it says, as of now ([now]): nothing left to say, nothing said.
    final said = now?.call() ?? text;
    if (said.trim().isEmpty) return;
    _recap = said;
    commit?.call();
    // A turn of theirs sent while this waited keeps the call working on it.
    final sentMeanwhile = _sent != null || phase == CallPhase.sending;
    if (!sentMeanwhile) phase = CallPhase.speaking;
    final spoken = await _say(said);
    heard?.call(spoken);
  }

  /// How often a wait for the person to stop talking looks again.
  static const _quietCheck = Duration(milliseconds: 100);

  /// The person is talking, or has words not yet sent as a turn.
  bool get _personTalking =>
      !muted &&
      (_voiceOn ||
          _silence != null ||
          _settled.isNotEmpty ||
          _volatile.isNotEmpty);

  Future<void> _untilQuiet() async {
    while (!_closed && _personTalking) {
      await Future<void>.delayed(_quietCheck);
    }
  }

  /// Speaks [text]; the person can talk over it. How it went: not heard at
  /// all when it was replaced, failed or the call ended first. While the
  /// agent works the phase stays as it is.
  Future<Spoken> _say(String text) async {
    const unheard = Spoken(completed: false, heard: '');
    if (_closed) return unheard;
    final mine = ++_utterance;
    _releaseHold();
    final working = phase == CallPhase.working || phase == CallPhase.sending;
    if (!working) phase = CallPhase.speaking;
    said = text;
    _speaking = _echo = text;
    timings.replyReady();
    notifyListeners();
    var spoken = unheard;
    try {
      // The call keeps the voice it began with: one that becomes ready
      // meanwhile waits for the next call.
      final natural = _natural ??= await voice.naturalReady();
      if (mine != _utterance || _closed) return unheard;
      spoken = await voice.speak(
        text,
        natural: natural,
        plan: natural ? speechPlan(speed: _speed, text: text) : null,
      );
    } catch (e) {
      problem = e.toString();
    }
    // The proposal, heard to the end: a yes now agrees to it.
    if (spoken.completed) {
      if (_proposal case (final d, final speech)
          when identical(d, draft) && text.contains(speech)) {
        _heardDraft = d;
      }
    }
    if (mine != _utterance) return spoken;
    _releaseHold();
    _speaking = null;
    _echoUntil = DateTime.now().add(const Duration(seconds: 2));
    if (!_closed && phase == CallPhase.speaking) {
      phase = CallPhase.listening;
    }
    if (!_closed) notifyListeners();
    return spoken;
  }
}

/// Something to say kept while the call is paused, with what goes with it
/// (see [CallController._deliver]).
typedef _Kept = ({
  String text,
  bool Function()? still,
  void Function()? commit,
  void Function(Spoken)? heard,
  String Function()? now,
});

/// What of [said] the person heard before cutting it short, for the
/// conversation's history: its start up to the last clause [heard] (the
/// start the device counts as played back) covers whole, marked as cut off;
/// a mark alone when not one clause was heard.
String heardBeforeCut(String said, String heard) {
  final text = said.trim(), got = heard.trim();
  var n = 0;
  while (n < text.length &&
      n < got.length &&
      text.codeUnitAt(n) == got.codeUnitAt(n)) {
    n++;
  }
  if (n >= text.length) return text;
  var end = 0;
  // A clause ends on its punctuation, followed by a space or the end of
  // what was heard.
  for (final m in RegExp(
    r'[.!?;:,…](?=\s|$)',
  ).allMatches(text.substring(0, n))) {
    end = m.end;
  }
  return end == 0
      ? '[cut off by the person: how much of it they heard is not known]'
      : '${text.substring(0, end)} [cut off here by the person]';
}

/// The messages in [now] that were not in [before]. The host appends to its
/// history and drops the oldest, so [now] starts with a tail of [before].
List newMessages(List before, List now) {
  bool same(List a, List b) => a[0] == b[0] && a[1] == b[1];
  for (var dropped = 0; dropped <= before.length; dropped++) {
    final kept = before.length - dropped;
    if (kept > now.length) continue;
    var matches = true;
    for (var k = 0; k < kept && matches; k++) {
      matches = same(before[dropped + k] as List, now[k] as List);
    }
    if (matches) return now.sublist(kept);
  }
  return now;
}

/// The agent's messages after the prompt [sent], among those added since
/// [before], joined; null until there are any. Messages are
/// `[fromUser, text]`. An earlier identical prompt is never matched.
String? replyAfter(List before, List now, String sent) {
  final replies = repliesAfter(before, now, sent);
  return replies.isEmpty ? null : replies.join('\n\n');
}

/// [text] without the wrapper Claude Code puts around a long paste,
/// `<pasted_content id="x">…</pasted_content id="x">`, which its history
/// keeps: a long prompt the app sent then reads back as it was sent.
String unwrapPaste(String text) {
  final m = RegExp(
    r'^\s*<pasted_content id="([^"]*)">([\s\S]*)</pasted_content id="\1">\s*$',
  ).firstMatch(text);
  return m == null ? text : m[2]!;
}

/// [replyAfter], one message each. With [throughHandoffs], replies that end
/// on a [isCheckingNote] (a check handed to a helper terminal) go on past
/// the notices that wake the agent with the helper's answer, to the
/// agent's next reply.
List<String> repliesAfter(
  List before,
  List now,
  String sent, {
  bool throughHandoffs = false,
}) {
  final added = newMessages(before, now);
  final want = _plain(sent);
  final at = added.indexWhere(
    (m) => (m as List)[0] == true && _plain(unwrapPaste('${m[1]}')) == want,
  );
  if (at < 0) return const [];
  // Up to the next prompt: what follows answers someone else's message.
  final replies = <String>[];
  for (final r in added.skip(at + 1)) {
    final (fromUser, text) = ((r as List)[0] == true, '${r[1]}');
    if (fromUser) {
      final handedOff =
          throughHandoffs &&
          replies.isNotEmpty &&
          isCheckingNote(replies.last) &&
          isConnectoNotice(text);
      if (!handedOff) break;
    } else if (text.trim().isNotEmpty) {
      replies.add(text);
    }
  }
  return replies;
}

/// [now] up to the end of the agent's reply to [sent], a prompt added since
/// [before]: the history a later read of what the agent says starts after.
/// All of [now] when [sent] is not there.
List throughReply(List before, List now, String sent) {
  final added = newMessages(before, now);
  final want = _plain(sent);
  final at = added.indexWhere(
    (m) => (m as List)[0] == true && _plain(unwrapPaste('${m[1]}')) == want,
  );
  if (at < 0) return now;
  var end = at + 1;
  while (end < added.length && (added[end] as List)[0] != true) {
    end++;
  }
  return now.sublist(0, now.length - added.length + end);
}

class CallPage extends StatefulWidget {
  const CallPage({
    super.key,
    required this.title,
    required this.call,
    required this.voice,
    required this.target,
    this.intro,
  });
  final String title;
  final Future<dynamic> Function(String, Data) call;
  final VoiceEngine voice;
  final Data target;

  /// Said before the greeting ([CallController.intro]).
  final String? intro;
  @override
  State<CallPage> createState() => _CallPageState();
}

class _CallPageState extends State<CallPage> {
  /// Owned by this state, so a rebuilt route never starts a second call.
  late final CallController c = CallController(
    call: widget.call,
    voice: widget.voice,
    target: widget.target,
    projectName: widget.title,
    intro: widget.intro,
  );

  @override
  void initState() {
    super.initState();
    c.addListener(_changed);
    c.start();
  }

  Future<void> _settings() => Navigator.push(
    context,
    MaterialPageRoute(
      builder: (_) => SettingsPage(
        voice: widget.voice,
        // Between turns the call switches at once; otherwise the next call
        // uses it.
        chooseLanguage: (id) async => c.canSwitchLanguage
            ? await c.switchLanguage(id)
            : await widget.voice.setLanguage(id),
      ),
    ),
  );

  /// A new voice session for the next task, in place of this one.
  void _newTask() => Navigator.pushReplacement(
    context,
    MaterialPageRoute(
      builder: (_) => CallPage(
        title: widget.title,
        call: widget.call,
        voice: widget.voice,
        target: widget.target,
      ),
    ),
  );

  void _changed() {
    if (mounted) setState(() {});
  }

  @override
  void dispose() {
    c.removeListener(_changed);
    c.dispose();
    super.dispose();
  }

  (String, IconData, Color) get _look => switch (c.phase) {
    CallPhase.listening ||
    CallPhase.thinking ||
    CallPhase.sending ||
    CallPhase.working ||
    CallPhase.speaking when c.paused => (
      'Paused · waiting for you',
      Icons.pause_circle_outline,
      ConnectoColors.amber,
    ),
    CallPhase.listening ||
    CallPhase.sending ||
    CallPhase.working when c.onHold && !c.muted => (
      c.validating
          ? 'On hold · checking with the project'
          : 'On hold · task under way',
      Icons.phone_paused_outlined,
      ConnectoColors.amber,
    ),
    CallPhase.connecting => ('Connecting…', Icons.call, ConnectoColors.muted),
    CallPhase.listening when c.muted => (
      'Muted',
      Icons.mic_off,
      ConnectoColors.amber,
    ),
    CallPhase.listening => ('Listening', Icons.mic, ConnectoColors.accent),
    CallPhase.thinking => (
      'Thinking…',
      Icons.psychology_outlined,
      ConnectoColors.accent,
    ),
    CallPhase.sending => ('Sending…', Icons.north_east, ConnectoColors.accent),
    CallPhase.working => (
      'Agent is working…',
      Icons.hourglass_top,
      ConnectoColors.amber,
    ),
    CallPhase.speaking => ('Speaking', Icons.graphic_eq, ConnectoColors.green),
    CallPhase.ended when c.failed => (
      'Task failed',
      Icons.error_outline,
      ConnectoColors.coral,
    ),
    CallPhase.ended when c.queued => (
      'Task queued',
      Icons.playlist_add_check,
      ConnectoColors.accent,
    ),
    CallPhase.ended when c.completed => (
      'Task complete',
      Icons.task_alt,
      ConnectoColors.green,
    ),
    CallPhase.ended => ('Call ended', Icons.call_end, ConnectoColors.coral),
  };

  @override
  Widget build(BuildContext context) {
    final (label, icon, color) = _look;
    final text = Theme.of(context).textTheme;
    return Scaffold(
      appBar: AppHeader(
        back: true,
        title: widget.title,
        subtitle: 'Voice task',
        actions: [
          IconButton(
            tooltip: 'Settings',
            onPressed: _settings,
            icon: const Icon(Icons.settings_outlined),
          ),
        ],
      ),
      body: SafeArea(
        top: false,
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 560),
            child: Padding(
              padding: const EdgeInsets.fromLTRB(16, 16, 16, 24),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  if (c.problem != null) ...[
                    NoticeBanner(c.problem!),
                    const SizedBox(height: 12),
                  ],
                  Expanded(
                    child: ListView(
                      children: [
                        const SizedBox(height: 24),
                        Center(
                          child: Semantics(
                            liveRegion: true,
                            label: 'Call: $label',
                            child: GestureDetector(
                              onTap: c.canSendNow ? c.sendNow : null,
                              child: Container(
                                width: 112,
                                height: 112,
                                decoration: BoxDecoration(
                                  shape: BoxShape.circle,
                                  color: color.withValues(alpha: 0.12),
                                  border: Border.all(color: color, width: 2),
                                ),
                                child: Icon(icon, size: 44, color: color),
                              ),
                            ),
                          ),
                        ),
                        const SizedBox(height: 14),
                        Text(
                          label,
                          textAlign: TextAlign.center,
                          style: text.titleMedium,
                        ),
                        if (c.timings.shown && c.timings.lines.isNotEmpty) ...[
                          const SizedBox(height: 6),
                          Column(
                            key: const Key('voice-timings'),
                            children: [
                              for (final line in c.timings.lines)
                                Text(
                                  line,
                                  textAlign: TextAlign.center,
                                  style: text.bodySmall?.copyWith(
                                    color: ConnectoColors.faint,
                                    fontFeatures: const [
                                      FontFeature.tabularFigures(),
                                    ],
                                  ),
                                ),
                            ],
                          ),
                        ],
                        if (c.notice != null) ...[
                          const SizedBox(height: 6),
                          Text(
                            c.notice!,
                            textAlign: TextAlign.center,
                            style: text.bodySmall,
                          ),
                        ],
                        const SizedBox(height: 28),
                        for (final (d, id) in c.tasks)
                          if (!identical(d, c.draft)) ...[
                            _handedOff(d, c.outcomes[id]),
                            const SizedBox(height: 8),
                          ],
                        if (c.draft case final d?) ...[
                          _task(d),
                          const SizedBox(height: 14),
                        ],
                        if (c.heard.isNotEmpty)
                          GestureDetector(
                            key: const Key('call-heard'),
                            onTap: c.canSendNow ? c.sendNow : null,
                            child: _line(
                              'You',
                              c.heard,
                              ConnectoColors.userBubble,
                            ),
                          ),
                        if (c.canSendNow && c.turnWait != null) ...[
                          const SizedBox(height: 8),
                          _countdown(c.turnWait!),
                        ],
                        if (c.said.isNotEmpty) ...[
                          const SizedBox(height: 10),
                          _line('Agent', c.said, ConnectoColors.surface),
                        ],
                      ],
                    ),
                  ),
                  const SizedBox(height: 12),
                  Row(
                    mainAxisAlignment: MainAxisAlignment.center,
                    children: [
                      if (c.ended)
                        OutlinedButton.icon(
                          onPressed: _newTask,
                          icon: const Icon(Icons.add_call),
                          label: const Text('New task'),
                        )
                      else if (c.paused)
                        OutlinedButton.icon(
                          onPressed: c.resume,
                          icon: const Icon(Icons.play_arrow),
                          label: const Text('Resume'),
                        )
                      else
                        OutlinedButton.icon(
                          onPressed: c.toggleMute,
                          icon: Icon(c.muted ? Icons.mic_off : Icons.mic_none),
                          label: Text(c.muted ? 'Unmute' : 'Mute'),
                        ),
                      const SizedBox(width: 12),
                      FilledButton.icon(
                        style: FilledButton.styleFrom(
                          backgroundColor: ConnectoColors.coral,
                          foregroundColor: ConnectoColors.bg,
                        ),
                        onPressed: c.ended
                            ? () => Navigator.maybePop(context)
                            : c.hangUp,
                        icon: const Icon(Icons.call_end),
                        label: Text(c.ended ? 'Close' : 'Hang up'),
                      ),
                    ],
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }

  /// A task this call already put on the board, with where it stands.
  Widget _handedOff(TaskDraft d, TaskOutcome? outcome) {
    final text = Theme.of(context).textTheme;
    final (label, icon, color) = switch (outcome) {
      TaskOutcome.complete => (
        'Complete',
        Icons.task_alt,
        ConnectoColors.green,
      ),
      TaskOutcome.failed => (
        'Failed',
        Icons.error_outline,
        ConnectoColors.coral,
      ),
      TaskOutcome.queued => (
        'Queued',
        Icons.playlist_add_check,
        ConnectoColors.accent,
      ),
      null => ('Started', Icons.play_arrow, ConnectoColors.amber),
    };
    return Row(
      children: [
        Icon(icon, size: 18, color: color),
        const SizedBox(width: 8),
        Expanded(child: Text(d.title, style: text.bodyMedium)),
        Text(label, style: text.labelMedium?.copyWith(color: color)),
      ],
    );
  }

  /// The task as understood: started with a tap (or a spoken yes), changed
  /// by talking on.
  Widget _task(TaskDraft d) {
    final text = Theme.of(context).textTheme;
    final canStart =
        !c.started &&
        !c.ended &&
        !c.validating &&
        c.phase != CallPhase.thinking;
    return Container(
      padding: const EdgeInsets.fromLTRB(14, 12, 14, 12),
      decoration: BoxDecoration(
        color: ConnectoColors.raised,
        border: Border.all(
          color: c.started ? ConnectoColors.border : ConnectoColors.accent,
        ),
        borderRadius: BorderRadius.circular(12),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(
            c.completed
                ? 'Task complete'
                : c.failed
                ? 'Task failed'
                : c.queued
                ? 'Task queued: it starts when the running one ends'
                : c.started
                ? 'Task started'
                : 'Proposed task',
            style: text.labelMedium?.copyWith(color: ConnectoColors.faint),
          ),
          const SizedBox(height: 4),
          Text(d.title, style: text.titleSmall),
          const SizedBox(height: 6),
          Text(d.task),
          if (!c.started && !c.ended) ...[
            const SizedBox(height: 6),
            Text(
              "It runs as the project's settings say, after any task "
              'already running. '
              'Keep talking to change it.',
              style: text.bodySmall?.copyWith(color: ConnectoColors.muted),
            ),
            const SizedBox(height: 10),
            FilledButton.icon(
              onPressed: canStart ? c.startTask : null,
              icon: const Icon(Icons.play_arrow),
              label: const Text('Start task'),
            ),
          ],
        ],
      ),
    );
  }

  /// The silence running out before the turn is sent, restarted whenever
  /// the person goes on; a tap sends it at once.
  Widget _countdown(Duration wait) => TweenAnimationBuilder<double>(
    key: ValueKey(c.turnArmed),
    tween: Tween(begin: 1, end: 0),
    duration: wait,
    builder: (context, left, _) => Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        LinearProgressIndicator(
          value: left,
          minHeight: 3,
          color: ConnectoColors.accent,
          backgroundColor: ConnectoColors.border,
        ),
        const SizedBox(height: 4),
        Text(
          'Sending when you pause · tap to send now',
          textAlign: TextAlign.center,
          style: Theme.of(context).textTheme.bodySmall
              ?.copyWith(color: ConnectoColors.muted),
        ),
      ],
    ),
  );

  Widget _line(String who, String words, Color background) => Container(
    padding: const EdgeInsets.symmetric(horizontal: 13, vertical: 10),
    decoration: BoxDecoration(
      color: background,
      border: Border.all(color: ConnectoColors.border),
      borderRadius: BorderRadius.circular(12),
    ),
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          who,
          style: Theme.of(context).textTheme.labelMedium
              ?.copyWith(color: ConnectoColors.faint),
        ),
        const SizedBox(height: 4),
        Text(words),
      ],
    ),
  );
}
