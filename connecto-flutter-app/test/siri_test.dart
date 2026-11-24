import 'dart:async';

import 'package:connecto_flutter_app/peer.dart';
import 'package:connecto_flutter_app/siri.dart';
import 'package:flutter/services.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  const channel = MethodChannel('connecto/siri');
  late List<MethodCall> calls;
  late List<Map<String, Object?>> stored;

  setUp(() {
    SiriLink.debugSupported = true;
    calls = [];
    // What the phone kept after the app restarted: a question whose
    // Accepted was lost on the way back.
    stored = [
      {
        'requestId': 'first',
        'host': 'Mac',
        'project': 'p1',
        'name': 'connecto',
        'text': 'do the tests pass?',
        'unconfirmed': true,
      },
    ];
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(channel, (call) async {
          calls.add(call);
          switch (call.method) {
            case 'openRequests':
              return stored;
            case 'forgetRequest':
              stored.removeWhere(
                (r) => r['requestId'] == (call.arguments as Map)['requestId'],
              );
            case 'reconcileRequest':
              for (final r in stored) {
                if (r['requestId'] == (call.arguments as Map)['requestId']) {
                  r['unconfirmed'] = false;
                  r['hostState'] = (call.arguments as Map)['hostState'];
                }
              }
          }
          return null;
        });
  });

  tearDown(() {
    SiriLink.debugSupported = null;
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(channel, null);
  });

  const connected = {
    'hosts': [
      {'name': 'Mac', 'link': 'Connected'},
    ],
  };

  for (final state in ['pending', 'uncertain', 'answered']) {
    test(
      'a question the host has ($state) keeps its route to the reply',
      () async {
        final lost = (await checkUnconfirmed(
          connected,
          (op, args) async => {'state': state},
        )).lost;
        expect(lost, isEmpty);
        expect(calls.map((c) => c.method), isNot(contains('forgetRequest')));
        expect(stored.single['hostState'], state);
        expect(stored.single['host'], 'Mac');
        expect(stored.single['project'], 'p1');
      },
    );
  }

  test('a question the host never got is offered again and let go', () async {
    final lost = (await checkUnconfirmed(
      connected,
      (op, args) async => {'state': 'unknown'},
    )).lost;
    expect(lost.single['requestId'], 'first');
    expect(stored, isEmpty);
  });

  test('a Siri ask still waiting on its answer is not reconciled', () async {
    stored.single['active'] = true;
    var asked = 0;
    final check = await checkUnconfirmed(connected, (op, args) async {
      asked++;
      return {'state': 'unknown'};
    });
    expect(check.lost, isEmpty);
    expect(check.waiting, {'Mac'});
    expect(asked, 0);
    expect(stored.single['requestId'], 'first');
  });

  test('a host that cannot look ids up gets no resend offer', () async {
    // The first attempt's answer was lost; after a reconnect the host is
    // one that does not know Siri: that says nothing of the first attempt.
    final lost = (await checkUnconfirmed(
      connected,
      (op, args) async => {'state': 'unsupported'},
    )).lost;
    expect(lost, isEmpty);
    expect(calls.map((c) => c.method), isNot(contains('forgetRequest')));
    expect(stored.single['requestId'], 'first');
    expect(stored.single['unconfirmed'], true);
  });

  test('a question still being asked is checked once its ask is over, '
      'on the same connection', () async {
    stored.single['active'] = true;
    final siri = SiriReconciler(registerPush: () async {});
    var asked = 0;
    Future<dynamic> call(String op, Data args) async {
      asked++;
      return {'state': 'unknown'};
    }

    // First refresh: the intent is still asking.
    expect(await siri.onState(connected, call), isEmpty);
    expect(asked, 0);
    // Its mark expires (the intent ended unconfirmed, or the app restarted).
    stored.single['active'] = false;
    final lost = await siri.onState(connected, call);
    expect(asked, 1);
    expect(lost.single['requestId'], 'first');
    // Checked once that is done: no more asks on this connection.
    await siri.onState(connected, call);
    expect(asked, 1);
  });

  test('a question that becomes unconfirmed on a connection already checked '
      'is still looked up', () async {
    final first = stored.removeAt(0);
    final siri = SiriReconciler(registerPush: () async {});
    var asked = 0;
    Future<dynamic> call(String op, Data args) async {
      asked++;
      return {'state': 'unknown'};
    }

    // Nothing to check at the first refresh of this connection.
    expect(await siri.onState(connected, call), isEmpty);
    expect(asked, 0);
    // A Siri ask then loses its acknowledgement, on the same connection.
    stored.add(first);
    final lost = await siri.onState(connected, call);
    expect(asked, 1);
    expect(lost.single['requestId'], 'first');
  });

  test('a second host connecting during a lookup neither asks nor offers it '
      'twice', () async {
    final siri = SiriReconciler(registerPush: () async {});
    final answer = Completer<dynamic>();
    var asked = 0;
    Future<dynamic> call(String op, Data args) {
      asked++;
      return answer.future;
    }

    // The Mac's lookup is outstanding...
    final firstScan = siri.onState(connected, call);
    await Future<void>.delayed(Duration.zero);
    expect(asked, 1);
    // ...when the Studio connects and a new scan starts.
    final both = {
      'hosts': [
        {'name': 'Mac', 'link': 'Connected'},
        {'name': 'Studio', 'link': 'Connected'},
      ],
    };
    expect(await siri.onState(both, call), isEmpty);
    expect(asked, 1);
    answer.complete({'state': 'unknown'});
    expect((await firstScan).single['requestId'], 'first');
    // Offered once: it is gone now.
    expect(await siri.onState(both, call), isEmpty);
    expect(asked, 1);
  });

  test(
    'without a verdict it is asked again only on a new connection',
    () async {
      final siri = SiriReconciler(registerPush: () async {});
      var asked = 0;
      Future<dynamic> call(String op, Data args) async {
        asked++;
        return {'state': 'unsupported'};
      }

      await siri.onState(connected, call);
      await siri.onState(connected, call);
      expect(asked, 1, reason: 'asked at every refresh');
      await siri.onState(const {'hosts': []}, call);
      await siri.onState(connected, call);
      expect(asked, 2, reason: 'not asked again after reconnecting');
      expect(stored.single['unconfirmed'], true);
    },
  );

  test('overlapping refreshes offer a lost question once', () async {
    // Two questions: A's lookup is slow, B's host never got it.
    stored.add({...stored.single, 'requestId': 'B', 'text': 'second'});
    final siri = SiriReconciler(registerPush: () async {});
    final slowA = Completer<dynamic>();
    final looked = <String>[];
    Future<dynamic> call(String op, Data args) {
      looked.add(args['requestId'] as String);
      return args['requestId'] == 'first'
          ? slowA.future
          : Future.value({'state': 'unknown'});
    }

    final firstScan = siri.onState(connected, call);
    await Future<void>.delayed(Duration.zero);
    // A second refresh while A is still being looked up runs to its end
    // (B found lost) before A's answer comes.
    final second = await siri.onState(connected, call);
    slowA.complete({'state': 'pending'});
    final offers = [...await firstScan, ...second];
    expect(offers.map((r) => r['requestId']), ['B']);
    expect(looked.where((id) => id == 'B'), hasLength(1));
    // Later refreshes find nothing more to do.
    expect(await siri.onState(connected, call), isEmpty);
    expect(looked.where((id) => id == 'B'), hasLength(1));
  });

  test('a lookup that ends without a verdict after a reconnect does not stand '
      'for the new connection', () async {
    final siri = SiriReconciler(registerPush: () async {});
    final old = Completer<dynamic>();
    var asked = 0;
    Future<dynamic> call(String op, Data args) {
      asked++;
      return asked == 1 ? old.future : Future.value({'state': 'unknown'});
    }

    final firstScan = siri.onState(connected, call);
    await Future<void>.delayed(Duration.zero);
    expect(asked, 1);
    // The host goes and comes back while that lookup is outstanding...
    await siri.onState(const {'hosts': []}, call);
    await siri.onState(connected, call);
    // ...then the old lookup ends with no verdict.
    old.complete({'state': 'unsupported'});
    final lost = [...await firstScan, ...await siri.onState(connected, call)];
    expect(asked, 2, reason: 'not asked again on the new connection');
    expect(lost.single['requestId'], 'first');
  });

  test('a host that does not answer leaves it to check later', () async {
    await checkUnconfirmed(connected, (op, args) async => throw 'offline');
    expect(stored.single['unconfirmed'], true);
  });

  testWidgets('a tap shows the first request\'s reply after later turns', (
    tester,
  ) async {
    final asked = <Data>[];
    await tester.pumpWidget(
      MaterialApp(
        home: SiriReplyPage(
          open: const {
            'kind': 'reply',
            'requestId': 'first',
            'host': 'Mac',
            'project': 'p1',
            'name': 'connecto',
          },
          // The host keeps each request's own reply; the chat has moved on.
          call: (op, args) async {
            asked.add({'op': op, ...args});
            return args['requestId'] == 'first'
                ? {'state': 'answered', 'reply': 'Yes, all 296 pass.'}
                : {'state': 'answered', 'reply': 'a later turn'};
          },
          openChat: (_, _, _) {},
        ),
      ),
    );
    await tester.pump();
    expect(find.text('Yes, all 296 pass.'), findsOneWidget);
    expect(asked.first['op'], 'siriResult');
    expect(asked.first['requestId'], 'first');
    expect(asked.first['host'], 'Mac');
    await tester.pumpWidget(const SizedBox());
  });
}
