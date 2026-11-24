import 'dart:async';
import 'dart:convert';

import 'package:connecto_flutter_app/assistant.dart';
import 'package:connecto_flutter_app/call.dart' show validationPrompt;
import 'package:connecto_flutter_app/functional_memory.dart';
import 'package:connecto_voice/connecto_voice.dart';
import 'package:flutter_test/flutter_test.dart';

import 'call_test.dart' show FakeVoice;

/// A model that answers with [answer] after [delay], counting its calls.
class FakeModel implements ChatModel {
  FakeModel(this.answer, {this.delay = Duration.zero});
  String? answer;
  Duration delay;
  var asked = 0;
  final prompts = <List<ChatMessage>>[];

  /// The cancellation each request was given.
  final cancels = <Cancellation?>[];

  @override
  Future<String?> complete(
    List<ChatMessage> messages, {
    required int maxTokens,
    required double temperature,
    required Duration deadline,
    Cancellation? cancel,
  }) async {
    asked++;
    prompts.add(messages);
    cancels.add(cancel);
    await Future<void>.delayed(delay);
    return answer;
  }
}

/// The peer's `complete` op, as the test wants it to go.
class FakeLink {
  FakeLink(this.reply);
  FutureOr<dynamic> Function() reply;
  final asked = <Map<String, Object?>>[];
  Future<dynamic> op(String name, Map<String, Object?> fields) async {
    expect(name, 'complete');
    asked.add(fields);
    return reply();
  }
}

void main() {
  group('parseIntake', () {
    test('reads the JSON asked for, however it is wrapped', () {
      expect(
        parseIntake('{"next":"talk","say":"Ciao! Cosa sistemiamo?"}')?.say,
        'Ciao! Cosa sistemiamo?',
      );
      expect(parseIntake('{"next":"check","say":""}')?.check, isTrue);
      expect(
        parseIntake(
          '<think>\nhm\n</think>\n```json\n{"next":"talk","say":"Quale pagina?"}\n```',
        )?.say,
        'Quale pagina?',
      );
      expect(parseIntake('Sure: {"next": "CHECK"}')?.check, isTrue);
    });

    test('plain words are said; nothing usable is null', () {
      expect(parseIntake('Certo, quale pagina?')?.say, 'Certo, quale pagina?');
      expect(parseIntake('check')?.check, isTrue);
      expect(parseIntake(''), isNull);
      expect(parseIntake('<think>only thinking'), isNull);
      expect(parseIntake('{"next":"talk","say":""}'), isNull);
      expect(parseIntake('{broken'), isNull);
    });

    test('the prompts carry the language and the recent turns', () {
      expect(intakeInstructions('it'), contains('in Italian'));
      expect(intakeInstructions('it'), contains('"next"'));
      expect(condenseInstructions('it'), contains('Always answer in Italian'));
      final turns = [for (var i = 0; i < 20; i++) (i.isEven, 't$i')];
      final prompt = intakePrompt(turns);
      expect(prompt, isNot(contains('t3\n')));
      expect(prompt, contains('Person: t4\n'));
      expect(prompt, endsWith('You: t19\n\nAnswer the person.'));
      expect(condensePrompt('x' * 20000).length, lessThan(12100));
    });
  });

  group('RemoteHealth', () {
    test('slow or failing links are skipped for a while, longer each time', () {
      var now = DateTime(2026, 11, 22, 10);
      final health = RemoteHealth(now: () => now);
      const deadline = Duration(milliseconds: 2500);
      expect(health.usable, isTrue);

      // A cold warm-up, however slow, is not a turn's time.
      health.succeeded(
        const Duration(seconds: 9),
        const Duration(seconds: 15),
        measure: false,
      );
      expect(health.share, isNull);
      health.succeeded(const Duration(milliseconds: 1000), deadline);
      expect(health.usable, isTrue, reason: 'fast enough');

      // A blip is smoothed over; each against its own deadline.
      health.succeeded(const Duration(milliseconds: 2600), deadline);
      expect(health.share, closeTo(0.4 * 0.6 + 1.04 * 0.4, 1e-9));
      expect(health.usable, isTrue, reason: 'one slow answer, smoothed');
      health.succeeded(const Duration(seconds: 3), const Duration(seconds: 6));
      expect(health.usable, isTrue, reason: 'condense has longer');
      health.succeeded(const Duration(milliseconds: 4000), deadline);
      expect(health.usable, isFalse, reason: 'slow of late');
      now = now.add(const Duration(seconds: 31));
      expect(health.usable, isTrue);

      health.failed();
      expect(health.usable, isFalse);
      now = now.add(const Duration(seconds: 16));
      expect(health.usable, isTrue);
      health.failed();
      now = now.add(const Duration(seconds: 16));
      expect(health.usable, isFalse, reason: 'the second wait is longer');
      now = now.add(const Duration(seconds: 15));
      expect(health.usable, isTrue);
      for (var i = 0; i < 12; i++) {
        health.failed();
      }
      now = now.add(const Duration(minutes: 5, seconds: 1));
      expect(health.usable, isTrue, reason: 'never more than five minutes');

      health.unavailable();
      now = now.add(const Duration(minutes: 9));
      expect(health.usable, isFalse);
      now = now.add(const Duration(minutes: 2));
      expect(health.usable, isTrue);
    });
  });

  group('RemoteModel', () {
    const deadline = Duration(milliseconds: 200);
    Future<String?> ask(RemoteModel model) => model.complete(
      [('user', 'Ciao')],
      maxTokens: 10,
      temperature: 0,
      deadline: deadline,
    );

    test('asks the host with a deadline and reads its answer', () async {
      final link = FakeLink(() => {'text': 'Ciao!'});
      final model = RemoteModel(link.op, 'Mac', health: RemoteHealth());
      expect(await ask(model), 'Ciao!');
      expect(link.asked.single['host'], 'Mac');
      expect(link.asked.single['timeoutMs'], 200);
      expect(link.asked.single['messages'], [
        {'role': 'user', 'content': 'Ciao'},
      ]);
      expect(model.health.rtt, isNotNull);
    });

    test('the warm-up asks for one token and is not timed', () async {
      final link = FakeLink(() => {'text': 'OK'});
      final model = RemoteModel(link.op, 'Mac', health: RemoteHealth());
      await model.prewarm();
      expect(link.asked.single['maxTokens'], 1);
      expect(link.asked.single['timeoutMs'], 15000);
      expect(model.health.share, isNull);
      expect(model.health.usable, isTrue);
    });

    test('a Mac with no model, or an older peer, is left alone', () async {
      for (final reply in [
        {
          'error': 'no conversation model is set up on this machine',
          'unavailable': true,
        },
        null,
      ]) {
        final link = FakeLink(() => reply);
        final model = RemoteModel(link.op, 'Mac', health: RemoteHealth());
        expect(await ask(model), isNull);
        expect(model.health.usable, isFalse);
        expect(await ask(model), isNull);
        expect(link.asked, hasLength(1), reason: 'not asked again soon');
      }
    });

    test('a called-off request is not waited for, nor held against the '
        'link', () async {
      final answer = Completer<dynamic>();
      final link = FakeLink(() => answer.future);
      final model = RemoteModel(link.op, 'Mac', health: RemoteHealth());
      final cancel = Cancellation();
      final asked = model.complete(
        [('user', 'Ciao')],
        maxTokens: 10,
        temperature: 0,
        deadline: const Duration(seconds: 5),
        cancel: cancel,
      );
      await Future<void>.delayed(const Duration(milliseconds: 20));
      final watch = Stopwatch()..start();
      cancel.cancel();
      expect(await asked, isNull);
      expect(watch.elapsed, lessThan(const Duration(milliseconds: 200)));
      expect(model.health.usable, isTrue);
      // Its late answer changes nothing.
      answer.complete({'text': 'Tardi.'});
      await Future<void>.delayed(Duration.zero);
      expect(model.health.rtt, isNull);
    });

    test('a busy Mac is not held against the link', () async {
      final link = FakeLink(() => {'error': 'the conversation model is busy'});
      final model = RemoteModel(link.op, 'Mac', health: RemoteHealth());
      expect(await ask(model), isNull);
      expect(model.health.usable, isTrue);
    });

    test('a link too slow or down is given up on and skipped', () async {
      final slow = FakeLink(() => Completer<dynamic>().future);
      final model = RemoteModel(slow.op, 'Mac', health: RemoteHealth());
      final watch = Stopwatch()..start();
      expect(await ask(model), isNull);
      expect(watch.elapsed, lessThan(const Duration(milliseconds: 900)));
      expect(model.health.usable, isFalse);

      final down = FakeLink(() => throw Exception('Host disconnected'));
      final other = RemoteModel(down.op, 'Mac', health: RemoteHealth());
      expect(await ask(other), isNull);
      expect(other.health.usable, isFalse);
      final failed = FakeLink(
        () => {
          'error': 'the conversation model took too long',
          'unavailable': false,
        },
      );
      final third = RemoteModel(failed.op, 'Mac', health: RemoteHealth());
      expect(await ask(third), isNull);
      expect(third.health.usable, isFalse);
    });
  });

  group('functional memory', () {
    const turns = [(true, 'Sistema il login')];
    List<MemoryFile> memory({String readme = 'README-NOTE'}) => [
      (path: 'README.md', text: readme),
      (path: 'overview.md', text: 'OVERVIEW-NOTE'),
      (path: 'features.md', text: 'FEATURES-NOTE ${'x' * 5000}'),
    ];
    const talk = '{"next":"talk","say":"Quale pagina?"}';

    test(
      'the device\'s Foundation Models gets README and overview only',
      () async {
        final voice = FakeVoice()..intake = (_) => const Intake(say: 'Certo.');
        final a = Assistant(voice: voice)..know(memory());
        await a.clarify(turns, language: 'it');
        final given = voice.clarifyMemory.single;
        expect(given, contains('README-NOTE'));
        expect(given, contains('OVERVIEW-NOTE'));
        expect(given, isNot(contains('FEATURES-NOTE')));
        expect(given.length, lessThanOrEqualTo(memoryExcerptBudget));
      },
    );

    test('the Mac\'s large model gets all of it in its instructions', () async {
      final mac = FakeModel(talk);
      final a = Assistant(voice: FakeVoice(), mac: mac)..know(memory());
      expect((await a.clarify(turns, language: 'it'))?.say, 'Quale pagina?');
      final system = mac.prompts.single.first.$2;
      for (final note in ['README-NOTE', 'OVERVIEW-NOTE', 'FEATURES-NOTE']) {
        expect(system, contains(note));
      }
      expect(system, endsWith('empty for check}.'));
    });

    test('the phone\'s small model gets a capped excerpt that fits', () async {
      final mac = FakeModel(null);
      final phone = FakeModel(talk);
      final a = Assistant(
        voice: FakeVoice(),
        mac: mac,
        phone: phone,
      )..know(memory(readme: 'README-NOTE\n${'Long line of notes.\n' * 1000}'));
      expect((await a.clarify(turns, language: 'it'))?.say, 'Quale pagina?');
      expect(mac.prompts.single.first.$2, contains('FEATURES-NOTE'));
      final system = phone.prompts.single.first.$2;
      expect(system, contains('README-NOTE'));
      expect(system, isNot(contains('FEATURES-NOTE')));
      // Whole within what the phone's model takes: the answer format, last,
      // is never cut off.
      expect(system.length, lessThan(PhoneModel.maxChars));
      expect(system, endsWith('empty for check}.'));
    });

    test('no memory: every model is asked exactly as before', () async {
      final voice = FakeVoice();
      final mac = FakeModel(null);
      final phone = FakeModel(talk);
      final a = Assistant(voice: voice, mac: mac, phone: phone)..know(const []);
      await a.clarify(turns, language: 'it');
      expect(a.knowsProject, isFalse);
      expect(voice.clarifyMemory.single, '');
      expect(mac.prompts.single.first.$2, intakeInstructions('it'));
      expect(phone.prompts.single.first.$2, intakeInstructions('it'));
      expect(intakeInstructions('it'), isNot(contains('functional memory')));
    });

    test('the full memory fits the peer link in UTF-8 bytes', () {
      // Two files of CJK text (3 bytes each) under the budget in characters.
      final half = '漢' * (memoryBudget ~/ 3);
      final (text, cut) = memoryDigest([
        (path: 'README.md', text: '漢字'),
        (path: 'a.md', text: half),
        (path: 'b.md', text: half),
      ]);
      expect(utf8.encode(text).length, lessThanOrEqualTo(memoryBudget));
      expect(cut, isTrue);
      final system = intakeInstructions('it', memory: text, shortened: cut);
      expect(utf8.encode(system).length, lessThan(64 * 1024));
    });

    test('the excerpt never splits an emoji', () {
      for (var pad = 0; pad < 4; pad++) {
        final readme = '${'a' * (memoryExcerptBudget - 20 + pad)}${'😀' * 40}';
        final excerpt = memoryExcerpt([(path: 'README.md', text: readme)]);
        expect(() => utf8.encode(excerpt.replaceAll('…', '')), returnsNormally);
        final units = excerpt.codeUnits;
        for (var i = 0; i < units.length; i++) {
          final high = units[i] >= 0xD800 && units[i] <= 0xDBFF;
          if (high) {
            expect(i + 1 < units.length, isTrue, reason: 'pad $pad');
            expect(units[i + 1], inInclusiveRange(0xDC00, 0xDFFF));
          }
        }
      }
    });

    test('the check starts from the memory only when there is one', () {
      expect(
        validationPrompt(turns, 'it', memory: true),
        contains('.functionalMemory/'),
      );
      expect(
        validationPrompt(turns, 'it'),
        isNot(contains('functionalMemory')),
      );
    });
  });

  group('Assistant', () {
    const turns = [(true, 'Sistema il login')];

    test('a called-off answer stops its model and asks no later one', () async {
      final mac = FakeModel(
        '{"next":"talk","say":"Dal Mac."}',
        delay: const Duration(milliseconds: 200),
      );
      final phone = FakeModel('{"next":"talk","say":"Dal telefono."}');
      final a = Assistant(voice: FakeVoice(), mac: mac, phone: phone);
      final cancel = Cancellation();
      final answer = a.clarify(turns, language: 'it', cancel: cancel);
      await Future<void>.delayed(const Duration(milliseconds: 20));
      cancel.cancel();
      expect(await answer, isNull);
      expect(mac.cancels.single, same(cancel), reason: 'the Mac is told');
      expect(phone.asked, 0, reason: 'no later model is asked');

      // Already called off: nothing is asked at all.
      final voice = FakeVoice()..intake = (_) => const Intake(say: 'Certo.');
      final b = Assistant(voice: voice, mac: mac, phone: phone);
      expect(await b.clarify(turns, cancel: Cancellation()..cancel()), isNull);
      expect(voice.clarified, isEmpty);
      expect(mac.asked, 1);
    });

    test('the phone model is given the call-off', () async {
      final cancel = Cancellation();
      Cancellation? given;
      final model = PhoneModel(_Recording((c) => given = c));
      await model.complete(
        [('user', 'Ciao')],
        maxTokens: 10,
        temperature: 0,
        deadline: const Duration(seconds: 1),
        cancel: cancel,
      );
      expect(given, same(cancel));
    });

    test("the device's own model goes first", () async {
      final voice = FakeVoice()..intake = (_) => const Intake(say: 'Certo.');
      final mac = FakeModel('{"next":"talk","say":"Dal Mac."}');
      final a = Assistant(voice: voice, mac: mac);
      expect((await a.clarify(turns, language: 'it'))?.say, 'Certo.');
      expect(mac.asked, 0);
    });

    test('a healthy relay: the Mac answers in the same format', () async {
      final mac = FakeModel('{"next":"talk","say":"Quale pagina?"}');
      final phone = FakeModel('{"next":"talk","say":"Dal telefono."}');
      final a = Assistant(voice: FakeVoice(), mac: mac, phone: phone);
      expect((await a.clarify(turns, language: 'it'))?.say, 'Quale pagina?');
      expect(phone.asked, 0);
      expect(mac.prompts.single.first.$2, contains('in Italian'));
      expect(mac.prompts.single.last.$2, contains('Person: Sistema il login'));

      mac.answer = 'Il login è sistemato. Vuoi altro?';
      expect(
        await a.condense('# Done\nlong…', language: 'it'),
        'Il login è sistemato. Vuoi altro?',
      );
    });

    test(
      'a slow relay falls back to the phone, then is skipped at once',
      () async {
        final link = FakeLink(
          () => Future<dynamic>.delayed(
            const Duration(seconds: 5),
            () => {'text': 'late'},
          ),
        );
        final mac = RemoteModel(link.op, 'Mac', health: RemoteHealth());
        final phone = FakeModel('{"next":"check"}');
        final a = Assistant(voice: FakeVoice(), mac: mac, phone: phone);
        final watch = Stopwatch()..start();
        expect((await a.clarify(turns))?.check, isTrue);
        expect(
          watch.elapsed,
          lessThan(const Duration(milliseconds: 3200)),
          reason: 'the deadline',
        );
        expect(phone.asked, 1);

        watch.reset();
        expect((await a.clarify(turns))?.check, isTrue);
        expect(
          watch.elapsed,
          lessThan(const Duration(milliseconds: 200)),
          reason: 'skipped, no wait',
        );
        expect(link.asked, hasLength(1));
      },
      timeout: const Timeout(Duration(seconds: 20)),
    );

    test('a relay down falls back to the phone', () async {
      final link = FakeLink(() => throw Exception('Host disconnected'));
      final phone = FakeModel('Il telefono risponde.');
      final a = Assistant(
        voice: FakeVoice(),
        mac: RemoteModel(link.op, 'Mac', health: RemoteHealth()),
        phone: phone,
      );
      expect((await a.clarify(turns))?.say, 'Il telefono risponde.');
      expect(await a.condense('long'), 'Il telefono risponde.');
      expect(link.asked, hasLength(1), reason: 'skipped the second time');
    });

    test(
      'the phone model: the Mac down, the phone answers, given longer',
      () async {
        final voice = FakeVoice()
          ..local = (messages) => '{"next":"talk","say":"Dal telefono."}';
        final link = FakeLink(() => throw Exception('Host disconnected'));
        final a = Assistant(
          voice: voice,
          mac: RemoteModel(link.op, 'Mac', health: RemoteHealth()),
          phone: PhoneModel(voice),
        );
        expect((await a.clarify(turns, language: 'it'))?.say, 'Dal telefono.');
        expect(voice.localAsked.single.$2, Assistant.phoneClarifyDeadline);
        expect(link.asked.single['timeoutMs'], 2500);

        voice.local = (_) => 'Fatto. Tutti i test passano.';
        expect(await a.condense('x' * 20000), 'Fatto. Tutti i test passano.');
        final (messages, timeout) = voice.localAsked.last;
        expect(timeout, Assistant.phoneCondenseDeadline);
        expect(
          messages.last.$2.length,
          PhoneModel.maxChars + 1,
          reason: 'cut to fit',
        );
      },
    );

    test('the phone model off or not downloaded gives nothing', () async {
      final voice = FakeVoice();
      expect(
        await PhoneModel(voice).complete(
          [('user', 'Ciao')],
          maxTokens: 10,
          temperature: 0,
          deadline: const Duration(seconds: 1),
        ),
        isNull,
      );
    });

    test('nothing available: none, and the call does without', () async {
      final a = Assistant(
        voice: FakeVoice(),
        mac: FakeModel(null),
        phone: FakeModel(null),
      );
      expect(await a.clarify(turns), isNull);
      expect(await a.condense('long'), isNull);
      expect(await Assistant(voice: FakeVoice()).clarify(turns), isNull);
    });
  });
}

/// A voice that reports the call-off each on-phone request is given.
class _Recording extends FakeVoice {
  _Recording(this.seen);
  final void Function(Cancellation?) seen;
  @override
  Future<String?> localComplete(
    List<(String, String)> messages, {
    int maxTokens = 120,
    double temperature = 0.3,
    Duration timeout = const Duration(seconds: 10),
    Cancellation? cancel,
  }) async {
    seen(cancel);
    return null;
  }
}
