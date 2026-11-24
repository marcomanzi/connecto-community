import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';

import 'peer.dart';
import 'theme.dart';
import 'widgets.dart';

/// The board file, in the project folder. The project's app session agent
/// writes it; the app reads it through the host's file read (ADR 0029).
const boardPath = '.connecto/board.json';

enum TaskStatus { queued, running, blocked, done, failed }

TaskStatus _status(Object? raw) {
  final s = '${raw ?? ''}'.trim().toLowerCase().replaceAll(RegExp('[ _-]'), '');
  return switch (s) {
    'running' || 'inprogress' || 'started' || 'active' => TaskStatus.running,
    'blocked' || 'waiting' || 'needsdecision' => TaskStatus.blocked,
    'done' || 'complete' || 'completed' => TaskStatus.done,
    'failed' || 'error' || 'cancelled' || 'canceled' => TaskStatus.failed,
    _ => TaskStatus.queued,
  };
}

/// One task on the board.
class BoardTask {
  const BoardTask({
    required this.id,
    required this.title,
    this.goal = '',
    this.scope = '',
    this.done = '',
    this.status = TaskStatus.queued,
    this.note = '',
  });

  /// As the agent wrote it; empty when it wrote none (such a task is shown
  /// but cannot be addressed, so it cannot be moved).
  final String id;
  final String title, goal, scope, done, note;
  final TaskStatus status;

  /// Running or blocked: the one slot work goes through.
  bool get active =>
      status == TaskStatus.running || status == TaskStatus.blocked;
  bool get finished => status == TaskStatus.done || status == TaskStatus.failed;
}

/// The project's tasks as the agent keeps them: queued ones run in the
/// order they are listed, one at a time.
class Board {
  const Board(this.tasks, {this.howToRun});
  final List<BoardTask> tasks;

  /// The project's instructions for running a board task; null while the
  /// board has none (the agent then sets [defaultHowToRun]).
  final String? howToRun;

  static const empty = Board([]);

  List<BoardTask> get active => [
    for (final t in tasks)
      if (t.active) t,
  ];
  List<BoardTask> get queued => [
    for (final t in tasks)
      if (t.status == TaskStatus.queued) t,
  ];

  /// Done and failed, the latest first.
  List<BoardTask> get finished => [
    for (final t in tasks.reversed)
      if (t.finished) t,
  ];

  /// The board in [text]; null when it is not a board (the agent may be
  /// writing it, or wrote it wrong): the last good one is kept then.
  static Board? parse(String text) {
    Object? json;
    try {
      json = jsonDecode(text);
    } on FormatException {
      return null;
    }
    final howToRun = switch (json) {
      // The earlier built-in default is replaced by the current one.
      {'howToRun': final String h}
          when h.trim().isNotEmpty && h.trim() != previousDefaultHowToRun =>
        h.trim(),
      _ => null,
    };
    final list = switch (json) {
      {'tasks': final List l} => l,
      final List l => l,
      _ => null,
    };
    if (list == null) return null;
    final tasks = <BoardTask>[];
    final ids = <String>{};
    for (final raw in list) {
      if (raw is! Map) continue;
      String s(String k) => raw[k] is String ? (raw[k] as String).trim() : '';
      final goal = s('goal');
      final title = s('title').isEmpty ? goal : s('title');
      // Only a task worth showing takes its id: a broken row never hides
      // a later good one.
      if (title.isEmpty) continue;
      final id = s('id');
      // A task listed twice counts once, where it first appears.
      if (id.isNotEmpty && !ids.add(id)) continue;
      tasks.add(
        BoardTask(
          id: id,
          title: title,
          goal: goal,
          scope: s('scope'),
          done: s('done'),
          status: _status(raw['status']),
          note: s('note'),
        ),
      );
    }
    return Board(tasks, howToRun: howToRun);
  }
}

/// A new task's id: short, unique enough for one project's board.
String newTaskId([DateTime? now]) =>
    't-${(now ?? DateTime.now()).millisecondsSinceEpoch.toRadixString(36)}';

/// The Codex model the reviewer runs on by default, as Codex lists it
/// (`models_cache.json`, the app's `models` op).
const defaultReviewModel = 'gpt-6.2-astra';

/// How a board task is run unless the project says otherwise: a coder and a
/// reviewer, each in a terminal of its own, on the coding CLIs the project
/// allows (its Tools setting, which the phone does not know: `connecto add`
/// refuses any other CLI and names the allowed ones).
const defaultHowToRun =
    '''Run every task with a coder and a reviewer, each in a new terminal of its own. Use only the coding CLIs this project allows: `connecto add` refuses any other and names the ones it allows.
1. The coder runs on the strongest allowed CLI: a Claude terminal on Opus when Claude is allowed, `connecto add claude --model opus --name <short name>`. Send it the task (title, goal, scope, done when) with `connecto send <id> --ask`.
2. When the coder is done, start the reviewer on a different allowed CLI if there is one, else on the coder's: a Codex terminal on $defaultReviewModel at medium effort when Codex is allowed, `connecto add codex --model $defaultReviewModel --name <short name>-review`. Ask it to review the change against the task.
3. Have the coder address what the review finds, and the reviewer check it again, until the reviewer has nothing left that matters.
4. Close both terminals when the task is done.''';

/// The built-in default before it kept to the project's CLIs, word for word
/// (it named Claude and Codex whatever the project allows). A board that
/// still has exactly this was given it by an earlier build, not written by
/// the user: it reads as having no instructions, and the agent replaces it
/// with [defaultHowToRun] ([taskPrompt]). Any other text is the project's
/// own and is kept.
const previousDefaultHowToRun = '''Run every task with a coder and a reviewer, each in a new terminal of its own:
1. The coder is a Claude terminal on Opus: `connecto add claude --model opus --name <short name>`. Send it the task (title, goal, scope, done when) with `connecto send <id> --ask`.
2. When the coder is done, start the reviewer, a Codex terminal on gpt-6.2-astra at medium effort: `connecto add codex --model gpt-6.2-astra --name <short name>-review`. Ask it to review the change against the task.
3. Have the coder address what the review finds, and the reviewer check it again, until the reviewer has nothing left that matters.
4. Close both terminals when the task is done.''';

/// The longest instructions the app sends.
const maxHowToRun = 4000;

/// How the agent keeps the board, repeated in every message about it so it
/// never depends on what the agent remembers.
const boardRules =
    '''The project's task board is the file `$boardPath` in the project folder. The Connecto Chat app reads it to show the tasks, so keep it exact:
- JSON: {"howToRun": "...", "tasks": [{"id": "...", "title": "...", "goal": "...", "scope": "...", "done": "...", "status": "queued", "note": ""}]}. status is one of queued, running, blocked, done, failed; note is one short line (why it failed or is blocked, or what changed).
- howToRun is the project's instructions for running a task. Follow it for every task you start, reading it from the board each time; change it only when I ask.
- Tasks run one at a time. The queued ones run in the order they are listed: the first queued task is next.
- Read it again before each change, write it whole to `$boardPath.tmp` and then move that file over the board, so it is never half-written. If the board is missing, create the `.connecto` folder and the board, and keep both files out of git: when the project folder is in a git repository, append the line `/<prefix>$boardPath*` to the file `git rev-parse --git-path info/exclude` names (relative to the project folder), where <prefix> is what `git rev-parse --show-prefix` prints there (empty at the repository's top), unless that line is there already. Outside a repository there is nothing to exclude.''';

/// What the agent is sent to put [task] first among the queued tasks. The
/// running task goes on.
String moveToFrontPrompt(BoardTask task) =>
    '''From Connecto Chat's task board: move task [${task.id}] "${task.title}" to the front of the queue, so it runs next. Don't stop or restart the task that is running, and change nothing else on the board. If that task is no longer queued, leave the board as it is.

$boardRules

Reply in one short sentence.''';

/// What the agent is sent to make [howToRun] the project's instructions for
/// running a task: they apply to every task it starts from then on.
String setHowToRunPrompt(String howToRun) =>
    '''From Connecto Chat's project settings: set the task board's howToRun to exactly the text between the two lines of dashes below, keeping its line breaks. Change nothing else on the board, and don't change a task that is already running: the new instructions apply to every task you start from now on.

$boardRules

----------
${howToRun.trim()}
----------

Reply in one short sentence.''';

/// How long a sent change may take to show on the board before the app
/// stops waiting for it (and lets it be sent again).
const boardConfirmWithin = Duration(minutes: 3);

/// The board and the project's session as the app follows them, one per
/// project for as long as the app runs, so a change survives leaving the
/// page it was made on. It polls while a page shows it or a change is
/// still on its way: the board every [boardEvery] (over the network, and
/// each read is audited on the host), the chat every [chatEvery] (the peer
/// holds it locally). Changes wait until the agent is between turns, go
/// one at a time, and are pending until the board shows them.
class BoardSync extends ChangeNotifier {
  BoardSync._(this.call, this.target, this.boardEvery, this.chatEvery);

  static final _projects = <(Object, Object?, Object?), BoardSync>{};

  /// The time, for when a sent change is given up on.
  @visibleForTesting
  static DateTime Function() clock = DateTime.now;

  /// The project's sync, made on first use.
  static BoardSync of(
    Future<dynamic> Function(String, Data) call,
    Data target, {
    Duration boardEvery = const Duration(seconds: 4),
    Duration chatEvery = const Duration(seconds: 1),
  }) => _projects.putIfAbsent((
    call,
    target['host'],
    target['project'],
  ), () => BoardSync._(call, target, boardEvery, chatEvery));

  /// Stops and forgets every project's sync.
  @visibleForTesting
  static void resetAll() {
    for (final s in _projects.values) {
      s._stop();
    }
    _projects.clear();
  }

  final Future<dynamic> Function(String, Data) call;
  final Data target;
  final Duration boardEvery, chatEvery;

  Board? board;

  /// No board file yet; [malformed]: the last read was not a board (the last
  /// good one, if any, is kept).
  bool missing = false, malformed = false;
  String? error, sendError;
  Data? chat;

  /// The task being moved to the front, and when the move went out (null
  /// while it waits for the agent).
  BoardTask? moving;
  DateTime? movedAt;

  /// The instructions being saved, and when they went out.
  String? saving;
  DateTime? savedAt;

  /// Why the last move or save was given up on.
  String? moveError, saveError;

  /// Each message with what it changes (`move`, `howToRun`) and what to
  /// note once it went out.
  final _outbox = <(String, String, void Function())>[];
  Timer? _boardTimer, _chatTimer;
  bool _readingBoard = false, _readingChat = false, _sending = false;
  int _holders = 0;

  /// Messages not sent yet.
  int get unsent => _outbox.length;

  /// A change not confirmed by the board yet.
  bool get pending => _outbox.isNotEmpty || moving != null || saving != null;

  /// A page shows this project's board: keep it fresh.
  void hold() {
    _holders++;
    _run();
  }

  void release() {
    _holders--;
    _stopIfIdle();
  }

  void _run() {
    if (_boardTimer != null) return;
    readChat().then((_) => readBoard());
    _boardTimer = Timer.periodic(boardEvery, (_) => readBoard());
    _chatTimer = Timer.periodic(chatEvery, (_) => readChat());
  }

  void _stop() {
    _boardTimer?.cancel();
    _chatTimer?.cancel();
    _boardTimer = _chatTimer = null;
  }

  void _stopIfIdle() {
    if (_holders <= 0 && !pending) _stop();
  }

  Future<void> readBoard() async {
    if (_readingBoard) return;
    _readingBoard = true;
    try {
      final data = await call('board', target) as Data?;
      final text = data?['text'] as String?;
      error = null;
      missing = text == null;
      final parsed = text == null ? Board.empty : Board.parse(text);
      malformed = parsed == null;
      board = parsed ?? board;
    } catch (e) {
      error = e.toString();
    } finally {
      _readingBoard = false;
    }
    _confirm();
    notifyListeners();
    _stopIfIdle();
  }

  /// Settles the move and the save the board now shows, or that waited too
  /// long since they went out.
  void _confirm() {
    final b = board;
    final now = clock();
    if (moving case final m? when b != null && movedAt != null) {
      final queued = b.queued;
      if (queued.firstOrNull?.id == m.id || !queued.any((t) => t.id == m.id)) {
        moving = null;
      } else if (now.difference(movedAt!) > boardConfirmWithin) {
        moving = null;
        moveError = "The agent hasn't moved the task yet. Check the chat.";
      }
    }
    if (saving case final s? when savedAt != null) {
      if (b?.howToRun == s) {
        saving = null;
      } else if (now.difference(savedAt!) > boardConfirmWithin) {
        saving = null;
        saveError =
            "The agent hasn't saved the instructions yet. Check the chat, "
            'or save again.';
      }
    }
  }

  Future<void> readChat() async {
    if (_readingChat) return;
    _readingChat = true;
    try {
      chat = await call('chat', target) as Data;
      notifyListeners();
      await _flush();
    } catch (_) {
      // The board read shows the link's state.
    } finally {
      _readingChat = false;
    }
  }

  /// The agent takes a message only between its turns.
  bool get agentFree {
    final c = chat;
    final status = c?['status'];
    return c?['chatReady'] == true &&
        c?['question'] == null &&
        (status == 'Ready' || status == 'Needs an answer');
  }

  /// Moves [task] to the front of the queue (through the agent).
  void moveToFront(BoardTask task) {
    if (moving != null || task.id.isEmpty) return;
    moving = task;
    movedAt = null;
    moveError = null;
    _send('move', moveToFrontPrompt(task), () => movedAt = clock());
  }

  /// Makes [howToRun] the project's instructions (through the agent).
  /// Makes [howToRun] the project's instructions (through the agent). A
  /// newer save replaces one not sent yet; one already going out goes on,
  /// and this one follows it.
  void saveHowToRun(String howToRun) {
    final text = howToRun.trim();
    final save = ++_saves;
    _outbox.removeWhere((m) => m.$1 == 'howToRun');
    saving = text;
    savedAt = null;
    saveError = null;
    _send('howToRun', setHowToRunPrompt(text), () {
      // Only this save's own send starts its wait: an earlier one that
      // was already going out when it was made does not.
      if (save == _saves) savedAt = clock();
    });
  }

  /// Counts saves, so a send is credited only to the save it carries.
  int _saves = 0;

  void _send(String kind, String text, void Function() sent) {
    _outbox.add((kind, text, sent));
    notifyListeners();
    _run();
    _flush();
  }

  Future<void> _flush() async {
    if (_outbox.isEmpty || _sending || !agentFree) return;
    _sending = true;
    final message = _outbox.first;
    final (_, text, sent) = message;
    try {
      await call('send', {
        ...target,
        'id': chat?['id'],
        'text': text,
        'question': null,
      });
      _outbox.remove(message);
      sendError = null;
      sent();
      // The agent is busy with it now: the rest waits for its next turn.
      chat = {...?chat, 'status': 'Running'};
    } catch (e) {
      // Tried again on the next chat read while the agent is free.
      sendError = e.toString();
    } finally {
      _sending = false;
    }
    notifyListeners();
  }
}

/// The project's task board: what runs, what is queued and in which order,
/// and what finished. A queued task can be moved to the front; the move is
/// sent to the project's agent, which keeps the board.
class BoardPage extends StatefulWidget {
  const BoardPage({
    super.key,
    required this.title,
    required this.call,
    required this.target,
    this.boardEvery = const Duration(seconds: 4),
    this.chatEvery = const Duration(seconds: 1),
  });
  final String title;
  final Future<dynamic> Function(String, Data) call;
  final Data target;
  final Duration boardEvery, chatEvery;
  @override
  State<BoardPage> createState() => _BoardPageState();
}

class _BoardPageState extends State<BoardPage> {
  late final sync = BoardSync.of(
    widget.call,
    widget.target,
    boardEvery: widget.boardEvery,
    chatEvery: widget.chatEvery,
  );

  @override
  void initState() {
    super.initState();
    sync.addListener(_changed);
    sync.hold();
  }

  @override
  void dispose() {
    sync.removeListener(_changed);
    sync.release();
    super.dispose();
  }

  void _changed() {
    if (mounted) setState(() {});
  }

  /// The queue as shown: a task being moved is already first.
  List<BoardTask> get queue {
    final q = sync.board?.queued ?? const <BoardTask>[];
    final m = sync.moving;
    if (m == null || !q.any((t) => t.id == m.id)) return q;
    return [
      q.firstWhere((t) => t.id == m.id),
      for (final t in q)
        if (t.id != m.id) t,
    ];
  }

  void openSettings() => Navigator.push(
    context,
    MaterialPageRoute(
      builder: (_) => ProjectSettingsPage(title: widget.title, sync: sync),
    ),
  );

  @override
  Widget build(BuildContext context) {
    final b = sync.board;
    final error = sync.error;
    final warning = sync.moveError ?? sync.sendError;
    final text = Theme.of(context).textTheme;
    final queue = this.queue;
    Widget quiet(String words) => Padding(
      padding: const EdgeInsets.symmetric(vertical: 32),
      child: Text(
        words,
        textAlign: TextAlign.center,
        style: text.bodyMedium?.copyWith(color: ConnectoColors.muted),
      ),
    );
    return Scaffold(
      appBar: AppHeader(
        back: true,
        title: widget.title,
        subtitle: 'Tasks',
        actions: [
          IconButton(
            tooltip: 'Project settings',
            onPressed: openSettings,
            icon: const Icon(Icons.tune),
          ),
        ],
      ),
      body: SafeArea(
        top: false,
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 640),
            child: ListView(
              padding: const EdgeInsets.fromLTRB(16, 12, 16, 24),
              children: [
                if (error != null) ...[
                  NoticeBanner(error),
                  const SizedBox(height: 12),
                ],
                if (sync.malformed && b != null) ...[
                  const NoticeBanner(
                    "The board file can't be read right now; this is how it "
                    'last read.',
                    tone: NoticeTone.warn,
                  ),
                  const SizedBox(height: 12),
                ],
                if (warning != null) ...[
                  NoticeBanner(warning, tone: NoticeTone.warn),
                  const SizedBox(height: 12),
                ],
                if (b == null && sync.malformed)
                  quiet(
                    "The board file can't be read yet. The agent may be "
                    'writing it.',
                  )
                else if (b == null && error == null)
                  const Padding(
                    padding: EdgeInsets.all(32),
                    child: Center(child: CircularProgressIndicator()),
                  )
                else if (b != null && b.tasks.isEmpty)
                  quiet(
                    sync.missing
                        ? 'No tasks yet. Tasks agreed in a call show here.'
                        : 'The board is empty.',
                  )
                else if (b != null) ...[
                  if (b.active.isNotEmpty) ...[
                    _heading('Now'),
                    for (final (i, t) in b.active.indexed) _task(t, i),
                  ],
                  if (queue.isNotEmpty) ...[
                    _heading('Up next'),
                    for (final (i, t) in queue.indexed)
                      _task(
                        t,
                        i,
                        place: i + 1,
                        canMove:
                            i > 0 && sync.moving == null && t.id.isNotEmpty,
                      ),
                  ],
                  if (b.finished.isNotEmpty) ...[
                    _heading('Finished'),
                    for (final (i, t) in b.finished.indexed) _task(t, i),
                  ],
                ],
              ],
            ),
          ),
        ),
      ),
    );
  }

  Widget _heading(String label) => Padding(
    padding: const EdgeInsets.fromLTRB(2, 14, 2, 8),
    child: Text(
      label,
      style: Theme.of(context).textTheme.labelLarge
          ?.copyWith(color: ConnectoColors.faint),
    ),
  );

  Widget _task(BoardTask t, int index, {int? place, bool canMove = false}) {
    final text = Theme.of(context).textTheme;
    final isMoving = sync.moving?.id == t.id && t.id.isNotEmpty;
    final (label, tone) = switch (t.status) {
      TaskStatus.running => ('Running', StatusTone.busy),
      TaskStatus.blocked => ('Blocked', StatusTone.attention),
      TaskStatus.done => ('Done', StatusTone.ok),
      TaskStatus.failed => ('Failed', StatusTone.error),
      TaskStatus.queued when isMoving => ('Moving to front…', StatusTone.busy),
      TaskStatus.queued => ('Queued · $place', StatusTone.neutral),
    };
    return Padding(
      padding: const EdgeInsets.only(bottom: 10),
      child: Container(
        key: ValueKey(
          t.id.isEmpty ? 'task-${t.status.name}-$index' : 'task-${t.id}',
        ),
        padding: const EdgeInsets.fromLTRB(14, 12, 8, 12),
        decoration: BoxDecoration(
          color: ConnectoColors.raised,
          border: Border.all(
            color: t.active ? ConnectoColors.accent : ConnectoColors.border,
          ),
          borderRadius: BorderRadius.circular(12),
        ),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  StatusPill(
                    label,
                    tone,
                    semanticsLabel: 'Task status: $label',
                  ),
                  const SizedBox(height: 6),
                  Text(t.title, style: text.titleSmall),
                  if (t.goal.isNotEmpty && t.goal != t.title) ...[
                    const SizedBox(height: 4),
                    Text(t.goal, style: text.bodySmall),
                  ],
                  if (t.note.isNotEmpty) ...[
                    const SizedBox(height: 4),
                    Text(
                      t.note,
                      style: text.bodySmall?.copyWith(
                        color: ConnectoColors.muted,
                      ),
                    ),
                  ],
                ],
              ),
            ),
            if (canMove)
              IconButton(
                tooltip: 'Move to front',
                onPressed: () => sync.moveToFront(t),
                icon: const Icon(Icons.vertical_align_top),
              ),
          ],
        ),
      ),
    );
  }
}

/// The project's settings: the instructions its agent follows to run each
/// board task. They live on the board, on the computer, so the agent applies
/// them with the phone closed; saving sends them to the agent, which writes
/// them there.
class ProjectSettingsPage extends StatefulWidget {
  const ProjectSettingsPage({
    super.key,
    required this.title,
    required this.sync,
  });
  final String title;
  final BoardSync sync;
  @override
  State<ProjectSettingsPage> createState() => _ProjectSettingsState();
}

class _ProjectSettingsState extends State<ProjectSettingsPage> {
  final field = TextEditingController();

  /// Whether the field has been filled from the board yet.
  bool filled = false;

  /// The board's text the field shows and follows; null while it shows the
  /// person's own text (typed, or a save on its way), which the board never
  /// replaces, whatever it holds meanwhile.
  String? following;

  BoardSync get sync => widget.sync;

  /// The instructions in force: the board's, or the default.
  String get current => sync.board?.howToRun ?? defaultHowToRun;

  @override
  void initState() {
    super.initState();
    sync.addListener(_changed);
    sync.hold();
    field.addListener(() => setState(() {}));
    _changed();
  }

  @override
  void dispose() {
    sync.removeListener(_changed);
    sync.release();
    field.dispose();
    super.dispose();
  }

  void _changed() {
    if (sync.board != null) {
      final board = current;
      final pending = sync.saving;
      if (!filled) {
        filled = true;
        // A save on its way is shown until the board has it.
        field.text = pending ?? board;
        following = pending == null ? board : null;
      } else if (pending != null && field.text.trim() == pending) {
        // It shows the save on its way: an earlier save landing on the
        // board first does not replace it.
        following = null;
      } else if (pending == null && field.text.trim() == board) {
        following = board;
      } else if (following != null &&
          field.text == following &&
          board != following) {
        following = board;
        field.text = board;
      }
    }
    if (mounted) setState(() {});
  }

  void save(String text) => sync.saveHowToRun(text);

  @override
  Widget build(BuildContext context) {
    final text = Theme.of(context).textTheme;
    final t = field.text.trim();
    final tooLong = t.length > maxHowToRun;
    // Compared with what the board will hold once a pending save lands: going
    // back to the board's text, or to the default, is a save of its own.
    final target = sync.saving ?? current;
    final changed = filled && t.isNotEmpty && t != target;
    final warning = sync.saveError ?? sync.sendError;
    return Scaffold(
      appBar: AppHeader(
        back: true,
        title: widget.title,
        subtitle: 'Project settings',
      ),
      body: SafeArea(
        top: false,
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 640),
            child: ListView(
              padding: const EdgeInsets.fromLTRB(16, 12, 16, 24),
              children: [
                if (sync.error != null) ...[
                  NoticeBanner(sync.error!),
                  const SizedBox(height: 12),
                ],
                if (warning != null) ...[
                  NoticeBanner(warning, tone: NoticeTone.warn),
                  const SizedBox(height: 12),
                ],
                Text('How tasks run', style: text.titleSmall),
                const SizedBox(height: 4),
                Text(
                  "The project's agent follows these instructions for every "
                  'task it starts from the board. They are kept on the board, '
                  'on the computer.',
                  style: text.bodySmall?.copyWith(color: ConnectoColors.muted),
                ),
                const SizedBox(height: 12),
                if (!filled)
                  const Padding(
                    padding: EdgeInsets.all(24),
                    child: Center(child: CircularProgressIndicator()),
                  )
                else
                  TextField(
                    key: const ValueKey('how-to-run'),
                    controller: field,
                    minLines: 6,
                    maxLines: 16,
                    decoration: InputDecoration(
                      border: const OutlineInputBorder(),
                      errorText: tooLong
                          ? 'At most $maxHowToRun characters.'
                          : null,
                    ),
                  ),
                const SizedBox(height: 8),
                if (sync.saving != null)
                  Text(
                    sync.savedAt != null
                        ? 'Sent. Waiting for the agent to update the board…'
                        : 'Waiting for the agent to be free…',
                    style: text.bodySmall?.copyWith(
                      color: ConnectoColors.amber,
                    ),
                  )
                else if (filled && sync.board?.howToRun == null)
                  Text(
                    'The default: the agent sets it on the board with the next '
                    'task.',
                    style: text.bodySmall?.copyWith(
                      color: ConnectoColors.muted,
                    ),
                  ),
                const SizedBox(height: 12),
                Row(
                  mainAxisAlignment: MainAxisAlignment.end,
                  children: [
                    TextButton(
                      onPressed: filled && target != defaultHowToRun
                          ? () {
                              field.text = defaultHowToRun;
                              save(defaultHowToRun);
                            }
                          : null,
                      child: const Text('Reset to default'),
                    ),
                    const SizedBox(width: 8),
                    FilledButton(
                      onPressed: changed && !tooLong ? () => save(t) : null,
                      child: const Text('Save'),
                    ),
                  ],
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
