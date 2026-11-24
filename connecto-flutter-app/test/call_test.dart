import 'dart:async';
import 'dart:math';

import 'package:connecto_flutter_app/call.dart';
import 'package:connecto_flutter_app/main.dart';
import 'package:connecto_flutter_app/peer.dart';
import 'package:connecto_flutter_app/settings.dart';
import 'package:connecto_voice/connecto_voice.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

/// A device voice that records what it is asked and lets the test say things.
class FakeVoice implements VoiceEngine {
  FakeVoice({this.available = true, this.condensed});
  final bool available;
  final String? condensed;
  final heard = StreamController<VoiceEvent>.broadcast();
  final spoken = <String>[];
  final condenseAsked = <String>[];
  var cues = 0;
  var stopped = false, speechStopped = 0;
  bool? muted;
  Completer<bool>? speaking;

  /// While set, speaking fails (no voice).
  var failSpeak = false;

  /// Speech resolves at once unless this is set; then it waits to be stopped
  /// or [finishSpeaking].
  var holdSpeech = false;

  /// What of an utterance stopped early counts as heard: its first this
  /// many characters.
  var heardWhenStopped = 0;

  void say(String text, {bool isFinal = false, double? start, double? end}) =>
      heard.add(Heard(text, isFinal: isFinal, start: start, end: end));
  void finishSpeaking() => speaking?.complete(true);

  @override
  Stream<VoiceEvent> get events => heard.stream;
  @override
  Future<VoiceSupport> support() async => VoiceSupport(available: available);
  @override
  Future<void> start() async => heard.add(const Listening());
  @override
  Future<void> stop() async => stopped = true;

  var languageList = const VoiceLanguages([
    VoiceLanguage('en-US', 'English (United States)', installed: true),
    VoiceLanguage('it-IT', 'Italian (Italy)', installed: false),
  ], current: 'en-US');
  final languagesSet = <String?>[];
  @override
  Future<VoiceLanguages> languages() async => languageList;
  @override
  Future<void> setLanguage(String? id) async {
    languagesSet.add(id);
    languageList = VoiceLanguages(
      languageList.all,
      selected: id,
      current: id ?? 'en-US',
    );
  }

  VoiceLanguages _with({String? voice, bool? showTimings}) => VoiceLanguages(
    languageList.all,
    selected: languageList.selected,
    current: languageList.current,
    voices: languageList.voices,
    voice: voice ?? languageList.voice,
    speed: languageList.speed,
    engine: languageList.engine,
    tooSlow: languageList.tooSlow,
    showTimings: showTimings ?? languageList.showTimings,
  );

  /// Every voice chosen, as the device keeps it across launches.
  final voicesSet = <String>[];
  @override
  Future<void> setVoice(String choice) async {
    voicesSet.add(choice);
    languageList = _with(voice: choice);
  }

  @override
  Future<void> setShowTimings(bool on) async =>
      languageList = _with(showTimings: on);

  /// Whether the natural voice is loaded, and how often a call asked.
  var naturalLoaded = false;
  var naturalAsked = 0;
  @override
  Future<bool> naturalReady() async {
    naturalAsked++;
    return naturalLoaded;
  }

  /// How each utterance was asked for: natural voice, and its plan.
  final spokenWith = <(bool, List<SpeechPart>?)>[];

  @override
  Future<void> mute(bool value) async => muted = value;
  @override
  Future<Spoken> speak(
    String text, {
    bool natural = true,
    List<SpeechPart>? plan,
  }) async {
    spoken.add(text);
    spokenWith.add((natural, plan));
    if (failSpeak) throw Exception('No voice.');
    if (!holdSpeech) return Spoken.all(text);
    final mine = speaking = Completer<bool>();
    return await mine.future
        ? Spoken.all(text)
        : Spoken(
            completed: false,
            heard: text.substring(0, min(heardWhenStopped, text.length)),
          );
  }

  /// How long stopping speech takes before the speech says how it went.
  var stopDelay = Duration.zero;

  @override
  Future<void> stopSpeaking() async {
    speechStopped++;
    if (stopDelay > Duration.zero) await Future<void>.delayed(stopDelay);
    if (speaking?.isCompleted == false) speaking!.complete(false);
  }

  @override
  Future<void> cue() async => cues++;

  var speechPaused = 0, speechResumed = 0, turnsEnded = 0;
  @override
  Future<void> pauseSpeaking() async => speechPaused++;
  @override
  Future<void> resumeSpeaking() async => speechResumed++;
  @override
  Future<void> endTurn() async => turnsEnded++;

  /// Every hold tone change, in order.
  final holds = <bool>[];
  @override
  Future<void> hold(bool on) async => holds.add(on);

  /// What the on-device intake answers; null: no model.
  Intake? Function(List<(bool, String)>)? intake;
  final clarified = <List<(bool, String)>>[];
  final modelLanguages = <String>[];

  /// While set, the on-device intake waits for it (or to be called off).
  Completer<void>? clarifyGate;

  /// The cancellation each intake was given.
  final clarifyCancels = <Cancellation?>[];

  /// The memory excerpt each on-device intake was given.
  final clarifyMemory = <String>[];
  @override
  Future<Intake?> clarify(
    List<(bool, String)> turns, {
    String language = 'en',
    Cancellation? cancel,
    String memory = '',
  }) async {
    clarified.add([...turns]);
    clarifyMemory.add(memory);
    modelLanguages.add(language);
    clarifyCancels.add(cancel);
    if (clarifyGate case final gate?) {
      final off = Completer<void>();
      cancel?.onCancel(() {
        if (!off.isCompleted) off.complete();
      });
      await Future.any([gate.future, off.future]);
      if (cancel?.cancelled ?? false) return null;
    }
    return intake?.call(turns);
  }

  /// The on-phone assistant, as the test sets it up.
  var phone = PhoneModelState.none;
  final phoneSet = <bool>[];
  var phonePrewarms = 0;

  /// What the on-phone assistant answers; null: nothing.
  String? Function(List<(String, String)>)? local;
  final localAsked = <(List<(String, String)>, Duration)>[];

  @override
  Future<PhoneModelState> phoneModel() async => phone;
  @override
  Future<PhoneModelState> setPhoneModel(bool on) async {
    phoneSet.add(on);
    phone = PhoneModelState(
      on: on,
      ready: false,
      progress: on ? 0 : null,
      bytes: phone.bytes,
    );
    return phone;
  }

  @override
  Future<void> prewarmPhoneModel() async => phonePrewarms++;
  @override
  Future<String?> localComplete(
    List<(String, String)> messages, {
    int maxTokens = 120,
    double temperature = 0.3,
    Duration timeout = const Duration(seconds: 10),
    Cancellation? cancel,
  }) async {
    localAsked.add((messages, timeout));
    return local?.call(messages);
  }

  /// While set, condensing waits for it.
  Completer<void>? condenseGate;
  @override
  Future<String?> condense(String text, {String language = 'en'}) async {
    condenseAsked.add(text);
    modelLanguages.add(language);
    if (condenseGate != null) await condenseGate!.future;
    return condensed;
  }
}

/// A host whose chat the test changes; records every op.
class FakeHost {
  Data chat = {
    'id': 's1',
    'status': 'Ready',
    'chatReady': true,
    'messages': <List>[],
  };
  final ops = <(String, Data)>[];

  /// When false, a send changes nothing until the test says so: the host
  /// has not shown the prompt yet.
  var echoes = true;

  /// When true, the stop op fails; while set, it waits for [stopGate].
  var failStop = false;
  Completer<void>? stopGate;

  /// While set, a send waits for it; [failSend] makes it fail.
  Completer<void>? sendGate;
  var failSend = false;
  Completer<void>? holdChat;

  /// Every op as the call made it (before `sendPrompt` is recorded as a
  /// send).
  final rawOps = <String>[];
  Future<dynamic> call(String op, Data fields) async {
    // The call's own prompts go through the host's conditional send (its
    // rules are the host's, tested there): recorded with the plain sends,
    // with no question, as the call sends them.
    rawOps.add(op);
    if (op == 'sendPrompt') {
      op = 'send';
      fields = {...fields, 'question': null};
    }
    ops.add((op, fields));
    if (op == 'stop') {
      if (stopGate != null) await stopGate!.future;
      if (failStop) throw Exception('No link to the host.');
      // As the native peer: only a running agent is stopped.
      if (chat['status'] != 'Running') throw Exception('Not running.');
    }
    if (op == 'chat' && holdChat != null) await holdChat!.future;
    if (op == 'send') {
      if (sendGate != null) await sendGate!.future;
      if (failSend) throw Exception('No link to the host.');
    }
    if (op == 'send' && echoes) {
      chat = {
        ...chat,
        'status': 'Running',
        'messages': [
          ...chat['messages'] as List,
          [true, fields['text']],
        ],
      };
    }
    if (op == 'functional_memory') {
      // Null: a host too old to know the op.
      if (memory == null) throw Exception('Unknown operation.');
      return {'files': memory, 'truncated': false};
    }
    if (op == 'complete') return completion?.call(fields);
    if (op == 'turnCheck') return await turnCheck?.call(fields);
    return op == 'chat' ? chat : null;
  }

  /// The project's functional memory files; null: a host without the op.
  List<Map<String, String>>? memory;

  /// What the Mac's Jev answers about a turn; null: a peer without the op.
  FutureOr<Object?> Function(Data)? turnCheck;

  /// What the host's conversation model answers; null: a host without one.
  Object? Function(Data)? completion;

  void reply(String text, {String status = 'Ready'}) => chat = {
    ...chat,
    'status': status,
    'messages': [
      ...chat['messages'] as List,
      [false, text],
    ],
  };

  List<Data> sent(String op) => [
    for (final (o, f) in ops)
      if (o == op) f,
  ];
}

const target = {'host': 'Host', 'project': 'p'};

/// Long enough for a turn with no end-of-turn estimate to be sent (the
/// default 2 s of silence, and a little).
const turnGap = Duration(milliseconds: 2100);

void main() {
  testWidgets('settings choose the voice and keep it; timings can be shown', (
    tester,
  ) async {
    final voice = FakeVoice()
      ..languageList = const VoiceLanguages(
        [VoiceLanguage('it-IT', 'Italiano', installed: true)],
        current: 'it-IT',
        voices: ['giovanni', 'vera', 'anna', 'eve', 'alba'],
        voice: 'giovanni',
      );
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsPage(voice: voice, version: () async => 'test'),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('Giovanni'), findsOneWidget);
    await tester.tap(find.text('Voice'));
    await tester.pumpAndSettle();
    for (final name in ['Vera', 'Anna', 'Eve', 'Alba', 'System voice']) {
      expect(find.text(name), findsOneWidget, reason: name);
    }
    expect(find.text('Recorded in this language'), findsOneWidget);
    await tester.tap(find.text('Vera'));
    await tester.pumpAndSettle();
    expect(voice.voicesSet, ['vera']);
    expect(find.text('Vera'), findsOneWidget, reason: 'shown as chosen');

    // Kept: a new Settings page reads it back.
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsPage(
          key: UniqueKey(),
          voice: voice,
          version: () async => 'test',
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('Vera'), findsOneWidget);

    await tester.tap(find.text('Show voice timings'));
    await tester.pumpAndSettle();
    expect(voice.languageList.showTimings, isTrue);

    // Too slow here: said once, in Settings.
    voice.languageList = const VoiceLanguages(
      [VoiceLanguage('it-IT', 'Italiano', installed: true)],
      current: 'it-IT',
      voices: ['giovanni'],
      voice: 'giovanni',
      speed: 0.8,
      tooSlow: true,
    );
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsPage(
          key: UniqueKey(),
          voice: voice,
          version: () async => 'test',
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.textContaining('slower than real time'), findsOneWidget);

    // A language it does not speak: no voice to choose.
    voice.languageList = const VoiceLanguages([
      VoiceLanguage('ja-JP', 'Japanese', installed: true),
    ], current: 'ja-JP');
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsPage(
          key: UniqueKey(),
          voice: voice,
          version: () async => 'test',
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('Voice'), findsNothing);
  });

  group('speakable', () {
    test('drops markdown and reads code only once', () {
      expect(
        speakable(
          '## Done\n'
          'I fixed **two** bugs in `main.dart`:\n'
          '- the *parser*\n'
          '- the [docs](https://example.com/x)\n'
          '```dart\nvoid main() {}\n```\n'
          '```sh\nls\n```\n'
          'See https://example.com for more',
        ),
        'Done. I fixed two bugs in main.dart: the parser. the docs. '
        'There is code in the chat. See a link for more.',
      );
    });

    test('keeps underscores inside names and drops tables', () {
      expect(
        speakable('Run snake_case_name.\n| a | b |\n|---|---|\n| 1 | 2 |'),
        'Run snake_case_name.',
      );
    });
  });

  group('spokenReply', () {
    test('short replies are said as they are, without the model', () async {
      final asked = <String>[];
      expect(
        await spokenReply('All tests pass.', (t) async {
          asked.add(t);
          return 'x';
        }),
        'All tests pass.',
      );
      expect(asked, isEmpty);
    });

    test('long replies use the on-device model', () async {
      final long = List.filled(40, 'The build is green.').join(' ');
      expect(
        await spokenReply(long, (_) async => '**Green**, ship it.'),
        'Green, ship it.',
      );
    });

    test(
      'without a model, long replies are cut and point to the chat',
      () async {
        final long = 'First thing. Second thing. ${'More. ' * 80}';
        expect(
          await spokenReply(long, (_) async => null),
          'First thing. Second thing. The rest is in the chat.',
        );
      },
    );
  });

  group('heardBeforeCut', () {
    const said = 'Sure. Which page shows it, the login or the signup?';
    test('keeps the whole clauses heard, marked as cut off', () {
      expect(
        heardBeforeCut(said, 'Sure. Which pa'),
        'Sure. [cut off here by the person]',
      );
      expect(
        heardBeforeCut(said, 'Sure. Which page shows it, the lo'),
        'Sure. Which page shows it, [cut off here by the person]',
      );
    });

    test('nothing whole heard is a mark alone; all of it is all of it', () {
      expect(
        heardBeforeCut(said, 'Su'),
        '[cut off by the person: how much of it they heard is not known]',
      );
      expect(
        heardBeforeCut(said, ''),
        '[cut off by the person: how much of it they heard is not known]',
      );
      expect(heardBeforeCut(said, said), said);
      // Not a prefix after all: only what matches counts.
      expect(
        heardBeforeCut(said, 'Sure. Whatever was said'),
        'Sure. [cut off here by the person]',
      );
    });

    test("the device's answer reads either way", () {
      expect(spokenFrom(null, 'Hi.'), const Spoken.all('Hi.'));
      expect(spokenFrom(true, 'Hi.'), const Spoken.all('Hi.'));
      expect(
        spokenFrom(false, 'Hi.'),
        const Spoken(completed: false, heard: ''),
      );
      expect(
        spokenFrom({'completed': false, 'heard': 'Sure.'}, 'Sure. Ok?'),
        const Spoken(completed: false, heard: 'Sure.'),
      );
      expect(
        spokenFrom({'completed': true, 'heard': 'Sure.'}, 'Sure. Ok?'),
        const Spoken.all('Sure. Ok?'),
      );
    });
  });

  group('Cancellation', () {
    test('tells its listeners once, and late ones at once', () {
      final c = Cancellation();
      var told = 0, gone = 0;
      c.onCancel(() => told++);
      final forget = c.onCancel(() => gone++);
      forget();
      c.cancel();
      c.cancel();
      expect((told, gone, c.cancelled), (1, 0, true));
      c.onCancel(() => told++);
      expect(told, 2);
    });
  });

  group('replyAfter', () {
    test('finds a long prompt that Claude Code kept inside its paste wrapper', () {
      const sent = 'Check this request.\n\nThe call so far:\nPerson: Ciao';
      final now = [
        [
          true,
          '\n\n<pasted_content id="0231">\n$sent\n</pasted_content id="0231">\n',
        ],
        [false, 'QUESTIONS\n1. Quale store?'],
      ];
      expect(replyAfter(const [], now, sent), 'QUESTIONS\n1. Quale store?');
      expect(
        unwrapPaste('<pasted_content id="a">x</pasted_content id="b">'),
        '<pasted_content id="a">x</pasted_content id="b">',
      );
    });

    final before = [
      [true, 'old'],
      [false, 'old reply'],
    ];

    test("takes the agent's messages after the new prompt", () {
      final now = [
        ...before,
        [true, 'Run the tests.'],
        [false, 'Running.'],
        [false, 'All green.'],
      ];
      expect(
        replyAfter(before, now, 'run the tests'),
        'Running.\n\nAll green.',
      );
    });

    test('is null until the prompt and a reply arrive', () {
      expect(replyAfter(const [], const [], 'hi'), isNull);
      expect(
        replyAfter(before, [
          ...before,
          [true, 'hi'],
        ], 'hi'),
        isNull,
      );
      expect(
        replyAfter(before, [
          ...before,
          [true, 'something else'],
          [false, 'reply'],
        ], 'hi'),
        isNull,
      );
    });

    test('a check handed to a helper is followed through the wake notice, '
        'and only through that', () {
      const notice = '[connecto notice] Request r1 ended (ok).';
      final now = [
        [true, 'check it'],
        [false, 'Checking. A helper reads the code.'],
        [true, '<pasted_content id="x">\n$notice\n</pasted_content id="x">'],
        [false, 'TASK\nTitle: Fix it'],
        [true, notice],
        [false, 'Something else.'],
      ];
      expect(repliesAfter(const [], now, 'check it'), [
        'Checking. A helper reads the code.',
      ]);
      expect(repliesAfter(const [], now, 'check it', throughHandoffs: true), [
        'Checking. A helper reads the code.',
        'TASK\nTitle: Fix it',
      ]);
      // The person's own words end it, notice or not.
      final person = [
        ...now.take(2),
        [true, 'And the other page?'],
        [false, 'TASK\nTitle: Other'],
      ];
      expect(
        repliesAfter(const [], person, 'check it', throughHandoffs: true),
        ['Checking. A helper reads the code.'],
      );
      // Where a later read starts: just after the reply.
      expect(throughReply(const [], now, 'check it'), now.take(2).toList());
    });

    test('check markers carry an id, or none', () {
      expect(checkMarker('Checking [c-1a2b]. Reading.', 'checking'), 'c-1a2b');
      expect(checkMarker('**Findings (C-9).** It was X.', 'findings'), 'c-9');
      expect(checkMarker('Checking. Reading.', 'checking'), '');
      expect(checkMarker('I am checking it.', 'checking'), isNull);
      expect(isCheckingNote('Findings [c-1]. X'), isFalse);
    });

    test('never answers with the reply to an earlier identical prompt', () {
      final history = [
        [true, 'run the tests'],
        [false, 'Old result.'],
      ];
      expect(replyAfter(history, history, 'run the tests'), isNull);
      expect(
        replyAfter(history, [
          ...history,
          [true, 'run the tests'],
          [false, 'New result.'],
        ], 'run the tests'),
        'New result.',
      );
    });

    test("stops at the next prompt: that reply is someone else's", () {
      expect(
        replyAfter(before, [
          ...before,
          [true, 'hi'],
          [false, 'hello'],
          [true, 'from the desktop'],
          [false, 'desktop answer'],
        ], 'hi'),
        'hello',
      );
    });

    test('follows a history whose oldest messages were dropped', () {
      final now = [
        before[1],
        [true, 'hi'],
        [false, 'hello'],
      ];
      expect(newMessages(before, now), now.sublist(1));
      expect(replyAfter(before, now, 'hi'), 'hello');
    });
  });

  group('CallController', () {
    late FakeHost host;
    late FakeVoice voice;
    late CallController call;

    Future<void> begin(WidgetTester tester) async {
      host = FakeHost();
      voice = FakeVoice();
      call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      // These tests follow a task already handed to the agent: what is said
      // is sent as it is.
      call.started = true;
      await call.start();
      await tester.pump();
    }

    /// A test with a started call, hung up before the binding checks timers.
    void callTest(String name, Future<void> Function(WidgetTester) body) =>
        testWidgets(name, (tester) async {
          await begin(tester);
          await body(tester);
          call.dispose();
          await tester.pump();
        });

    callTest('sends a turn after silence and speaks the reply', (tester) async {
      expect(call.phase, CallPhase.listening);
      expect(voice.spoken, ['What would you like done?']);

      voice.say('Run the');
      await tester.pump(const Duration(milliseconds: 500));
      voice.say('Run the tests', isFinal: true);
      await tester.pump(const Duration(milliseconds: 1500));
      expect(host.sent('send'), isEmpty, reason: 'a pause to think');
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.sent('send').single, {
        ...target,
        'id': 's1',
        'text': 'Run the tests',
        'question': null,
      });
      expect(call.phase, CallPhase.working);

      await tester.pump(const Duration(milliseconds: 200));
      expect(call.phase, CallPhase.working);
      host.reply('# Result\nAll **42** tests pass.');
      await tester.pump(const Duration(milliseconds: 200));
      expect(voice.spoken.last, 'Result. All 42 tests pass.');
      expect(call.phase, CallPhase.listening);
    });

    callTest("a reply waits for the person's voice to stop, then is said", (
      tester,
    ) async {
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.working);
      // Their voice, before any words: the reply is not said over it.
      voice.heard.add(const VoiceActivity(true));
      await tester.pump();
      host.reply('Done.');
      await tester.pump(const Duration(milliseconds: 500));
      expect(voice.spoken, isNot(contains('Done.')));
      expect(call.phase, CallPhase.listening);

      voice.heard.add(const VoiceActivity(false));
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Done.');
    });

    callTest('a voice that never stops holds a reply back only so long', (
      tester,
    ) async {
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      voice.heard.add(const VoiceActivity(true));
      await tester.pump();
      host.reply('Done.');
      await tester.pump(const Duration(seconds: 7));
      expect(voice.spoken, isNot(contains('Done.')));
      await tester.pump(const Duration(seconds: 2));
      expect(voice.spoken.last, 'Done.', reason: 'a noisy room, not a person');
    });

    callTest('words said as the reply comes go first; the reply after them', (
      tester,
    ) async {
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      voice.say('and also the docs');
      await tester.pump(const Duration(milliseconds: 100));
      host.reply('Done.');
      await tester.pump(const Duration(milliseconds: 500));
      expect(voice.spoken, isNot(contains('Done.')));

      voice.say('and also the docs', isFinal: true);
      await tester.pump(turnGap);
      // Their turn went to the agent, then the reply was said.
      expect(host.sent('send').last['text'], 'and also the docs');
      expect(voice.spoken.last, 'Done.');
      expect(call.phase, CallPhase.working, reason: 'their turn is awaited');
    });

    callTest('words said as a question comes are not its answer', (
      tester,
    ) async {
      voice.say('run it', isFinal: true);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.working);
      voice.say('and the docs');
      await tester.pump(const Duration(milliseconds: 100));
      host.chat = {
        ...host.chat,
        'status': 'Needs an answer',
        'question': 'Which branch?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, isNot(contains('Which branch?')));

      // Their turn ends: kept for later, not sent as the answer.
      voice.say('and the docs too', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('send'), hasLength(1));
      expect(voice.spoken.last, contains('Which branch?'));

      // Heard now: the next words answer it.
      voice.say('main', isFinal: true, start: 20, end: 20.5);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'main');
      expect(host.sent('send').last['question'], 'Which branch?');
      // An answer to the agent's question goes as before; the call's own
      // prompts through the host's conditional send (ADR 0034, decision c).
      expect(host.rawOps.where((op) => op == 'send'), hasLength(1));
      host.chat = {...host.chat, 'question': null};
      host.reply('On main.');
      await tester.pump(const Duration(milliseconds: 500));
      expect(host.sent('send').last['text'], 'and the docs too');
      expect(host.rawOps.where((op) => op == 'sendPrompt'), hasLength(2));
    });

    callTest('a reply that asks something in words is not answered by words '
        'said before it', (tester) async {
      voice.say('run it', isFinal: true);
      await tester.pump(turnGap);
      voice.say('and the');
      await tester.pump(const Duration(milliseconds: 100));
      host.reply('Done, one thing first.', status: 'Needs an answer');
      host.chat = {
        ...host.chat,
        'question': 'Which branch?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('and the docs', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('send'), hasLength(1), reason: 'kept, not the answer');
      expect(voice.spoken.last, contains('Which branch?'));

      voice.say('main', isFinal: true, start: 20, end: 20.5);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'main');
      expect(host.sent('send').last['question'], 'Which branch?');
    });

    callTest('a question replaced while it waits: the newer one is asked and '
        'answered', (tester) async {
      voice.say('run it', isFinal: true);
      await tester.pump(turnGap);
      voice.say('and the');
      await tester.pump(const Duration(milliseconds: 100));
      host.chat = {
        ...host.chat,
        'status': 'Needs an answer',
        'question': 'Which branch?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 300));
      // Replaced before it could be said.
      host.chat = {...host.chat, 'question': 'Which remote?'};
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('and the docs', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.spoken.last, contains('Which remote?'));
      expect(voice.spoken, isNot(contains(contains('Which branch?'))));

      voice.say('origin', isFinal: true, start: 20, end: 20.5);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'origin');
      expect(host.sent('send').last['question'], 'Which remote?');

      // Asked and said; then replaced after it was said: words for the old
      // one wait, and the new one is asked.
      host.chat = {
        ...host.chat,
        'status': 'Needs an answer',
        'question': 'Push now?',
      };
      await tester.pump(const Duration(milliseconds: 400));
      expect(voice.spoken.last, contains('Push now?'));
      host.chat = {...host.chat, 'question': 'Push to which branch?'};
      await tester.pump(const Duration(milliseconds: 200));
      final sends = host.sent('send').length;
      voice.say('yes push', isFinal: true, start: 30, end: 30.4);
      await tester.pump(turnGap);
      expect(host.sent('send'), hasLength(sends), reason: 'not its answer');
      expect(voice.spoken.last, contains('Push to which branch?'));
      voice.say('main', isFinal: true, start: 40, end: 40.4);
      await tester.pump(turnGap);
      expect(host.sent('send').last['question'], 'Push to which branch?');
    });

    callTest('words that cut a reply short are not the answer to the question '
        'after it', (tester) async {
      voice.say('run it', isFinal: true);
      await tester.pump(turnGap);
      voice.holdSpeech = true;
      host.reply('I ran it and the build is green.', status: 'Needs an answer');
      host.chat = {
        ...host.chat,
        'question': 'Which branch?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'I ran it and the build is green.');

      // Cut short before the question: their words are kept, the question
      // is asked after them.
      voice.holdSpeech = false;
      voice.say('also update the docs', isFinal: true, start: 10, end: 11);
      await tester.pump(turnGap);
      expect(host.sent('send'), hasLength(1), reason: 'not its answer');
      expect(voice.spoken.last, contains('Which branch?'));

      voice.say('main', isFinal: true, start: 20, end: 20.5);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'main');
      expect(host.sent('send').last['question'], 'Which branch?');
    });

    callTest('a question withdrawn while the reply waits: the turn sent '
        'meanwhile gets its answer', (tester) async {
      voice.say('run it', isFinal: true);
      await tester.pump(turnGap);
      voice.say('and the');
      await tester.pump(const Duration(milliseconds: 100));
      host.reply('Done, one thing.', status: 'Needs an answer');
      host.chat = {
        ...host.chat,
        'question': 'Which branch?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 300));
      // Withdrawn before anything was said.
      host.chat = {...host.chat, 'status': 'Ready', 'question': null};
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('and the docs', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.spoken, contains('Done, one thing.'));
      expect(voice.spoken, isNot(contains(contains('Which branch?'))));
      expect(host.sent('send').last['text'], 'and the docs');
      expect(call.phase, CallPhase.working);

      host.reply('The docs are updated.');
      await tester.pump(const Duration(milliseconds: 400));
      expect(voice.spoken.last, 'The docs are updated.');
    });

    callTest('the chat is still read while a reply waits for the person', (
      tester,
    ) async {
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      voice.heard.add(const VoiceActivity(true));
      await tester.pump();
      host.reply('Done.');
      await tester.pump(const Duration(milliseconds: 300));
      final reads = host.sent('chat').length;
      await tester.pump(const Duration(seconds: 1));
      expect(host.sent('chat').length, greaterThan(reads + 3));
      voice.heard.add(const VoiceActivity(false));
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Done.');
    });

    callTest('real words stop what is said while the agent works too', (
      tester,
    ) async {
      voice.say('run it', isFinal: true);
      await tester.pump(turnGap);
      voice.holdSpeech = true;
      // Said while the agent works: "Noted".
      voice.say('and also the docs', isFinal: true, start: 10, end: 11);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.working);
      final stops = voice.speechStopped;
      voice.say('no wait stop that', start: 14, end: 15);
      await tester.pump();
      expect(voice.speechStopped, greaterThan(stops));
      expect(call.phase, CallPhase.working, reason: 'still on the agent');
    });

    callTest('switching language restarts recognition between turns', (
      tester,
    ) async {
      voice.say('half a sen');
      await tester.pump(const Duration(milliseconds: 300));
      await call.switchLanguage('it-IT');
      await tester.pump();
      expect(voice.languagesSet, ['it-IT']);
      expect(call.heard, '', reason: 'words in the old language are dropped');
      expect(call.phase, CallPhase.listening);
      expect(call.language, 'it-IT');
      expect(voice.spoken.last, 'Dimmi pure.', reason: 'now in Italian');
      await tester.pump(const Duration(milliseconds: 1500));
      expect(host.sent('send'), isEmpty);

      voice.heard.add(const VoiceDownload('it-IT', 0.42));
      await tester.pump();
      expect(call.notice, 'Downloading the speech model… 42%');

      // Audio counts from zero in the new recognizer: early words still go.
      voice.say('Esegui i test', isFinal: true, start: 0.2, end: 1.0);
      await tester.pump(turnGap);
      expect(host.sent('send').single['text'], 'Esegui i test');

      // Not while a turn is on its way.
      expect(call.canSwitchLanguage, isFalse);
      await call.switchLanguage('en-US');
      expect(voice.languagesSet, ['it-IT']);
    });

    callTest('talking over the reply stops it; its echo does not', (
      tester,
    ) async {
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      voice.holdSpeech = true;
      host.reply('The answer is that the cache was stale.');
      await tester.pump(const Duration(milliseconds: 200));
      expect(call.phase, CallPhase.speaking);

      voice.say('the cache was');
      await tester.pump();
      expect(call.phase, CallPhase.speaking, reason: 'echo of the reply');
      voice.say('mh yeah');
      await tester.pump();
      expect(call.phase, CallPhase.speaking, reason: 'only following along');
      voice.say('please');
      await tester.pump();
      expect(call.phase, CallPhase.speaking, reason: 'one word is not enough');

      voice.say('no please stop');
      await tester.pump();
      expect(call.phase, CallPhase.listening);
      expect(voice.speechStopped, greaterThan(0));
      expect(call.heard, 'no please stop');
    });

    callTest('a misheard echo of the reply does not stop it', (tester) async {
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      voice.holdSpeech = true;
      host.reply('Ho controllato il progetto e i test passano tutti.');
      await tester.pump(const Duration(milliseconds: 200));
      expect(call.phase, CallPhase.speaking);

      // The speaker's sound through the microphone, transcribed a bit wrong.
      voice.say('controllata il progetti e test passa');
      await tester.pump();
      expect(call.phase, CallPhase.speaking, reason: 'echo, misheard');

      voice.say('no cambia il progetto');
      await tester.pump();
      expect(call.phase, CallPhase.listening, reason: 'the person, not echo');
      expect(voice.speechStopped, greaterThan(0));
    });

    callTest('"wait" alone stops speech and the call just listens', (
      tester,
    ) async {
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      voice.holdSpeech = true;
      host.reply('The answer is that the cache was stale.');
      await tester.pump(const Duration(milliseconds: 200));
      expect(call.phase, CallPhase.speaking);
      final said = voice.spoken.length;

      voice.say('Wait.', isFinal: true);
      await tester.pump();
      expect(call.phase, CallPhase.listening);
      await tester.pump(turnGap);
      expect(voice.spoken, hasLength(said), reason: 'nothing more is said');
      expect(host.sent('send'), hasLength(1), reason: 'nothing is sent');
    });

    callTest('a voice over speech holds it; it goes on without real words', (
      tester,
    ) async {
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      voice.holdSpeech = true;
      host.reply('The answer is that the cache was stale.');
      await tester.pump(const Duration(milliseconds: 200));

      voice.heard.add(const VoiceActivity(true));
      await tester.pump();
      expect(voice.speechPaused, 1);
      voice.say('mhm');
      await tester.pump(const Duration(milliseconds: 1300));
      expect(voice.speechResumed, 1);
      expect(call.phase, CallPhase.speaking);

      // Once held, one real word is enough.
      voice.heard.add(const VoiceActivity(true));
      await tester.pump();
      voice.say('actually');
      await tester.pump();
      expect(call.phase, CallPhase.listening);
      expect(voice.speechStopped, greaterThan(0));
    });

    callTest('a turn that sounds finished goes after a short wait', (
      tester,
    ) async {
      voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
      await tester.pump(const Duration(milliseconds: 100));
      voice.heard.add(const TurnEstimate(0.95, at: 1.05));
      await tester.pump(const Duration(milliseconds: 1100));
      expect(host.sent('send'), isEmpty, reason: 'room for a breath');
      await tester.pump(const Duration(milliseconds: 150));
      expect(host.sent('send').single['text'], 'Run the tests');
      expect(voice.turnsEnded, 1);
    });

    callTest('an unsure estimate waits as long as none', (tester) async {
      voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
      voice.heard.add(const TurnEstimate(0.65, at: 1.05));
      await tester.pump(const Duration(milliseconds: 1900));
      expect(host.sent('send'), isEmpty, reason: 'between 0.5 and 0.8');
      await tester.pump(const Duration(milliseconds: 200));
      expect(host.sent('send').single['text'], 'Run the tests');
    });

    callTest('words late for audio before the pause keep the estimate', (
      tester,
    ) async {
      voice.say('Run the', start: 0.2, end: 0.7);
      voice.heard.add(const TurnEstimate(0.9, at: 1.05));
      await tester.pump(const Duration(milliseconds: 200));
      voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
      // The short wait, not the 2 s one: the estimate stood.
      await tester.pump(const Duration(milliseconds: 1300));
      expect(host.sent('send').single['text'], 'Run the tests');
    });

    callTest('a turn that sounds unfinished is waited for', (tester) async {
      voice.say('Run the tests and', isFinal: true, start: 0.2, end: 1.2);
      voice.heard.add(const TurnEstimate(0.9, at: 1.25));
      await tester.pump(const Duration(milliseconds: 2500));
      expect(host.sent('send'), isEmpty, reason: 'it trails off on "and"');

      voice.say('then lint', isFinal: true, start: 2.5, end: 3.2);
      voice.heard.add(const TurnEstimate(0.2, at: 3.25));
      await tester.pump(const Duration(milliseconds: 2500));
      expect(host.sent('send'), isEmpty, reason: 'it sounded unfinished');
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.sent('send').single['text'], 'Run the tests and then lint');
    });

    callTest('an Italian thinking pause is waited for', (tester) async {
      voice.say('il problema è', isFinal: true, start: 0.2, end: 1.0);
      voice.heard.add(const TurnEstimate(0.9, at: 1.05));
      await tester.pump(const Duration(milliseconds: 2500));
      expect(host.sent('send'), isEmpty, reason: 'it ends on "è"');
      voice.say('che il login non parte', isFinal: true, start: 3.4, end: 4.6);
      await tester.pump(turnGap);
      expect(
        host.sent('send').single['text'],
        'il problema è che il login non parte',
      );
    });

    group('with Jev on the Mac', () {
      /// Jev, faked: cut off unless the words end as [finished] do.
      void jev(String finished) => host.turnCheck = (f) => {
        'complete': '${f['text']}'.endsWith(finished) ? 0.9 : 0.05,
      };

      callTest('a sentence left half-way is not sent at the pause', (
        tester,
      ) async {
        jev('folder');
        voice.say(
          'Check the tests in the native',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        voice.heard.add(const VoiceActivity(true));
        voice.heard.add(const VoiceActivity(false));
        // Smart Turn is fooled: it sounded finished.
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 3500));
        expect(
          host.sent('turnCheck').last['text'],
          'Check the tests in the native',
        );
        expect(host.sent('send'), isEmpty, reason: 'Jev: cut off');

        voice.heard.add(const VoiceActivity(true));
        voice.say('folder', isFinal: true, start: 4.4, end: 5.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 5.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(
          host.sent('send').single['text'],
          'Check the tests in the native folder',
        );
      });

      callTest('a cut-off turn is listened to; at the bound the call asks', (
        tester,
      ) async {
        jev('folder');
        voice.say('Check the tests in the native', start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        // Busy with something else for a while: still listening.
        await tester.pump(const Duration(seconds: 90));
        expect(host.sent('send'), isEmpty);
        expect(call.canSendNow, isTrue, reason: 'the turn is still open');
        final said = voice.spoken.length;
        await tester.pump(const Duration(seconds: 31));
        // Two minutes on: asked, not sent.
        expect(host.sent('send'), isEmpty);
        expect(voice.spoken, hasLength(said + 1));
        expect(
          voice.spoken.last,
          'You stopped at: "Check the tests in the native". '
          'Shall I send it, or do you want to go on?',
        );
        // What comes next goes on with it.
        voice.say('folder', isFinal: true, start: 130, end: 131);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(
          host.sent('send').single['text'],
          'Check the tests in the native folder',
        );
      });

      callTest('a finished sentence goes as fast as without Jev', (
        tester,
      ) async {
        jev('tests');
        voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1250));
        expect(host.sent('send').single['text'], 'Run the tests');
        expect(host.sent('turnCheck'), hasLength(1));
      });

      callTest('a slow Jev changes nothing', (tester) async {
        host.turnCheck = (_) async {
          await Future<void>.delayed(const Duration(seconds: 5));
          return {'complete': 0.0};
        };
        voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1250));
        expect(host.sent('send').single['text'], 'Run the tests');
        await tester.pump(const Duration(seconds: 5));
      });

      callTest('a Mac without a key is asked once, and nothing changes', (
        tester,
      ) async {
        host.turnCheck = (_) => {'error': 'no key', 'unavailable': true};
        voice.say('Check the tests', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.6, at: 1.05));
        await tester.pump(const Duration(milliseconds: 500));
        voice.heard.add(const VoiceActivity(true));
        voice.say('in the native folder', isFinal: true, start: 1.6, end: 2.6);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 2.65));
        await tester.pump(const Duration(milliseconds: 1150));
        expect(host.sent('send'), isEmpty);
        await tester.pump(const Duration(milliseconds: 100));
        expect(
          host.sent('send').single['text'],
          'Check the tests in the native folder',
        );
        expect(host.sent('turnCheck'), hasLength(1));
      });
    });

    group('with Jev on the Mac, bounds', () {
      callTest('a turn sent on purpose is never held, and goes at once', (
        tester,
      ) async {
        host.turnCheck = (_) => {'complete': 0.05};
        voice.say('Run the tests, vai', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 450));
        expect(host.sent('send').single['text'], 'Run the tests');
        // Judged only for the fit, without the word that sends it.
        expect(host.sent('turnCheck').single['text'], 'Run the tests');
      });

      callTest('repeated estimates do not extend Jev\'s hold', (tester) async {
        host.turnCheck = (_) => {'complete': 0.05};
        voice.say('Check the tests in the native', isFinal: true);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(seconds: 60));
        // The pause judged again, the same words: no new two minutes.
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        final said = voice.spoken.length;
        await tester.pump(const Duration(milliseconds: 59800));
        expect(voice.spoken, hasLength(said), reason: 'Jev holds it');
        await tester.pump(const Duration(milliseconds: 500));
        expect(voice.spoken.last, startsWith('You stopped at:'));
        expect(host.sent('send'), isEmpty);
        expect(host.sent('turnCheck'), hasLength(1));
      });

      callTest('a voice hold still holds past Jev\'s; it gives no new one', (
        tester,
      ) async {
        host.turnCheck = (_) => {'complete': 0.05};
        voice.say('Check the tests in the native', isFinal: true);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(seconds: 118));
        // Their voice, no words: the patient 3 s from here, as without Jev.
        voice.heard.add(const VoiceActivity(true));
        voice.heard.add(const VoiceActivity(false));
        final said = voice.spoken.length;
        await tester.pump(const Duration(milliseconds: 2900));
        expect(voice.spoken, hasLength(said));
        await tester.pump(const Duration(milliseconds: 200));
        expect(voice.spoken.last, startsWith('You stopped at:'));
        expect(host.sent('turnCheck'), hasLength(1));
      });

      callTest('a late answer about a sent turn does not hold the next', (
        tester,
      ) async {
        var asked = 0;
        host.turnCheck = (_) async {
          // The first, about the sent turn, comes back while the second,
          // about the same words, is still on its way.
          final first = asked++ == 0;
          await Future<void>.delayed(
            Duration(milliseconds: first ? 1000 : 1500),
          );
          return {'complete': first ? 0.05 : 0.9};
        };
        voice.say('Check the tests', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 300));
        call.sendNow();
        expect(voice.turnsEnded, 1);
        await tester.pump(const Duration(milliseconds: 100));
        voice.heard.add(const VoiceActivity(true));
        voice.say('Check the tests', isFinal: true, start: 1.5, end: 2.2);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 2.25));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(voice.turnsEnded, 2, reason: 'the short wait, as without Jev');
        await tester.pump(const Duration(seconds: 2));
      });

      callTest('words revised for the same audio are judged afresh', (
        tester,
      ) async {
        host.turnCheck = (f) => {
          'complete': '${f['text']}'.endsWith('folder') ? 0.9 : 0.05,
        };
        voice.say('Check the tests in the native', start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 400));
        // The recognizer's final corrects the partial, same end: no new
        // speech, but other words.
        voice.say(
          'Check the tests in the native folder',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        await tester.pump(const Duration(milliseconds: 300));
        expect(
          host.sent('turnCheck').last['text'],
          'Check the tests in the native folder',
        );
        // The short wait from the pause, as without Jev.
        await tester.pump(const Duration(milliseconds: 600));
        expect(
          host.sent('send').single['text'],
          'Check the tests in the native folder',
        );
      });

      callTest('a revision lets go of a hold its old words had', (
        tester,
      ) async {
        host.turnCheck = (f) => {
          'complete': '${f['text']}'.endsWith('folder') ? 0.9 : 0.05,
        };
        voice.say('Check the tests in the native', start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        // The short silence has run out; Jev holds the turn.
        await tester.pump(const Duration(milliseconds: 2000));
        expect(host.sent('send'), isEmpty);
        voice.say(
          'Check the tests in the native folder',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        // Judged, then sent on Jev's answer: not at the end of the old hold.
        await tester.pump(const Duration(milliseconds: 300));
        expect(
          host.sent('turnCheck').last['text'],
          'Check the tests in the native folder',
        );
        expect(
          host.sent('send').single['text'],
          'Check the tests in the native folder',
        );
      });

      callTest('a held partial revised into side talk is asked about', (
        tester,
      ) async {
        host.turnCheck = (f) => '${f['text']}'.contains('pasta')
            ? {'complete': 0.9, 'fits': 0.05}
            : {'complete': 0.05, 'fits': 0.97};
        voice.say('Check the', start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        // Past the patient wait ("the" is open): Jev holds the turn.
        await tester.pump(const Duration(milliseconds: 3500));
        expect(host.sent('send'), isEmpty);
        voice.say('Check the pasta', isFinal: true, start: 0.2, end: 1.0);
        await tester.pump(const Duration(milliseconds: 300));
        expect(host.sent('send'), isEmpty);
        expect(voice.spoken.last, startsWith('Sorry, was that for me?'));
      });

      callTest('a recheck Jev does not answer ends on its timer', (
        tester,
      ) async {
        var first = true;
        host.turnCheck = (f) async {
          if (first) {
            first = false;
            return {'complete': 0.05};
          }
          await Future<void>.delayed(const Duration(seconds: 5));
          return {'complete': 0.9};
        };
        voice.say('Check the tests in the native', start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 2000));
        voice.say(
          'Check the tests in the native folder',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        await tester.pump(const Duration(milliseconds: 1500));
        expect(host.sent('send'), isEmpty);
        await tester.pump(const Duration(milliseconds: 300));
        expect(
          host.sent('send').single['text'],
          'Check the tests in the native folder',
        );
        await tester.pump(const Duration(seconds: 5));
      });

      callTest('the countdown shows Jev\'s hold, not a shorter silence', (
        tester,
      ) async {
        host.turnCheck = (_) => {'complete': 0.05};
        voice.say('Check the tests in the native', isFinal: true);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 300));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump();
        expect(call.turnWait, greaterThan(const Duration(seconds: 5)));
        await tester.pump(const Duration(seconds: 7));
      });

      callTest('switching language forgets Jev\'s verdict and hold', (
        tester,
      ) async {
        var answer = 0.05;
        host.turnCheck = (_) => {'complete': answer};
        voice.say('Check the tests', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 300));
        expect(host.sent('turnCheck'), hasLength(1));
        await call.switchLanguage('it-IT');
        await tester.pump();
        answer = 0.9;
        // The very same words in the new recognizer: asked again, and not
        // held by the old verdict.
        voice.say('Check the tests', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(host.sent('turnCheck'), hasLength(2));
        expect(host.sent('send').single['text'], 'Check the tests');
        await tester.pump(const Duration(seconds: 5));
      });

      callTest('no turn checks while the call is asked to wait', (
        tester,
      ) async {
        host.turnCheck = (_) => {'complete': 0.05};
        voice.say('Run the tests', isFinal: true);
        await tester.pump(turnGap);
        voice.say('Give me a minute', isFinal: true);
        await tester.pump(turnGap);
        expect(call.paused, isTrue);
        final checks = host.sent('turnCheck').length;
        // Side talk, with pauses, after the grace: ignored, and not judged.
        await tester.pump(const Duration(seconds: 4));
        voice.say('Sì ho quasi finito con questa cosa qui', isFinal: true);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(call.paused, isTrue);
        // The way back still works, and is not judged either.
        voice.say('Ok eccomi', isFinal: true);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(call.paused, isFalse);
        expect(host.sent('turnCheck'), hasLength(checks));
      });
    });

    group('Jev on whether words fit the call', () {
      /// Jev, faked: finished; it fits unless it speaks of pasta.
      void jev() => host.turnCheck = (f) => {
        'complete': 0.9,
        'fits': '${f['text']}'.contains('pasta') ? 0.05 : 0.97,
      };

      Future<void> offCall(WidgetTester tester) async {
        jev();
        voice.say(
          'honey take the pasta off the stove',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(host.sent('send'), isEmpty, reason: 'asked first');
        expect(
          voice.spoken.last,
          'Sorry, was that for me? I heard: '
          '"honey take the pasta off the stove". '
          'Shall I send it, or what did you mean?',
        );
        voice.finishSpeaking();
        await tester.pump();
      }

      callTest('words not for the call are asked about; a yes sends them', (
        tester,
      ) async {
        await offCall(tester);
        voice.say('yes', isFinal: true, start: 3, end: 3.4);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(
          host.sent('send').single['text'],
          'honey take the pasta off the stove',
        );
      });

      callTest('"no" lets them go', (tester) async {
        await offCall(tester);
        voice.say('no', isFinal: true, start: 3, end: 3.4);
        await tester.pump(turnGap);
        expect(host.sent('send'), isEmpty);
        expect(voice.spoken.last, "Okay, I'll leave it.");
      });

      callTest('what the person meant replaces them', (tester) async {
        await offCall(tester);
        voice.say(
          'sorry, I meant run the tests',
          isFinal: true,
          start: 3,
          end: 4,
        );
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 4.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(
          host.sent('send').single['text'],
          'sorry, I meant run the tests',
        );
      });

      callTest('a question that could not be said is not answered by a yes', (
        tester,
      ) async {
        jev();
        voice.failSpeak = true;
        voice.say(
          'honey take the pasta off the stove',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(voice.spoken.last, startsWith('Sorry, was that for me?'));
        voice.failSpeak = false;
        voice.say('yes', isFinal: true, start: 3, end: 3.4);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(
          host.sent('send').map((f) => f['text']),
          isNot(contains('honey take the pasta off the stove')),
        );
      });

      callTest('a question cut before a word was heard is not answered', (
        tester,
      ) async {
        jev();
        voice
          ..holdSpeech = true
          ..heardWhenStopped = 0;
        voice.say(
          'honey take the pasta off the stove',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(voice.spoken.last, startsWith('Sorry, was that for me?'));
        // Stopped before any of it was heard.
        await voice.stopSpeaking();
        await tester.pump();
        voice.holdSpeech = false;
        voice.say('yes', isFinal: true, start: 3, end: 3.4);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(
          host.sent('send').map((f) => f['text']),
          isNot(contains('honey take the pasta off the stove')),
        );
      });

      callTest('an early yes waits for the question to be delivered', (
        tester,
      ) async {
        jev();
        voice
          ..holdSpeech = true
          ..heardWhenStopped = 0
          ..stopDelay = const Duration(milliseconds: 200);
        voice.say(
          'honey take the pasta off the stove',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(voice.spoken.last, startsWith('Sorry, was that for me?'));
        // Over the question, before a word of it is heard: "go ahead", and
        // a tap on Send. Stopping the speech takes 200 ms to report.
        voice.say('go ahead', isFinal: true, start: 1.4, end: 1.9);
        await tester.pump();
        call.sendNow();
        await tester.pump(const Duration(milliseconds: 100));
        expect(host.sent('send'), isEmpty, reason: 'waits for the playback');
        await tester.pump(const Duration(milliseconds: 300));
        voice.holdSpeech = false;
        await tester.pump(turnGap);
        expect(
          host.sent('send').map((f) => f['text']),
          isNot(contains('honey take the pasta off the stove')),
        );
      });

      callTest('a yes once the question was heard through sends', (
        tester,
      ) async {
        jev();
        voice
          ..holdSpeech = true
          ..stopDelay = const Duration(milliseconds: 200);
        voice.say(
          'honey take the pasta off the stove',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        final question = voice.spoken.last;
        // Cut just after the quoted words were heard: delivered.
        voice.heardWhenStopped = question.lastIndexOf('"') + 1;
        voice.say('go ahead', isFinal: true, start: 1.4, end: 1.9);
        await tester.pump();
        call.sendNow();
        await tester.pump(const Duration(milliseconds: 400));
        voice.holdSpeech = false;
        await tester.pump(turnGap);
        expect(
          host.sent('send').map((f) => f['text']),
          contains('honey take the pasta off the stove'),
        );
      });

      callTest('quotes in the words do not end the quoted words early', (
        tester,
      ) async {
        jev();
        voice
          ..holdSpeech = true
          ..stopDelay = const Duration(milliseconds: 200);
        voice.say(
          'honey "take the pasta off the stove"',
          isFinal: true,
          start: 0.2,
          end: 1.0,
        );
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        final question = voice.spoken.last;
        expect(
          question,
          startsWith('Sorry, was that for me? I heard: "honey "take'),
        );
        // Cut just after the quote inside the words (the second quote of
        // the question): the words were not heard through.
        voice.heardWhenStopped = question.indexOf('"take') + 1;
        voice.say('go ahead', isFinal: true, start: 1.4, end: 1.9);
        await tester.pump();
        call.sendNow();
        await tester.pump(const Duration(milliseconds: 400));
        voice.holdSpeech = false;
        await tester.pump(turnGap);
        expect(
          host.sent('send').map((f) => f['text']),
          isNot(contains('honey "take the pasta off the stove"')),
        );
      });

      callTest('side talk in reply leaves the question open; yes sends', (
        tester,
      ) async {
        await offCall(tester);
        voice.say('the pasta is on the table', isFinal: true, start: 3, end: 4);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 4.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(host.sent('send'), isEmpty, reason: 'not an answer');
        voice.say('yes', isFinal: true, start: 6, end: 6.4);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(
          host.sent('send').single['text'],
          'honey take the pasta off the stove',
        );
      });

      callTest('left half-way and not for the call: a reply replaces it', (
        tester,
      ) async {
        host.turnCheck = (f) => '${f['text']}'.contains('pasta')
            ? {'complete': 0.05, 'fits': 0.05}
            : {'complete': 0.9, 'fits': 0.97};
        voice.say('honey the pasta is', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(seconds: 121));
        expect(host.sent('send'), isEmpty);
        expect(voice.spoken.last, startsWith('Sorry, was that for me?'));
        voice.finishSpeaking();
        await tester.pump();
        voice.say('run the tests', isFinal: true, start: 130, end: 131);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(host.sent('send').single['text'], 'run the tests');
      });

      callTest('anything the call says meanwhile ends its own question', (
        tester,
      ) async {
        jev();
        voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
        await tester.pump(turnGap);
        expect(host.sent('send'), hasLength(1));
        voice.say(
          'honey take the pasta off the stove',
          isFinal: true,
          start: 3,
          end: 4,
        );
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 4.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(voice.spoken.last, startsWith('Sorry, was that for me?'));
        voice.finishSpeaking();
        await tester.pump();
        // The agent answers before the person does: the call says that.
        host.reply('All tests pass.');
        await tester.pump(const Duration(milliseconds: 300));
        expect(voice.spoken.last, 'All tests pass.');
        voice.finishSpeaking();
        await tester.pump();
        voice.say('yes', isFinal: true, start: 6, end: 6.4);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        // The yes is not for the old question: the room talk never goes.
        expect(
          host.sent('send').map((f) => f['text']),
          isNot(contains('honey take the pasta off the stove')),
        );
      });

      callTest('switching language ends the call\'s question', (tester) async {
        await offCall(tester);
        await call.switchLanguage('it-IT');
        await tester.pump();
        voice.finishSpeaking();
        await tester.pump();
        voice.say('sì', isFinal: true, start: 0.2, end: 0.6);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        expect(
          host.sent('send').map((s) => s['text']),
          isNot(contains('honey take the pasta off the stove')),
        );
      });

      callTest('a command Jev takes for cut off is not held, nor shown so', (
        tester,
      ) async {
        host.turnCheck = (_) => {'complete': 0.05, 'fits': 0.1};
        voice.say('aspetta', isFinal: true, start: 0.2, end: 0.8);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 300));
        expect(call.turnWait, lessThan(const Duration(seconds: 5)));
        await tester.pump(turnGap);
        expect(voice.turnsEnded, 1);
        expect(host.sent('send'), isEmpty);
      });

      callTest('the call\'s context goes with the words', (tester) async {
        jev();
        voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(turnGap);
        final context = '${host.sent('turnCheck').single['context']}';
        expect(context, startsWith('Project: p'));
        expect(context, contains('Conversation so far:'));
        expect(context, contains('Last said to the person: '));
        expect(host.sent('send').single['text'], 'Run the tests');
      });

      callTest('a tap sends at once, unless Jev knows it is not for the call', (
        tester,
      ) async {
        jev();
        voice.say('the tests are ready', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 500));
        call.sendNow();
        expect(host.sent('send').single['text'], 'the tests are ready');

        voice.say('the pasta is ready', isFinal: true, start: 2, end: 3);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 500));
        call.sendNow();
        await tester.pump();
        expect(host.sent('send'), hasLength(1), reason: 'asked first');
        expect(voice.spoken.last, startsWith('Sorry, was that for me?'));
      });

      callTest('a word that sends is judged for the fit, without waiting', (
        tester,
      ) async {
        jev();
        voice.say('the pasta is ready, vai', isFinal: true, start: 0.2, end: 1);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 450));
        // Judged without "vai", and asked about, not sent.
        expect(host.sent('turnCheck').single['text'], 'the pasta is ready');
        expect(host.sent('send'), isEmpty);
        expect(voice.spoken.last, contains('"the pasta is ready"'));
      });

      callTest('a word that sends goes on time when Jev is late', (
        tester,
      ) async {
        host.turnCheck = (_) async {
          await Future<void>.delayed(const Duration(seconds: 1));
          return {'complete': 0.9, 'fits': 0.05};
        };
        voice.say('the pasta is ready, vai', isFinal: true, start: 0.2, end: 1);
        voice.heard.add(const VoiceActivity(false));
        await tester.pump(const Duration(milliseconds: 450));
        expect(host.sent('send').single['text'], 'the pasta is ready');
        await tester.pump(const Duration(seconds: 1));
      });

      callTest('a command is not held for Jev nor asked about', (tester) async {
        // As measured: Jev took "aspetta un attimo" for cut off.
        host.turnCheck = (_) => {'complete': 0.19, 'fits': 0.1};
        voice.say('Hang up.', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(call.ended, isTrue);
      });

      callTest('a bad answer about the fit changes nothing', (tester) async {
        host.turnCheck = (_) => {'complete': 0.9, 'fits': -1};
        voice.say('the pasta tests', isFinal: true, start: 0.2, end: 1.0);
        voice.heard.add(const VoiceActivity(false));
        voice.heard.add(const TurnEstimate(0.95, at: 1.05));
        await tester.pump(const Duration(milliseconds: 1300));
        expect(host.sent('send').single['text'], 'the pasta tests');
      });
    });

    callTest('a final for words before muting is kept; new words join it', (
      tester,
    ) async {
      host.turnCheck = (_) => {'complete': 0.05};
      voice.say('Check the tests in the', start: 0.2, end: 1.0);
      await tester.pump(const Duration(milliseconds: 300));
      await call.toggleMute();
      // The recognizer finishes the segment said before muting.
      voice.say(
        'Check the tests in the native',
        isFinal: true,
        start: 0.2,
        end: 1.3,
      );
      voice.heard.add(const VoiceActivity(false));
      await tester.pump(const Duration(seconds: 10));
      expect(host.sent('send'), isEmpty);
      expect(host.sent('turnCheck'), isEmpty, reason: 'never while muted');
      await call.toggleMute();
      voice.say('folder', start: 1.5, end: 2.0);
      await tester.pump(const Duration(milliseconds: 200));
      voice.say('folder', isFinal: true, start: 1.5, end: 2.0);
      host.turnCheck = (_) => {'complete': 0.9};
      voice.heard.add(const VoiceActivity(false));
      await tester.pump(const Duration(seconds: 4));
      expect(
        host.sent('send').single['text'],
        'Check the tests in the native folder',
      );
    });

    callTest('words that come in only once muted wait for the unmute', (
      tester,
    ) async {
      await call.toggleMute();
      voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
      await tester.pump(const Duration(seconds: 10));
      expect(host.sent('send'), isEmpty);
      await call.toggleMute();
      await tester.pump(const Duration(milliseconds: 2900));
      expect(host.sent('send'), isEmpty, reason: 'the patient wait');
      await tester.pump(const Duration(milliseconds: 200));
      expect(host.sent('send').single['text'], 'Run the tests');
    });

    callTest('a muted pause keeps the turn; it goes on once unmuted', (
      tester,
    ) async {
      voice.say('Check the tests in the native', isFinal: true);
      await tester.pump(const Duration(milliseconds: 500));
      await call.toggleMute();
      await tester.pump(const Duration(seconds: 10));
      expect(host.sent('send'), isEmpty, reason: 'muted, not finished');
      await call.toggleMute();
      voice.say('folder', isFinal: true);
      await tester.pump(turnGap);
      expect(
        host.sent('send').single['text'],
        'Check the tests in the native folder',
      );
    });

    callTest('the person speaking again holds the turn before any words', (
      tester,
    ) async {
      voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
      voice.heard.add(const TurnEstimate(0.95, at: 1.05));
      await tester.pump(const Duration(milliseconds: 1000));
      // Their voice is back; the recognizer has no words for it yet.
      voice.heard.add(const VoiceActivity(true));
      await tester.pump(const Duration(milliseconds: 1500));
      expect(host.sent('send'), isEmpty, reason: 'held open by their voice');
      voice.say('and then lint', isFinal: true, start: 2.3, end: 3.0);
      await tester.pump(turnGap);
      expect(host.sent('send').single['text'], 'Run the tests and then lint');
    });

    callTest('a voice with no words holds the turn only a few times', (
      tester,
    ) async {
      voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
      await tester.pump(const Duration(milliseconds: 1000));
      for (var i = 0; i < 3; i++) {
        voice.heard.add(const VoiceActivity(true));
        await tester.pump(const Duration(milliseconds: 2500));
        expect(host.sent('send'), isEmpty, reason: 'hold ${i + 1}');
      }
      // Noise in the room: the fourth no longer holds it.
      voice.heard.add(const VoiceActivity(true));
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.sent('send').single['text'], 'Run the tests');
    });

    callTest('a word that sends the turn sends it at once, without it', (
      tester,
    ) async {
      voice.say('Controlla i test, vai.', isFinal: true, start: 0.2, end: 1.5);
      await tester.pump(const Duration(milliseconds: 500));
      expect(host.sent('send').single['text'], 'Controlla i test');

      host.reply('Fatto.');
      await tester.pump(const Duration(milliseconds: 300));
      // Part of the sentence, not a send: the usual wait.
      voice.say('make sure it is done', isFinal: true, start: 3, end: 4);
      await tester.pump(const Duration(milliseconds: 500));
      expect(host.sent('send'), hasLength(1));
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'make sure it is done');
    });

    callTest('a send word said apart, as a result of its own, sends', (
      tester,
    ) async {
      voice.say('aggiorna il readme', isFinal: true, start: 0.2, end: 1.4);
      await tester.pump(const Duration(milliseconds: 1000));
      voice.say('fatto', isFinal: true, start: 2.4, end: 2.8);
      await tester.pump(const Duration(milliseconds: 500));
      expect(host.sent('send').single['text'], 'aggiorna il readme');

      host.reply('Ok.');
      await tester.pump(const Duration(milliseconds: 300));
      // In the same result, with no pause: part of the sentence.
      voice.say('press send', isFinal: true, start: 4, end: 5);
      await tester.pump(const Duration(milliseconds: 500));
      expect(host.sent('send'), hasLength(1));
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'press send');
    });

    callTest('pauses in noise do not hold the turn past the voice holds', (
      tester,
    ) async {
      voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
      await tester.pump(const Duration(milliseconds: 1000));
      // Noise: the detector's start, then a stop judged as a pause.
      for (var i = 0; i < 3; i++) {
        voice.heard.add(const VoiceActivity(true));
        voice.heard.add(TurnEstimate(0.3, at: 2.0 + 3 * i));
        await tester.pump(const Duration(milliseconds: 2500));
        expect(host.sent('send'), isEmpty, reason: 'hold ${i + 1}');
      }
      voice.heard.add(const VoiceActivity(true));
      voice.heard.add(const TurnEstimate(0.3, at: 11));
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.sent('send').single['text'], 'Run the tests');
    });

    callTest('a tap sends the turn at once', (tester) async {
      expect(call.canSendNow, isFalse);
      voice.say('Run the tests', isFinal: true, start: 0.2, end: 1.0);
      await tester.pump(const Duration(milliseconds: 100));
      expect(call.canSendNow, isTrue);
      expect(call.turnWait, const Duration(seconds: 2));
      call.sendNow();
      await tester.pump();
      expect(host.sent('send').single['text'], 'Run the tests');
      expect(call.canSendNow, isFalse);
      expect(call.turnWait, isNull);
    });

    callTest('the rest of a sentence cut short still reaches the agent', (
      tester,
    ) async {
      voice.say('vorrei che il login', start: 0, end: 1.0);
      voice.heard.add(const TurnEstimate(0.95, at: 1.05));
      await tester.pump(const Duration(milliseconds: 1300));
      expect(host.sent('send').single['text'], 'vorrei che il login');
      expect(call.phase, CallPhase.working);

      // The recognizer reports the segment from its start, with the rest.
      voice.say(
        'vorrei che il login funzionasse con Google',
        start: 0,
        end: 2.6,
      );
      await tester.pump();
      expect(call.heard, 'funzionasse con Google');
      await tester.pump(turnGap);
      expect(voice.spoken.last, "Got it, I'll pass it on.");
      expect(host.sent('send'), hasLength(1), reason: 'the agent is busy');

      // Once the agent's turn is over it gets the rest as a follow-up.
      host.reply('Which login page?');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken, contains('Which login page?'));
      await tester.pump(const Duration(milliseconds: 300));
      expect(host.sent('send').last['text'], 'funzionasse con Google');

      // A final of the same segment with nothing more is dropped.
      voice.say(
        'Vorrei che il login funzionasse con Google.',
        isFinal: true,
        start: 0,
        end: 2.65,
      );
      await tester.pump(turnGap);
      expect(host.sent('send'), hasLength(2));
    });

    callTest('only the words of the segment reported again are taken off', (
      tester,
    ) async {
      voice.say('Apri il progetto', isFinal: true, start: 0, end: 1.2);
      voice.say('controlla il login', start: 1.5, end: 2.5);
      voice.heard.add(const TurnEstimate(0.95, at: 2.55));
      await tester.pump(const Duration(milliseconds: 1300));
      expect(
        host.sent('send').single['text'],
        'Apri il progetto controlla il login',
      );

      // Only the last segment comes again, with the rest.
      voice.say('controlla il login con Google', start: 1.5, end: 3.6);
      await tester.pump();
      expect(call.heard, 'con Google');
      // And again, longer: still only that segment's sent words go.
      voice.say(
        'controlla il login con Google e GitHub',
        isFinal: true,
        start: 1.5,
        end: 4.4,
      );
      await tester.pump();
      expect(call.heard, 'con Google e GitHub');
      await tester.pump(turnGap);
      host.reply('Ok.');
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.sent('send').last['text'], 'con Google e GitHub');
    });

    callTest('"aspetta un attimo" pauses until the person is back', (
      tester,
    ) async {
      voice.say('Run the tests', isFinal: true);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.working);
      voice.say(
        'Aspetta un attimo che devo fare una cosa e poi continuiamo',
        isFinal: true,
      );
      await tester.pump(turnGap);
      expect(call.paused, isTrue);
      expect(voice.spoken.last, startsWith('Sure, take your time'));
      expect(call.onHold, isFalse, reason: 'no hold tone while paused');

      // The reply comes meanwhile: kept, not said.
      host.reply('All tests pass.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, startsWith('Sure, take your time'));

      // Talk in the room, later on, is not for the call.
      await tester.pump(const Duration(seconds: 4));
      voice.say('Sì ho quasi finito con questa cosa qui', isFinal: true);
      await tester.pump(turnGap);
      expect(call.paused, isTrue);
      expect(host.sent('send'), hasLength(1));

      voice.say('Ok eccomi', isFinal: true);
      await tester.pump(turnGap);
      expect(call.paused, isFalse);
      expect(voice.spoken.last, 'All tests pass.');
    });

    callTest('back with nothing new, the call says where things stood', (
      tester,
    ) async {
      voice.say('Run the tests', isFinal: true);
      await tester.pump(turnGap);
      host.reply('Which suite?');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('Give me a minute', isFinal: true);
      await tester.pump(turnGap);
      expect(call.paused, isTrue);
      await call.resume();
      expect(voice.spoken.last, "Okay, I'm here. Where we were: Which suite?");
    });

    callTest('"stop" while the agent works stops it', (tester) async {
      voice.say('refactor everything', isFinal: true);
      await tester.pump(turnGap);
      await tester.pump(const Duration(milliseconds: 200));
      expect(call.phase, CallPhase.working);

      voice.say('Stop.', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('stop').single['id'], 's1');
      expect(host.sent('send'), hasLength(1));
      expect(voice.spoken.last, 'Okay, stopping it.');
    });

    callTest('what is said while the agent works follows once it is free', (
      tester,
    ) async {
      voice.say('first', isFinal: true);
      await tester.pump(turnGap);
      voice.say('second thing', isFinal: true);
      await tester.pump(turnGap);
      voice.say('and a third', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('send'), hasLength(1), reason: 'not while it works');
      expect(voice.spoken.last, "Got it, I'll pass it on.");

      host.reply('First done.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken, contains('First done.'));
      await tester.pump(const Duration(milliseconds: 300));
      expect(host.sent('send').last['text'], 'second thing and a third');
      expect(call.phase, CallPhase.working);
      host.reply('Both done too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Both done too.');
      expect(host.sent('send'), hasLength(2));
    });

    callTest('a choice on screen is not answered by voice', (tester) async {
      host.chat = {
        ...host.chat,
        'status': 'Needs an answer',
        'question': 'Allow edit?',
        'textAnswer': false,
      };
      await tester.pump(const Duration(milliseconds: 200));
      voice.say('yes', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('send'), isEmpty);
      expect(voice.spoken.last, contains('choice on screen'));
    });

    callTest('a text question is answered by voice', (tester) async {
      host.chat = {
        ...host.chat,
        'status': 'Needs an answer',
        'question': 'Which branch?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 200));
      voice.say('main', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('send').single['question'], 'Which branch?');
    });

    // Muted, the device hears nothing (it drops the microphone's audio):
    // what the recognizer still reports is for words said before muting.
    callTest('muted, nothing is sent; "hang up" ends the call', (tester) async {
      await call.toggleMute();
      expect(voice.muted, isTrue);
      await tester.pump(turnGap);
      expect(host.sent('send'), isEmpty);
      await call.toggleMute();

      voice.say('Hang up.', isFinal: true);
      await tester.pump(turnGap);
      expect(call.ended, isTrue);
      expect(voice.stopped, isTrue);
      expect(host.sent('send'), isEmpty);
    });

    callTest('a final for audio already sent is not sent again', (
      tester,
    ) async {
      voice.say('run the tests', start: 0, end: 2.0);
      await tester.pump(turnGap);
      expect(host.sent('send'), hasLength(1));
      host.reply('Done.');
      await tester.pump(const Duration(milliseconds: 200));
      expect(call.phase, CallPhase.listening);

      // The final revises the sent partial and ends a little later.
      voice.say('run the tests', isFinal: true, start: 0, end: 2.2);
      await tester.pump(turnGap);
      expect(host.sent('send'), hasLength(1));

      voice.say('and lint', isFinal: true, start: 3.0, end: 4.0);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'and lint');
    });

    callTest('a repeated prompt waits for its own answer', (tester) async {
      host.chat = {
        ...host.chat,
        'messages': [
          [true, 'run the tests'],
          [false, 'Old result.'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 200));
      host.echoes = false;
      voice.say('run the tests', isFinal: true);
      await tester.pump(turnGap);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.phase, CallPhase.working);
      expect(voice.spoken, isNot(contains('Old result.')));

      host.chat = {
        ...host.chat,
        'messages': [
          ...host.chat['messages'] as List,
          [true, 'run the tests'],
          [false, 'New result.'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 200));
      expect(voice.spoken.last, 'New result.');
    });

    callTest('the echo of finished speech is not taken as a turn', (
      tester,
    ) async {
      voice.say('would you like done', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('send'), isEmpty);
    });

    callTest('"stop" right after "say stop" is not taken for echo', (
      tester,
    ) async {
      voice.say('refactor everything', isFinal: true);
      await tester.pump(turnGap);
      host.reply('Started. Say stop to interrupt it.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, contains('Say stop'));
      // Another turn, started on screen.
      host.chat = {...host.chat, 'status': 'Running'};
      voice.say('stop', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('stop'), hasLength(1));
    });

    callTest('a final that only settles heard words does not extend the turn', (
      tester,
    ) async {
      voice.say('please run the whole test suite', start: 0, end: 3.0);
      await tester.pump(const Duration(milliseconds: 600));
      // The recognizer finalizing the long utterance: same words, same audio.
      voice.say(
        'Please run the whole test suite.',
        isFinal: true,
        start: 0,
        end: 3.0,
      );
      await tester.pump(const Duration(milliseconds: 1500));
      expect(
        host.sent('send').single['text'],
        'Please run the whole test suite.',
      );
    });

    callTest('new words keep the turn open', (tester) async {
      voice.say('run the', start: 0, end: 1.0);
      await tester.pump(const Duration(milliseconds: 1500));
      voice.say('run the tests', start: 0, end: 2.0);
      await tester.pump(const Duration(milliseconds: 1500));
      expect(host.sent('send'), isEmpty, reason: 'still talking');
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.sent('send').single['text'], 'run the tests');
    });

    callTest('a cue is heard when the turn is sent, not for commands', (
      tester,
    ) async {
      voice.say('Hang', isFinal: true);
      expect(voice.cues, 0);
      voice.say('refactor everything', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.cues, 1);
      voice.say('stop', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.cues, 1);
    });

    testWidgets('the reply is read soon after the agent is done', (
      tester,
    ) async {
      host = FakeHost();
      voice = FakeVoice();
      call = CallController(call: host.call, voice: voice, target: target)
        ..started = true;
      await call.start();
      await tester.pump();
      voice.say('hello', isFinal: true);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.working);
      host.reply('Hi.');
      await tester.pump(const Duration(milliseconds: 250));
      expect(voice.spoken.last, 'Hi.', reason: 'polled fast while working');
      call.dispose();
      await tester.pump();
    });

    callTest('a long reply is condensed while the agent still runs', (
      tester,
    ) async {
      voice = FakeVoice(condensed: 'Green, ship it.');
      await call.hangUp();
      call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
        pollWorking: const Duration(milliseconds: 100),
      )..started = true;
      await call.start();
      voice.say('run the tests', isFinal: true);
      await tester.pump(turnGap);
      final long = List.filled(40, 'The build is green.').join(' ');
      host.reply(long, status: 'Running');
      await tester.pump(const Duration(milliseconds: 200));
      expect(voice.condenseAsked, hasLength(1));
      expect(call.phase, CallPhase.working);

      host.chat = {...host.chat, 'status': 'Ready'};
      await tester.pump(const Duration(milliseconds: 200));
      expect(voice.spoken.last, 'Green, ship it.');
      expect(voice.condenseAsked, hasLength(1), reason: 'reused');
    });

    callTest('a report that lands with an update in one poll ends the task', (
      tester,
    ) async {
      voice.say('run the tests', isFinal: true);
      await tester.pump(turnGap);
      host.reply('On it.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.phase, CallPhase.listening);
      host.chat = {
        ...host.chat,
        'messages': [
          ...host.chat['messages'] as List,
          [false, 'The review is under way.'],
          [false, 'Task complete. All tests pass.'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, contains('Task complete.'));
      expect(call.completed, isTrue);
      expect(call.ended, isTrue);
    });

    testWidgets('a failed start ends the call with the reason', (tester) async {
      host = FakeHost();
      voice = _FailingVoice();
      call = CallController(call: host.call, voice: voice, target: target);
      await call.start();
      expect(call.ended, isTrue);
      expect(call.problem, 'Allow microphone access in Settings.');
      call.dispose();
    });
  });

  group('voice task', () {
    test('a short yes agrees; more words change the task', () {
      for (final yes in [
        'Yes',
        'ok',
        'go ahead',
        'Sì, vai',
        'va bene',
        'yes please',
      ]) {
        expect(agrees(yes), isTrue, reason: yes);
      }
      for (final no in [
        'no',
        'yes but also add tests to it',
        'change the title',
      ]) {
        expect(agrees(no), isFalse, reason: no);
      }
    });

    test('Italian agreement words, not longer turns', () {
      for (final yes in [
        'Sì, procedi',
        'va benissimo',
        'Perfetto!',
        'per me va bene',
        'Certo.',
      ]) {
        expect(agrees(yes), isTrue, reason: yes);
      }
      for (final no in [
        'no',
        'aspetta',
        'sì ma cambia anche il titolo',
        'Start later',
        'yes, but wait',
        'sì, ma aspetta',
        'va bene ma prima dimmi',
      ]) {
        expect(agrees(no), isFalse, reason: no);
      }
    });

    test('spoken commands in English and Italian', () {
      for (final stop in ['Stop.', 'Basta!', 'fermati', 'interrompi']) {
        expect(isStop(stop), isTrue, reason: stop);
      }
      for (final bye in ['hang up', 'Riattacca.', 'chiudi la chiamata']) {
        expect(isHangUp(bye), isTrue, reason: bye);
      }
      expect(isStop('basta con i test lenti'), isFalse);
    });

    test('pauses, returns, holds and listener words', () {
      for (final away in [
        'Aspetto un attimo che devo fare una cosa e poi continuiamo',
        'un attimo',
        'Scusa, torno subito',
        'ok dammi un minuto',
        'Give me a minute.',
        'hold on, be right back',
      ]) {
        expect(isPause(away), isTrue, reason: away);
      }
      for (final not in [
        'aspetta un secondo il file da cambiare è un altro',
        'il bottone aspetta un attimo prima di chiudersi e poi sparisce',
        'aspetta',
      ]) {
        expect(isPause(not), isFalse, reason: not);
      }
      expect(resumeRest('Eccomi!'), '');
      expect(resumeRest('ok ci sono, allora il login'), 'allora il login');
      expect(resumeRest("I'm back"), '');
      expect(resumeRest('sono andato a prendere un caffè'), isNull);
      expect(resumeRest('ti ho detto che ci sono'), isNull);
      expect(isHoldOn('No, aspetta.'), isTrue);
      expect(isHoldOn('aspetta che il test finisca'), isFalse);
      expect(isBackchannel('Mh, sì sì.'), isTrue);
      expect(isBackchannel('uh huh'), isTrue);
      expect(isBackchannel('sì ma no'), isFalse);
      expect(trailsOff('vorrei che il login e'), isTrue);
      expect(trailsOff('Run the tests and'), isTrue);
      expect(trailsOff('Run the tests.'), isFalse);
      for (final open in [
        'il problema è',
        'il login non',
        'prendi i dati da',
        'mettilo su',
        'uno dei',
        'il file che ho',
        'questo',
        'ehm',
        'eh',
        'beh',
        'cioè',
        'allora',
        'quindi',
        'the problem is',
        'they are',
        'put it on',
        'load it from',
      ]) {
        expect(trailsOff(open), isTrue, reason: open);
      }
      expect(trailsOff('il login non parte'), isFalse);
    });

    test('a word at the end sends the turn, unless the sentence needs it', () {
      expect(withoutSendWord('Controlla i test, vai.'), 'Controlla i test');
      expect(
        withoutSendWord('aggiorna il readme, fatto'),
        'aggiorna il readme',
      );
      // Without a pause before it, only as a result of its own.
      expect(withoutSendWord('aggiorna il readme fatto'), isNull);
      expect(
        withoutSendWord('aggiorna il readme fatto', separate: true),
        'aggiorna il readme',
      );
      expect(withoutSendWord('il test che ho fatto', separate: true), isNull);
      expect(withoutSendWord('Run the tests. Done.'), 'Run the tests.');
      expect(withoutSendWord('fix the build, go ahead'), 'fix the build');
      expect(withoutSendWord('fix the build, send'), 'fix the build');
      for (final not in [
        'vai',
        'done',
        'dimmi dove vai',
        'il test che ho fatto',
        'controlla che sia già fatto',
        "tell me when it's done",
        'make sure the tests are done',
        'write the email and send',
        'let it go ahead',
        'press send',
        'press send only after confirmation',
        'run the tests',
      ]) {
        expect(withoutSendWord(not), isNull, reason: not);
      }
    });

    test('the phrases follow the call language, English by default', () {
      expect(CallWords.of('it-IT'), same(CallWords.it));
      expect(CallWords.of('it'), same(CallWords.it));
      expect(CallWords.of('en-GB'), same(CallWords.en));
      expect(CallWords.of('fr-FR'), same(CallWords.en));
      expect(CallWords.of(''), same(CallWords.en));
      expect(languageCode('pt_BR'), 'pt');
      expect(
        speakable(
          'See https://x.y and `a much longer snippet of inline code than forty`',
          CallWords.it,
        ),
        'See un link and del codice.',
      );
    });

    const login = TaskDraft(
      title: 'Fix login error',
      goal: 'Show an error for a wrong password.',
      scope: 'The login page form.',
      done: 'A wrong password shows the error',
    );

    test('the task goes out with how to run it, and its end is recognized', () {
      final prompt = taskPrompt(login, id: 't-k3x9', language: 'it-IT');
      expect(prompt, contains('connecto add claude --model opus'));
      expect(prompt, contains('connecto add codex'));
      expect(prompt, contains('"Task complete [<its id>]."'));
      expect(prompt, contains('"Queued [t-k3x9]."'));
      expect(prompt, contains('.connecto/board.json'));
      expect(prompt, contains('in Italian'));
      expect(
        prompt,
        endsWith(
          'Id: t-k3x9\n'
          'Task: Fix login error\n'
          'Goal: Show an error for a wrong password.\n'
          'Scope: The login page form.\n'
          'Done when: A wrong password shows the error',
        ),
      );
      expect(isTaskComplete('**Task complete.** The login works.'), isTrue);
      expect(isTaskComplete('Attività completata. Il login funziona.'), isTrue);
      expect(isTaskComplete('Started claude-25 on it.'), isFalse);
      expect(
        isTaskComplete('Task complete? No, the review is pending.'),
        isFalse,
      );
      expect(
        reportsComplete([
          'Still working.\n\nTask complete? No, the review is pending.',
        ]),
        isFalse,
        reason: 'a paragraph is not a message',
      );
      expect(reportsComplete(['Review under way.', 'Task complete.']), isTrue);
      expect(
        proposalSpeech(login, CallWords.it),
        'Ecco il compito: Show an error for a wrong password. '
        'Cosa cambia: The login page form. '
        'Sarà finito quando: A wrong password shows the error. Procedo?',
      );
    });

    test(
      'the check asks for no work and a strict answer, in the call language',
      () {
        final prompt = validationPrompt([
          (true, 'Sistema il login'),
          (false, 'Quale errore?'),
        ], 'it-IT');
        expect(prompt, contains("Don't change anything"));
        // Deeper when it needs to be: a read-only helper, no yes needed; a
        // request only to find something out is answered, not proposed.
        expect(prompt, isNot(contains("don't start terminals")));
        expect(prompt, isNot(contains('only as far as')));
        expect(prompt, contains(analysisHelperRules));
        expect(prompt, contains('begins with "Checking."'));
        expect(prompt, contains("only wants to find out or check something"));
        expect(prompt, contains('in Italian'));
        expect(prompt, contains('TASK\nTitle:'));
        expect(prompt, contains('QUESTIONS\n1.'));
        expect(
          prompt,
          endsWith('Person: Sistema il login\nCall: Quale errore?'),
        );
      },
    );

    group('parseValidation', () {
      test('a full task is proposed, through markdown', () {
        final v = parseValidation(
          '**TASK**\n'
          '- **Title:** Fix login error\n'
          '- Goal: Show an error for a wrong password.\n'
          '- Scope: The login page form.\n'
          '- Done: A wrong password shows the error.',
        );
        expect(v, isA<Proposed>());
        final d = (v as Proposed).draft;
        expect(d.title, 'Fix login error');
        expect(d.scope, 'The login page form.');
        expect(d.done, 'A wrong password shows the error.');
      });

      test('Italian labels are read too', () {
        final v = parseValidation(
          'COMPITO\nTitolo: Errore login\nObiettivo: Mostra un errore.\n'
          'Ambito: Il form di login.\nFatto: Appare l\'errore.',
        );
        expect((v as Proposed).draft.goal, 'Mostra un errore.');
      });

      test('a task without how to tell it is done is not proposed', () {
        expect(
          parseValidation('TASK\nTitle: X\nGoal: Do x.\nScope: Here.'),
          isNull,
        );
        expect(
          parseValidation(
            'TASK\nGoal: Do x.\nScope: Here.\nDone:\n'
            'QUESTIONS\n1. How will you check it?',
          ),
          isA<Questions>(),
        );
      });

      test('at most two questions, numbering dropped', () {
        final v = parseValidation(
          'QUESTIONS\n1. Which page?\n2) Which error?\n3. And a third?',
        );
        expect((v as Questions).questions, ['Which page?', 'Which error?']);
      });

      test('identifiers and operators survive into the task sent', () {
        final v = parseValidation(
          '## TASK\n'
          '* **Title**: Limit retries\n'
          '* Goal: Stop after `max_retries`.\n'
          '* Scope: __init__.py and user_id in db_client.dart.\n'
          '* Done: count > 5 fails with *no* retry.',
        );
        final d = (v as Proposed).draft;
        expect(d.title, 'Limit retries');
        final prompt = taskPrompt(d, id: 't-1');
        expect(prompt, contains('Goal: Stop after `max_retries`.'));
        expect(
          prompt,
          contains('Scope: __init__.py and user_id in db_client.dart.'),
        );
        expect(prompt, contains('Done when: count > 5 fails with *no* retry.'));
      });

      test('anything else is not an answer in the form', () {
        expect(parseValidation('I had a look, the login is fine.'), isNull);
      });
    });

    const taskReply =
        'TASK\nTitle: Fix login error\n'
        'Goal: Show an error for a wrong password.\n'
        'Scope: The login page form.\n'
        'Done: A wrong password shows the error.';

    List<String> checks(FakeHost host) => [
      for (final f in host.sent('send'))
        if ((f['text'] as String).startsWith('A request taken by voice'))
          f['text'] as String,
    ];

    for (final known in [true, false]) {
      testWidgets(
        known
            ? 'with a functional memory, the intake knows it and the check '
                  'starts from it'
            : 'a host without the functional memory: the call is as before',
        (tester) async {
          final host = FakeHost()
            ..memory = known
                ? [
                    {'path': 'README.md', 'text': 'Connecto pairs a phone.'},
                    {'path': 'features.md', 'text': 'Calls, chats.'},
                  ]
                : null;
          final voice = FakeVoice()..intake = (_) => const Intake(check: true);
          final call = CallController(
            call: host.call,
            voice: voice,
            target: target,
            poll: const Duration(milliseconds: 100),
          );
          await call.start();
          await tester.pump();
          voice.say('Fix the login error', isFinal: true);
          await tester.pump(turnGap);
          await tester.pump();
          expect(host.sent('functional_memory'), [target]);
          final check = checks(host).single;
          if (known) {
            expect(voice.clarifyMemory.single, contains('Connecto pairs'));
            expect(voice.clarifyMemory.single, isNot(contains('Calls, chats')));
            expect(check, contains('.functionalMemory/'));
          } else {
            expect(voice.clarifyMemory.single, '');
            expect(
              check,
              validationPrompt([(true, 'Fix the login error')], 'en'),
            );
          }
          call.dispose();
          await tester.pump();
        },
      );
    }

    testWidgets('going on over the answer takes both parts as one turn', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()
        ..intake = (_) => const Intake(say: 'Sure. Which page?');
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();

      voice.holdSpeech = true;
      voice.say('Fix the error message', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.spoken.last, 'Sure. Which page?');
      expect(call.phase, CallPhase.speaking);
      // The answer had just begun; the person had not finished.
      voice.holdSpeech = false;
      voice.say('on the login page', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.clarified.last, [
        (true, 'Fix the error message on the login page'),
      ]);

      // An answer heard to the end is answered, not continued.
      voice.say('and the signup page too', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.clarified.last, hasLength(3));
      call.dispose();
      await tester.pump();
    });

    testWidgets(
      'talks, checks with the project on hold, proposes, starts on a yes, '
      'ends when it is done',
      (tester) async {
        final host = FakeHost();
        final voice = FakeVoice()
          ..intake = (turns) => turns.length == 1
              ? const Intake(say: 'Sure. Which page shows it?')
              : const Intake(check: true);
        final call = CallController(
          call: host.call,
          voice: voice,
          target: target,
          poll: const Duration(milliseconds: 100),
        );
        await call.start();
        await tester.pump();

        voice.say('Fix the error message', isFinal: true);
        await tester.pump(turnGap);
        expect(voice.cues, 1, reason: 'acknowledged at once');
        expect(voice.spoken.last, 'Sure. Which page shows it?');
        expect(call.draft, isNull);
        expect(host.sent('send'), isEmpty, reason: 'nothing goes out yet');
        expect(call.onHold, isFalse);

        voice.say('The login page', isFinal: true);
        await tester.pump(turnGap);
        expect(voice.clarified.last, [
          (true, 'Fix the error message'),
          (false, 'Sure. Which page shows it?'),
          (true, 'The login page'),
        ]);
        expect(voice.spoken.last, 'Okay, checking.');
        expect(checks(host), hasLength(1));
        expect(checks(host).single, endsWith('Person: The login page'));
        expect(call.validating, isTrue);
        expect(call.onHold, isTrue);
        expect(voice.holds.last, isTrue, reason: 'the hold tone plays');
        expect(call.draft, isNull);

        // The project asks first; the call speaks its questions.
        host.reply('QUESTIONS\n1. Should it also lock the account?');
        await tester.pump(const Duration(milliseconds: 300));
        expect(voice.spoken.last, 'Should it also lock the account?');
        expect(call.validating, isFalse);
        expect(voice.holds.last, isFalse, reason: 'off hold');
        expect(call.draft, isNull);

        voice.say('No, just the message', isFinal: true);
        await tester.pump(turnGap);
        expect(checks(host), hasLength(2));
        host.reply(taskReply);
        await tester.pump(const Duration(milliseconds: 300));
        expect(call.draft?.title, 'Fix login error');
        expect(voice.spoken.last, endsWith('Shall I start?'));
        expect(host.sent('send'), hasLength(2), reason: 'nothing started');

        voice.say('yes', isFinal: true);
        await tester.pump(turnGap);
        final sent = host.sent('send').last['text'] as String;
        expect(sent, contains('Show an error for a wrong password'));
        expect(sent, contains('--model opus'));
        expect(voice.spoken.last, startsWith("Great, I'm on it"));
        expect(call.started, isTrue);
        expect(call.phase, CallPhase.working);
        expect(call.onHold, isTrue);

        host.reply('Started claude-25 on it, with a Codex review after.');
        await tester.pump(const Duration(milliseconds: 300));
        expect(voice.spoken.last, contains('Started claude-25'));
        expect(call.ended, isFalse, reason: 'the task is not over yet');
        expect(call.onHold, isTrue, reason: 'on hold until it reports');

        // Later, with no prompt from the person, the agent reports back.
        host.reply('Task complete. The login page shows the error.');
        await tester.pump(const Duration(milliseconds: 300));
        expect(
          voice.spoken.last,
          'Task complete. The login page shows the error.',
        );
        expect(call.completed, isTrue);
        expect(call.ended, isTrue);
        expect(voice.holds.last, isFalse);
        call.dispose();
        await tester.pump();
      },
    );

    // A check the agent handed to a read-only helper ends its turn with a
    // "Checking." note: the call keeps waiting for the answer that follows,
    // saying it is still working (the call's clock is the real one).
    Future<(FakeHost, FakeVoice, CallController)> helperCheck(
      WidgetTester tester,
      Duration limit,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
        stillWorking: Duration.zero,
        helperCheckLimit: limit,
      );
      await call.start();
      await tester.pump();
      voice.say('Why does the login fail?', isFinal: true);
      await tester.pump(turnGap);
      expect(call.validating, isTrue);
      expect(checks(host).single, contains(analysisHelperRules));
      host.reply('Checking. A helper reads the login code.');
      for (var i = 0; i < 5; i++) {
        await tester.pump(const Duration(milliseconds: 100));
      }
      return (host, voice, call);
    }

    testWidgets('a check handed to a helper is waited for', (tester) async {
      final (host, voice, call) = await helperCheck(
        tester,
        const Duration(minutes: 10),
      );
      expect(call.validating, isTrue, reason: 'the answer comes later');
      expect(call.draft, isNull);
      expect(voice.spoken.where((s) => s.startsWith('Checking')), isEmpty);
      expect(voice.spoken.last, 'Still working.');

      // Regression: Connecto's notice waking the agent with the helper's
      // answer is a prompt in the history, and the reply after it was not
      // read as the check's answer.
      host.chat = {
        ...host.chat,
        'messages': [
          ...host.chat['messages'] as List,
          [true, '[connecto notice] Request r1 ended (ok).'],
        ],
      };
      host.reply(taskReply);
      for (var i = 0; i < 5; i++) {
        await tester.pump(const Duration(milliseconds: 100));
      }
      expect(call.validating, isFalse);
      expect(call.draft?.title, 'Fix login error');
      expect(voice.spoken.last, endsWith('Shall I start?'));
      call.dispose();
      await tester.pump();
    });

    testWidgets(
      'an answer from a helper that never comes is waited for no longer '
      'than the limit',
      (tester) async {
        final (_, voice, call) = await helperCheck(tester, Duration.zero);
        expect(call.validating, isFalse);
        expect(call.draft, isNull);
        expect(voice.spoken.last, "I didn't hear back. Check the chat.");
        call.dispose();
        await tester.pump();
      },
    );

    testWidgets('what is said on hold goes into a new check', (tester) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Add a dark mode', isFinal: true);
      await tester.pump(turnGap);
      expect(checks(host), hasLength(1));

      voice.say('only for the settings page', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.spoken.last, "Got it, I'll pass it on.");
      expect(checks(host), hasLength(1), reason: 'not while the agent runs');

      // The answer predates the addition: checked again, not proposed.
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft, isNull);
      expect(checks(host), hasLength(2));
      expect(checks(host).last, endsWith('Person: only for the settings page'));
      expect(call.validating, isTrue);

      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft, isNotNull);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a revised request drops the old proposal', (tester) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Add a dark mode', isFinal: true);
      await tester.pump(turnGap);
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft, isNotNull);

      voice.say('only in the settings', isFinal: true);
      await tester.pump(turnGap);
      expect(call.draft, isNull, reason: 'revised: the old task is gone');
      host.reply('QUESTIONS\n1. Should it follow the system theme?');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Should it follow the system theme?');

      voice.say('yes', isFinal: true);
      await tester.pump(turnGap);
      expect(call.started, isFalse);
      expect(
        host.sent('send').map((f) => f['text'] as String),
        everyElement(isNot(contains('--model opus'))),
      );
      call.dispose();
      await tester.pump();
    });

    Future<(FakeHost, FakeVoice, CallController)> onHold(
      WidgetTester tester,
      FakeHost host,
    ) async {
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Add a dark mode', isFinal: true);
      await tester.pump(turnGap);
      expect(call.validating, isTrue);
      return (host, voice, call);
    }

    testWidgets('"stop" before the check shows as running stops it once it '
        'does', (tester) async {
      final (host, voice, call) = await onHold(
        tester,
        FakeHost()..echoes = false,
      );
      voice.say('stop', isFinal: true);
      await tester.pump(turnGap);
      expect(call.validating, isFalse);
      expect(call.phase, CallPhase.listening);
      expect(voice.spoken.last, 'Okay, stopping it.');
      expect(host.sent('stop'), isEmpty, reason: 'the host would refuse it');

      // The host shows the check as the running turn.
      host.chat = {
        ...host.chat,
        'status': 'Running',
        'messages': [
          ...host.chat['messages'] as List,
          [true, host.sent('send').single['text']],
        ],
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(host.sent('stop'), hasLength(1));
      await tester.pump(const Duration(milliseconds: 300));
      expect(host.sent('stop'), hasLength(1), reason: 'stopped: no retries');
      call.dispose();
      await tester.pump();
    });

    testWidgets('a failed stop is retried; the answer is dropped', (
      tester,
    ) async {
      final (host, voice, call) = await onHold(
        tester,
        FakeHost()..failStop = true,
      );
      await tester.pump(const Duration(milliseconds: 200));
      voice.say('basta', isFinal: true);
      await tester.pump(turnGap);
      expect(call.validating, isFalse);
      final tries = host.sent('stop').length;
      expect(tries, greaterThan(0));
      host.failStop = false;
      await tester.pump(const Duration(milliseconds: 300));
      expect(host.sent('stop').length, greaterThan(tries));
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft, isNull, reason: 'called off: not proposed');
      call.dispose();
      await tester.pump();
    });

    testWidgets('a check answered while the stop is on its way is dropped', (
      tester,
    ) async {
      final (host, voice, call) = await onHold(
        tester,
        FakeHost()..stopGate = Completer<void>(),
      );
      await tester.pump(const Duration(milliseconds: 200));
      voice.say('stop', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('stop'), hasLength(1));
      host.reply(taskReply, status: 'Running');
      await tester.pump(const Duration(milliseconds: 300));
      host.chat = {...host.chat, 'status': 'Ready'};
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft, isNull);

      // Hanging up while the stop is pending keeps the call ended.
      await call.hangUp();
      host.chat = {...host.chat, 'status': 'Running'};
      host.stopGate!.complete();
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a stop called as the check answers never reaches the next', (
      tester,
    ) async {
      final (host, voice, call) = await onHold(tester, FakeHost());
      voice.say('stop', isFinal: true);
      await tester.pump(const Duration(milliseconds: 300));
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 1800));
      expect(call.validating, isFalse);
      expect(call.draft, isNull, reason: 'it came in while stop was said');
      expect(host.sent('stop'), isEmpty, reason: 'nothing was running');

      voice.say(
        'Add a dark mode to settings',
        isFinal: true,
        start: 9,
        end: 10,
      );
      await tester.pump(turnGap);
      expect(call.validating, isTrue, reason: 'a second check');
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.chat['status'], 'Running');
      expect(host.sent('stop'), isEmpty);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a stop after the check answered never reaches later turns', (
      tester,
    ) async {
      final (host, voice, call) = await onHold(tester, FakeHost());
      voice.say('stop', isFinal: true);
      await tester.pump(const Duration(milliseconds: 300));
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 1800));
      expect(call.draft, isNull);
      // A turn started from the project's chat on screen.
      host.chat = {...host.chat, 'status': 'Running'};
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.sent('stop'), isEmpty);
      call.dispose();
      await tester.pump();
    });

    testWidgets('hanging up while a reply is condensed stays ended', (
      tester,
    ) async {
      final (host, voice, call) = await onHold(tester, FakeHost());
      voice.condenseGate = Completer<void>();
      host.reply(List.filled(40, 'Not in the form at all.').join(' '));
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.condenseAsked, hasLength(1));
      await call.hangUp();
      voice.condenseGate!.complete();
      await tester.pump();
      expect(call.phase, CallPhase.ended);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a check called off while it is sent, whose send fails, '
        'stops nothing', (tester) async {
      final host = FakeHost()..sendGate = Completer<void>();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Add a dark mode', isFinal: true);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.sending);
      voice.say('stop', isFinal: true, start: 5, end: 5.5);
      await tester.pump(turnGap);
      expect(call.validating, isFalse);
      host
        ..failSend = true
        ..sendGate!.complete();
      await tester.pump();
      // A turn started from the project's chat on screen.
      host.chat = {
        ...host.chat,
        'status': 'Running',
        'messages': [
          ...host.chat['messages'] as List,
          [true, 'typed on the desktop'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 600));
      expect(host.sent('stop'), isEmpty);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a send that fails after hanging up leaves the call ended', (
      tester,
    ) async {
      final host = FakeHost()..sendGate = Completer<void>();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
      );
      await call.start();
      await tester.pump();
      voice.say('Add a dark mode', isFinal: true);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.sending);
      await call.hangUp();
      host
        ..failSend = true
        ..sendGate!.complete();
      await tester.pump();
      expect(call.ended, isTrue);
      expect(call.phase, CallPhase.ended);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a re-check refused for a choice on screen leaves the hold', (
      tester,
    ) async {
      final (host, voice, call) = await onHold(tester, FakeHost());
      voice.say('only in settings', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.spoken.last, "Got it, I'll pass it on.");
      host.reply(taskReply, status: 'Needs an answer');
      host.chat = {
        ...host.chat,
        'question': 'Allow edit?',
        'textAnswer': false,
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, contains('choice on screen'));
      expect(call.validating, isFalse);
      expect(call.phase, CallPhase.listening);
      await tester.pump(const Duration(milliseconds: 1000));
      expect(call.problem, isNull, reason: 'the polls go on cleanly');
      call.dispose();
      await tester.pump();
    });

    testWidgets('a yes said on hold does not accept a proposal not yet heard', (
      tester,
    ) async {
      final (host, voice, call) = await onHold(tester, FakeHost());
      voice.say('yes', isFinal: true);
      await tester.pump(const Duration(milliseconds: 300));
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft, isNotNull);
      await tester.pump(const Duration(milliseconds: 1500));
      expect(call.started, isFalse);
      expect(
        host.sent('send').map((f) => f['text'] as String),
        everyElement(isNot(contains('--model opus'))),
      );

      // A yes after hearing it starts it.
      voice.say('yes', isFinal: true, start: 5, end: 5.5);
      await tester.pump(turnGap);
      expect(call.started, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a yes to a proposal cut short says it again; a yes after it '
        'starts', (tester) async {
      final (host, voice, call) = await onHold(tester, FakeHost());
      voice.holdSpeech = true;
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft, isNotNull);
      final proposal = voice.spoken.last;
      final firstClause = proposal.indexOf('. ') + 1;
      expect(firstClause, greaterThan(0));
      voice.heardWhenStopped = firstClause + 3;

      // Cut short: the history keeps what was heard of it.
      voice.say('Wait.', isFinal: true, start: 5, end: 5.4);
      await tester.pump();
      expect(call.phase, CallPhase.listening);
      await tester.pump(turnGap);
      expect(call.intake.last, (
        false,
        '${proposal.substring(0, firstClause)} '
            '[cut off here by the person]',
      ));

      // A yes to it now: it is said again, whole, and does not start.
      voice.holdSpeech = false;
      voice.say('yes', isFinal: true, start: 8, end: 8.4);
      await tester.pump(turnGap);
      expect(call.started, isFalse);
      expect(voice.spoken.where((s) => s == proposal), hasLength(2));
      expect(call.intake.last, (false, proposal));

      // Heard to the end now: a yes starts it.
      voice.say('yes', isFinal: true, start: 12, end: 12.4);
      await tester.pump(turnGap);
      expect(call.started, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('an answer cut short stays in the history as far as it was '
        'heard', (tester) async {
      final host = FakeHost();
      final voice = FakeVoice()
        ..intake = (_) =>
            const Intake(say: 'Sure. Which page shows it, login or signup?');
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.holdSpeech = true;
      voice.heardWhenStopped = 'Sure. Wh'.length;
      voice.say('Fix the error message', isFinal: true);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.speaking);
      // Well into the answer: not a continuation of their turn.
      await tester.pump(const Duration(seconds: 3));
      voice.say('the signup one', isFinal: true, start: 10, end: 11);
      await tester.pump(turnGap);
      expect(voice.clarified.last, [
        (true, 'Fix the error message'),
        (false, 'Sure. [cut off here by the person]'),
        (true, 'the signup one'),
      ]);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a proposal kept through a pause, then cut short, stays in '
        'the history as far as it was heard', (tester) async {
      final (host, voice, call) = await onHold(tester, FakeHost());
      voice.say('Give me a minute.', isFinal: true, start: 3, end: 4);
      await tester.pump(turnGap);
      expect(call.paused, isTrue);
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.intake.where((e) => !e.$1), isEmpty, reason: 'not said yet');

      voice.holdSpeech = true;
      // Back: the proposal is said now, and held to be cut short.
      unawaited(call.resume());
      await tester.pump();
      final proposal = voice.spoken.last;
      voice.heardWhenStopped = proposal.indexOf('. ') + 3;
      voice.say('Wait.', isFinal: true, start: 9, end: 9.4);
      await tester.pump(turnGap);
      expect(call.intake.last.$2, endsWith('[cut off here by the person]'));
      expect(call.intake.last.$2, isNot(proposal));
      call.dispose();
      await tester.pump();
    });

    testWidgets('a pause while the proposal waits keeps it for later', (
      tester,
    ) async {
      final (host, voice, call) = await onHold(tester, FakeHost());
      voice.say('give me', start: 3, end: 3.5);
      await tester.pump(const Duration(milliseconds: 100));
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft, isNotNull);
      voice.say('Give me a minute.', isFinal: true, start: 3, end: 4);
      await tester.pump(turnGap);
      expect(call.paused, isTrue);
      expect(call.intake.where((e) => !e.$1), isEmpty, reason: 'not said yet');

      await call.resume();
      await tester.pump();
      final proposal = voice.spoken.last;
      expect(call.intake.last, (false, proposal));
      voice.say('yes', isFinal: true, start: 9, end: 9.4);
      await tester.pump(turnGap);
      expect(call.started, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a newer turn calls off the answer being worked out', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()
        ..intake = ((_) => const Intake(say: 'Which page?'))
        ..clarifyGate = Completer<void>();
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Fix the error message', isFinal: true);
      await tester.pump(turnGap);
      expect(call.phase, CallPhase.thinking);
      final first = voice.clarifyCancels.single!;
      expect(first.cancelled, isFalse);

      // They go on: the answer under way is useless, and called off.
      voice.say('on the signup page', isFinal: true, start: 5, end: 6);
      await tester.pump();
      expect(first.cancelled, isTrue);
      await tester.pump(turnGap);
      expect(voice.clarified.last, [
        (true, 'Fix the error message on the signup page'),
      ]);
      voice.clarifyGate!.complete();
      await tester.pump(const Duration(milliseconds: 100));
      expect(voice.spoken.where((s) => s == 'Which page?'), hasLength(1));

      // Hanging up calls off the one under way too.
      voice.clarifyGate = Completer<void>();
      voice.say('and the docs', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      await call.hangUp();
      expect(voice.clarifyCancels.last!.cancelled, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('"stop" on hold stops the check', (tester) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Add a dark mode', isFinal: true);
      await tester.pump(turnGap);
      await tester.pump(const Duration(milliseconds: 200));
      voice.say('basta', isFinal: true);
      await tester.pump(turnGap);
      expect(host.sent('stop'), hasLength(1));
      expect(call.validating, isFalse);
      call.dispose();
      await tester.pump();
    });

    testWidgets('an Italian call speaks, asks and checks in Italian', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()
        ..languageList = const VoiceLanguages(
          [VoiceLanguage('it-IT', 'Italiano', installed: true)],
          selected: 'it-IT',
          current: 'it-IT',
        )
        ..intake = (turns) => turns.length == 1
            ? const Intake(say: 'Certo. In quale pagina?')
            : const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      expect(voice.spoken.single, "Ciao, cosa c'è da fare?");

      voice.say("Sistema l'errore", isFinal: true);
      await tester.pump(turnGap);
      expect(voice.modelLanguages, ['it'], reason: 'the model answers in it');

      voice.say('Nella pagina di login', isFinal: true);
      await tester.pump(turnGap);
      expect(voice.spoken.last, 'Ok, controllo.');
      expect(checks(host).single, contains('in Italian'));

      host.reply(
        'TASK\nTitle: Errore login\nGoal: Mostra un errore.\n'
        'Scope: Il form di login.\nDone: Appare l\'errore.',
      );
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, endsWith('Procedo?'));

      voice.say('Sì, procedi', isFinal: true);
      await tester.pump(turnGap);
      expect(call.started, isTrue);
      expect(voice.spoken.last, startsWith('Perfetto, ci penso io'));
      expect(host.sent('send').last['text'], contains('in Italian'));

      // Long updates are condensed in Italian too.
      host.reply(List.filled(40, 'Il build è verde.').join(' '));
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.modelLanguages.last, 'it');
      expect(voice.spoken.last, endsWith('Il resto è nella chat.'));

      // The English marker is said in Italian; it still ends the task.
      host.reply('Task complete. Il login mostra l\'errore.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(
        voice.spoken.last,
        "Compito completato. Il login mostra l'errore.",
      );
      expect(call.completed, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('without the on-device model the project helps at once', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice();
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Run the tests', isFinal: true);
      await tester.pump(turnGap);
      expect(checks(host).single, endsWith('Person: Run the tests'));
      expect(call.validating, isTrue);
      host.reply(
        'TASK\nTitle: Run tests\nGoal: Run the tests.\n'
        'Scope: The whole suite.\nDone: They all pass.',
      );
      await tester.pump(const Duration(milliseconds: 300));
      await call.startTask();
      expect(
        host.sent('send').last['text'],
        endsWith('Done when: They all pass.'),
      );
      call.dispose();
      await tester.pump();
    });
  });

  testWidgets('"still working" is never said over the person', (tester) async {
    final host = FakeHost();
    final voice = FakeVoice();
    final call = CallController(
      call: host.call,
      voice: voice,
      target: target,
      poll: const Duration(milliseconds: 100),
      // Due at every poll: only the person talking holds it back.
      stillWorking: Duration.zero,
    )..started = true;
    await call.start();
    await tester.pump();
    voice.say('run the tests', isFinal: true);
    await tester.pump(turnGap);
    expect(call.phase, CallPhase.working);

    voice.heard.add(const VoiceActivity(true));
    await tester.pump();
    final before = voice.spoken.length;
    await tester.pump(const Duration(milliseconds: 1500));
    expect(voice.spoken, hasLength(before), reason: 'never over their voice');
    voice.heard.add(const VoiceActivity(false));
    await tester.pump(const Duration(milliseconds: 300));
    expect(voice.spoken.skip(before), contains('Still working.'));
    call.dispose();
    await tester.pump();
  });

  testWidgets('the call page counts down to the send; a tap sends at once', (
    tester,
  ) async {
    final host = FakeHost();
    final voice = FakeVoice();
    await tester.pumpWidget(
      MaterialApp(
        home: CallPage(
          title: 'p',
          call: host.call,
          voice: voice,
          target: target,
        ),
      ),
    );
    await tester.pump(const Duration(milliseconds: 100));
    const hint = 'Sending when you pause · tap to send now';
    expect(find.text(hint), findsNothing);

    voice.say('Fix the login page', isFinal: true, start: 0.2, end: 1.2);
    await tester.pump(const Duration(milliseconds: 100));
    expect(find.text(hint), findsOneWidget);
    final bar = tester.widget<LinearProgressIndicator>(
      find.byType(LinearProgressIndicator),
    );
    expect(bar.value, 1);
    await tester.pump(const Duration(milliseconds: 1000));
    expect(
      tester
          .widget<LinearProgressIndicator>(find.byType(LinearProgressIndicator))
          .value,
      lessThan(bar.value!),
      reason: 'it runs out',
    );

    await tester.tap(find.byKey(const Key('call-heard')));
    await tester.pump();
    expect(find.text(hint), findsNothing);
    // No on-device model: the request goes to the project at once.
    expect(
      host.sent('send').single['text'],
      contains('Person: Fix the login page'),
    );
    await tester.pumpWidget(const SizedBox());
    await tester.pump();
  });

  testWidgets('the rest of a sentence cut short goes on in the conversation', (
    tester,
  ) async {
    final host = FakeHost();
    final voice = FakeVoice()
      ..intake = (_) => const Intake(say: 'Sure. Which page?');
    final call = CallController(
      call: host.call,
      voice: voice,
      target: target,
      poll: const Duration(milliseconds: 100),
    );
    await call.start();
    await tester.pump();

    voice.holdSpeech = true;
    voice.say('Fix the error', start: 0, end: 1.0);
    voice.heard.add(const TurnEstimate(0.95, at: 1.05));
    await tester.pump(const Duration(milliseconds: 1300));
    expect(voice.spoken.last, 'Sure. Which page?');

    // The recognizer reports the segment again, from its start, with more.
    voice.say('Fix the error message on the login page', start: 0, end: 2.8);
    await tester.pump();
    expect(call.phase, CallPhase.listening, reason: 'the answer stopped');
    voice.holdSpeech = false;
    await tester.pump(turnGap);
    expect(voice.clarified.last, [
      (true, 'Fix the error message on the login page'),
    ]);
    call.dispose();
    await tester.pump();
  });

  testWidgets("without the device's model, the Mac's answers the call", (
    tester,
  ) async {
    final host = FakeHost()
      ..completion = (fields) {
        final messages = fields['messages'] as List;
        return (messages.last as Map)['content'] == 'OK'
            ? {'text': 'OK'}
            : {'text': '{"next":"talk","say":"Certo. Quale pagina?"}'};
      };
    final voice = FakeVoice();
    final call = CallController(
      call: host.call,
      voice: voice,
      target: const {'host': 'Studio Mac', 'project': 'p'},
      poll: const Duration(milliseconds: 100),
    );
    await call.start();
    await tester.pump();
    // The model was warmed up as the call began.
    expect(host.sent('complete'), hasLength(1));

    voice.say('Fix the error message', isFinal: true);
    await tester.pump(turnGap);
    await tester.pump(const Duration(milliseconds: 100));
    expect(voice.spoken.last, 'Certo. Quale pagina?');
    expect(host.sent('send'), isEmpty, reason: 'no check yet');
    final asked = host.sent('complete').last;
    expect(asked['host'], 'Studio Mac');
    expect(asked['timeoutMs'], 2500);
    call.dispose();
    await tester.pump();
  });

  testWidgets('the call warms the phone model and shows its download', (
    tester,
  ) async {
    final host = FakeHost();
    final voice = FakeVoice()
      ..phone = const PhoneModelState(
        on: true,
        progress: 0.42,
        bytes: 1327696992,
      );
    final call = CallController(
      call: host.call,
      voice: voice,
      target: const {'host': 'Offline Mac', 'project': 'p'},
      poll: const Duration(milliseconds: 100),
    );
    await call.start();
    await tester.pump();
    expect(voice.phonePrewarms, 1);
    expect(call.notice, 'Downloading the on-phone assistant… 42%');
    voice.phone = const PhoneModelState(
      on: true,
      ready: true,
      bytes: 1327696992,
    );
    await tester.pump(const Duration(seconds: 1));
    await tester.pump();
    expect(call.notice, isNull, reason: 'done');
    call.dispose();
    await tester.pump(const Duration(seconds: 1));
  });

  testWidgets('settings turn the on-phone assistant on, with its size', (
    tester,
  ) async {
    final voice = FakeVoice()..phone = const PhoneModelState(bytes: 1327696992);
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsPage(voice: voice, version: () async => 'test'),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('On-phone assistant (offline)'), findsOneWidget);
    expect(
      find.textContaining('Downloads 1.3 GB once, on Wi-Fi'),
      findsOneWidget,
    );
    await tester.tap(find.text('On-phone assistant (offline)'));
    await tester.pump();
    expect(voice.phoneSet, [true]);
    expect(find.textContaining('Downloading… 0% of 1.3 GB'), findsOneWidget);
    voice.phone = const PhoneModelState(
      on: true,
      progress: 0,
      waiting: true,
      bytes: 1327696992,
    );
    await tester.pump(const Duration(seconds: 1));
    await tester.pump();
    expect(find.textContaining('Waiting for Wi-Fi'), findsOneWidget);
    voice.phone = const PhoneModelState(
      on: true,
      ready: true,
      bytes: 1327696992,
    );
    await tester.pump(const Duration(seconds: 1));
    await tester.pump();
    expect(find.textContaining('Ready.'), findsOneWidget);

    // Where it is not offered, no switch.
    voice.phone = PhoneModelState.none;
    await tester.pumpWidget(
      MaterialApp(
        home: SettingsPage(
          key: UniqueKey(),
          voice: voice,
          version: () async => 'test',
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('On-phone assistant (offline)'), findsNothing);
  });

  testWidgets('hanging up while the call starts never opens the microphone', (
    tester,
  ) async {
    final host = FakeHost()..holdChat = Completer<void>();
    final voice = _CountingVoice();
    final call = CallController(call: host.call, voice: voice, target: target);
    final starting = call.start();
    await call.hangUp();
    host.holdChat!.complete();
    await starting;
    expect(voice.starts, 0);
    expect(call.ended, isTrue);
    call.dispose();
  });

  testWidgets('the chat offers a call only where the device supports it', (
    tester,
  ) async {
    final host = FakeHost();
    Future<void> open(FakeVoice voice) async {
      await tester.pumpWidget(
        MaterialApp(
          home: ChatPage(
            key: ObjectKey(voice),
            host: 'Host',
            project: 'p',
            title: 'Project',
            call: host.call,
            voice: voice,
          ),
        ),
      );
      await tester.pump();
      await tester.pump();
    }

    await open(FakeVoice(available: false));
    expect(find.byTooltip('Call the agent'), findsNothing);

    final voice = FakeVoice();
    await open(voice);
    await tester.tap(find.byTooltip('Call the agent'));
    await tester.pumpAndSettle(const Duration(milliseconds: 100));
    expect(find.text('Listening'), findsOneWidget);
    expect(find.text('What would you like done?'), findsOneWidget);

    // The call language is picked in the settings; missing models download.
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle(const Duration(milliseconds: 100));
    expect(find.text('Device (English (United States))'), findsOneWidget);
    expect(find.text('Version'), findsOneWidget);
    await tester.tap(find.text('Call language'));
    await tester.pumpAndSettle(const Duration(milliseconds: 100));
    expect(find.text('Downloads when chosen'), findsOneWidget);
    await tester.tap(find.text('Italian (Italy)'));
    await tester.pumpAndSettle(const Duration(milliseconds: 100));
    expect(voice.languagesSet, ['it-IT']);
    expect(find.text('Italian (Italy)'), findsOneWidget);
    await tester.pageBack();
    await tester.pumpAndSettle(const Duration(milliseconds: 100));
    expect(find.text('Dimmi pure.'), findsOneWidget);

    // The task is worked out by voice and checked on hold; a tap hands it
    // to the agent.
    voice.intake = (_) => const Intake(check: true);
    voice.say('Sistema il bug del login', isFinal: true);
    await tester.pump(turnGap);
    await tester.pump(const Duration(milliseconds: 300));
    expect(find.text('On hold · checking with the project'), findsOneWidget);
    host.reply(
      'TASK\nTitle: Fix the login bug\nGoal: Show an error.\n'
      'Scope: The login form.\nDone: A wrong password shows it.',
    );
    await tester.pump(const Duration(milliseconds: 300));
    await tester.pumpAndSettle(const Duration(milliseconds: 100));
    expect(find.text('Proposed task'), findsOneWidget);
    await tester.tap(find.text('Start task'));
    await tester.pump();
    expect(host.sent('send').last['text'], contains('--model opus'));
    expect(find.text('Task started'), findsOneWidget);

    await tester.tap(find.text('Hang up'));
    await tester.pump();
    expect(find.text('Call ended'), findsOneWidget);
    expect(voice.stopped, isTrue);
    await tester.tap(find.text('Close'));
    await tester.pumpAndSettle(const Duration(milliseconds: 100));
    expect(find.text('Call ended'), findsNothing);
    await tester.pumpWidget(const SizedBox());
  });
}

class _FailingVoice extends FakeVoice {
  @override
  Future<void> start() async =>
      throw const VoiceError('Allow microphone access in Settings.');
}

class _CountingVoice extends FakeVoice {
  var starts = 0;
  @override
  Future<void> start() async {
    starts++;
    await super.start();
  }
}
