import 'dart:async';
import 'dart:convert';

import 'package:connecto_flutter_app/board.dart';
import 'package:connecto_flutter_app/call.dart';
import 'package:connecto_flutter_app/peer.dart';
import 'package:connecto_voice/connecto_voice.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'call_test.dart' show FakeHost, FakeVoice, target, turnGap;

String boardJson(List<(String, String, String)> tasks) => jsonEncode({
  'tasks': [
    for (final (id, title, status) in tasks)
      {'id': id, 'title': title, 'goal': '$title.', 'status': status},
  ],
});

void main() {
  group('Board.parse', () {
    test('keeps the queue in file order and sorts tasks by state', () {
      final b = Board.parse(
        boardJson([
          ('t-1', 'Fix login', 'done'),
          ('t-2', 'Add search', 'running'),
          ('t-3', 'Dark mode', 'queued'),
          ('t-4', 'Export CSV', 'queued'),
          ('t-5', 'Old thing', 'failed'),
        ]),
      )!;
      expect(b.active.map((t) => t.id), ['t-2']);
      expect(b.queued.map((t) => t.id), ['t-3', 't-4']);
      expect(b.finished.map((t) => t.id), [
        't-5',
        't-1',
      ], reason: 'latest first');
    });

    test('reads loose statuses, a bare list, and skips what is not a task', () {
      final b = Board.parse(
        jsonEncode([
          {'id': 'a', 'title': 'One', 'status': 'In Progress'},
          {'id': 'b', 'goal': 'Only a goal', 'status': 'completed'},
          {'id': 'c', 'title': 'Waits', 'status': 'needs_decision'},
          {'id': 'a', 'title': 'Twice', 'status': 'queued'},
          {'id': 'd', 'status': 'queued'},
          'junk',
          {'id': 'e', 'title': 'New', 'status': 'whatever'},
        ]),
      )!;
      expect(
        [for (final t in b.tasks) (t.id, t.title, t.status)],
        [
          ('a', 'One', TaskStatus.running),
          ('b', 'Only a goal', TaskStatus.done),
          ('c', 'Waits', TaskStatus.blocked),
          ('e', 'New', TaskStatus.queued),
        ],
      );
    });

    test('the project\'s instructions for running a task', () {
      expect(Board.parse('{"tasks": []}')!.howToRun, isNull);
      expect(
        Board.parse('{"howToRun": " Sonnet codes. ", "tasks": []}')!.howToRun,
        'Sonnet codes.',
      );
    });

    test('a broken row never hides a later good one with its id', () {
      final b = Board.parse(
        jsonEncode([
          {'id': 't-1', 'status': 'queued'},
          {'id': 't-1', 'title': 'Real', 'status': 'queued'},
          {'title': 'No id', 'status': 'queued'},
        ]),
      )!;
      expect(
        [for (final t in b.tasks) (t.id, t.title)],
        [('t-1', 'Real'), ('', 'No id')],
      );
    });

    test('a half-written board is not a board', () {
      expect(Board.parse('{"tasks": [{"id": "t-1", "tit'), isNull);
      expect(Board.parse('"hello"'), isNull);
      expect(Board.parse('{"tasks": []}')!.tasks, isEmpty);
    });
  });

  group('markers', () {
    test('a report counts only for its own task', () {
      expect(
        reportsComplete(['Task complete [t-a]. Done.'], task: 't-a'),
        isTrue,
      );
      expect(
        reportsComplete(['Task complete [t-b]. Done.'], task: 't-a'),
        isFalse,
      );
      expect(
        reportsComplete(['Task complete. Done.'], task: 't-a'),
        isTrue,
        reason: 'no id: as before',
      );
      expect(reportsComplete(['**Task complete (t-a).** Done.']), isTrue);
      expect(
        reportsQueued(['Queued [t-a]. Search runs now.'], task: 't-a'),
        isTrue,
      );
      expect(
        reportsQueued(['Queued [t-b]. Search runs now.'], task: 't-a'),
        isFalse,
      );
      expect(reportsQueued(['Queued tasks: two.'], task: 't-a'), isFalse);
      expect(reportsQueued(['Task complete [t-a].'], task: 't-a'), isFalse);
    });

    test('ids are not read aloud, markers are said in the call language', () {
      expect(
        localizeComplete('Task complete [t-a]. Login fixed.', CallWords.it),
        'Compito completato. Login fixed.',
      );
      expect(
        localizeComplete('Queued [t-a]. Ora gira la ricerca.', CallWords.it),
        'In coda. Ora gira la ricerca.',
      );
      expect(
        localizeComplete('Started [t-b]: dark mode.', CallWords.en),
        'Started: dark mode.',
      );
    });

    test('the move to front names the task and how the board is kept', () {
      final p = moveToFrontPrompt(
        const BoardTask(id: 't-9', title: 'Dark mode'),
      );
      expect(p, contains('[t-9] "Dark mode"'));
      expect(p, contains("Don't stop or restart the task that is running"));
      expect(p, contains(boardPath));
      expect(newTaskId(DateTime.utc(2026)), startsWith('t-'));
    });
  });

  group('how tasks run', () {
    test('by default a coder on Opus and a reviewer on Codex Astra', () {
      expect(defaultHowToRun, contains('connecto add claude --model opus'));
      expect(
        defaultHowToRun,
        contains('connecto add codex --model gpt-6.2-astra'),
      );
      expect(defaultHowToRun, contains('medium effort'));
    });

    test('the default keeps to the CLIs the project allows', () {
      expect(
        defaultHowToRun,
        contains('Use only the coding CLIs this project allows'),
      );
      expect(defaultHowToRun, contains('when Claude is allowed'));
      expect(defaultHowToRun, contains('when Codex is allowed'));
      expect(
        defaultHowToRun,
        contains('a different allowed CLI if there is one'),
      );
    });

    test('a board with the earlier built-in default gets the new one', () {
      String board(String howToRun) =>
          jsonEncode({'howToRun': howToRun, 'tasks': []});
      // Exactly the earlier default: read as none, so the new one shows.
      expect(Board.parse(board(previousDefaultHowToRun))!.howToRun, isNull);
      expect(
        Board.parse(board('  $previousDefaultHowToRun\n'))!.howToRun,
        isNull,
      );
      // Anything else is the project's own.
      final edited = previousDefaultHowToRun.replaceFirst('Opus', 'Sonnet');
      expect(Board.parse(board(edited))!.howToRun, edited);
      expect(Board.parse(board('Just do it.'))!.howToRun, 'Just do it.');
      // The agent is told to replace exactly that text, and keep any other.
      final p = taskPrompt(
        const TaskDraft(title: 'T', goal: 'G', scope: 'S', done: 'D'),
        id: 't-1',
      );
      expect(p, contains(previousDefaultHowToRun));
      expect(p, contains('is exactly the earlier default instructions'));
      expect(p, contains("Any other howToRun is the project's own: keep it."));
      expect(previousDefaultHowToRun, isNot(defaultHowToRun));
    });

    test('every task prompt carries the default and the board\'s rule', () {
      final p = taskPrompt(
        const TaskDraft(title: 'T', goal: 'G', scope: 'S', done: 'D'),
        id: 't-1',
      );
      expect(p, contains(defaultHowToRun));
      expect(p, contains("as the board's howToRun says"));
      expect(p, contains('Every task you start, now or later'));
      expect(p, contains('"Task failed [<its id>]."'));
      expect(p, contains('If id t-1 is already on the board'));
      expect(p, contains('It may be an older task, not this one.'));
      expect(p, contains('your last message of a turn is what I see'));
      expect(p, contains('git rev-parse --git-path info/exclude'));
      expect(
        p.indexOf('set it to the default instructions'),
        lessThan(p.indexOf('If no task is running or blocked')),
        reason: 'the default is set even when the task waits',
      );
    });

    test('a new prompt is sent whole, between markers', () {
      final p = setHowToRunPrompt('  Sonnet codes.\nGemini reviews.  ');
      expect(
        p,
        contains('----------\nSonnet codes.\nGemini reviews.\n----------'),
      );
      expect(p, contains("don't change a task that is already running"));
    });
  });

  group('a voice task on the board', () {
    const taskReply =
        'TASK\nTitle: Dark mode\n'
        'Goal: Add a dark theme.\n'
        'Scope: The theme file.\n'
        'Done: The app follows the system theme.';

    Future<CallController> agreed(
      WidgetTester tester,
      FakeHost host,
      FakeVoice voice,
    ) async {
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Add dark mode', isFinal: true);
      await tester.pump(turnGap);
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('yes', isFinal: true);
      await tester.pump(turnGap);
      expect(call.started, isTrue);
      return call;
    }

    testWidgets(
      'agreed while another runs, it is queued and the call goes on',
      (tester) async {
        final host = FakeHost();
        final voice = FakeVoice()..intake = (_) => const Intake(check: true);
        final call = await agreed(tester, host, voice);
        final id = call.taskId!;
        final sent = host.sent('send').last['text'] as String;
        expect(sent, contains('Id: $id'));
        expect(sent, contains('"Queued [$id]."'));

        host.reply('Queued [$id]. Search runs now; dark mode is second.');
        await tester.pump(const Duration(milliseconds: 300));
        expect(
          voice.spoken.last,
          'Queued. Search runs now; dark mode is second.',
        );
        expect(call.outcomes[id], TaskOutcome.queued);
        expect(call.completed, isFalse);
        expect(call.ended, isFalse, reason: 'another task can be agreed');
        call.dispose();
        await tester.pump();
      },
    );

    const csvReply =
        'TASK\nTitle: Export CSV\n'
        'Goal: Export the list as CSV.\n'
        'Scope: The list page.\n'
        'Done: A CSV file downloads.';

    /// The task prompts sent so far, in order.
    List<String> taskPrompts(FakeHost host) => [
      for (final s in host.sent('send'))
        if ('${s['text']}'.startsWith(taskPromptLead)) '${s['text']}',
    ];

    /// Dark mode agreed and queued, then a CSV export agreed in the same
    /// call and sent: the call and both ids.
    Future<(CallController, String, String)> twoAgreed(
      WidgetTester tester,
      FakeHost host,
      FakeVoice voice,
    ) async {
      final call = await agreed(tester, host, voice);
      final first = call.taskId!;
      host.reply('Queued [$first]. Search runs now; dark mode is second.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isFalse, reason: 'the call goes on for another task');
      expect(call.started, isFalse);
      expect(call.draft, isNull);

      voice.say('Also a CSV export', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      final check = host.sent('send').last['text'] as String;
      expect(
        check,
        isNot(startsWith('Also a CSV export')),
        reason: 'a new request is checked, not passed to the agent as is',
      );
      host.reply(csvReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft?.title, 'Export CSV');
      voice.say('yes', isFinal: true, start: 30, end: 31);
      await tester.pump(turnGap);
      expect(call.started, isTrue);
      final second = call.taskId!;
      expect(second, isNot(first));
      return (call, first, second);
    }

    // Regression: a call stayed on its first task. Once it was queued the
    // call hung up, and once it ran everything said went to the agent as
    // a follow-up, so no second task could be proposed.
    testWidgets('a second task in the same call goes on the board after it', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final (call, first, second) = await twoAgreed(tester, host, voice);
      final tasks = taskPrompts(host);
      expect(tasks, hasLength(2));
      expect(tasks[0], contains('Id: $first'));
      expect(tasks[0], contains('Task: Dark mode'));
      expect(tasks[1], contains('Id: $second'));
      expect(tasks[1], contains('Task: Export CSV'));
      expect(call.tasks.map((t) => t.$2), [first, second]);

      host.reply('Queued [$second]. It runs after dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isFalse);
      // Both reports are told; the call ends once both are in.
      host.reply('Task complete [$first]. The app is dark at night.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Task complete. The app is dark at night.');
      expect(call.ended, isFalse, reason: 'the CSV export is still to come');
      host.reply('Task complete [$second]. The list exports as CSV.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Task complete. The list exports as CSV.');
      expect(call.ended, isTrue);
      expect(call.completed, isTrue);
      call.dispose();
      await tester.pump();
    });

    // Regression: the second check carried the first task's conversation,
    // proposal included, as part of the new request.
    testWidgets('the next request is checked on its own', (tester) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final first = call.taskId!;
      final firstCheck = host.sent('send').first['text'] as String;
      expect(firstCheck, contains('Add dark mode'));
      host.reply('Queued [$first]. Search runs now.');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('Also a CSV export', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      final check = host.sent('send').last['text'] as String;
      expect(check, contains('Also a CSV export'));
      expect(check, isNot(contains('Add dark mode')));
      expect(check, isNot(contains('Add a dark theme')));
      call.dispose();
      await tester.pump();
    });

    // Regression: two reports in one poll: the first was said, and the
    // cursor went past the second, which was never read.
    testWidgets('two reports that land in one poll are both told', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final (call, first, second) = await twoAgreed(tester, host, voice);
      host.reply('Queued [$second]. It runs after dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      host.chat = {
        ...host.chat,
        'messages': [
          ...host.chat['messages'] as List,
          [false, 'Task complete [$first]. The app is dark at night.'],
          [false, 'Task failed [$second]. The list has no data source.'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 300));
      await tester.pump(const Duration(milliseconds: 300));
      expect(
        voice.spoken,
        contains('Task complete. The app is dark at night.'),
      );
      expect(voice.spoken.last, 'Task failed. The list has no data source.');
      expect(call.outcomes[second], TaskOutcome.failed);
      expect(call.ended, isTrue);
      call.dispose();
      await tester.pump();
    });

    // Regression: a report that was the only message after the next
    // request's check left the check stranded: its answer was never read.
    testWidgets('a report during the next check does not strand it', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final first = call.taskId!;
      host.reply('Started dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('Also a CSV export', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      expect(call.validating, isTrue);
      host.reply('Task complete [$first]. The app is dark at night.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.validating, isTrue, reason: 'the check is still answered');
      host.reply(csvReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft?.title, 'Export CSV');
      await tester.pump(const Duration(milliseconds: 600));
      expect(
        voice.spoken,
        contains('Task complete. The app is dark at night.'),
      );
      expect(call.ended, isFalse, reason: 'a new task is being agreed');
      call.dispose();
      await tester.pump();
    });

    // Regression: a start retried after a send whose outcome is unknown
    // went out with a new id, and the board took the task twice.
    testWidgets('a retried start keeps its board id', (tester) async {
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
      voice.say('Add dark mode', isFinal: true);
      await tester.pump(turnGap);
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      host.failSend = true;
      voice.say('yes', isFinal: true);
      await tester.pump(turnGap);
      expect(call.started, isFalse);
      host.failSend = false;
      await call.startTask();
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.started, isTrue);
      final ids = [
        for (final t in taskPrompts(host))
          RegExp(r'^Id: (\S+)$', multiLine: true).firstMatch(t)![1],
      ];
      expect(ids, hasLength(2));
      expect(ids[1], ids[0]);
      call.dispose();
      await tester.pump();
    });

    // Regression: a report and another task's question in one poll: the
    // report was said and the question, read past, was never heard.
    testWidgets('a report and a question in one poll are both heard', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final (call, first, second) = await twoAgreed(tester, host, voice);
      host.reply('Queued [$second]. It runs after dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      host.chat = {
        ...host.chat,
        'messages': [
          ...host.chat['messages'] as List,
          [false, 'Task complete [$first]. The app is dark at night.'],
          [false, 'Question [$second]. Which columns go in the file?'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 600));
      expect(
        voice.spoken,
        contains('Task complete. The app is dark at night.'),
      );
      expect(voice.spoken.last, 'Which columns go in the file?');
      voice.say('every one of them', isFinal: true, start: 40, end: 41);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'every one of them');
      expect(call.ended, isFalse);
      call.dispose();
      await tester.pump();
    });

    // Regression: a report landing while an answer to the agent was out
    // took that answer's place, and the agent's reply to it was dropped.
    testWidgets('a report during an answer to the agent keeps its reply', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final (call, first, second) = await twoAgreed(tester, host, voice);
      host.reply('Queued [$second]. It runs after dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      host.reply('Question [$first]. Settings or the menu?');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('under appearance', isFinal: true, start: 40, end: 41);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'under appearance');
      host.reply('Task failed [$second]. No data source.', status: 'Running');
      host.reply('Thinking.', status: 'Running');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Task failed. No data source.');
      final messages = host.chat['messages'] as List;
      host.chat = {
        ...host.chat,
        'status': 'Ready',
        'messages': [
          ...messages.take(messages.length - 1),
          [false, 'Got it: under Appearance.'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 600));
      expect(voice.spoken.last, 'Got it: under Appearance.');
      expect(call.outcomes[second], TaskOutcome.failed);
      call.dispose();
      await tester.pump();
    });

    // Regression: a check whose only reply was a report waited forever,
    // even once the session stopped.
    testWidgets('a check that only got a report still ends when stopped', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final first = call.taskId!;
      host.reply('Started dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('Also a CSV export', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      expect(call.validating, isTrue);
      host.reply(
        'Task complete [$first]. The app is dark at night.',
        status: 'Stopped',
      );
      await tester.pump(const Duration(milliseconds: 600));
      expect(call.validating, isFalse);
      expect(voice.spoken, contains(CallWords.en.sessionStopped));
      expect(
        voice.spoken,
        contains('Task complete. The app is dark at night.'),
      );
      call.dispose();
      await tester.pump();
    });

    // Regression: a running task's question during the next check was
    // taken for the check's answer, and the proposal was missed.
    testWidgets('a question during the next check is asked after it', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final first = call.taskId!;
      host.reply('Started dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('Also a CSV export', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      host.reply('Question [$first]. Settings or the menu?', status: 'Running');
      await tester.pump(const Duration(milliseconds: 300));
      host.reply(csvReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft?.title, 'Export CSV');
      await tester.pump(const Duration(milliseconds: 600));
      expect(voice.spoken.last, 'Settings or the menu?');
      voice.say('under appearance', isFinal: true, start: 40, end: 41);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'under appearance');
      expect(call.draft?.title, 'Export CSV', reason: 'still proposed');
      call.dispose();
      await tester.pump();
    });

    // Regression: a poll that saw the check's prompt before its send was
    // acknowledged took it for an old prompt, and dropped its answer.
    testWidgets('a check whose prompt shows before the send returns', (
      tester,
    ) async {
      final host = FakeHost()
        ..echoes = false
        ..sendGate = Completer<void>();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = CallController(
        call: host.call,
        voice: voice,
        target: target,
        poll: const Duration(milliseconds: 100),
      );
      await call.start();
      await tester.pump();
      voice.say('Add dark mode', isFinal: true);
      await tester.pump(turnGap);
      final prompt = host.sent('send').single['text'] as String;
      host.chat = {
        ...host.chat,
        'status': 'Running',
        'messages': [
          ...host.chat['messages'] as List,
          [true, prompt],
        ],
      };
      await tester.pump(const Duration(milliseconds: 300));
      host.reply(taskReply);
      await tester.pump(const Duration(milliseconds: 300));
      host.sendGate!.complete();
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft?.title, 'Dark mode');
      call.dispose();
      await tester.pump();
    });

    // Regression: an earlier task's report after the next start's prompt
    // was taken for that start's answer; its own "Queued" then no longer
    // handed it off, and the call stayed on the start.
    testWidgets('only its own answer acknowledges a start', (tester) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final (call, first, second) = await twoAgreed(tester, host, voice);
      host.reply('Task complete [$first]. The app is dark at night.');
      await tester.pump(const Duration(milliseconds: 300));
      await tester.pump(const Duration(milliseconds: 300));
      expect(
        voice.spoken,
        contains('Task complete. The app is dark at night.'),
      );
      expect(call.started, isTrue, reason: 'its start is still unanswered');
      host.reply('Queued [$second]. It runs next.');
      await tester.pump(const Duration(milliseconds: 600));
      expect(call.started, isFalse);
      expect(call.outcomes[second], TaskOutcome.queued);
      expect(call.ended, isFalse);
      call.dispose();
      await tester.pump();
    });

    // Regression: a start answered by its marker, with a host question in
    // the same snapshot: the question was never said.
    testWidgets("a host question with the start's answer is asked", (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      host.chat = {
        ...host.chat,
        'status': 'Needs an answer',
        'question': 'Allow editing the theme file?',
        'textAnswer': true,
        'messages': [
          ...host.chat['messages'] as List,
          [false, 'Queued [$id]. Search runs now.'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 600));
      expect(voice.spoken, contains('Queued. Search runs now.'));
      expect(voice.spoken.last, contains('Allow editing the theme file?'));
      call.dispose();
      await tester.pump();
    });

    /// Dark mode started; a CSV export proposed and heard to the end; then
    /// dark mode's task asks something. The call and dark mode's id.
    Future<(CallController, String)> askedOverProposal(
      WidgetTester tester,
      FakeHost host,
      FakeVoice voice,
    ) async {
      final call = await agreed(tester, host, voice);
      final first = call.taskId!;
      host.reply('Started dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('Also a CSV export', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      host.reply(csvReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft?.title, 'Export CSV');
      host.reply('Question [$first]. Settings or the menu?');
      await tester.pump(const Duration(milliseconds: 600));
      expect(voice.spoken.last, 'Settings or the menu?');
      return (call, first);
    }

    /// The proposal is read back on a yes, and starts on the yes after it,
    /// once.
    Future<void> confirmedAgain(
      WidgetTester tester,
      FakeHost host,
      FakeVoice voice,
      CallController call, {
      required double at,
    }) async {
      voice.say('yes', isFinal: true, start: at, end: at + 1);
      await tester.pump(turnGap);
      expect(call.started, isFalse, reason: 'read back first');
      expect(taskPrompts(host), hasLength(1));
      expect(voice.spoken.last, contains('Export the list as CSV'));
      voice.say('yes', isFinal: true, start: at + 10, end: at + 11);
      await tester.pump(turnGap);
      expect(call.started, isTrue);
      final tasks = taskPrompts(host);
      expect(tasks, hasLength(2));
      expect(tasks[1], contains('Task: Export CSV'));
    }

    // Regression: the blocked task's question was dropped when its answer
    // failed to send, and the yes said again started the proposal.
    testWidgets('a question stays open until its answer is sent', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final (call, _) = await askedOverProposal(tester, host, voice);
      host.failSend = true;
      voice.say('yes', isFinal: true, start: 30, end: 31);
      await tester.pump(turnGap);
      expect(taskPrompts(host), hasLength(1));
      host.failSend = false;
      voice.say('yes', isFinal: true, start: 40, end: 41);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'yes');
      expect(taskPrompts(host), hasLength(1), reason: 'no start for the CSV');
      host.reply('Okay, in Settings.');
      await tester.pump(const Duration(milliseconds: 600));
      await confirmedAgain(tester, host, voice, call, at: 50);
      call.dispose();
      await tester.pump();
    });

    // Regression: a host question said after the blocked task's answer
    // was not the call's open question while a proposal was: a yes to it
    // started the proposal, and its prompt went as the host's answer.
    testWidgets('a host question comes before a pending proposal', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final (call, _) = await askedOverProposal(tester, host, voice);
      voice.say('under appearance', isFinal: true, start: 30, end: 31);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'under appearance');
      host.chat = {
        ...host.chat,
        'status': 'Needs an answer',
        'question': 'Overwrite the theme file?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 600));
      expect(voice.spoken.last, contains('Overwrite the theme file?'));
      voice.say('yes', isFinal: true, start: 40, end: 41);
      await tester.pump(turnGap);
      final answer = host.sent('send').last;
      expect(answer['text'], 'yes');
      expect(answer['question'], 'Overwrite the theme file?');
      expect(taskPrompts(host), hasLength(1), reason: 'no start for the CSV');
      host.chat = {...host.chat, 'question': null, 'textAnswer': null};
      host.reply('Overwritten.');
      await tester.pump(const Duration(milliseconds: 600));
      expect(voice.spoken.last, 'Overwritten.');
      await confirmedAgain(tester, host, voice, call, at: 50);
      call.dispose();
      await tester.pump();
    });

    // Regression: a blocked task's question cut before its first word was
    // still taken as asked: a yes meant for the proposal heard last went
    // to that task as its answer.
    testWidgets('a question cut before it was heard is not answered', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final first = call.taskId!;
      host.reply('Started dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('Also a CSV export', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      host.reply(csvReply);
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.draft?.title, 'Export CSV');
      final sends = host.sent('send').length;

      voice
        ..holdSpeech = true
        ..heardWhenStopped = 0;
      host.reply('Question [$first]. Settings or the menu?');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Settings or the menu?');
      // Stopped before a word of it was heard.
      await voice.stopSpeaking();
      await tester.pump();
      voice.holdSpeech = false;
      voice.say('yes', isFinal: true, start: 30, end: 31);
      await tester.pump(turnGap);
      await tester.pump(const Duration(milliseconds: 600));
      expect(
        host.sent('send').skip(sends).map((f) => f['text']),
        isEmpty,
        reason: 'the yes neither answers the question nor starts the CSV',
      );
      expect(call.started, isFalse);
      // The question is said again, whole: a yes now answers it.
      expect(voice.spoken.last, 'Settings or the menu?');
      voice.say('yes', isFinal: true, start: 40, end: 41);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'yes');
      expect(taskPrompts(host), hasLength(1));
      call.dispose();
      await tester.pump();
    });

    testWidgets('a running task that asks gets the next turn as its answer', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      host.reply('Started dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.started, isFalse);
      // Blocked: the agent asks on its own, woken by its coder's answer.
      host.chat = {
        ...host.chat,
        'messages': [
          ...host.chat['messages'] as List,
          [true, '[connecto from claude-9, request r2.] The coder is stuck.'],
          [false, 'Question [$id]. Should the toggle go in Settings?'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Should the toggle go in Settings?');
      voice.say('under appearance', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'under appearance');
      // Its next question comes as the answer to that turn.
      host.reply('Question [$id]. Light or dark by default?');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Light or dark by default?');
      voice.say('follow the phone', isFinal: true, start: 30, end: 31);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'follow the phone');
      call.dispose();
      await tester.pump();
    });

    testWidgets('"anything else?" leaves the next turn to the conversation', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      host.reply('Started dark mode. Anything else?');
      await tester.pump(const Duration(milliseconds: 300));
      host.reply('Anything else for the board?');
      await tester.pump(const Duration(milliseconds: 300));
      voice.say('Also a CSV export', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], isNot('Also a CSV export'));
      expect(call.validating, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('its own report counts after a typed message in one poll', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      host.reply('Started dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isFalse);
      // A message typed on screen (or sent by the board), its answer, and
      // this task's report, all between two polls.
      host.chat = {
        ...host.chat,
        'messages': [
          ...host.chat['messages'] as List,
          [true, 'From the board: move task [t-x] to the front.'],
          [false, 'Moved.'],
          [false, 'Task complete [$id]. The app is dark at night.'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Task complete. The app is dark at night.');
      expect(call.completed, isTrue);
      expect(call.ended, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a report still being written waits until it is whole', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      host.reply('Task complete [$id]. The app', status: 'Running');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isFalse);
      final messages = host.chat['messages'] as List;
      host.chat = {
        ...host.chat,
        'status': 'Ready',
        'messages': [
          ...messages.take(messages.length - 1),
          [false, 'Task complete [$id]. The app is dark at night.'],
        ],
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Task complete. The app is dark at night.');
      expect(call.ended, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a follow-up said before the report still reaches the agent', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      voice.say('also the settings page', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      expect(voice.spoken.last, "Got it, I'll pass it on.");

      // The report lands, with more after it, while the agent still runs.
      host.reply('Task complete [$id]. Dark mode is on.', status: 'Running');
      host.reply('Cleaning up.', status: 'Running');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken, contains('Task complete. Dark mode is on.'));
      expect(call.ended, isFalse, reason: 'the follow-up is still to go');

      host.chat = {...host.chat, 'status': 'Ready'};
      await tester.pump(const Duration(milliseconds: 300));
      expect(host.sent('send').last['text'], 'also the settings page');
      host.reply('Settings page done too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Settings page done too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isTrue);
      expect(call.completed, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a report while the person is still talking waits for them', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      voice.say(
        'also check the settings page',
        isFinal: true,
        start: 20,
        end: 21,
      );
      await tester.pump(const Duration(milliseconds: 500));
      // The report comes before that turn's silence has run out: it is not
      // said over them.
      host.reply('Task complete [$id]. Dark mode is on.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken, isNot(contains('Task complete. Dark mode is on.')));
      expect(call.ended, isFalse);

      // Their turn goes out, then the report is said.
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'also check the settings page');
      expect(voice.spoken.last, 'Task complete. Dark mode is on.');
      expect(call.ended, isFalse);
      host.reply('Checked, it follows too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Checked, it follows too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a follow-up sent while the report is spoken is answered', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      voice.say(
        'also check the settings page',
        isFinal: true,
        start: 20,
        end: 21,
      );
      await tester.pump(const Duration(milliseconds: 500));
      voice.holdSpeech = true;
      host.reply('Task complete [$id]. Dark mode is on.');
      await tester.pump(const Duration(milliseconds: 300));
      // The turn goes out first; the report is said after it.
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'also check the settings page');
      expect(voice.spoken.last, 'Task complete. Dark mode is on.');
      voice.holdSpeech = false;
      voice.finishSpeaking();
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isFalse, reason: 'its answer is still to come');

      host.reply('Checked, it follows too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Checked, it follows too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isTrue);
      expect(call.completed, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a question about a follow-up is answered before the end', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      voice.say('also the settings page', isFinal: true, start: 20, end: 21);
      await tester.pump(turnGap);
      host.reply('Task complete [$id]. Dark mode is on.');
      await tester.pump(const Duration(milliseconds: 300));
      await tester.pump(const Duration(milliseconds: 300));
      expect(host.sent('send').last['text'], 'also the settings page');

      host.chat = {
        ...host.chat,
        'status': 'Needs an answer',
        'question': 'Which settings page?',
        'textAnswer': true,
      };
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, contains('Which settings page?'));
      await tester.pump(const Duration(milliseconds: 600));
      expect(call.ended, isFalse, reason: 'the question waits for an answer');

      voice.say('the theme one', isFinal: true, start: 30, end: 31);
      await tester.pump(turnGap);
      expect(host.sent('send').last['text'], 'the theme one');
      expect(host.sent('send').last['question'], 'Which settings page?');
      host.chat = {...host.chat, 'question': null, 'textAnswer': false};
      host.reply('Theme settings follow too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Theme settings follow too.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(call.ended, isTrue);
      call.dispose();
      await tester.pump();
    });

    testWidgets('a failed task ends the call without saying it succeeded', (
      tester,
    ) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      host.reply('Started dark mode.');
      await tester.pump(const Duration(milliseconds: 300));
      host.reply('Task failed [$id]. The theme file is generated.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Task failed. The theme file is generated.');
      expect(call.failed, isTrue);
      expect(call.completed, isFalse);
      expect(call.ended, isTrue);
      expect(
        localizeComplete('Task failed [t-a]. No.', CallWords.it),
        'Compito non riuscito. No.',
      );
      call.dispose();
      await tester.pump();
    });

    test('an untagged Queued is not a report for a known task', () {
      expect(reportsQueued(['Queued. Later.'], task: 't-a'), isFalse);
      expect(reportsComplete(['Task complete. Done.'], task: 't-a'), isTrue);
      expect(isOtherTasksReport('Task complete [t-b].', 't-a'), isTrue);
      expect(isOtherTasksReport('Task complete [t-a].', 't-a'), isFalse);
      expect(isOtherTasksReport('Task complete.', 't-a'), isFalse);
      expect(taggedOutcome('Task failed [t-a].', 't-a'), TaskOutcome.failed);
    });

    testWidgets('another task finishing does not end this one', (tester) async {
      final host = FakeHost();
      final voice = FakeVoice()..intake = (_) => const Intake(check: true);
      final call = await agreed(tester, host, voice);
      final id = call.taskId!;
      host.reply('Started dark mode on claude-30.');
      await tester.pump(const Duration(milliseconds: 300));

      host.reply('Task complete [t-older]. Search works.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(
        voice.spoken.last,
        'Started dark mode on claude-30.',
        reason: "another task's report is not said",
      );
      expect(call.ended, isFalse);

      host.reply('Task complete [$id]. The app is dark at night.');
      await tester.pump(const Duration(milliseconds: 300));
      expect(voice.spoken.last, 'Task complete. The app is dark at night.');
      expect(call.completed, isTrue);
      expect(call.ended, isTrue);
      call.dispose();
      await tester.pump();
    });
  });

  group('BoardPage', () {
    late FakeHost host;
    late String? boardText;
    late List<(String, Data)> ops;

    Future<dynamic> call(String op, Data fields) async {
      if (op == 'board') {
        ops.add((op, fields));
        return {'text': boardText};
      }
      return host.call(op, fields).then((v) {
        ops.add((op, fields));
        return v;
      });
    }

    Future<void> open(WidgetTester tester) async {
      BoardSync.clock = () => tester.binding.clock.now();
      await tester.pumpWidget(
        MaterialApp(
          home: BoardPage(
            title: 'Project',
            call: call,
            target: target,
            boardEvery: const Duration(milliseconds: 500),
            chatEvery: const Duration(milliseconds: 100),
          ),
        ),
      );
      await tester.pump(const Duration(milliseconds: 50));
      await tester.pump(const Duration(milliseconds: 50));
    }

    tearDown(() => BoardSync.clock = DateTime.now);

    setUp(() {
      BoardSync.resetAll();
      host = FakeHost();
      ops = [];
      boardText = null;
    });

    double top(WidgetTester tester, String id) =>
        tester.getTopLeft(find.byKey(ValueKey('task-$id'))).dy;

    testWidgets('no board yet', (tester) async {
      await open(tester);
      expect(find.textContaining('No tasks yet'), findsOneWidget);
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('shows each state, and moving a task to the front sends it '
        'once the agent is free', (tester) async {
      boardText = boardJson([
        ('t-1', 'Fix login', 'done'),
        ('t-2', 'Add search', 'running'),
        ('t-3', 'Dark mode', 'queued'),
        ('t-4', 'Export CSV', 'queued'),
      ]);
      // The agent is mid-turn: the move waits for it.
      host.chat = {...host.chat, 'status': 'Running'};
      await open(tester);
      expect(find.text('Running'), findsOneWidget);
      expect(find.text('Queued · 1'), findsOneWidget);
      expect(find.text('Queued · 2'), findsOneWidget);
      expect(find.text('Done'), findsOneWidget);
      expect(top(tester, 't-3'), lessThan(top(tester, 't-4')));
      expect(
        find.byTooltip('Move to front'),
        findsOneWidget,
        reason: 'the first queued task is already next',
      );

      await tester.tap(find.byTooltip('Move to front'));
      await tester.pump(const Duration(milliseconds: 150));
      expect(top(tester, 't-4'), lessThan(top(tester, 't-3')));
      expect(find.text('Moving to front…'), findsOneWidget);
      expect(host.sent('send'), isEmpty, reason: 'the agent is busy');

      host.chat = {...host.chat, 'status': 'Ready'};
      await tester.pump(const Duration(milliseconds: 150));
      await tester.pump(const Duration(milliseconds: 150));
      expect(host.sent('send'), hasLength(1));
      final text = host.sent('send').single['text'] as String;
      expect(text, contains('[t-4] "Export CSV"'));
      expect(host.sent('send').single['id'], 's1');

      // The agent rewrites the board: the move is done.
      boardText = boardJson([
        ('t-1', 'Fix login', 'done'),
        ('t-2', 'Add search', 'running'),
        ('t-4', 'Export CSV', 'queued'),
        ('t-3', 'Dark mode', 'queued'),
      ]);
      await tester.pump(const Duration(milliseconds: 600));
      await tester.pump(const Duration(milliseconds: 50));
      expect(find.text('Moving to front…'), findsNothing);
      expect(top(tester, 't-4'), lessThan(top(tester, 't-3')));
      expect(host.sent('send'), hasLength(1), reason: 'sent once');

      // The running task ends; the next one starts by itself.
      boardText = boardJson([
        ('t-1', 'Fix login', 'done'),
        ('t-2', 'Add search', 'done'),
        ('t-4', 'Export CSV', 'running'),
        ('t-3', 'Dark mode', 'queued'),
      ]);
      await tester.pump(const Duration(milliseconds: 600));
      await tester.pump(const Duration(milliseconds: 50));
      expect(find.text('Running'), findsOneWidget);
      expect(find.text('Done'), findsNWidgets(2));
      expect(find.byTooltip('Move to front'), findsNothing);
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('project settings show the default, and a saved prompt goes '
        'to the agent until the board has it', (tester) async {
      boardText = boardJson([]);
      tester.view.physicalSize = const Size(800, 1600);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await open(tester);
      await tester.tap(find.byTooltip('Project settings'));
      await tester.pumpAndSettle(const Duration(milliseconds: 100));
      final field = find.byKey(const ValueKey('how-to-run'));
      expect(tester.widget<TextField>(field).controller!.text, defaultHowToRun);
      expect(find.textContaining('The default'), findsOneWidget);
      expect(
        tester
            .widget<FilledButton>(find.widgetWithText(FilledButton, 'Save'))
            .onPressed,
        isNull,
        reason: 'nothing changed',
      );

      await tester.enterText(field, 'Sonnet codes, Opus reviews.');
      await tester.pump();
      await tester.tap(find.text('Save'));
      await tester.pump(const Duration(milliseconds: 150));
      expect(host.sent('send'), hasLength(1));
      expect(
        host.sent('send').single['text'],
        setHowToRunPrompt('Sonnet codes, Opus reviews.'),
      );
      expect(
        find.textContaining('Waiting for the agent to update'),
        findsOneWidget,
      );

      boardText = jsonEncode({
        'howToRun': 'Sonnet codes, Opus reviews.',
        'tasks': [],
      });
      await tester.pump(const Duration(milliseconds: 600));
      await tester.pump(const Duration(milliseconds: 50));
      expect(find.textContaining('Waiting for the agent'), findsNothing);
      expect(
        tester
            .widget<TextButton>(
              find.widgetWithText(TextButton, 'Reset to default'),
            )
            .onPressed,
        isNotNull,
      );
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    Future<void> openSettings(WidgetTester tester) async {
      tester.view.physicalSize = const Size(800, 1600);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await open(tester);
      await tester.tap(find.byTooltip('Project settings'));
      await tester.pumpAndSettle(const Duration(milliseconds: 100));
    }

    String fieldText(WidgetTester tester) => tester
        .widget<TextField>(find.byKey(const ValueKey('how-to-run')))
        .controller!
        .text;

    Future<void> poll(WidgetTester tester) async {
      await tester.pump(const Duration(milliseconds: 600));
      await tester.pump(const Duration(milliseconds: 50));
    }

    testWidgets('a move made while the agent works is sent after leaving the '
        'board, and still shown when it is opened again', (tester) async {
      boardText = boardJson([
        ('t-2', 'Add search', 'running'),
        ('t-3', 'Dark mode', 'queued'),
        ('t-4', 'Export CSV', 'queued'),
      ]);
      host.chat = {...host.chat, 'status': 'Running'};
      await open(tester);
      await tester.tap(find.byTooltip('Move to front'));
      await tester.pump(const Duration(milliseconds: 150));
      await tester.pumpWidget(const SizedBox());
      expect(host.sent('send'), isEmpty);

      host.chat = {...host.chat, 'status': 'Ready'};
      await tester.pump(const Duration(milliseconds: 150));
      await tester.pump(const Duration(milliseconds: 150));
      expect(host.sent('send'), hasLength(1), reason: 'sent with no page');

      await open(tester);
      expect(find.text('Moving to front…'), findsOneWidget);
      expect(top(tester, 't-4'), lessThan(top(tester, 't-3')));
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('a failed send is retried, and its warning goes once it is '
        'through', (tester) async {
      boardText = boardJson([
        ('t-2', 'Add search', 'running'),
        ('t-3', 'Dark mode', 'queued'),
        ('t-4', 'Export CSV', 'queued'),
      ]);
      host.failSend = true;
      await open(tester);
      await tester.tap(find.byTooltip('Move to front'));
      await tester.pump(const Duration(milliseconds: 50));
      expect(find.textContaining('No link to the host'), findsOneWidget);
      host.failSend = false;
      await tester.pump(const Duration(milliseconds: 150));
      await tester.pump(const Duration(milliseconds: 50));
      expect(
        host.sent('send').where((f) => '${f['text']}'.contains('[t-4]')),
        hasLength(2),
      );
      expect(find.textContaining('No link to the host'), findsNothing);
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('a move not seen on the board in time is given up on, timed '
        'from when it went out', (tester) async {
      boardText = boardJson([
        ('t-3', 'Dark mode', 'queued'),
        ('t-4', 'Export CSV', 'queued'),
      ]);
      host.chat = {...host.chat, 'status': 'Running'};
      await open(tester);
      await tester.tap(find.byTooltip('Move to front'));
      // Waiting for the agent does not count against it.
      await tester.pump(const Duration(minutes: 4));
      expect(find.text('Moving to front…'), findsOneWidget);
      host.chat = {...host.chat, 'status': 'Ready'};
      await poll(tester);
      expect(host.sent('send'), hasLength(1));
      await tester.pump(const Duration(minutes: 2));
      expect(find.text('Moving to front…'), findsOneWidget);
      await tester.pump(const Duration(minutes: 2));
      expect(find.text('Moving to front…'), findsNothing);
      expect(find.textContaining("hasn't moved the task"), findsOneWidget);
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('a saving prompt is shown again on reopening, the field '
        'follows the board while untouched, and an unconfirmed save can be '
        'sent again', (tester) async {
      boardText = jsonEncode({'howToRun': 'Old way.', 'tasks': []});
      host.chat = {...host.chat, 'status': 'Running'};
      await openSettings(tester);
      expect(fieldText(tester), 'Old way.');
      await tester.enterText(
        find.byKey(const ValueKey('how-to-run')),
        'New way.',
      );
      await tester.pump();
      await tester.tap(find.text('Save'));
      await tester.pump(const Duration(milliseconds: 50));
      expect(
        find.textContaining('Waiting for the agent to be free'),
        findsOneWidget,
      );

      // Away and back: the save is still on its way.
      await tester.pumpWidget(const SizedBox());
      await openSettings(tester);
      expect(fieldText(tester), 'New way.');
      expect(find.textContaining('Waiting for the agent'), findsOneWidget);

      host.chat = {...host.chat, 'status': 'Ready'};
      await poll(tester);
      expect(host.sent('send'), hasLength(1));
      expect(find.textContaining('Sent. Waiting'), findsOneWidget);
      // The agent answers but never writes it.
      host.reply('Saved.');
      await tester.pump(const Duration(minutes: 4));
      expect(
        find.textContaining("hasn't saved the instructions"),
        findsOneWidget,
      );
      expect(
        tester
            .widget<FilledButton>(find.widgetWithText(FilledButton, 'Save'))
            .onPressed,
        isNotNull,
        reason: 'the same text can be sent again',
      );
      await tester.tap(find.text('Save'));
      await poll(tester);
      expect(host.sent('send'), hasLength(2));

      // Written on the board, then changed there from elsewhere: the
      // untouched field follows.
      boardText = jsonEncode({'howToRun': 'New way.', 'tasks': []});
      await poll(tester);
      expect(find.textContaining('Waiting for the agent'), findsNothing);
      boardText = jsonEncode({
        'howToRun': 'Changed on the computer.',
        'tasks': [],
      });
      await poll(tester);
      expect(fieldText(tester), 'Changed on the computer.');
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('a save made while another is going out waits for its own '
        'send, and the form can go back to the board\'s text or the default', (
      tester,
    ) async {
      boardText = jsonEncode({'howToRun': 'Old.', 'tasks': []});
      await openSettings(tester);
      final sync = BoardSync.of(call, target);
      final field = find.byKey(const ValueKey('how-to-run'));
      Finder button(String label) => find.ancestor(
        of: find.text(label),
        matching: find.byWidgetPredicate((w) => w is ButtonStyleButton),
      );
      bool enabled(String label) =>
          tester.widget<ButtonStyleButton>(button(label)).onPressed != null;

      // A goes out and is held there.
      host.sendGate = Completer<void>();
      await tester.enterText(field, 'A.');
      await tester.pump();
      await tester.tap(find.text('Save'));
      await tester.pump(const Duration(milliseconds: 20));
      expect(host.sent('send'), hasLength(1));

      // B is made while A is still going out.
      await tester.enterText(field, 'B.');
      await tester.pump();
      await tester.tap(find.text('Save'));
      await tester.pump(const Duration(milliseconds: 20));
      expect(sync.saving, 'B.');
      expect(sync.savedAt, isNull);

      // A arrives; the agent works on it, and B waits.
      host.sendGate!.complete();
      host.sendGate = null;
      await tester.pump(const Duration(milliseconds: 20));
      expect(sync.savedAt, isNull, reason: "A's send is not B's");
      expect(
        find.textContaining('Waiting for the agent to be free'),
        findsOneWidget,
      );
      await tester.pump(const Duration(minutes: 4));
      expect(sync.saving, 'B.', reason: 'not timed out before it went out');
      expect(find.textContaining("hasn't saved"), findsNothing);

      // Back to the board's text, and the default, while B is pending.
      await tester.enterText(field, 'Old.');
      await tester.pump();
      expect(enabled('Save'), isTrue);
      expect(enabled('Reset to default'), isTrue);
      await tester.tap(find.text('Save'));
      await tester.pump(const Duration(milliseconds: 20));
      expect(sync.saving, 'Old.');
      expect(sync.unsent, 1, reason: 'it replaces B, not yet sent');

      // The agent is done with A: the last save goes out, after it.
      boardText = jsonEncode({'howToRun': 'A.', 'tasks': []});
      host.reply('Saved.');
      await poll(tester);
      expect(host.sent('send'), hasLength(2));
      expect(host.sent('send').last['text'], setHowToRunPrompt('Old.'));
      expect(
        tester.widget<TextField>(field).controller!.text,
        'Old.',
        reason: 'the earlier save landing first does not replace it',
      );
      expect(enabled('Save'), isFalse);
      expect(sync.savedAt, isNotNull);
      boardText = jsonEncode({'howToRun': 'Old.', 'tasks': []});
      await poll(tester);
      expect(sync.saving, isNull);
      expect(find.textContaining('Waiting for the agent'), findsNothing);
      // Confirmed: the field follows the board again.
      boardText = jsonEncode({'howToRun': 'From the computer.', 'tasks': []});
      await poll(tester);
      expect(
        tester.widget<TextField>(field).controller!.text,
        'From the computer.',
      );
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('reset to default is offered while a save away from the '
        'default is pending', (tester) async {
      boardText = jsonEncode({'tasks': []});
      host.chat = {...host.chat, 'status': 'Running'};
      await openSettings(tester);
      final reset = find.widgetWithText(TextButton, 'Reset to default');
      expect(tester.widget<TextButton>(reset).onPressed, isNull);
      await tester.enterText(find.byKey(const ValueKey('how-to-run')), 'X.');
      await tester.pump();
      await tester.tap(find.text('Save'));
      await tester.pump(const Duration(milliseconds: 20));
      expect(tester.widget<TextButton>(reset).onPressed, isNotNull);
      await tester.tap(reset);
      await tester.pump(const Duration(milliseconds: 20));
      expect(BoardSync.of(call, target).saving, defaultHowToRun);
      expect(BoardSync.of(call, target).unsent, 1);
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('a first read that is not a board says so', (tester) async {
      boardText = '{"tasks": [';
      await open(tester);
      expect(find.byType(CircularProgressIndicator), findsNothing);
      expect(find.textContaining("can't be read yet"), findsOneWidget);
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('a task without an id is shown but cannot be moved', (
      tester,
    ) async {
      boardText = jsonEncode([
        {'id': 't-3', 'title': 'Dark mode', 'status': 'queued'},
        {'title': 'No id', 'status': 'queued'},
      ]);
      await open(tester);
      expect(find.text('No id'), findsOneWidget);
      expect(find.byTooltip('Move to front'), findsNothing);
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });

    testWidgets('a half-written board keeps the last one shown', (
      tester,
    ) async {
      boardText = boardJson([('t-2', 'Add search', 'running')]);
      await open(tester);
      expect(find.text('Add search'), findsOneWidget);
      boardText = '{"tasks": [{"id"';
      await tester.pump(const Duration(milliseconds: 600));
      await tester.pump(const Duration(milliseconds: 50));
      expect(find.text('Add search'), findsOneWidget);
      expect(find.textContaining("can't be read right now"), findsOneWidget);
      await tester.pumpWidget(const SizedBox());
      BoardSync.resetAll();
    });
  });
}
