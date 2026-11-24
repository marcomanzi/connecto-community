import 'dart:io';

import 'package:connecto_flutter_app/composer.dart';
import 'package:connecto_flutter_app/main.dart';
import 'package:connecto_flutter_app/peer.dart';
import 'package:connecto_flutter_app/theme.dart';
import 'package:connecto_flutter_app/widgets.dart';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('opens the project chooser', (tester) async {
    await tester.pumpWidget(const ConnectoApp());
    expect(find.text('Projects'), findsOneWidget);
    expect(find.byTooltip('Settings'), findsOneWidget);
  });

  testWidgets('a long chat opens at its last message and follows new ones', (
    tester,
  ) async {
    var messages = [
      for (var i = 0; i < 80; i++) [i.isEven, 'Message $i'],
    ];
    await tester.pumpWidget(
      MaterialApp(
        home: ChatPage(
          host: 'Host',
          project: 'project',
          title: 'Project',
          call: (op, fields) async => op == 'chat'
              ? <String, dynamic>{
                  'id': '1',
                  'status': 'Ready',
                  'chatReady': true,
                  'messages': messages,
                }
              : null,
        ),
      ),
    );
    await tester.pump();
    await tester.pump();
    expect(find.text('Message 79'), findsOneWidget);
    expect(find.text('Message 0'), findsNothing);

    messages = [
      ...messages,
      [false, 'Message 80'],
    ];
    await tester.pump(const Duration(milliseconds: 800));
    await tester.pump();
    expect(find.text('Message 80'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('waits for history and recovers from unanswered chat requests', (
    tester,
  ) async {
    const guidance =
        'The chat connection keeps closing before history arrives. '
        'If the host is running an older build, update Connecto on the host '
        'to a build that supports project chat.';
    var response = <String, dynamic>{
      'id': '1',
      'status': 'Ready',
      'chatReady': false,
      'messages': null,
      'question': 'Hidden terminal question',
    };
    final sent = <Data>[];
    await tester.pumpWidget(
      MaterialApp(
        home: ChatPage(
          host: 'Host',
          project: 'project',
          title: 'Project',
          call: (op, fields) async {
            if (op == 'send') sent.add(fields);
            return response;
          },
        ),
      ),
    );
    await tester.pump();

    final composer = find.byType(TextField);
    final send = find.byWidgetPredicate(
      (widget) => widget is IconButton && widget.tooltip == 'Send message',
    );
    expect(
      find.widgetWithText(StatusPill, 'Connecting to chat…'),
      findsOneWidget,
    );
    expect(find.text('Ready'), findsNothing);
    expect(find.text('Hidden terminal question'), findsNothing);
    expect(find.text('Confirm'), findsNothing);
    expect(tester.widget<TextField>(composer).enabled, isFalse);
    expect(tester.widget<IconButton>(send).onPressed, isNull);

    response = {...response, 'error': guidance};
    await tester.pump(const Duration(milliseconds: 750));
    await tester.pump();
    expect(find.text('Chat unavailable'), findsOneWidget);
    expect(find.text(guidance), findsOneWidget);
    expect(tester.widget<IconButton>(send).onPressed, isNull);

    response = {
      ...response,
      'chatReady': true,
      'messages': [],
      'question': null,
      'error': null,
    };
    await tester.pump(const Duration(milliseconds: 750));
    await tester.pump();
    expect(find.text(guidance), findsNothing);
    expect(find.text('Ready'), findsOneWidget);
    expect(tester.widget<TextField>(composer).enabled, isTrue);
    await tester.enterText(composer, 'Hello');
    await tester.pump();
    await tester.tap(send);
    await tester.pump();
    expect(sent.single['text'], 'Hello');

    response = {
      ...response,
      'chatReady': false,
      'messages': [
        [true, 'Hello'],
      ],
      'question': 'Stale question',
    };
    await tester.pump(const Duration(milliseconds: 750));
    await tester.pump();
    expect(
      find.widgetWithText(StatusPill, 'Connecting to chat…'),
      findsOneWidget,
    );
    expect(find.text('Hello'), findsOneWidget);
    expect(find.text('Stale question'), findsNothing);
    expect(tester.widget<IconButton>(send).onPressed, isNull);
    expect(tester.widget<TextField>(composer).enabled, isFalse);
    await tester.pumpWidget(const SizedBox.shrink());
  });

  testWidgets('the composer explains why it is disabled', (tester) async {
    var response = <String, dynamic>{
      'id': '1',
      'status': 'Running',
      'chatReady': true,
      'messages': [],
    };
    await tester.pumpWidget(
      MaterialApp(
        theme: connectoTheme(),
        home: ChatPage(
          host: 'Host',
          project: 'project',
          title: 'Project',
          call: (_, _) async => response,
        ),
      ),
    );
    await tester.pump();
    final composer = find.byType(TextField);
    void expectDisabled(String hint) {
      final field = tester.widget<TextField>(composer);
      expect(field.enabled, isFalse);
      expect(field.decoration?.hintText, hint);
    }

    expectDisabled('Agent is working…');
    response = {...response, 'status': 'Stopped'};
    await tester.pump(const Duration(milliseconds: 750));
    await tester.pump();
    expectDisabled('Start the session to chat');
    response = {...response, 'status': 'Failed'};
    await tester.pump(const Duration(milliseconds: 750));
    await tester.pump();
    expectDisabled('Start the session to chat');
    response = {
      ...response,
      'status': 'Needs an answer',
      'question': 'Choose one',
      'textAnswer': false,
    };
    await tester.pump(const Duration(milliseconds: 750));
    await tester.pump();
    expectDisabled('Choose an option above');
    expect(find.text('Confirm'), findsOneWidget);
    expect(
      tester.getBottomLeft(find.byType(Composer)).dy,
      closeTo(
        tester.view.physicalSize.height / tester.view.devicePixelRatio - 12,
        1,
      ),
    );
    response = {...response, 'chatReady': false};
    await tester.pump(const Duration(milliseconds: 750));
    await tester.pump();
    expectDisabled('Connecting to chat…');
    expect(find.text('Confirm'), findsNothing);
    await tester.pumpWidget(const SizedBox.shrink());
  });

  testWidgets('desktop Enter sends and Shift+Enter inserts a newline', (
    tester,
  ) async {
    final sent = <Data>[];
    await tester.pumpWidget(
      MaterialApp(
        theme: connectoTheme().copyWith(platform: TargetPlatform.macOS),
        home: ChatPage(
          host: 'Host',
          project: 'project',
          title: 'Project',
          call: (op, fields) async {
            if (op == 'send') sent.add(fields);
            return {
              'id': '1',
              'status': 'Ready',
              'chatReady': true,
              'messages': [],
            };
          },
        ),
      ),
    );
    await tester.pump();
    final composer = find.byType(TextField);
    final send = find.byWidgetPredicate(
      (widget) => widget is IconButton && widget.tooltip == 'Send message',
    );
    expect(tester.widget<IconButton>(send).onPressed, isNull);
    await tester.enterText(composer, ' \t ');
    await tester.pump();
    expect(tester.widget<IconButton>(send).onPressed, isNull);
    await tester.tap(send);
    await tester.showKeyboard(composer);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.pump();
    expect(sent, isEmpty);

    await tester.enterText(composer, 'First line');
    await tester.sendKeyDownEvent(LogicalKeyboardKey.shiftLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.shiftLeft);
    await tester.pump();
    expect(sent, isEmpty);
    expect(tester.widget<IconButton>(send).onPressed, isNotNull);
    expect(tester.widget<TextField>(composer).controller!.text, 'First line\n');

    await tester.enterText(composer, 'First line\nSecond line');
    await tester.sendKeyEvent(LogicalKeyboardKey.enter);
    await tester.pump();
    expect(sent.single, {
      'host': 'Host',
      'project': 'project',
      'id': '1',
      'text': 'First line\nSecond line',
      'question': null,
    });
    await tester.pumpWidget(const SizedBox.shrink());
  });

  testWidgets('a disconnected chat shows its offline status and error', (
    tester,
  ) async {
    final semantics = tester.ensureSemantics();
    await tester.pumpWidget(
      MaterialApp(
        theme: connectoTheme(),
        home: ChatPage(
          host: 'Host',
          project: 'project',
          title: 'Project',
          call: (_, _) async => throw const PeerError('Host disconnected'),
        ),
      ),
    );
    await tester.pump();
    expect(find.bySemanticsLabel('Session status: Offline'), findsOneWidget);
    expect(find.text("Can't reach the host"), findsOneWidget);
    expect(find.text('Host disconnected'), findsOneWidget);
    final composer = tester.widget<TextField>(find.byType(TextField));
    expect(composer.decoration?.hintText, 'Offline');
    expect(composer.enabled, isFalse);
    await tester.pumpWidget(const SizedBox.shrink());
    semantics.dispose();
  });

  group('attachments', () {
    late Directory dir;
    setUp(() => dir = Directory.systemTemp.createTempSync('composer-test'));
    tearDown(() => dir.deleteSync(recursive: true));

    String file(String name, [int bytes = 2048]) {
      final f = File('${dir.path}/$name')
        ..writeAsBytesSync(List.filled(bytes, 1));
      return f.path;
    }

    final ready = {
      'id': '1',
      'status': 'Ready',
      'chatReady': true,
      'messages': [],
      'attach': {'maxBytes': 10 * 1024 * 1024, 'maxFiles': 4},
    };
    Finder button(String tooltip) =>
        find.byWidgetPredicate((w) => w is IconButton && w.tooltip == tooltip);

    Future<List<(String, Data)>> open(
      WidgetTester tester, {
      required List<String> Function() picked,
      Data chat = const {},
      Future<dynamic> Function(String, Data)? override,
    }) async {
      final calls = <(String, Data)>[];
      await tester.pumpWidget(
        MaterialApp(
          theme: connectoTheme().copyWith(platform: TargetPlatform.macOS),
          home: ChatPage(
            host: 'Host',
            project: 'project',
            title: 'Project',
            pickFiles: () async => picked(),
            call: (op, fields) async {
              if (op != 'chat') calls.add((op, fields));
              if (override != null) {
                final result = await override(op, fields);
                if (result != null) return result;
              }
              return switch (op) {
                'chat' => {...ready, ...chat},
                'attach' => {
                  'token': 't-${fields['path'].toString().split('/').last}',
                  'name': fields['path'].toString().split('/').last,
                  'bytes': 2048,
                },
                _ => null,
              };
            },
          ),
        ),
      );
      await tester.pump();
      return calls;
    }

    testWidgets('picked files are staged, shown and sent without text', (
      tester,
    ) async {
      final paths = [file('notes.md'), file('screen.png')];
      final calls = await open(tester, picked: () => paths);
      expect(
        tester.widget<IconButton>(button('Send message')).onPressed,
        isNull,
      );
      await tester.tap(button('Attach files'));
      await tester.pump();
      await tester.pump();
      expect(find.text('notes.md'), findsOneWidget);
      expect(find.text('screen.png'), findsOneWidget);
      expect(find.text('2 KiB'), findsNWidgets(2));
      expect([for (final c in calls) c.$1], ['attach', 'attach']);
      expect(calls.first.$2['path'], paths.first);
      expect(calls.first.$2['id'], '1');

      await tester.tap(button('Send message'));
      await tester.pump();
      await tester.pump();
      final send = calls.lastWhere((c) => c.$1 == 'send').$2;
      expect(send['text'], '');
      expect(send['attachments'], ['t-notes.md', 't-screen.png']);
      expect(find.text('notes.md'), findsNothing);
      await tester.pumpWidget(const SizedBox.shrink());
    });

    testWidgets('a failed send keeps the draft and the files', (tester) async {
      final calls = await open(
        tester,
        picked: () => [file('a.txt')],
        override: (op, _) async =>
            op == 'send' ? throw const PeerError('Upload interrupted.') : null,
      );
      await tester.tap(button('Attach files'));
      await tester.pump();
      await tester.pump();
      await tester.enterText(find.byType(TextField), 'Look at this');
      await tester.tap(button('Send message'));
      await tester.pump();
      await tester.pump();
      expect(calls.last.$1, 'send');
      expect(find.text('Upload interrupted.'), findsOneWidget);
      expect(find.text('a.txt'), findsOneWidget);
      expect(
        tester.widget<TextField>(find.byType(TextField)).controller!.text,
        'Look at this',
      );
      await tester.tap(button('Remove a.txt'));
      await tester.pump();
      expect(find.text('a.txt'), findsNothing);
      expect(calls.last.$1, 'detach');
      expect(calls.last.$2['token'], 't-a.txt');
      await tester.pumpWidget(const SizedBox.shrink());
    });

    testWidgets('a refused file shows why, blocks sending and can be retried', (
      tester,
    ) async {
      var refuse = true;
      await open(
        tester,
        picked: () => [file('big.bin')],
        override: (op, _) async => op == 'attach' && refuse
            ? throw const PeerError('big.bin is larger than the 10 MiB limit.')
            : null,
      );
      await tester.tap(button('Attach files'));
      await tester.pump();
      await tester.pump();
      expect(find.text('Failed'), findsOneWidget);
      expect(
        find.byTooltip('big.bin is larger than the 10 MiB limit.'),
        findsOneWidget,
      );
      await tester.enterText(find.byType(TextField), 'hi');
      await tester.pump();
      expect(
        tester.widget<IconButton>(button('Send message')).onPressed,
        isNull,
      );
      refuse = false;
      await tester.tap(button('Retry big.bin'));
      await tester.pump();
      await tester.pump();
      expect(find.text('Failed'), findsNothing);
      expect(
        tester.widget<IconButton>(button('Send message')).onPressed,
        isNotNull,
      );
      await tester.pumpWidget(const SizedBox.shrink());
    });

    testWidgets('folders, duplicates and files over the limit are refused', (
      tester,
    ) async {
      final calls = await open(
        tester,
        picked: () => const [],
        chat: {
          'attach': {'maxBytes': 1024 * 1024, 'maxFiles': 2},
        },
      );
      final composer = tester.state<ComposerState>(find.byType(Composer));
      final a = file('a.txt'), b = file('b.txt'), c = file('c.txt');
      Directory('${dir.path}/folder').createSync();
      composer.addPaths([a, a, '${dir.path}/folder', b, c]);
      await tester.pump();
      await tester.pump();
      expect([for (final c in calls) c.$2['path']], [a, b]);
      expect(
        find.text("Folders can't be attached. You can attach up to 2 files."),
        findsOneWidget,
      );
      await tester.pumpWidget(const SizedBox.shrink());
    });

    testWidgets('a host without file transfer explains why', (tester) async {
      final calls = await open(
        tester,
        picked: () => [file('a.txt')],
        chat: {'attach': null},
      );
      const reason =
          "This host can't receive files. Update Connecto on the host.";
      expect(button('Attach files'), findsNothing);
      expect(button(reason), findsNothing);
      tester.state<ComposerState>(find.byType(Composer)).addPaths([
        file('b.txt'),
      ]);
      await tester.pump();
      expect(find.text(reason), findsOneWidget);
      expect(calls, isEmpty);
      await tester.pumpWidget(const SizedBox.shrink());
    });

    testWidgets('answering a question sends only the text and keeps files', (
      tester,
    ) async {
      var chat = <String, dynamic>{};
      final calls = await open(
        tester,
        picked: () => [file('a.txt')],
        override: (op, _) async => op == 'chat' ? {...ready, ...chat} : null,
      );
      await tester.tap(button('Attach files'));
      await tester.pump();
      await tester.pump();
      chat = {
        'status': 'Needs an answer',
        'question': 'Which branch?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 750));
      await tester.pump();
      expect(button('Attach files'), findsNothing);
      expect(find.text('a.txt'), findsOneWidget);
      expect(
        tester.widget<IconButton>(button('Send message')).onPressed,
        isNull,
      );
      await tester.enterText(find.byType(TextField), 'main');
      await tester.sendKeyEvent(LogicalKeyboardKey.enter);
      await tester.pump();
      await tester.pump();
      final send = calls.lastWhere((c) => c.$1 == 'send').$2;
      expect(send['text'], 'main');
      expect(send['question'], 'Which branch?');
      expect(send.containsKey('attachments'), isFalse);
      expect(find.text('a.txt'), findsOneWidget);
      await tester.pumpWidget(const SizedBox.shrink());
    });

    testWidgets('a replaced session drops files staged for the old one', (
      tester,
    ) async {
      var id = '1';
      final calls = await open(
        tester,
        picked: () => [file('a.txt')],
        override: (op, _) async => op == 'chat' ? {...ready, 'id': id} : null,
      );
      await tester.tap(button('Attach files'));
      await tester.pump();
      await tester.pump();
      id = '2';
      await tester.pump(const Duration(milliseconds: 750));
      await tester.pump();
      expect(find.text('a.txt'), findsNothing);
      expect(
        find.text('The session changed. Attach the files again.'),
        findsOneWidget,
      );
      final detach = calls.lastWhere((c) => c.$1 == 'detach').$2;
      expect(detach['id'], '1');
      expect(detach['token'], 't-a.txt');
      await tester.pumpWidget(const SizedBox.shrink());
    });

    testWidgets('Cmd+U opens the picker', (tester) async {
      var opened = 0;
      await open(
        tester,
        picked: () {
          opened++;
          return const [];
        },
      );
      await tester.showKeyboard(find.byType(TextField));
      await tester.sendKeyDownEvent(LogicalKeyboardKey.metaLeft);
      await tester.sendKeyEvent(LogicalKeyboardKey.keyU);
      await tester.sendKeyUpEvent(LogicalKeyboardKey.metaLeft);
      await tester.pump();
      expect(opened, 1);
      await tester.pumpWidget(const SizedBox.shrink());
    });

    testWidgets('the single-line composer keeps its buttons inside', (
      tester,
    ) async {
      await open(tester, picked: () => const []);
      final box = tester.getRect(find.byType(Composer));
      for (final tooltip in ['Attach files', 'Send message']) {
        final r = tester.getRect(button(tooltip));
        expect(r.bottom, closeTo(box.bottom - 7, 0.5), reason: tooltip);
        expect(r.top - box.top, closeTo(box.bottom - r.bottom, 0.5));
      }
      expect(
        box.right - tester.getRect(button('Send message')).right,
        closeTo(7, 0.5),
      );
      final field = tester.getRect(find.byType(TextField));
      expect(
        field.center.dy,
        closeTo(tester.getRect(button('Send message')).center.dy, 1),
      );
      await tester.pumpWidget(const SizedBox.shrink());
    });
  });

  test('working projects come first, each group in host order', () {
    final projects = [
      {'id': 'a', 'name': 'A'},
      {'id': 'b', 'name': 'B', 'working': true},
      {'id': 'c', 'name': 'C'},
      {'id': 'd', 'name': 'D', 'working': true},
    ];
    List<String> ids(List<Data> list) => [for (final p in list) p['id']];
    expect(ids(arrangeProjects(projects)), ['b', 'd', 'a', 'c']);
    // Held: the order last shown stands, and a new project goes last.
    expect(
      ids(
        arrangeProjects(
          [
            ...projects,
            {'id': 'e', 'name': 'E', 'working': true},
          ],
          ['a', 'b', 'c', 'd'],
        ),
      ),
      ['a', 'b', 'c', 'd', 'e'],
    );
  });

  testWidgets('the project list follows what is working on the host', (
    tester,
  ) async {
    var working = <String>{};
    await tester.pumpWidget(
      MaterialApp(
        theme: connectoTheme(),
        home: ProjectsPage(
          supportDirectory: () async => Directory.systemTemp,
          call: (op, fields) async {
            if (op != 'state') return null;
            return {
              'hosts': [
                {
                  'name': 'mac',
                  'link': 'Connected',
                  'projects': [
                    for (final id in ['alpha', 'beta', 'gamma'])
                      {'id': id, 'name': id, 'working': working.contains(id)},
                  ],
                },
              ],
              'pairing': {'phase': 'idle'},
              'relay': '',
            };
          },
        ),
      ),
    );
    await tester.pump();
    await tester.pump();
    List<String> shown() =>
        [
          for (final id in ['alpha', 'beta', 'gamma'])
            if (find.text(id).evaluate().isNotEmpty) id,
        ]..sort(
          (a, b) => tester
              .getTopLeft(find.text(a))
              .dy
              .compareTo(tester.getTopLeft(find.text(b)).dy),
        );
    expect(shown(), ['alpha', 'beta', 'gamma']);
    expect(find.text('Working'), findsNothing);

    working = {'gamma'};
    await tester.pump(const Duration(seconds: 1));
    await tester.pump();
    expect(shown(), ['gamma', 'alpha', 'beta']);
    expect(find.text('Working'), findsOneWidget);

    // Not while a finger is on the list.
    final gesture = await tester.startGesture(
      tester.getCenter(find.text('alpha')),
    );
    working = {'beta'};
    await tester.pump(const Duration(seconds: 1));
    await tester.pump();
    expect(shown(), ['gamma', 'alpha', 'beta']);
    await gesture.cancel();
    await tester.pump(const Duration(seconds: 1));
    await tester.pump();
    expect(shown(), ['beta', 'alpha', 'gamma']);
    await tester.pumpWidget(const SizedBox.shrink());
  });

  testWidgets('only the agents the host project allows are offered', (
    tester,
  ) async {
    final calls = <String>[];
    var answer = <String, Object?>{'status': 'New', 'agentsLoading': true};
    await tester.pumpWidget(
      MaterialApp(
        theme: connectoTheme(),
        home: ChatPage(
          host: 'Host',
          project: 'project',
          title: 'Project',
          call: (op, fields) async {
            calls.add(op);
            if (op == 'models') {
              return {'supported': false, 'default': null, 'models': []};
            }
            return answer;
          },
        ),
      ),
    );
    await tester.pump();
    // Loading: the agents are shown but cannot be chosen yet.
    expect(
      find.textContaining("Loading the project's settings"),
      findsOneWidget,
    );
    await tester.tap(find.text('Claude'));
    await tester.pump();
    expect(calls, isNot(contains('models')));
    // The host's project allows Codex and Pi.
    answer = {
      'status': 'New',
      'agents': ['Codex', 'Pi'],
      'agentsLoading': false,
    };
    await tester.pump(const Duration(seconds: 1));
    await tester.pump();
    expect(find.text('Claude'), findsNothing);
    expect(find.text('Antigravity'), findsNothing);
    expect(find.text('Codex'), findsOneWidget);
    expect(find.text('Pi'), findsOneWidget);
    await tester.tap(find.text('Pi'));
    await tester.pump();
    await tester.pump();
    expect(calls, contains('models'));
    await tester.pumpWidget(const SizedBox.shrink());
  });

  testWidgets('an agent is chosen, then its model, or its default', (
    tester,
  ) async {
    final calls = <(String, Data)>[];
    Future<void> open({required bool supported}) async {
      calls.clear();
      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pumpWidget(
        MaterialApp(
          theme: connectoTheme(),
          home: ChatPage(
            host: 'Host',
            project: 'project',
            title: 'Project',
            call: (op, fields) async {
              calls.add((op, fields));
              if (op == 'models') {
                return supported
                    ? {
                        'supported': true,
                        'default': null,
                        'models': [
                          'gemini-4-argon-high',
                          'gemini-3.1-pro-high',
                        ],
                      }
                    : {'supported': false, 'default': null, 'models': []};
              }
              return {'status': 'New'};
            },
          ),
        ),
      );
      await tester.pump();
    }

    Data last(String op) => calls.lastWhere((c) => c.$1 == op).$2;

    await open(supported: true);
    for (final agent in ['Claude', 'Codex', 'Pi', 'Antigravity', 'OpenCode']) {
      expect(find.text(agent), findsOneWidget);
    }
    await tester.tap(find.text('Antigravity'));
    await tester.pump();
    await tester.pump();
    expect(last('models')['tool'], 'antigravity');
    expect(last('models')['project'], 'project');
    expect(find.text('Choose a model for Antigravity'), findsOneWidget);
    expect(find.text("The agent's own default"), findsOneWidget);

    // Back to the agents, and on to another one.
    await tester.tap(find.text('Change agent'));
    await tester.pump();
    expect(find.text('Choose an agent'), findsOneWidget);
    await tester.tap(find.text('Antigravity'));
    await tester.pump();
    await tester.pump();
    await tester.tap(find.text('gemini-3.1-pro-high'));
    await tester.pump();
    expect(last('create')['tool'], 'antigravity');
    expect(last('create')['model'], 'gemini-3.1-pro-high');
    expect(
      find.text('Starting Antigravity (gemini-3.1-pro-high)…'),
      findsOneWidget,
    );

    // A host from before models offers only the default.
    await open(supported: false);
    await tester.tap(find.text('Claude'));
    await tester.pump();
    await tester.pump();
    expect(
      find.textContaining('offers only the default model'),
      findsOneWidget,
    );
    await tester.tap(find.text('Default'));
    await tester.pump();
    expect(last('create')['tool'], 'claude');
    expect(last('create').containsKey('model'), isFalse);
    await tester.pumpWidget(const SizedBox.shrink());
  });

  testWidgets('a saved host can be removed after a named confirmation', (
    tester,
  ) async {
    var hosts = [
      {'name': 'work mac', 'link': 'Disconnected', 'projects': []},
      {'name': 'home mac', 'link': 'Connected', 'projects': []},
    ];
    final forgets = <Data>[];
    var fail = true;
    await tester.pumpWidget(
      MaterialApp(
        theme: connectoTheme(),
        home: ProjectsPage(
          supportDirectory: () async => Directory.systemTemp,
          call: (op, fields) async {
            if (op == 'forget') {
              forgets.add(fields);
              if (fail) throw const PeerError('Could not remove work mac.');
              hosts = [
                for (final h in hosts)
                  if (h['name'] != fields['host']) h,
              ];
            }
            if (op != 'state') return null;
            return {
              'hosts': hosts,
              'pairing': {'phase': 'idle'},
              'relay': '',
            };
          },
        ),
      ),
    );
    await tester.pump();
    await tester.pump();

    Future<void> openRemove() async {
      await tester.tap(find.byTooltip('Options for work mac'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Remove host…'));
      await tester.pumpAndSettle();
      expect(find.text('Remove work mac?'), findsOneWidget);
    }

    await openRemove();
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(forgets, isEmpty);
    expect(find.text('work mac'), findsOneWidget);

    await openRemove();
    await tester.tap(find.widgetWithText(FilledButton, 'Remove host'));
    await tester.pumpAndSettle();
    expect(forgets.single, {'host': 'work mac'});
    expect(find.text('Could not remove work mac.'), findsOneWidget);
    expect(find.text('work mac'), findsOneWidget);

    fail = false;
    await openRemove();
    await tester.tap(find.widgetWithText(FilledButton, 'Remove host'));
    await tester.pumpAndSettle();
    expect(forgets.length, 2);
    expect(find.text('work mac'), findsNothing);
    expect(find.text('home mac'), findsOneWidget);
    expect(find.text('Could not remove work mac.'), findsNothing);
    await tester.pumpWidget(const SizedBox.shrink());
  });
}
