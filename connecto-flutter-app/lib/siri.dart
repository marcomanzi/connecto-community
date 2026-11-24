import 'dart:async';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'peer.dart';
import 'theme.dart';

/// Siri's side of the app on iOS (`ios/Runner/Siri`): the app hands Siri
/// the projects it sees, and Siri asks the app to show a project, a call or
/// one request's reply when it brings the app to the front.
class SiriLink {
  SiriLink._();

  static const _channel = MethodChannel('connecto/siri');
  static String? _lastState;

  /// Tests on another platform turn the channel on.
  @visibleForTesting
  static bool? debugSupported;

  static bool get supported => debugSupported ?? (!kIsWeb && Platform.isIOS);

  /// Gives Siri the hosts and projects of a peer `state`, when they changed.
  static Future<void> state(Data state) async {
    if (!supported) return;
    final hosts = [
      for (final h in state['hosts'] as List? ?? const [])
        {
          'name': h['name'],
          'link': h['link'],
          'projects': [
            for (final p in h['projects'] as List? ?? const [])
              {'id': p['id'], 'name': p['name']},
          ],
        },
    ];
    final key = hosts.toString();
    if (key == _lastState) return;
    _lastState = key;
    try {
      await _channel.invokeMethod('state', {'hosts': hosts});
    } on MissingPluginException {
      // Not on a build with Siri.
    } on PlatformException {
      _lastState = null;
    }
  }

  /// Calls [onOpen] with what Siri wants shown: `{kind: project | call |
  /// reply, host, project, name, requestId?, preview?}`. True from it when
  /// it was shown.
  static void listen(Future<bool> Function(Data open) onOpen) {
    if (!supported) return;
    _channel.setMethodCallHandler((call) async {
      if (call.method != 'open') return null;
      return onOpen(Map<String, dynamic>.from(call.arguments as Map));
    });
  }

  /// What Siri asked to show before the app was up.
  static Future<Data?> takePending() async {
    if (!supported) return null;
    try {
      final v = await _channel.invokeMethod('takePending');
      return v == null ? null : Map<String, dynamic>.from(v as Map);
    } on MissingPluginException {
      return null;
    }
  }

  /// Siri questions whose fate is not known here yet.
  static Future<List<Data>> openRequests() async {
    if (!supported) return const [];
    try {
      final v = await _channel.invokeMethod('openRequests') as List?;
      return [
        for (final r in v ?? const []) Map<String, dynamic>.from(r as Map),
      ];
    } on MissingPluginException {
      return const [];
    }
  }

  /// A host connected: Siri tells it where this phone's notices go
  /// (`PushRegister`).
  static Future<void> registerPush() async {
    if (!supported) return;
    try {
      await _channel.invokeMethod('registerPush');
    } on MissingPluginException {
      // Not on a build with Siri.
    }
  }

  /// Records what the host said of a request whose sending was not
  /// confirmed; its route is kept.
  static Future<void> reconcile(String requestId, String hostState) async {
    if (!supported) return;
    try {
      await _channel.invokeMethod('reconcileRequest', {
        'requestId': requestId,
        'hostState': hostState,
      });
    } on MissingPluginException {
      // Nothing kept.
    }
  }

  static Future<void> forget(String requestId) async {
    if (!supported) return;
    try {
      await _channel.invokeMethod('forgetRequest', {'requestId': requestId});
    } on MissingPluginException {
      // Nothing kept.
    }
  }
}

/// What one reconciliation found: questions the host never got (for the app
/// to offer again), and hosts with a question still being asked, to check
/// again once that ask is over.
class SiriCheck {
  const SiriCheck(this.lost, this.waiting);
  final List<Data> lost;
  final Set<String> waiting;
}

/// Checks Siri questions whose sending was not confirmed (§5.4) once their
/// host is connected: the host says what became of each, and never types it
/// again.
Future<SiriCheck> checkUnconfirmed(
  Data state,
  Future<dynamic> Function(String, Data) call,
) async {
  final connected = {
    for (final h in state['hosts'] as List? ?? const [])
      if (h['link'] == 'Connected') h['name'] as String,
  };
  final lost = <Data>[];
  final waiting = <String>{};
  for (final r in await SiriLink.openRequests()) {
    if (r['unconfirmed'] != true || !connected.contains(r['host'])) continue;
    // A Siri ask still waiting on its answer: the host may not know it yet,
    // and "unknown" now would offer to send it twice. Checked again later.
    if (r['active'] == true) {
      waiting.add(r['host'] as String);
      continue;
    }
    switch (await _lookUp(r, call)) {
      case _Verdict.lost:
        lost.add(r);
      case _Verdict.settled || _Verdict.undecided:
    }
  }
  return SiriCheck(lost, waiting);
}

/// What one lookup of an unconfirmed question decided.
enum _Verdict {
  /// The host has it: reconciled, its route kept.
  settled,

  /// The host never got it: forgotten here, for the app to offer again.
  lost,

  /// No verdict (no answer, or a host that cannot look ids up): it stays
  /// unconfirmed, never offered.
  undecided,
}

/// Asks `r`'s host what became of it, and records the verdict.
Future<_Verdict> _lookUp(
  Data r,
  Future<dynamic> Function(String, Data) call,
) async {
  try {
    final result = await call('siriResult', {
      'host': r['host'],
      'project': r['project'],
      'requestId': r['requestId'],
    }) as Data?;
    final state = result?['state'] as String?;
    // "unsupported": a host that cannot look ids up says nothing of
    // whether the question arrived.
    if (state == null || state == 'unsupported') return _Verdict.undecided;
    if (state == 'unknown') {
      await SiriLink.forget(r['requestId'] as String);
      return _Verdict.lost;
    }
    // It went: kept, with what the host said, as the route a later
    // notice's tap takes to this exact reply.
    await SiriLink.reconcile(r['requestId'] as String, state);
    return _Verdict.settled;
  } catch (_) {
    return _Verdict.undecided;
  }
}

/// When to reconcile, by question rather than by host. At every refresh,
/// each unconfirmed question on a connected host that Siri is no longer
/// asking is looked up, unless it was already looked up on this same
/// connection to its host without a verdict (asked again once the host
/// reconnects). So a question that becomes unconfirmed on a connection
/// already checked is still looked up.
///
/// One scan runs at a time. A refresh that comes during a scan only asks
/// for another pass, which reads a fresh snapshot of the questions (one
/// found lost is forgotten before then, so it is never offered twice) and
/// the latest connections. A lookup's outcome is recorded against the
/// connection it was sent on, so one that ends after a reconnect does not
/// stand for the new connection.
class SiriReconciler {
  SiriReconciler({this.registerPush = SiriLink.registerPush});

  /// Tells the hosts that just connected where this phone's notices go.
  final Future<void> Function() registerPush;

  /// The hosts connected at the last refresh, and each one's connection
  /// number (it grows each time the host connects again).
  var _live = <String>{};
  final _connection = <String, int>{};

  /// Questions looked up without a verdict: on which connection.
  final _undecided = <String, (String, int)>{};

  /// Whether a scan runs, and whether a refresh came during it.
  var _scanning = false;
  var _again = false;
  late Future<dynamic> Function(String, Data) _call;

  /// For each peer `state` (each refresh): the questions to offer again.
  /// During a scan it returns nothing at once: that scan's caller gets
  /// what the extra pass finds.
  Future<List<Data>> onState(
    Data state,
    Future<dynamic> Function(String, Data) call,
  ) async {
    final connected = {
      for (final h in state['hosts'] as List? ?? const [])
        if (h['link'] == 'Connected') h['name'] as String,
    };
    final joined = connected.difference(_live);
    for (final h in joined) {
      _connection[h] = (_connection[h] ?? 0) + 1;
    }
    _live = connected;
    _call = call;
    if (joined.isNotEmpty) unawaited(registerPush());
    if (_scanning) {
      _again = true;
      return const [];
    }
    _scanning = true;
    final lost = <Data>[];
    try {
      do {
        _again = false;
        lost.addAll(await _pass());
      } while (_again);
    } finally {
      _scanning = false;
    }
    return lost;
  }

  /// One pass over a fresh snapshot of the questions.
  Future<List<Data>> _pass() async {
    final lost = <Data>[];
    for (final r in await SiriLink.openRequests()) {
      final id = r['requestId'] as String;
      final host = r['host'] as String;
      // The connections as they are now: they change during a pass.
      if (r['unconfirmed'] != true || !_live.contains(host)) continue;
      // Still being asked by Siri: the host may not know it yet.
      if (r['active'] == true) continue;
      final sentOn = (host, _connection[host]!);
      if (_undecided[id] == sentOn) continue;
      switch (await _lookUp(r, _call)) {
        case _Verdict.lost:
          lost.add(r);
          _undecided.remove(id);
        case _Verdict.settled:
          _undecided.remove(id);
        case _Verdict.undecided:
          _undecided[id] = sentOn;
      }
    }
    return lost;
  }
}

/// One Siri question's reply, exactly as the host kept it for that request
/// (§5.5), even after later turns in the project.
class SiriReplyPage extends StatefulWidget {
  const SiriReplyPage({
    super.key,
    required this.open,
    required this.call,
    required this.openChat,
  });

  /// `{requestId, host?, project?, name?, preview?}`.
  final Data open;
  final Future<dynamic> Function(String, Data) call;
  final void Function(String host, String project, String name) openChat;

  @override
  State<SiriReplyPage> createState() => _SiriReplyPageState();
}

class _SiriReplyPageState extends State<SiriReplyPage> {
  Data? result;
  String? problem;
  Timer? timer;

  String? get host => widget.open['host'] as String?;
  String? get project => widget.open['project'] as String?;
  String get name => widget.open['name'] as String? ?? 'Connecto';

  @override
  void initState() {
    super.initState();
    fetch();
    timer = Timer.periodic(const Duration(seconds: 2), (_) {
      if (result == null || result!['state'] == 'pending') fetch();
    });
  }

  @override
  void dispose() {
    timer?.cancel();
    super.dispose();
  }

  Future<void> fetch() async {
    if (host == null || project == null) {
      setState(() => problem = 'This reply is no longer on this phone.');
      return;
    }
    try {
      final v = await widget.call('siriResult', {
        'host': host,
        'project': project,
        'requestId': widget.open['requestId'],
      }) as Data?;
      if (mounted) {
        setState(() {
          result = v;
          problem = null;
        });
      }
    } catch (e) {
      if (mounted) setState(() => problem = e.toString());
    }
  }

  String get body {
    final r = result;
    final preview = widget.open['preview'] as String?;
    if (r == null) {
      return preview ?? (problem == null ? 'Loading the reply…' : '');
    }
    return switch (r['state']) {
      'answered' => r['reply'] as String? ?? '',
      'pending' => 'The agent is still working on it.',
      'uncertain' => 'Connecto is not sure the question was sent.',
      'needs-answer' =>
        'The agent is waiting for an answer: ${r['question'] ?? ''}',
      'needs-choice' => 'There is a choice to make in the chat.',
      'failed' => switch (r['why']) {
        'not_sent' => 'The question was probably not sent. Check the chat.',
        'session' => 'The session stopped before replying.',
        'timeout' => 'No reply came within six hours.',
        _ => 'The reply could not be found in the chat.',
      },
      'unknown' =>
        'The host no longer keeps this reply (it keeps them for a day).',
      'unsupported' => 'Update Connecto on the host to see Siri replies.',
      _ => preview ?? '',
    };
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: Text(name)),
    body: ListView(
      padding: const EdgeInsets.all(16),
      children: [
        if (problem != null && result == null) ...[
          Text(
            widget.open['preview'] == null
                ? problem!
                : 'The host is not reachable: this is the preview.',
            style: Theme.of(context).textTheme.bodySmall
                ?.copyWith(color: ConnectoColors.muted),
          ),
          const SizedBox(height: 12),
        ],
        SelectableText(body),
        const SizedBox(height: 20),
        if (host != null && project != null)
          OutlinedButton(
            onPressed: () => widget.openChat(host!, project!, name),
            child: const Text('Open the chat'),
          ),
      ],
    ),
  );
}
