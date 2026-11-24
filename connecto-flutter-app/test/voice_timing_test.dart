import 'dart:math';

import 'package:connecto_flutter_app/call.dart';
import 'package:connecto_voice/connecto_voice.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'call_test.dart' show FakeHost, FakeVoice, target, turnGap;

void main() {
  group('speechPlan', () {
    // 140 characters: at most 17.5 s of speech.
    final long = 'x' * 140;

    test('a voice comfortably faster than real time holds back two frames', () {
      expect(speechPlan(speed: 1.5, text: long), [SpeechPart(long, 0.16)]);
      expect(holdSeconds(speed: 1.12, text: long), 0.16);
    });

    test('a little faster than real time holds back a share of it', () {
      // 28 characters, at most 3.5 s, at 0.9 effective: 3.5 * 0.1 * 1.1.
      expect(holdSeconds(speed: 1.0, text: 'x' * 28), closeTo(0.385, 1e-9));
      expect(
        holdSeconds(speed: 1.05, text: 'x' * 70),
        greaterThan(holdSeconds(speed: 1.05, text: 'x' * 28)),
      );
    });

    test('below real time, or unmeasured, every sentence is made whole', () {
      expect(holdSeconds(speed: 0.99, text: 'x' * 28), double.infinity);
      expect(holdSeconds(speed: 0, text: long), double.infinity);
      const reply =
          'Ho trovato il problema nel login. Adesso funziona tutto quanto.';
      final plan = speechPlan(speed: 0.8, text: reply);
      expect(plan, [
        const SpeechPart('Ho trovato il problema nel login.', double.infinity),
        const SpeechPart('Adesso funziona tutto quanto.', double.infinity),
      ]);
    });

    test('past the cap it goes sentence by sentence, never mid-sentence', () {
      const reply =
          'Ho controllato il progetto e ho trovato il problema. '
          'Il bottone di accesso con Google non aveva un gestore, quindi non '
          'succedeva niente quando lo premevi e la pagina restava ferma. '
          'Ok. Adesso funziona.';
      final plan = speechPlan(speed: 1.02, text: reply);
      expect(plan.length, greaterThan(1));
      expect(plan.map((p) => p.text).join(' '), reply, reason: 'all of it');
      for (final part in plan) {
        final need = holdSeconds(speed: 1.02, text: part.text);
        expect(part.hold, need <= maxPrebufferSeconds ? need : double.infinity);
      }
      // "Ok." is too short to stand alone.
      expect(plan.any((p) => p.text == 'Ok.'), isFalse);
    });

    test('playback never runs dry inside a part, whatever its real length', () {
      // Each part is synthesized from when it starts, at speeds as slow as
      // 90% of the measured one; it plays once its hold is made, or once it
      // is all made. Its real length is anything up to the longest the plan
      // assumes, not the character estimate.
      for (final speed in [0.6, 0.8, 0.95, 1.0, 1.05, 1.2, 1.5]) {
        for (final chars in [20, 60, 120, 400]) {
          final sentence = '${'x' * (chars - 1)}.';
          final text = List.filled(3, sentence).join(' ');
          for (final part in speechPlan(speed: speed, text: text)) {
            final longest = part.text.length / slowestCharsPerSecond;
            for (final share in [0.2, 0.5, 0.8, 1.0]) {
              final audio = longest * share;
              final effective = speed * 0.9;
              final held = part.hold.isInfinite ? audio : min(part.hold, audio);
              final start = held / effective;
              for (var t = start; t <= start + audio; t += 0.01) {
                final made = min(t * effective, audio);
                final played = min(t - start, audio);
                expect(
                  made + 1e-9,
                  greaterThanOrEqualTo(played),
                  reason: 'speed $speed, $chars chars, ${audio}s, t $t',
                );
              }
            }
          }
        }
      }
    });
  });

  group('CallTimings', () {
    test('turn sent, reply ready and first sound, then the voice', () {
      var now = DateTime(2026, 11, 22, 10);
      final t = CallTimings(now: () => now);
      expect(t.lines, isEmpty);
      t.turnSent();
      now = now.add(const Duration(milliseconds: 2400));
      t.replyReady();
      now = now.add(const Duration(milliseconds: 300));
      t.replyReady(); // a later phrase does not count
      t.firstSound();
      expect(t.toReply, const Duration(milliseconds: 2400));
      expect(t.toSound, const Duration(milliseconds: 300));
      t.measured(
        const VoiceTiming(
          speed: 1.37,
          firstAudio: Duration(milliseconds: 420),
          engine: 'ane-fp16',
          voice: 'giovanni',
        ),
      );
      expect(t.lines, [
        'Turn → reply 2.40 s → first sound 0.30 s',
        'Voice giovanni · ane-fp16 · 1.37× real time · first audio 0.42 s',
      ]);
      // A new turn starts over.
      t.turnSent();
      expect(t.toReply, isNull);
    });
  });

  group('the call keeps its voice', () {
    Future<CallController> begin(
      WidgetTester tester,
      FakeHost host,
      FakeVoice voice,
    ) async {
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      )..started = true;
      await call.start();
      await tester.pump();
      return call;
    }

    testWidgets('a voice ready mid-call waits for the next call', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()
        ..languageList = const VoiceLanguages(
          [],
          current: 'it-IT',
          voices: ['giovanni'],
          voice: 'giovanni',
          speed: 1.4,
        );
      final call = await begin(tester, host, voice);
      expect(voice.spokenWith.single, (false, null), reason: 'greeting');

      // The natural voice finishes loading during the call.
      voice.naturalLoaded = true;
      voice.say('Run the tests', isFinal: true);
      await tester.pump(turnGap);
      host.reply('All tests pass.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'All tests pass.');
      expect(voice.spokenWith.last.$1, isFalse, reason: 'same voice');
      expect(voice.naturalAsked, 1, reason: 'decided once');
      call.dispose();
      await tester.pump();

      // The next call begins with it.
      final next = await begin(tester, host, voice);
      final (natural, plan) = voice.spokenWith.last;
      expect(natural, isTrue);
      expect(
        plan,
        speechPlan(speed: 1.4, text: voice.spoken.last),
        reason: 'planned by the measured speed',
      );
      next.dispose();
      await tester.pump();
    });

    testWidgets('a slower utterance makes the next hold back more', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..naturalLoaded = true;
      final call = await begin(tester, host, voice);
      voice.heard.add(
        const VoiceTiming(speed: 0.8, engine: 'gpu-fp16', voice: 'alba'),
      );
      await tester.pump();
      voice.say('Run the tests', isFinal: true);
      await tester.pump(turnGap);
      final long = List.filled(6, 'The build is green and ready.').join(' ');
      host.reply(long);
      await tester.pump(const Duration(milliseconds: 300));
      final plan = voice.spokenWith.last.$2!;
      expect(plan, speechPlan(speed: 0.8, text: voice.spoken.last));
      expect(plan.length, greaterThan(1), reason: 'sentence by sentence');
      call.dispose();
      await tester.pump();
    });

    testWidgets('the call screen shows the timings when asked to', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()
        ..languageList = const VoiceLanguages(
          [],
          current: 'en-US',
          showTimings: true,
        );
      // Off by default: nothing shown.
      expect(const VoiceLanguages([], current: 'en').showTimings, isFalse);
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
      voice.say('Fix the login page', isFinal: true);
      await tester.pump(turnGap);
      voice.heard.add(const FirstSound());
      voice.heard.add(
        const VoiceTiming(speed: 1.25, engine: 'gpu-int8', voice: 'alba'),
      );
      await tester.pump();
      await tester.pump();
      expect(find.byKey(const Key('voice-timings')), findsOneWidget);
      expect(find.textContaining('Turn → reply'), findsOneWidget);
      expect(find.textContaining('1.25× real time'), findsOneWidget);
      await tester.pumpWidget(const SizedBox());
      await tester.pump();
    });
  });
}
