import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:connecto_voice/connecto_voice.dart';

import 'functional_memory.dart';

/// A chat message for a language model: `(role, text)`, the role being
/// `system`, `user` or `assistant`.
typedef ChatMessage = (String, String);

/// One language model the call can ask: on the Mac, through the relay, or
/// on the phone. Null when it gave no answer (unavailable, too slow, failed,
/// or called off through [cancel]: a newer turn made it useless).
abstract interface class ChatModel {
  Future<String?> complete(
    List<ChatMessage> messages, {
    required int maxTokens,
    required double temperature,
    required Duration deadline,
    Cancellation? cancel,
  });
}

/// [work], or null as soon as [cancel] is called: the caller stops waiting
/// for an answer it no longer wants.
Future<T?> unlessCancelled<T>(Future<T?> work, Cancellation? cancel) {
  if (cancel == null) return work;
  if (cancel.cancelled) return Future.value(null);
  final out = Completer<T?>();
  final forget = cancel.onCancel(() {
    if (!out.isCompleted) out.complete(null);
  });
  work
      .then(
        (value) {
          if (!out.isCompleted) out.complete(value);
        },
        onError: (Object e, StackTrace st) {
          if (!out.isCompleted) out.completeError(e, st);
        },
      )
      .whenComplete(forget);
  return out.future;
}

/// The language's name in English, for the prompts.
String languageName(String code) => switch (code.toLowerCase()) {
  'it' => 'Italian',
  'en' => 'English',
  'fr' => 'French',
  'de' => 'German',
  'es' => 'Spanish',
  'pt' => 'Portuguese',
  'nl' => 'Dutch',
  'ja' => 'Japanese',
  'zh' => 'Chinese',
  'ko' => 'Korean',
  'ru' => 'Russian',
  'pl' => 'Polish',
  'sv' => 'Swedish',
  'da' => 'Danish',
  'tr' => 'Turkish',
  _ => 'the language the person speaks',
};

/// The call's front desk, as on-device Foundation Models is told (the same
/// words, so every model answers alike), with the answer as JSON since other
/// models have no guided generation. [memory], when given, is what the
/// project does, from its functional memory ([shortened] when part of it
/// was left out).
String intakeInstructions(
  String language, {
  String memory = '',
  bool shortened = false,
}) {
  final name = languageName(language);
  final known = memory.trim().isEmpty
      ? ''
      : 'What the project does today, from its functional memory'
            '${shortened ? ' (shortened)' : ''}. Use it to understand the '
            'person and ask informed questions; how the code is built is '
            'still for the project to check.\n\n${memory.trim()}\n\n';
  return 'You answer a phone call for an AI coding agent that works on the '
      "person's software project. You cannot see the project's code. Hold a "
      'short, natural conversation in $name to understand what the person '
      'wants done: the goal, what to change, and how to tell it is done. Reply '
      'in one or two short sentences, like a person on the phone. Ask at most '
      'one question at a time, and only what matters. Answer greetings and '
      'small talk briefly and warmly. Never propose a task, sum it up or ask '
      'for confirmation yourself. Choose check as soon as the request is '
      "nearly clear, or when the next question needs knowledge of the project's "
      'code: the project will check it. Otherwise choose talk. Always write in '
      '$name. No markdown, lists or code.\n\n'
      '$known'
      'Answer with one JSON object and nothing else: '
      '{"next": "talk" or "check", "say": what to say aloud now when next is '
      'talk, one or two short sentences; empty for check}.';
}

/// The prompt for the conversation so far, as Foundation Models gets it.
String intakePrompt(List<(bool, String)> turns) {
  final recent = turns.length > 16 ? turns.sublist(turns.length - 16) : turns;
  final lines = [
    for (final (mine, text) in recent) '${mine ? 'Person' : 'You'}: $text',
  ];
  return 'The conversation so far:\n\n${lines.join('\n')}\n\nAnswer the person.';
}

/// How a long reply is turned into speech, as Foundation Models is told.
String condenseInstructions(String language) =>
    "You turn an AI coding agent's written reply into what a phone assistant "
    'would say out loud. Use at most three short sentences in plain speech: no '
    'markdown, no lists, no code, no file paths or URLs read out. Keep the '
    'outcome, any question the agent asks, and anything the person must do. '
    'Always answer in ${languageName(language)}, whatever the reply\'s language.';

/// The most of a reply a model is given to condense.
const condenseBudget = 12000;

String condensePrompt(String reply) {
  final cut = reply.length > condenseBudget
      ? '${reply.substring(0, condenseBudget)}…'
      : reply;
  return 'Reply to read aloud:\n\n$cut';
}

/// [text] without a model's reasoning (`<think>…</think>`, or one never
/// closed) and without code fences around it.
String withoutReasoning(String text) {
  var out = text.replaceAll(RegExp(r'<think>[\s\S]*?</think>'), '');
  final open = out.indexOf('<think>');
  if (open >= 0) out = out.substring(0, open);
  final close = out.lastIndexOf('</think>');
  if (close >= 0) out = out.substring(close + '</think>'.length);
  out = out.trim();
  final fence = RegExp(r'^```[a-zA-Z]*\s*([\s\S]*?)\s*```$').firstMatch(out);
  return (fence?[1] ?? out).trim();
}

/// A model's intake answer: the JSON asked for, or, from a model that wrote
/// plain words, those words to say. Null when there is nothing to use.
Intake? parseIntake(String answer) {
  final text = withoutReasoning(answer);
  if (text.isEmpty) return null;
  final start = text.indexOf('{'), end = text.lastIndexOf('}');
  if (start >= 0 && end > start) {
    try {
      final v = jsonDecode(text.substring(start, end + 1));
      if (v is Map) {
        final next = '${v['next'] ?? ''}'.trim().toLowerCase();
        final say = '${v['say'] ?? ''}'.trim();
        if (next == 'check') return const Intake(check: true);
        return say.isEmpty ? null : Intake(say: say);
      }
    } on FormatException {
      // Not JSON after all: taken as words below.
    }
  }
  if (RegExp(r'^\W*check\W*$', caseSensitive: false).hasMatch(text)) {
    return const Intake(check: true);
  }
  // Plain words, as a model may answer despite the format.
  return text.contains('{') ? null : Intake(say: text);
}

/// How the link to the Mac's model has been doing: recent round trips and
/// failures. A slow or failing link is skipped for a while (longer each
/// time), so a turn never waits out a timeout it can expect; a Mac with no
/// model is asked again only after a long while.
class RemoteHealth {
  RemoteHealth({DateTime Function()? now}) : _now = now ?? DateTime.now;
  final DateTime Function() _now;

  /// One per host, for the life of the app: calls come and go.
  static final _byHost = <String, RemoteHealth>{};
  static RemoteHealth of(String host) =>
      _byHost.putIfAbsent(host, RemoteHealth.new);

  static const unavailableFor = Duration(minutes: 10);
  static const firstBackoff = Duration(seconds: 15);
  static const maxBackoff = Duration(minutes: 5);

  /// A round trip over this share of its deadline counts as slow.
  static const slowShare = 0.8;
  static const slowFor = Duration(seconds: 30);

  DateTime _skipUntil = DateTime(0);
  int _failures = 0;

  /// The last round trip of a turn, and how much of their deadlines recent
  /// ones took (smoothed: 1 is all of it); null before any.
  Duration? rtt;
  double? share;

  bool get usable => !_now().isBefore(_skipUntil);

  /// An answer came after [took], for a request given [deadline]. A warm-up
  /// ([measure] false) says the link works, not how fast turns go: loading
  /// the model is slow once.
  void succeeded(Duration took, Duration deadline, {bool measure = true}) {
    _failures = 0;
    if (!measure) return;
    rtt = took;
    final now = took.inMicroseconds / deadline.inMicroseconds;
    share = share == null ? now : share! * 0.6 + now * 0.4;
    if (share! > slowShare) _skipUntil = _now().add(slowFor);
  }

  void failed() {
    _failures++;
    final ms = firstBackoff.inMilliseconds * pow(2, min(_failures - 1, 10));
    _skipUntil = _now().add(
      Duration(milliseconds: min(ms.toInt(), maxBackoff.inMilliseconds)),
    );
  }

  void unavailable() {
    _failures = 0;
    _skipUntil = _now().add(unavailableFor);
  }
}

/// The Mac's conversation model, asked through the peer link: `op` runs a
/// peer operation (the call's `call`), on host [host].
class RemoteModel implements ChatModel {
  RemoteModel(this.op, this.host, {RemoteHealth? health})
    : health = health ?? RemoteHealth.of(host);
  final Future<dynamic> Function(String, Map<String, Object?>) op;
  final String host;
  final RemoteHealth health;

  /// How much longer than the deadline the peer may take to say it gave up.
  static const grace = Duration(milliseconds: 300);

  @override
  Future<String?> complete(
    List<ChatMessage> messages, {
    required int maxTokens,
    required double temperature,
    required Duration deadline,
    Cancellation? cancel,
  }) => _ask(
    messages,
    maxTokens: maxTokens,
    temperature: temperature,
    deadline: deadline,
    cancel: cancel,
  );

  /// Called off ([cancel]), the answer is no longer waited for. The peer
  /// link has no way to stop the Mac's model, which gives up by itself at
  /// [deadline]; an abandoned request says nothing about the link's health.
  Future<String?> _ask(
    List<ChatMessage> messages, {
    required int maxTokens,
    required double temperature,
    required Duration deadline,
    bool measure = true,
    Cancellation? cancel,
  }) async {
    if (!health.usable || (cancel?.cancelled ?? false)) return null;
    final started = Stopwatch()..start();
    try {
      final asked = op('complete', {
        'host': host,
        'messages': [
          for (final (role, text) in messages) {'role': role, 'content': text},
        ],
        'maxTokens': maxTokens,
        'temperature': temperature,
        'timeoutMs': deadline.inMilliseconds,
      }).timeout(deadline + grace);
      final reply = await unlessCancelled<Object?>(asked, cancel);
      if (cancel?.cancelled ?? false) {
        // Its answer, when one comes, is not wanted; nor is its error.
        unawaited(asked.then((_) {}, onError: (_) {}));
        return null;
      }
      final text = reply is Map ? reply['text'] : null;
      if (text is String && text.trim().isNotEmpty) {
        health.succeeded(started.elapsed, deadline, measure: measure);
        return text;
      }
      // No answer, and no reason: a peer that does not know the op.
      if (reply is! Map || reply['unavailable'] == true) {
        health.unavailable();
      } else if ('${reply['error']}'.contains('busy')) {
        // The Mac's model is still on earlier requests (called off here,
        // which the peer link cannot stop): not the link's fault, and the
        // next turn may find it free.
      } else {
        health.failed();
      }
    } catch (_) {
      // Too slow, or the link failed.
      if (!(cancel?.cancelled ?? false)) health.failed();
    }
    return null;
  }

  /// Gets the Mac's model loaded, and finds out whether it answers at all,
  /// before the first turn needs it. Its time is not a turn's.
  Future<void> prewarm() async {
    if (!health.usable) return;
    await _ask(
      [('user', 'OK')],
      maxTokens: 1,
      temperature: 0,
      deadline: const Duration(seconds: 15),
      measure: false,
    );
  }
}

/// The on-phone assistant (a small model run on this phone), when the person
/// turned it on and it is downloaded; null otherwise.
class PhoneModel implements ChatModel {
  PhoneModel(this.voice);
  final VoiceEngine voice;

  /// The most of each message it is given: its context is small.
  static const maxChars = 6000;

  @override
  Future<String?> complete(
    List<ChatMessage> messages, {
    required int maxTokens,
    required double temperature,
    required Duration deadline,
    Cancellation? cancel,
  }) async {
    if (cancel?.cancelled ?? false) return null;
    try {
      return await voice
          .localComplete(
            [
              for (final (role, text) in messages)
                (
                  role,
                  text.length > maxChars
                      ? '${text.substring(0, maxChars)}…'
                      : text,
                ),
            ],
            maxTokens: maxTokens,
            temperature: temperature,
            timeout: deadline,
            cancel: cancel,
          )
          // It stops itself at the deadline; this is only a backstop.
          .timeout(deadline + const Duration(seconds: 1));
    } catch (_) {
      return null;
    }
  }
}

/// The call's language model work, from the first backend that answers:
/// Foundation Models on the device, then the Mac's model, then the phone's
/// own small one. Null from all: the call does without (it asks the project
/// at once, and cuts long replies short).
class Assistant {
  Assistant({required this.voice, this.mac, this.phone});
  final VoiceEngine voice;
  final ChatModel? mac, phone;

  /// What the project does, from its functional memory: all of it (up to
  /// [memoryBudget]) for a large model, README.md and overview.md only for
  /// a small one on the phone. Empty: none, or a host too old to say.
  String _memory = '', _excerpt = '';
  bool _shortened = false;

  /// Whether the project's functional memory is known.
  bool get knowsProject => _memory.isNotEmpty;

  /// Gives the models the project's functional memory ([truncated] when
  /// the host left some of it out).
  void know(List<MemoryFile> memory, {bool truncated = false}) {
    final (all, shortened) = memoryDigest(memory, truncated: truncated);
    _memory = all.trim();
    _shortened = shortened;
    _excerpt = memoryExcerpt(memory);
  }

  /// How long the Mac is given: the link should answer fast or be skipped.
  static const clarifyDeadline = Duration(milliseconds: 2500);
  static const condenseDeadline = Duration(seconds: 6);

  /// How long the phone is given: it is the last model, and slower.
  static const phoneClarifyDeadline = Duration(seconds: 10);
  static const phoneCondenseDeadline = Duration(seconds: 20);

  /// The one-or-two-sentence answer to the conversation so far, or a check.
  /// Called off through [cancel] (a newer turn made it useless), the model
  /// working on it stops (on the phone at once; the Mac's at its deadline)
  /// and no later one is asked: null.
  Future<Intake?> clarify(
    List<(bool, String)> turns, {
    String language = 'en',
    Cancellation? cancel,
  }) async {
    bool off() => cancel?.cancelled ?? false;
    if (off()) return null;
    try {
      final own = await voice.clarify(
        turns,
        language: language,
        cancel: cancel,
        memory: _excerpt,
      );
      if (off()) return null;
      if (own != null) return own;
    } catch (_) {
      // Taken as no on-device model.
    }
    for (final model in [?mac, ?phone]) {
      if (off()) return null;
      // The phone's own model has a small context: the excerpt only.
      final small = identical(model, phone);
      final messages = [
        (
          'system',
          intakeInstructions(
            language,
            memory: small ? _excerpt : _memory,
            shortened: small || _shortened,
          ),
        ),
        ('user', intakePrompt(turns)),
      ];
      final answer = await model.complete(
        messages,
        maxTokens: 120,
        temperature: 0.3,
        deadline: identical(model, phone)
            ? phoneClarifyDeadline
            : clarifyDeadline,
        cancel: cancel,
      );
      if (off()) return null;
      final intake = answer == null ? null : parseIntake(answer);
      if (intake != null) return intake;
    }
    return null;
  }

  /// [text] as a few sentences to say.
  Future<String?> condense(String text, {String language = 'en'}) async {
    try {
      final own = await voice.condense(text, language: language);
      if (own != null && own.trim().isNotEmpty) return own;
    } catch (_) {
      // Taken as no on-device model.
    }
    final messages = [
      ('system', condenseInstructions(language)),
      ('user', condensePrompt(text)),
    ];
    for (final model in [?mac, ?phone]) {
      final answer = await model.complete(
        messages,
        maxTokens: 160,
        temperature: 0.3,
        deadline: identical(model, phone)
            ? phoneCondenseDeadline
            : condenseDeadline,
      );
      final spoken = answer == null ? '' : withoutReasoning(answer);
      if (spoken.isNotEmpty) return spoken;
    }
    return null;
  }
}
