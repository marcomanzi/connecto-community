import 'dart:convert';

import 'package:connecto_flutter_app/turn_judge.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  late List<Map<String, Object?>> asked;
  Object? answer;
  TurnJudge judge() {
    asked = [];
    return TurnJudge((op, fields) async {
      asked.add({'op': op, ...fields});
      return answer;
    }, 'Mac');
  }

  test('a transcript is sent whole, up to the limit, and never cut', () async {
    answer = {'complete': 0.07};
    final j = judge();
    final most = 'a' * (TurnJudge.maxBytes - 4);
    expect(await j.complete('$most fine'), isNull, reason: 'one byte over');
    expect(asked, isEmpty);
    expect(await j.complete('${'a' * (TurnJudge.maxBytes - 5)} fine'), 0.07);
    expect(asked.single['text'], endsWith(' fine'));
    expect(asked.single['text'], hasLength(TurnJudge.maxBytes));

    // A key's value right at where a cut would fall is not sent at all.
    asked.clear();
    final secret =
        '${'a' * (TurnJudge.maxBytes - 20)} api_key=' + 'sk-live-0123456';
    expect(await j.complete(secret), isNull);
    expect(asked, isEmpty);
    // Bytes, not characters: 2049 "é" are 4098 bytes.
    expect(await j.complete('é' * 2049), isNull);
    expect(asked, isEmpty);
  });

  test('a host that cannot judge is not asked again; a failure is', () async {
    answer = {'error': 'no key', 'unavailable': true};
    final j = judge();
    expect(await j.complete('ciao'), isNull);
    expect(j.available, isFalse);
    expect(await j.complete('ciao'), isNull);
    expect(asked, hasLength(1));

    answer = {'error': 'too slow', 'unavailable': false};
    final k = judge();
    await k.complete('ciao');
    await k.complete('ciao');
    expect(asked, hasLength(2));
    expect(k.available, isTrue);
  });

  test('anything but a probability is no answer', () async {
    for (final bad in [-0.2, 1.5, double.nan]) {
      answer = {'complete': bad};
      expect(await judge().complete('ciao'), isNull, reason: '$bad');
    }
    answer = {'complete': 1};
    expect(await judge().complete('ciao'), 1.0);
  });

  test('the context is whole pieces: the head, then the latest lines', () {
    const max = TurnJudge.maxBytes;
    final line = 'x' * 1400;
    final context = TurnJudge.contextOf(
      ['Project: p'],
      ['old one', line, line, line, 'Person: latest'],
    )!;
    expect(context, startsWith('Project: p\n'));
    expect(context, endsWith('Person: latest'));
    expect(context, isNot(contains('old one')), reason: 'the oldest goes');
    expect(utf8.encode(context).length, lessThanOrEqualTo(max));
    // A line too long ends what is taken: nothing older, nothing cut.
    final secret = 'api_key=sk-live-${'a' * max}';
    expect(
      TurnJudge.contextOf(['Project: p'], ['older', secret, 'Person: hi']),
      'Project: p\nPerson: hi',
    );
    expect(TurnJudge.contextOf(['h' * (max + 1)], const []), isNull);
  });

  test('the context goes with the words; the fit comes back', () async {
    answer = {'complete': 0.9, 'fits': 0.05};
    final j = judge();
    expect(await j.judge('honey', context: 'Project: p'), (
      complete: 0.9,
      fits: 0.05,
    ));
    expect(asked.single['context'], 'Project: p');
    for (final bad in [-1, 2, 'x']) {
      answer = {'complete': 0.9, 'fits': bad};
      expect(await judge().judge('a', context: 'c'), isNull, reason: '$bad');
    }
    answer = {'complete': 0.9, 'fits': null};
    expect(await judge().judge('a'), (complete: 0.9, fits: null));
    // A context too long is never cut: nothing is asked.
    final j2 = judge();
    expect(
      await j2.judge('a', context: 'x' * (TurnJudge.maxBytes + 1)),
      isNull,
    );
    expect(asked, isEmpty);
  });
}
