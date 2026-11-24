import 'dart:async';
import 'dart:io';

import 'package:connecto_voice/connecto_voice.dart';
import 'package:desktop_drop/desktop_drop.dart';
import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:path_provider/path_provider.dart';

import 'board.dart';
import 'live_call.dart';
import 'settings.dart';
import 'siri.dart';
import 'composer.dart';
import 'peer.dart';
import 'theme.dart';
import 'widgets.dart';

void main() => runApp(const ConnectoApp());

/// A host's projects for the list: those with an agent working (a turn, or
/// background work after one) first, then the rest, each group in the host's
/// order. With [order], the ids as last shown: kept while the list is being
/// touched, so a row does not move under a finger. New projects go last.
List<Data> arrangeProjects(List projects, [List<String>? order]) {
  final all = [for (final p in projects) p as Data];
  if (order == null) {
    return [
      for (final p in all)
        if (p['working'] == true) p,
      for (final p in all)
        if (p['working'] != true) p,
    ];
  }
  int rank(Data p) {
    final at = order.indexOf(p['id'] as String);
    return at < 0 ? order.length : at;
  }

  // List.sort is not stable: the host's index breaks ties.
  final indexed = [for (var i = 0; i < all.length; i++) (i, all[i])];
  indexed.sort((a, b) {
    final byRank = rank(a.$2).compareTo(rank(b.$2));
    return byRank != 0 ? byRank : a.$1.compareTo(b.$1);
  });
  return [for (final (_, p) in indexed) p];
}

class ConnectoApp extends StatelessWidget {
  const ConnectoApp({super.key});
  @override
  Widget build(BuildContext context) => MaterialApp(
    title: 'Connecto',
    debugShowCheckedModeBanner: false,
    theme: connectoTheme(),
    home: const ProjectsPage(),
  );
}

class ProjectsPage extends StatefulWidget {
  const ProjectsPage({
    super.key,
    this.call = peer,
    this.supportDirectory = getApplicationSupportDirectory,
    this.voice = const PlatformVoice(),
  });

  final Future<dynamic> Function(String, Data) call;
  final Future<Directory> Function() supportDirectory;
  final VoiceEngine voice;
  @override
  State<ProjectsPage> createState() => _ProjectsState();
}

class _ProjectsState extends State<ProjectsPage> {
  Data? state;
  String? error;
  Timer? timer;
  bool polling = false, busy = false;
  final code = TextEditingController();
  final name = TextEditingController();

  /// Each host's project ids as last arranged. Not re-sorted while a pointer
  /// is down, so a row never moves away from a tap.
  Map<String, List<String>> order = {};
  int pointers = 0;
  @override
  void initState() {
    super.initState();
    start();
  }

  void arrange(Data next) {
    if (pointers > 0 && order.isNotEmpty) return;
    order = {
      for (final host in next['hosts'] as List? ?? [])
        host['name'] as String: [
          for (final p in arrangeProjects(host['projects'] as List))
            p['id'] as String,
        ],
    };
  }

  Future<void> start() async {
    try {
      final dir = await widget.supportDirectory();
      await widget.call('init', {'dir': '${dir.path}/remote'});
      if (!mounted) return;
      SiriLink.listen(siriOpen);
      await refresh();
      final pending = await SiriLink.takePending();
      if (pending != null && mounted) await siriOpen(pending);
      if (mounted) {
        timer = Timer.periodic(const Duration(seconds: 1), (_) => refresh());
      }
    } catch (e) {
      if (mounted) setState(() => error = e.toString());
    }
  }

  Future<void> refresh() async {
    if (polling) return;
    polling = true;
    try {
      final next = await widget.call('state', const {}) as Data;
      if (mounted) {
        setState(() {
          if (next['pairing']['phase'] == 'name' &&
              state?['pairing']['phase'] != 'name') {
            name.text = next['pairing']['name'];
          }
          state = next;
          arrange(next);
        });
        unawaited(SiriLink.state(next));
        unawaited(checkSiri(next));
      }
    } finally {
      polling = false;
    }
  }

  /// When the unconfirmed Siri questions of each connected host are checked.
  final siri = SiriReconciler();

  /// Once a host connects: what became of Siri questions whose sending was
  /// not confirmed. One the host never got is offered again, never resent.
  Future<void> checkSiri(Data next) async {
    final lost = await siri.onState(next, widget.call);
    for (final r in lost) {
      if (!mounted) return;
      final again = await showDialog<bool>(
        context: context,
        builder: (context) => AlertDialog(
          title: const Text('Question not sent'),
          content: Text(
            'Siri could not send this to ${r['name']}:\n\n"${r['text']}"',
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context, false),
              child: const Text('Dismiss'),
            ),
            FilledButton(
              onPressed: () => Navigator.pop(context, true),
              child: const Text('Send it again'),
            ),
          ],
        ),
      );
      if (again == true && mounted) {
        openChat(
          r['host'] as String,
          r['project'] as String,
          r['name'] as String,
          draft: r['text'] as String,
        );
      }
    }
  }

  void openChat(String host, String project, String title, {String? draft}) =>
      Navigator.push(
        context,
        MaterialPageRoute(
          builder: (_) => ChatPage(
            host: host,
            project: project,
            title: title,
            call: widget.call,
            voice: widget.voice,
            initialDraft: draft,
          ),
        ),
      );

  /// Shows what Siri asked for: a project's chat, a call in it, or one
  /// question's reply. True once shown.
  Future<bool> siriOpen(Data open) async {
    if (!mounted) return false;
    final host = open['host'] as String?;
    final project = open['project'] as String?;
    final name = open['name'] as String? ?? project ?? 'Connecto';
    Navigator.popUntil(context, (route) => route.isFirst);
    switch (open['kind']) {
      case 'reply':
        unawaited(
          Navigator.push(
            context,
            MaterialPageRoute(
              builder: (_) => SiriReplyPage(
                open: open,
                call: widget.call,
                openChat: openChat,
              ),
            ),
          ),
        );
      case 'call' when host != null && project != null:
        openChat(host, project, name);
        unawaited(
          openCall(
            context,
            title: name,
            call: widget.call,
            voice: widget.voice,
            target: {'host': host, 'project': project},
          ),
        );
      case _ when host != null && project != null:
        openChat(host, project, name);
      default:
        return false;
    }
    return true;
  }

  Future<void> act(String op, [Data args = const {}]) async {
    if (busy) return;
    setState(() {
      busy = true;
      error = null;
    });
    try {
      await widget.call(op, args);
      await refresh();
    } catch (e) {
      if (mounted) setState(() => error = e.toString());
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  Future<void> relay() async {
    String url = state?['relay'] ?? '', token = '';
    var hideToken = true;
    final ok = await showDialog<bool>(
      context: context,
      builder: (_) => StatefulBuilder(
        builder: (context, setDialogState) => AlertDialog(
          constraints: const BoxConstraints(maxWidth: 420),
          insetPadding: const EdgeInsets.symmetric(
            horizontal: 16,
            vertical: 24,
          ),
          scrollable: true,
          title: const Text('Relay settings'),
          titleTextStyle: Theme.of(context).textTheme.titleLarge,
          content: SizedBox(
            width: 420,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Text(
                  'Relay URL',
                  style: Theme.of(context).textTheme.labelLarge,
                ),
                const SizedBox(height: 6),
                TextFormField(
                  initialValue: url,
                  onChanged: (s) => url = s,
                  keyboardType: TextInputType.url,
                  decoration: const InputDecoration(
                    hintText: 'https://relay.connecto.studio',
                    helperText: 'Leave empty to use the default relay.',
                    helperMaxLines: 2,
                  ),
                ),
                const SizedBox(height: 16),
                Text(
                  'Access token (optional)',
                  style: Theme.of(context).textTheme.labelLarge,
                ),
                const SizedBox(height: 6),
                TextField(
                  onChanged: (s) => token = s,
                  obscureText: hideToken,
                  decoration: InputDecoration(
                    suffixIcon: IconButton(
                      tooltip: hideToken ? 'Show token' : 'Hide token',
                      onPressed: () =>
                          setDialogState(() => hideToken = !hideToken),
                      icon: Icon(
                        hideToken
                            ? Icons.visibility_outlined
                            : Icons.visibility_off_outlined,
                      ),
                    ),
                  ),
                ),
              ],
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context),
              child: const Text('Cancel'),
            ),
            FilledButton(
              onPressed: () => Navigator.pop(context, true),
              child: const Text('Save'),
            ),
          ],
        ),
      ),
    );
    if (ok == true) {
      await act('relay', {
        'url': url.trim(),
        if (token.isNotEmpty) 'token': token,
      });
    }
  }

  @override
  void dispose() {
    timer?.cancel();
    code.dispose();
    name.dispose();
    super.dispose();
  }

  void pair() {
    code.clear();
    act('pair');
  }

  /// Asks before forgetting a saved host; other pairings are kept.
  Future<void> forget(String hostName) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        constraints: const BoxConstraints(maxWidth: 420),
        title: Text('Remove $hostName?'),
        titleTextStyle: Theme.of(context).textTheme.titleLarge,
        content: Text(
          'This device forgets $hostName and its pairing. To use it again, '
          'pair it again from Pair a device on that host. Other hosts stay '
          'paired.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            style: FilledButton.styleFrom(
              backgroundColor: ConnectoColors.coral,
              foregroundColor: ConnectoColors.bg,
            ),
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Remove host'),
          ),
        ],
      ),
    );
    if (ok == true) await act('forget', {'host': hostName});
  }

  Widget hostCard(Data host) {
    final hostName = host['name'] as String;
    final link = host['link'] as String;
    final connected = link == 'Connected';
    final projects = arrangeProjects(host['projects'] as List, order[hostName]);
    final tone = connected
        ? StatusTone.ok
        : link.toLowerCase().contains('fail')
        ? StatusTone.error
        : StatusTone.neutral;
    return SectionCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(14, 10, 12, 10),
            child: Row(
              children: [
                const Icon(
                  Icons.computer_outlined,
                  color: ConnectoColors.muted,
                  size: 18,
                ),
                const SizedBox(width: 10),
                Expanded(
                  child: Wrap(
                    crossAxisAlignment: WrapCrossAlignment.center,
                    spacing: 10,
                    runSpacing: 5,
                    children: [
                      Text(
                        hostName,
                        style: Theme.of(context).textTheme.titleMedium,
                      ),
                      StatusPill(
                        link,
                        tone,
                        semanticsLabel: 'Host $hostName: $link',
                      ),
                    ],
                  ),
                ),
                if (!connected) ...[
                  const SizedBox(width: 8),
                  OutlinedButton(
                    onPressed: busy
                        ? null
                        : () => act('connect', {'host': hostName}),
                    child: const Text('Connect'),
                  ),
                ],
                const SizedBox(width: 4),
                MenuAnchor(
                  alignmentOffset: const Offset(-120, 4),
                  menuChildren: [
                    MenuItemButton(
                      leadingIcon: const Icon(
                        Icons.link_off,
                        size: 18,
                        color: ConnectoColors.coral,
                      ),
                      onPressed: busy ? null : () => forget(hostName),
                      child: const Text(
                        'Remove host…',
                        style: TextStyle(color: ConnectoColors.coral),
                      ),
                    ),
                  ],
                  builder: (context, menu, _) => IconButton(
                    tooltip: 'Options for $hostName',
                    onPressed: () => menu.isOpen ? menu.close() : menu.open(),
                    icon: const Icon(
                      Icons.more_horiz,
                      color: ConnectoColors.muted,
                    ),
                  ),
                ),
              ],
            ),
          ),
          if (projects.isEmpty) ...[
            const Divider(),
            Padding(
              padding: const EdgeInsets.all(14),
              child: Text(
                connected
                    ? 'Open a project in Connecto on this host.'
                    : 'Connect to this host to see its projects.',
                style: Theme.of(context).textTheme.bodySmall,
              ),
            ),
          ],
          for (final project in projects) ...[
            const Divider(),
            RowAction(
              onTap: connected
                  ? () => Navigator.push(
                      context,
                      MaterialPageRoute(
                        builder: (_) => ChatPage(
                          host: hostName,
                          project: project['id'],
                          title: project['name'],
                          call: widget.call,
                        ),
                      ),
                    )
                  : null,
              child: Opacity(
                opacity: connected ? 1 : 0.55,
                child: Row(
                  children: [
                    const Icon(
                      Icons.folder_outlined,
                      color: ConnectoColors.muted,
                      size: 18,
                    ),
                    const SizedBox(width: 10),
                    Expanded(child: Text(project['name'])),
                    if (connected && project['working'] == true) ...[
                      const SizedBox(width: 8),
                      StatusPill(
                        'Working',
                        StatusTone.busy,
                        semanticsLabel: '${project['name']}: agent is working',
                      ),
                    ],
                    const Icon(
                      Icons.chevron_right,
                      color: ConnectoColors.muted,
                      size: 18,
                    ),
                  ],
                ),
              ),
            ),
          ],
        ],
      ),
    );
  }

  Widget pairingCard(Data phase) {
    void submitCode() => act('code', {'code': code.text});
    void submitName() => act('name', {'name': name.text});
    final phaseName = phase['phase'] as String;
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 440),
      child: SectionCard(
        child: Padding(
          padding: const EdgeInsets.all(20),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Text(
                'Pair a host',
                style: Theme.of(context).textTheme.titleLarge,
              ),
              const SizedBox(height: 12),
              if (phaseName == 'code') ...[
                const Text(
                  'Open Pair a device in Connecto on your host. Enter this code there.',
                ),
                const SizedBox(height: 12),
                DecoratedBox(
                  decoration: BoxDecoration(
                    color: ConnectoColors.surface,
                    border: Border.all(color: ConnectoColors.borderStrong),
                    borderRadius: BorderRadius.circular(8),
                  ),
                  child: Padding(
                    padding: const EdgeInsets.fromLTRB(12, 4, 4, 4),
                    child: Row(
                      children: [
                        Expanded(
                          child: FittedBox(
                            alignment: Alignment.centerLeft,
                            fit: BoxFit.scaleDown,
                            child: SelectableText(
                              phase['code'],
                              style: monoStyle.copyWith(
                                fontSize: 34,
                                letterSpacing: 6,
                              ),
                            ),
                          ),
                        ),
                        IconButton(
                          tooltip: 'Copy code',
                          onPressed: () => Clipboard.setData(
                            ClipboardData(text: phase['code']),
                          ),
                          icon: const Icon(Icons.copy_outlined),
                        ),
                      ],
                    ),
                  ),
                ),
                const SizedBox(height: 6),
                Text(
                  'Expires in ${phase['seconds']} s',
                  style: Theme.of(context).textTheme.bodySmall,
                ),
                const SizedBox(height: 16),
                TextField(
                  controller: code,
                  style: monoStyle,
                  keyboardType: TextInputType.number,
                  inputFormatters: [
                    FilteringTextInputFormatter.digitsOnly,
                    LengthLimitingTextInputFormatter(8),
                  ],
                  onSubmitted: busy ? null : (_) => submitCode(),
                  decoration: const InputDecoration(
                    labelText: 'Code shown on host',
                  ),
                ),
                const SizedBox(height: 12),
                FilledButton(
                  onPressed: busy ? null : submitCode,
                  child: const Text('Connect'),
                ),
                if (phase['waiting'] == true) ...[
                  const SizedBox(height: 10),
                  Row(
                    children: [
                      const SizedBox.square(
                        dimension: 14,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      ),
                      const SizedBox(width: 8),
                      Expanded(
                        child: Text(
                          'Waiting for the host to confirm…',
                          style: Theme.of(context).textTheme.bodySmall,
                        ),
                      ),
                    ],
                  ),
                ],
              ] else if (phaseName == 'name') ...[
                TextField(
                  controller: name,
                  onSubmitted: busy ? null : (_) => submitName(),
                  decoration: const InputDecoration(labelText: 'Host name'),
                ),
                const SizedBox(height: 12),
                FilledButton(
                  onPressed: busy ? null : submitName,
                  child: const Text('Finish pairing'),
                ),
              ] else if (phaseName == 'failed')
                NoticeBanner(phase['message'])
              else
                Text(phaseName),
              const SizedBox(height: 10),
              Align(
                alignment: Alignment.centerLeft,
                child: TextButton(
                  onPressed: busy ? null : () => act('cancel'),
                  child: const Text('Close pairing'),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final phase = state?['pairing'] as Data? ?? {};
    final pairing = phase['phase'] != null && phase['phase'] != 'idle';
    final hosts = state?['hosts'] as List? ?? [];
    final problem = error ?? state?['error'] as String?;
    final loading = state == null && problem == null;
    final empty = !pairing && state != null && hosts.isEmpty;
    return Scaffold(
      appBar: AppHeader(
        title: 'Connecto',
        leading: const BrandMark(20),
        actions: [
          IconButton(
            tooltip: 'Settings',
            onPressed: () => Navigator.push(
              context,
              MaterialPageRoute(
                builder: (_) => SettingsPage(
                  voice: widget.voice,
                  relay: () async {
                    if (!busy) await relay();
                  },
                ),
              ),
            ),
            icon: const Icon(Icons.settings_outlined),
          ),
        ],
      ),
      body: SafeArea(
        top: false,
        child: Column(
          children: [
            if (loading) const LinearProgressIndicator(),
            Expanded(
              child: Center(
                child: ConstrainedBox(
                  constraints: const BoxConstraints(maxWidth: 720),
                  child: LayoutBuilder(
                    builder: (context, constraints) {
                      final inset = constraints.maxWidth >= 600 ? 24.0 : 16.0;
                      return CustomScrollView(
                        slivers: [
                          SliverPadding(
                            padding: EdgeInsets.fromLTRB(inset, 18, inset, 0),
                            sliver: SliverList.list(
                              children: [
                                if (!pairing)
                                  Row(
                                    children: [
                                      Expanded(
                                        child: Text(
                                          'Projects',
                                          style: Theme.of(context)
                                              .textTheme
                                              .titleLarge,
                                        ),
                                      ),
                                      if (hosts.isNotEmpty)
                                        FilledButton.tonalIcon(
                                          onPressed: busy ? null : pair,
                                          icon: const Icon(
                                            Icons.add_link,
                                            size: 17,
                                          ),
                                          label: const Text('Pair a host'),
                                        ),
                                    ],
                                  ),
                                if (problem != null) ...[
                                  const SizedBox(height: 14),
                                  NoticeBanner(
                                    problem,
                                    action: state == null
                                        ? TextButton(
                                            onPressed: () {
                                              setState(() => error = null);
                                              start();
                                            },
                                            child: const Text('Retry'),
                                          )
                                        : null,
                                  ),
                                ],
                                if (!pairing)
                                  for (final host in hosts)
                                    Padding(
                                      padding: const EdgeInsets.only(top: 14),
                                      // A finger on a card holds the
                                      // order (see [arrange]).
                                      child: Listener(
                                        onPointerDown: (_) => pointers++,
                                        onPointerUp: (_) =>
                                            pointers > 0 ? pointers-- : null,
                                        onPointerCancel: (_) =>
                                            pointers > 0 ? pointers-- : null,
                                        child: hostCard(host as Data),
                                      ),
                                    ),
                                const SizedBox(height: 14),
                              ],
                            ),
                          ),
                          if (pairing || empty)
                            SliverFillRemaining(
                              hasScrollBody: false,
                              child: Padding(
                                padding: EdgeInsets.fromLTRB(
                                  inset,
                                  0,
                                  inset,
                                  24,
                                ),
                                child: Center(
                                  child: pairing
                                      ? pairingCard(phase)
                                      : Column(
                                          mainAxisSize: MainAxisSize.min,
                                          children: [
                                            const BrandMark(48),
                                            const SizedBox(height: 14),
                                            Text(
                                              'Pair the computer where your projects and agents run.',
                                              textAlign: TextAlign.center,
                                              style: Theme.of(context)
                                                  .textTheme
                                                  .bodySmall,
                                            ),
                                            const SizedBox(height: 14),
                                            FilledButton.icon(
                                              onPressed: busy ? null : pair,
                                              icon: const Icon(
                                                Icons.add_link,
                                                size: 17,
                                              ),
                                              label: const Text('Pair a host'),
                                            ),
                                          ],
                                        ),
                                ),
                              ),
                            ),
                        ],
                      );
                    },
                  ),
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

Future<List<String>> pickWithSystemPanel() async => [
  for (final file in await openFiles(confirmButtonText: 'Attach'))
    if (file.path.isNotEmpty) file.path,
];

class ChatPage extends StatefulWidget {
  final String host, project, title;
  final Future<dynamic> Function(String, Data) call;
  final Future<List<String>> Function() pickFiles;
  final VoiceEngine voice;
  const ChatPage({
    super.key,
    required this.host,
    required this.project,
    required this.title,
    this.call = peer,
    this.pickFiles = pickWithSystemPanel,
    this.voice = const PlatformVoice(),
    this.initialDraft,
  });

  /// Put in the composer, unsent: a Siri question the host never got.
  final String? initialDraft;
  @override
  State<ChatPage> createState() => _ChatState();
}

class _ChatState extends State<ChatPage> {
  final draft = TextEditingController();
  final composerFocus = FocusNode();
  final scroll = ScrollController();
  final composerKey = GlobalKey<ComposerState>();
  bool dragging = false;
  Data? chat;
  String? error, connectionError;
  bool polling = false, busy = false;
  Timer? timer;
  DateTime? creating;
  String? creatingTool;

  /// The agent picked in the chooser's first step, and what the host offers
  /// for it: `null` while asked.
  String? pickedTool;
  Data? toolModels;
  String? modelsError;
  final modelFilter = TextEditingController();

  /// Whether this device can hold a voice call (Apple, OS 26 or later).
  VoiceSupport? voice;
  Data get target => {'host': widget.host, 'project': widget.project};
  @override
  void initState() {
    super.initState();
    if (widget.initialDraft case final text?) draft.text = text;
    widget.voice.support().then((v) {
      if (mounted) setState(() => voice = v);
    });
    refresh();
    timer = Timer.periodic(const Duration(milliseconds: 750), (_) => refresh());
  }

  Future<void> refresh() async {
    if (polling) return;
    polling = true;
    try {
      final data = await widget.call('chat', target) as Data;
      if (!mounted) return;
      setState(() {
        connectionError = null;
        chat = data;
        if (data['status'] != 'New') {
          creating = null;
          pickedTool = null;
        }
        if (creating != null &&
            DateTime.now().difference(creating!).inSeconds >= 15) {
          creating = null;
          error =
              'The host has not listed the session. Check the host and retry.';
        }
      });
    } catch (e) {
      if (mounted) setState(() => connectionError = e.toString());
    } finally {
      polling = false;
    }
  }

  /// Runs an op on the app session; true when the host accepted it.
  Future<bool> act(String op, [Data args = const {}]) async {
    if (busy) return false;
    setState(() {
      busy = true;
      error = null;
    });
    try {
      await widget.call(op, {...target, 'id': chat?['id'], ...args});
      if (!mounted) return true;
      if (op == 'send' && draft.text == args['text']) draft.clear();
      if (op == 'create') creating = DateTime.now();
      await refresh();
      return true;
    } catch (e) {
      if (mounted) setState(() => error = e.toString());
      return false;
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  void dispose() {
    timer?.cancel();
    modelFilter.dispose();
    composerFocus.dispose();
    draft.dispose();
    scroll.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final status = chat?['status'] as String?;
    final chatReady = chat?['chatReady'] == true;
    final hostError = chat?['error'] as String?;
    final problem =
        connectionError ??
        (chatReady ? error ?? hostError : hostError ?? error);
    final question = chatReady ? (chat?['question'] as String?) : null;
    final textAnswer = chat?['textAnswer'] == true;
    final enabled = !busy && connectionError == null;
    final canSend =
        enabled &&
        chatReady &&
        (question == null || textAnswer) &&
        (status == 'Ready' || status == 'Needs an answer');
    final displayStatus =
        !chatReady && (status == 'Ready' || status == 'Needs an answer')
        ? (problem == null ? 'Connecting to chat…' : 'Chat unavailable')
        : status;
    final pill = connectionError != null ? 'Offline' : displayStatus;
    final provider = chat?['provider'] as String?;
    final messages = chat?['messages'] as List? ?? [];
    final limits = AttachLimits.from(chat?['attach']);
    final hint = switch (status) {
      _ when connectionError != null => 'Offline',
      _ when !chatReady => 'Connecting to chat…',
      'Running' => 'Agent is working…',
      'Stopped' || 'Failed' => 'Start the session to chat',
      _ when question != null && !textAnswer => 'Choose an option above',
      'Needs an answer' => 'Your answer',
      _ => 'Message',
    };
    return Scaffold(
      appBar: AppHeader(
        back: true,
        title: widget.title,
        subtitle: [widget.host, ?provider].join(' · '),
        compact: MediaQuery.sizeOf(context).width <= 420,
        status: pill == null
            ? null
            : StatusPill(pill, switch (pill) {
                'Ready' => StatusTone.ok,
                'Running' || 'Connecting to chat…' => StatusTone.busy,
                'Needs an answer' => StatusTone.attention,
                'Chat unavailable' || 'Failed' => StatusTone.error,
                _ => StatusTone.neutral,
              }, semanticsLabel: 'Session status: $pill'),
        actions: [
          if (voice?.available == true && chatReady)
            IconButton(
              tooltip: 'Call the agent',
              onPressed: enabled ? startCall : null,
              icon: const Icon(Icons.call_outlined),
            ),
          if (chat?['id'] != null)
            IconButton(
              tooltip: 'Tasks',
              onPressed: connectionError == null ? openBoard : null,
              icon: const Icon(Icons.view_agenda_outlined),
            ),
          if (chat?['id'] != null)
            IconButton(
              tooltip: 'New session',
              onPressed: enabled ? clearSession : null,
              icon: const Icon(Icons.restart_alt),
            ),
          if (status == 'Running' ||
              status == 'Stopped' ||
              status == 'Failed') ...[
            if (status == 'Running')
              OutlinedButton(
                onPressed: enabled ? () => act('stop') : null,
                child: const Text('Stop'),
              )
            else
              OutlinedButton(
                onPressed: enabled ? () => act('resume') : null,
                child: const Text('Start session'),
              ),
          ],
        ],
      ),
      body: dropZone(
        limits: limits,
        blocker: status == 'New' || status == null
            ? 'Choose an agent before attaching files.'
            : attachBlocker(
                limits: limits,
                canSend: canSend,
                answering: question != null,
              ),
        child: SafeArea(
          top: false,
          child: Center(
            child: ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 760),
              child: Padding(
                padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    if (problem != null) ...[
                      NoticeBanner(
                        problem,
                        title: connectionError == null
                            ? null
                            : "Can't reach the host",
                      ),
                      const SizedBox(height: 10),
                    ],
                    if (status == null && problem == null)
                      const LinearProgressIndicator(),
                    if (status == 'New')
                      Expanded(child: agentOptions(enabled))
                    else if (status != null || connectionError != null) ...[
                      Expanded(
                        child: LayoutBuilder(
                          builder: (context, constraints) => Column(
                            crossAxisAlignment: CrossAxisAlignment.stretch,
                            children: [
                              Expanded(
                                child: transcript(
                                  messages,
                                  provider,
                                  chatReady,
                                ),
                              ),
                              if (question != null)
                                ConstrainedBox(
                                  constraints: BoxConstraints(
                                    maxHeight: constraints.maxHeight * 0.4,
                                  ),
                                  child: questionPanel(
                                    question,
                                    textAnswer,
                                    enabled,
                                  ),
                                ),
                            ],
                          ),
                        ),
                      ),
                      const SizedBox(height: 8),
                      Composer(
                        key: composerKey,
                        draft: draft,
                        focusNode: composerFocus,
                        canSend: canSend,
                        busy: busy,
                        hint: hint,
                        limits: limits,
                        answering: question != null,
                        session: chat?['id'] as String?,
                        pickFiles: widget.pickFiles,
                        onSend: (tokens) => act('send', {
                          'text': draft.text,
                          'question': question,
                          if (tokens.isNotEmpty) 'attachments': tokens,
                        }),
                        stage: (path, session) async => await widget.call(
                          'attach',
                          {...target, 'id': session, 'path': path},
                        ) as Data,
                        discard: (token, session) async {
                          try {
                            await widget.call('detach', {
                              ...target,
                              'id': session,
                              'token': token,
                            });
                          } catch (_) {
                            // Best effort: the staged copy is cleared on the next start.
                          }
                        },
                      ),
                    ],
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  /// Closes the app session (the agent and its conversation) after asking;
  /// the agent chooser then starts a new one.
  Future<void> clearSession() async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Start a new session?'),
        content: const Text(
          "This closes the project's app session on the host, with the "
          'terminals it started, and clears its conversation.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('New session'),
          ),
        ],
      ),
    );
    if (ok != true || !mounted) return;
    if (await act('clear') && mounted) {
      setState(() {
        pickedTool = null;
        toolModels = null;
        draft.clear();
      });
    }
  }

  /// The project's task board, which its session keeps.
  void openBoard() => Navigator.push(
    context,
    MaterialPageRoute(
      builder: (_) =>
          BoardPage(title: widget.title, call: widget.call, target: target),
    ),
  );

  /// A voice session for one task: worked out by voice, then handed to this
  /// project's session.
  /// Through Gemini Live when it is set up in Settings (ADR 0034).
  void startCall() => openCall(
    context,
    title: widget.title,
    call: widget.call,
    voice: widget.voice,
    target: target,
  );

  bool get desktop => switch (Theme.of(context).platform) {
    TargetPlatform.macOS ||
    TargetPlatform.windows ||
    TargetPlatform.linux => true,
    _ => false,
  };

  /// Files dropped anywhere on the chat, and Cmd/Ctrl+U, go to the composer.
  Widget dropZone({
    required AttachLimits? limits,
    required String? blocker,
    required Widget child,
  }) {
    final body = CallbackShortcuts(
      bindings: {
        const SingleActivator(LogicalKeyboardKey.keyU, meta: true): () =>
            composerKey.currentState?.pick(),
        const SingleActivator(LogicalKeyboardKey.keyU, control: true): () =>
            composerKey.currentState?.pick(),
      },
      child: Stack(
        children: [
          child,
          if (dragging)
            Positioned.fill(
              child: DropOverlay(limits: limits, blocker: blocker),
            ),
        ],
      ),
    );
    if (!desktop) return body;
    return DropTarget(
      onDragEntered: (_) => setState(() => dragging = true),
      onDragExited: (_) => setState(() => dragging = false),
      onDragDone: (details) {
        setState(() => dragging = false);
        final paths = [
          for (final file in details.files)
            if (file.path.isNotEmpty) file.path,
        ];
        final composer = composerKey.currentState;
        if (composer == null) {
          setState(() => error = blocker);
        } else {
          composer.addPaths(paths);
        }
      },
      child: body,
    );
  }

  static const agents = ['Claude', 'Codex', 'Pi', 'Antigravity', 'OpenCode'];

  /// The agents to offer: those the host project's Tools setting allows,
  /// when the host says (`agents`); every one from a host that does not.
  List<String> get offeredAgents => switch (chat?['agents']) {
    final List allowed => [
      for (final a in agents)
        if (allowed.contains(a)) a,
    ],
    _ => agents,
  };

  /// The host project's settings are still on their way: no agent yet.
  bool get agentsLoading => chat?['agentsLoading'] == true;

  /// The chooser's first step: the agent. Its models are asked for at once.
  Future<void> pickAgent(String tool) async {
    setState(() {
      pickedTool = tool;
      toolModels = null;
      modelsError = null;
      modelFilter.clear();
    });
    try {
      final models = await widget.call('models', {
        ...target,
        'tool': tool.toLowerCase(),
      }) as Data;
      if (mounted && pickedTool == tool) setState(() => toolModels = models);
    } catch (e) {
      // The default stays available: the host starts the agent on its own.
      if (mounted && pickedTool == tool) {
        setState(() {
          modelsError = e.toString();
          toolModels = const {'supported': false, 'models': []};
        });
      }
    }
  }

  /// The second step: create on [model], or the agent's default for `null`.
  void create(String tool, String? model) {
    creatingTool = model == null ? tool : '$tool ($model)';
    act('create', {'tool': tool.toLowerCase(), 'model': ?model});
  }

  Widget agentOptions(bool enabled) => switch (pickedTool) {
    null => agentStep(enabled),
    final tool => modelStep(tool, enabled),
  };

  Widget optionCard(String label, VoidCallback? onTap, {String? detail}) =>
      SectionCard(
        child: RowAction(
          onTap: onTap,
          child: Row(
            children: [
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(label, style: Theme.of(context).textTheme.titleSmall),
                    if (detail != null)
                      Text(
                        detail,
                        style: Theme.of(context).textTheme.bodySmall,
                      ),
                  ],
                ),
              ),
              const Icon(
                Icons.chevron_right,
                size: 16,
                color: ConnectoColors.muted,
              ),
            ],
          ),
        ),
      );

  Widget agentStep(bool enabled) => ListView(
    padding: const EdgeInsets.only(top: 14),
    children: [
      Text('Choose an agent', style: Theme.of(context).textTheme.titleLarge),
      const SizedBox(height: 6),
      const Text('This project will use the same agent each time you open it.'),
      if (agentsLoading) ...[
        const SizedBox(height: 8),
        const Text("Loading the project's settings from its host…"),
      ] else if (offeredAgents.isEmpty) ...[
        const SizedBox(height: 8),
        const Text("This project's settings on its host allow no agent."),
      ],
      const SizedBox(height: 16),
      LayoutBuilder(
        builder: (context, constraints) {
          final columns = constraints.maxWidth >= 560
              ? 4
              : constraints.maxWidth >= 300
              ? 2
              : 1;
          return Wrap(
            spacing: 8,
            runSpacing: 8,
            children: [
              for (final tool in offeredAgents)
                SizedBox(
                  width: (constraints.maxWidth - 8 * (columns - 1)) / columns,
                  child: optionCard(
                    tool,
                    enabled && creating == null && !agentsLoading
                        ? () => pickAgent(tool)
                        : null,
                  ),
                ),
            ],
          );
        },
      ),
    ],
  );

  Widget modelStep(String tool, bool enabled) {
    final offered = toolModels;
    final models = [for (final m in offered?['models'] as List? ?? []) '$m'];
    final hostDefault = offered?['default'] as String?;
    final query = modelFilter.text.trim().toLowerCase();
    final shown = [
      for (final m in models)
        if (query.isEmpty || m.toLowerCase().contains(query)) m,
    ];
    final canPick = enabled && creating == null && offered != null;
    return ListView(
      padding: const EdgeInsets.only(top: 6),
      children: [
        Align(
          alignment: Alignment.centerLeft,
          child: TextButton.icon(
            onPressed: creating == null
                ? () => setState(() => pickedTool = null)
                : null,
            icon: const Icon(Icons.arrow_back, size: 16),
            label: const Text('Change agent'),
          ),
        ),
        const SizedBox(height: 4),
        Text(
          'Choose a model for $tool',
          style: Theme.of(context).textTheme.titleLarge,
        ),
        const SizedBox(height: 6),
        const Text('Default lets the agent use its own model.'),
        const SizedBox(height: 16),
        if (offered == null) ...[
          const LinearProgressIndicator(),
          const SizedBox(height: 6),
          Text(
            'Asking the host for $tool models…',
            style: Theme.of(context).textTheme.bodySmall,
          ),
          const SizedBox(height: 12),
        ],
        if (modelsError != null) ...[
          NoticeBanner(
            "The host's models could not be loaded: $modelsError",
            tone: NoticeTone.warn,
          ),
          const SizedBox(height: 12),
        ] else if (offered != null && offered['supported'] != true) ...[
          const NoticeBanner(
            'This host offers only the default model. Update Connecto on the '
            'host to choose one.',
            tone: NoticeTone.info,
          ),
          const SizedBox(height: 12),
        ],
        optionCard(
          'Default',
          canPick ? () => create(tool, null) : null,
          detail: hostDefault == null
              ? "The agent's own default"
              : 'Currently $hostDefault',
        ),
        if (models.length > 8) ...[
          const SizedBox(height: 12),
          TextField(
            controller: modelFilter,
            onChanged: (_) => setState(() {}),
            decoration: const InputDecoration(
              hintText: 'Filter models',
              prefixIcon: Icon(Icons.search, size: 18),
            ),
          ),
        ],
        for (final model in shown) ...[
          const SizedBox(height: 8),
          optionCard(model, canPick ? () => create(tool, model) : null),
        ],
        if (models.isNotEmpty && shown.isEmpty) ...[
          const SizedBox(height: 8),
          Text(
            'No model matches "${modelFilter.text.trim()}".',
            style: Theme.of(context).textTheme.bodySmall,
          ),
        ],
        if (creating != null) ...[
          const SizedBox(height: 14),
          const LinearProgressIndicator(),
          const SizedBox(height: 6),
          Text(
            'Starting $creatingTool…',
            style: Theme.of(context).textTheme.bodySmall,
          ),
        ],
      ],
    );
  }

  Widget transcript(List messages, String? provider, bool chatReady) {
    if (messages.isEmpty && chatReady) {
      return Center(
        child: Text(
          'No messages yet',
          style: Theme.of(context).textTheme.bodySmall,
        ),
      );
    }
    return LayoutBuilder(
      builder: (context, constraints) => Opacity(
        opacity: chatReady ? 1 : 0.6,
        // Built from the newest message up: the chat opens at its last line,
        // and stays there as messages arrive or grow while the person is at
        // the bottom.
        child: ListView.builder(
          controller: scroll,
          reverse: true,
          padding: const EdgeInsets.only(top: 8, bottom: 8),
          itemCount: messages.length,
          itemBuilder: (context, reversed) {
            final index = messages.length - 1 - reversed;
            final message = messages[index];
            final fromUser = message[0] == true;
            final startsTurn = index == 0 || messages[index - 1][0] != fromUser;
            final content = SelectableText(
              chatDisplayText('${message[1]}', fromUser: fromUser),
            );
            return Padding(
              padding: EdgeInsets.only(
                top: index == 0 ? 0 : (startsTurn ? 16 : 6),
              ),
              child: fromUser
                  ? Align(
                      alignment: Alignment.centerRight,
                      child: ConstrainedBox(
                        constraints: BoxConstraints(
                          maxWidth: constraints.maxWidth * 0.78,
                        ),
                        child: Container(
                          padding: const EdgeInsets.symmetric(
                            horizontal: 13,
                            vertical: 9,
                          ),
                          decoration: BoxDecoration(
                            color: ConnectoColors.userBubble,
                            border: Border.all(
                              color: ConnectoColors.userBorder,
                            ),
                            borderRadius: BorderRadius.circular(12),
                          ),
                          child: content,
                        ),
                      ),
                    )
                  : Column(
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: [
                        if (startsTurn && provider != null)
                          Padding(
                            padding: const EdgeInsets.only(bottom: 4),
                            child: Text(
                              provider,
                              style: Theme.of(context).textTheme.labelMedium
                                  ?.copyWith(color: ConnectoColors.faint),
                            ),
                          ),
                        content,
                      ],
                    ),
            );
          },
        ),
      ),
    );
  }

  Widget questionPanel(
    String question,
    bool textAnswer,
    bool enabled,
  ) => SectionCard(
    child: DecoratedBox(
      decoration: const BoxDecoration(
        border: Border(left: BorderSide(color: ConnectoColors.amber, width: 3)),
      ),
      child: Padding(
        padding: const EdgeInsets.fromLTRB(15, 10, 12, 10),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(
              'Needs an answer',
              style: Theme.of(context).textTheme.titleSmall
                  ?.copyWith(color: ConnectoColors.amber),
            ),
            const SizedBox(height: 6),
            Flexible(
              child: SingleChildScrollView(
                child: SelectableText(question, style: monoStyle),
              ),
            ),
            if (!textAnswer) ...[
              const SizedBox(height: 8),
              Wrap(
                spacing: 8,
                runSpacing: 8,
                children: [
                  for (final item in [
                    ('up', '↑'),
                    ('down', '↓'),
                    if (question.toLowerCase().contains('tab')) ('tab', 'Tab'),
                    if (question.toLowerCase().contains('space'))
                      ('space', 'Select'),
                    ('enter', 'Confirm'),
                    ('escape', 'Cancel'),
                  ])
                    questionKey(item, question, enabled),
                ],
              ),
            ],
          ],
        ),
      ),
    ),
  );

  Widget questionKey((String, String) item, String question, bool enabled) {
    final onPressed = enabled
        ? () => act('key', {'key': item.$1, 'question': question})
        : null;
    final label = Text(item.$2);
    const style = ButtonStyle(tapTargetSize: MaterialTapTargetSize.shrinkWrap);
    return item.$1 == 'enter'
        ? FilledButton(onPressed: onPressed, style: style, child: label)
        : OutlinedButton(onPressed: onPressed, style: style, child: label);
  }
}
