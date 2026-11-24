import 'dart:async';
import 'dart:io';
import 'dart:ui' as ui;

import 'package:connecto_flutter_app/composer.dart';
import 'package:connecto_flutter_app/main.dart';
import 'package:connecto_flutter_app/peer.dart';
import 'package:connecto_flutter_app/theme.dart';
import 'package:connecto_flutter_app/widgets.dart';
import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

const projects = [
  {'id': 'api', 'name': 'Connecto API'},
  {'id': 'mobile', 'name': 'Mobile client'},
  {'id': 'release', 'name': 'Release scripts'},
];

const hosts = [
  {'name': 'Studio Mac', 'link': 'Connected', 'projects': projects},
  {
    'name': 'Build machine',
    'link': 'Disconnected',
    'projects': [
      {'id': 'build', 'name': 'Build tools'},
    ],
  },
];

const messages = [
  [true, 'Can you check the release workflow for anything blocking the build?'],
  [
    false,
    'The macOS build and tests passed. I found one unsigned artifact in the staging directory and updated the workflow to sign it before publishing.',
  ],
  [true, 'Great. Please verify the signed artifact.'],
  [false, 'The signature and notarization checks both passed.'],
];

const attach = {'maxBytes': 10 * 1024 * 1024, 'maxFiles': 4};

const chat = <String, dynamic>{
  'id': '1',
  'status': 'Ready',
  'provider': 'Codex',
  'chatReady': true,
  'messages': messages,
};

Future<void> loadFont(String family, String path) async {
  final bytes = await File(path).readAsBytes();
  await (FontLoader(
    family,
  )..addFont(Future.value(ByteData.sublistView(bytes)))).load();
}

void main() {
  final output = Platform.environment['SHOT_DIR'];
  if (output != null) {
    setUpAll(() async {
      await Directory(output).create(recursive: true);
      await loadFont('Screenshot UI', '/System/Library/Fonts/SFNS.ttf');
      await loadFont('Menlo', '/System/Library/Fonts/Menlo.ttc');
      await (FontLoader(
        'MaterialIcons',
      )..addFont(rootBundle.load('fonts/MaterialIcons-Regular.otf'))).load();
    });
  }

  final projectStates = <String, Data>{
    'projects': {'hosts': hosts},
    'no-hosts': {'hosts': []},
    'loading-error': {
      'failure': 'Could not open the peer. Check the connection and try again.',
    },
    'pair-code': {
      'pairing': {
        'phase': 'code',
        'code': '27160438',
        'seconds': 76,
        'waiting': true,
      },
    },
    'pair-name': {
      'pairing': {'phase': 'name', 'name': 'Studio Mac'},
    },
    'relay-dialog': {'hosts': hosts},
    'remove-host': {'hosts': hosts},
  };
  final chatStates = <String, Data>{
    'chat-new': {'status': 'New'},
    'chat-ready': chat,
    'chat-running': {...chat, 'status': 'Running'},
    'chat-choice': {
      ...chat,
      'status': 'Needs an answer',
      'question': 'Approve this command?\n\n  1. Yes, run the checks\n  2. No, keep the draft\n\nTab to navigate · Space to select',
    },
    'chat-text-answer': {
      ...chat,
      'status': 'Needs an answer',
      'textAnswer': true,
      'question': 'Which release channel should I use for the signed build?',
    },
    'chat-connecting': {...chat, 'chatReady': false},
    'chat-unavailable': {
      ...chat,
      'chatReady': false,
      'messages': null,
      'error': 'The chat connection keeps closing before history arrives. If the host is running an older build, update Connecto on the host to a build that supports project chat.',
    },
    'chat-offline': {...chat, 'offline': true},
    'chat-attach': {...chat, 'attach': attach, 'files': true},
    'chat-multiline': {...chat, 'attach': attach, 'draft': true},
    'chat-drop': {...chat, 'attach': attach, 'drop': true},
    'chat-drop-legacy': {...chat, 'drop': true},
  };

  for (final (size, label, scale) in [
    (const Size(1100, 760), 'desktop', 1.0),
    (const Size(400, 800), 'mobile', 1.3),
  ]) {
    for (final (name, state, isChat) in [
      for (final item in projectStates.entries) (item.key, item.value, false),
      for (final item in chatStates.entries) (item.key, item.value, true),
    ]) {
      testWidgets('screenshot $name $label', (tester) async {
        tester.view.devicePixelRatio = 1;
        tester.view.physicalSize = size;
        addTearDown(tester.view.resetDevicePixelRatio);
        addTearDown(tester.view.resetPhysicalSize);
        final capture = GlobalKey();
        var offline = false;
        await tester.pumpWidget(
          RepaintBoundary(
            key: capture,
            child: MaterialApp(
              debugShowCheckedModeBanner: false,
              theme: connectoTheme(fontFamily: 'Screenshot UI'),
              builder: (context, child) => MediaQuery(
                data: MediaQuery.of(context).copyWith(
                  textScaler: TextScaler.linear(scale),
                  disableAnimations: true,
                ),
                child: child!,
              ),
              home: isChat
                  ? Scaffold(
                      body: Builder(
                        builder: (context) => TextButton(
                          onPressed: () => Navigator.of(context).push(
                            MaterialPageRoute<void>(
                              builder: (_) => ChatPage(
                                host: 'Studio Mac',
                                project: 'mobile',
                                title: 'Mobile client',
                                call: (op, fields) async {
                                  if (offline) {
                                    throw const PeerError('Host disconnected');
                                  }
                                  if (op == 'attach') {
                                    final name = fields['path']
                                        .toString()
                                        .split('/')
                                        .last;
                                    if (name.startsWith('build-log')) {
                                      throw PeerError(
                                        '$name is larger than the 10 MiB limit.',
                                      );
                                    }
                                    if (name.startsWith('design')) {
                                      return Completer<Data>().future;
                                    }
                                    return {
                                      'token': name,
                                      'name': name,
                                      'bytes': name.endsWith('png')
                                          ? 482113
                                          : 12940,
                                    };
                                  }
                                  return state;
                                },
                              ),
                            ),
                          ),
                          child: const Text('Open chat'),
                        ),
                      ),
                    )
                  : ProjectsPage(
                      supportDirectory: () async => Directory.systemTemp,
                      call: (op, _) async {
                        if (state['failure'] case final String failure) {
                          throw PeerError(failure);
                        }
                        if (op != 'state') return null;
                        return {
                          'hosts': [],
                          'pairing': {'phase': 'idle'},
                          'relay': '',
                          ...state,
                        };
                      },
                    ),
            ),
          ),
        );
        if (!isChat) {
          await tester.runAsync(
            () => precacheImage(
              const AssetImage(BrandMark.asset),
              capture.currentContext!,
            ),
          );
        }
        await tester.pump();
        await tester.pump();
        if (name == 'pair-code') {
          await tester.pump(const Duration(milliseconds: 125));
        }
        if (isChat) {
          await tester.tap(find.text('Open chat'));
          await tester.pumpAndSettle();
          if (state['files'] == true) {
            tester.state<ComposerState>(find.byType(Composer)).addPaths([
              '/Users/me/Desktop/release-checklist.md',
              '/Users/me/Desktop/signing-error.png',
              '/Users/me/Desktop/design-review-notes-final-v3.pdf',
              '/Users/me/Desktop/build-log.txt',
            ]);
            await tester.pump();
            await tester.pump();
            await tester.enterText(
              find.byType(TextField),
              'Why does notarization reject the signed build?',
            );
            await tester.pump();
          }
          if (state['draft'] == true) {
            await tester.enterText(
              find.byType(TextField),
              'Please check three things before the release:\n'
              '1. the signing identity\n2. the notarization log\n'
              '3. the updater feed',
            );
            await tester.pump();
          }
          if (state['drop'] == true) {
            final page = tester.state(find.byType(ChatPage)) as dynamic;
            page.dragging = true;
            page.setState(() {});
            await tester.pump();
          }
          if (state['offline'] == true) {
            offline = true;
            await tester.pump(const Duration(milliseconds: 750));
            await tester.pump();
          }
        } else if (name == 'remove-host') {
          await tester.tap(find.byTooltip('Options for Build machine'));
          await tester.pumpAndSettle();
          await tester.tap(find.text('Remove host…'));
          await tester.pumpAndSettle();
        } else if (name == 'relay-dialog') {
          await tester.tap(find.byTooltip('Settings'));
          await tester.pumpAndSettle();
          await tester.tap(find.text('Relay'));
          await tester.pump();
          await tester.pump(const Duration(milliseconds: 250));
        }
        expect(tester.takeException(), isNull);
        if (name == 'chat-connecting') {
          final pill = find.byType(StatusPill);
          final pillText = find.descendant(
            of: pill,
            matching: find.text('Connecting to chat…'),
          );
          expect(
            tester.getTopRight(pill).dx,
            lessThanOrEqualTo(size.width - 16),
          );
          expect(
            tester.renderObject<RenderParagraph>(pillText).didExceedMaxLines,
            isFalse,
          );
          final title = find.text('Mobile client');
          expect(
            tester.renderObject<RenderParagraph>(title).didExceedMaxLines,
            isFalse,
          );
          expect(find.byTooltip('Mobile client'), findsOneWidget);
          if (label == 'mobile') {
            expect(
              tester.getTopLeft(pill).dy,
              greaterThan(tester.getTopLeft(title).dy),
            );
          }
        }

        await tester.runAsync(() async {
          final boundary =
              capture.currentContext!.findRenderObject()!
                  as RenderRepaintBoundary;
          final image = await boundary.toImage();
          final data = (await image.toByteData(
            format: ui.ImageByteFormat.png,
          ))!;
          await File('$output/$name-$label.png').writeAsBytes(
            data.buffer.asUint8List(data.offsetInBytes, data.lengthInBytes),
          );
          image.dispose();
        });
        await tester.pumpWidget(const SizedBox.shrink());
        expect(tester.takeException(), isNull);
      }, skip: output == null);
    }
  }
}
