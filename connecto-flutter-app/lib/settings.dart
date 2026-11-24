import 'dart:async';

import 'package:connecto_voice/connecto_voice.dart';
import 'package:flutter/material.dart';
import 'package:package_info_plus/package_info_plus.dart';

import 'live_call.dart';
import 'theme.dart';
import 'widgets.dart';

/// The commit the app was built from, when the build passed one.
const buildCommit = String.fromEnvironment('CONNECTO_COMMIT');

/// "0.2.16 (3) · 7c9f035": the installed version, build and commit.
Future<String> appVersion() async {
  try {
    final info = await PackageInfo.fromPlatform();
    return [
      '${info.version} (${info.buildNumber})',
      if (buildCommit.isNotEmpty) buildCommit,
    ].join(' · ');
  } catch (_) {
    return buildCommit.isEmpty ? 'Unknown' : buildCommit;
  }
}

/// The app's settings: the relay (from the projects list), the language
/// calls use, and the installed version.
class SettingsPage extends StatefulWidget {
  const SettingsPage({
    super.key,
    required this.voice,
    this.relay,
    this.chooseLanguage,
    this.version = appVersion,
    this.live = const PlatformLiveAudio(),
    this.validateKey = validateLiveKey,
  });
  final VoiceEngine voice;

  /// The Gemini Live key (in the Keychain) and switch (ADR 0034).
  final LiveKeyStore live;

  /// Opens a Live session with a key: null when it works, else why not.
  final Future<String?> Function(String key) validateKey;

  /// Opens the relay settings; absent where they do not apply.
  final Future<void> Function()? relay;

  /// Applies a chosen language (null: the device's); by default it is only
  /// saved for the next call. A call switches to it at once.
  final Future<void> Function(String?)? chooseLanguage;
  final Future<String> Function() version;
  @override
  State<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends State<SettingsPage> {
  VoiceLanguages? languages;
  String? version;
  PhoneModelState phone = PhoneModelState.none;
  bool _watchingPhone = false;

  /// The Gemini Live settings: whether a key is saved and calls use it,
  /// and the last test's result while it runs or once it is in.
  ({bool hasKey, bool enabled})? live;
  String? liveTest;
  bool testing = false;

  @override
  void initState() {
    super.initState();
    _load();
    widget.version().then((v) {
      if (mounted) setState(() => version = v);
    });
  }

  Future<void> _loadLive() async {
    final found = await widget.live.settings();
    if (mounted) setState(() => live = found);
  }

  /// Asks for a key (never shown back), saves it in the Keychain and tests
  /// it; an empty entry removes it.
  Future<void> _editKey() async {
    final field = TextEditingController();
    final entered = await showDialog<String>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Gemini API key'),
        content: TextField(
          key: const Key('live-key-field'),
          controller: field,
          obscureText: true,
          autocorrect: false,
          enableSuggestions: false,
          decoration: const InputDecoration(
            hintText: 'Paste the key from Google AI Studio',
          ),
        ),
        actions: [
          if (live?.hasKey == true)
            TextButton(
              onPressed: () => Navigator.pop(context, ''),
              child: const Text('Remove'),
            ),
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, field.text.trim()),
            child: const Text('Save and test'),
          ),
        ],
      ),
    );
    field.dispose();
    if (entered == null || !mounted) return;
    try {
      await widget.live.setKey(entered.isEmpty ? null : entered);
      if (entered.isEmpty) await widget.live.setEnabled(false);
    } on VoiceError catch (e) {
      setState(() => liveTest = e.message);
      return;
    }
    setState(() => liveTest = null);
    await _loadLive();
    if (entered.isNotEmpty) await _testKey();
  }

  Future<void> _testKey() async {
    setState(() {
      testing = true;
      liveTest = null;
    });
    final key = await widget.live.key();
    final why = key == null || key.isEmpty
        ? 'No key is saved.'
        : await widget.validateKey(key);
    if (!mounted) return;
    setState(() {
      testing = false;
      liveTest = why ?? 'The key works: Gemini Live answered.';
    });
  }

  Future<void> _load() async {
    unawaited(_loadLive());
    final found = await widget.voice.languages();
    if (mounted) setState(() => languages = found);
    _showPhone(await widget.voice.phoneModel());
  }

  /// Shows [state], and follows a download until it ends.
  void _showPhone(PhoneModelState state) {
    if (!mounted) return;
    setState(() => phone = state);
    if (state.progress == null || _watchingPhone) return;
    _watchingPhone = true;
    Future<void>.delayed(const Duration(seconds: 1), () async {
      _watchingPhone = false;
      if (mounted) _showPhone(await widget.voice.phoneModel());
    });
  }

  String get _phoneNote {
    final size = '${(phone.bytes / 1e9).toStringAsFixed(1)} GB';
    if (phone.waiting) return 'Waiting for Wi-Fi to download $size.';
    if (phone.progress case final p?) {
      return 'Downloading… ${(p * 100).round()}% of $size.';
    }
    if (phone.error case final e?) return e;
    if (phone.on && phone.ready) {
      return 'Ready. Answers in calls when the Mac cannot: offline, or on '
          'a poor connection. Turn it off to free $size.';
    }
    return 'Answers in calls when the Mac cannot: offline, or on a poor '
        'connection. Downloads $size once, on Wi-Fi.';
  }

  Future<void> _chooseLanguage(VoiceLanguages l) async {
    final picked = await showModalBottomSheet<(String?,)>(
      context: context,
      showDragHandle: true,
      isScrollControlled: true,
      builder: (_) => LanguageSheet(languages: l),
    );
    if (picked == null || !mounted) return;
    final (id,) = picked;
    if (id == l.selected) return;
    await (widget.chooseLanguage ?? widget.voice.setLanguage)(id);
    await _load();
  }

  Future<void> _chooseVoice(VoiceLanguages l) async {
    final picked = await showModalBottomSheet<String>(
      context: context,
      showDragHandle: true,
      isScrollControlled: true,
      builder: (_) => SafeArea(
        child: ListView(
          shrinkWrap: true,
          children: [
            for (final (i, v) in [...l.voices, systemVoice].indexed)
              ListTile(
                title: Text(voiceLabel(v)),
                subtitle: i == 0 && v != systemVoice
                    ? const Text('Recorded in this language')
                    : null,
                trailing: v == l.voice
                    ? const Icon(Icons.check, color: ConnectoColors.accent)
                    : null,
                onTap: () => Navigator.pop(context, v),
              ),
          ],
        ),
      ),
    );
    if (picked == null || picked == l.voice || !mounted) return;
    await widget.voice.setVoice(picked);
    await _load();
  }

  Widget _row(String label, String? value, {VoidCallback? onTap}) =>
      SectionCard(
        child: RowAction(
          onTap: onTap,
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 13),
            child: Row(
              children: [
                Expanded(child: Text(label)),
                if (value != null)
                  Flexible(
                    child: Text(
                      value,
                      textAlign: TextAlign.end,
                      style: const TextStyle(color: ConnectoColors.muted),
                    ),
                  ),
                if (onTap != null) ...[
                  const SizedBox(width: 6),
                  const Icon(Icons.chevron_right, color: ConnectoColors.faint),
                ],
              ],
            ),
          ),
        ),
      );

  @override
  Widget build(BuildContext context) {
    final l = languages;
    return Scaffold(
      appBar: const AppHeader(back: true, title: 'Settings'),
      body: SafeArea(
        top: false,
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 560),
            child: ListView(
              padding: const EdgeInsets.all(16),
              children: [
                if (widget.relay case final relay?) ...[
                  _row('Relay', null, onTap: relay),
                  const SizedBox(height: 10),
                ],
                if (l != null && l.all.isNotEmpty) ...[
                  _row(
                    'Call language',
                    l.selected == null
                        ? 'Device (${languageName(l)})'
                        : languageName(l),
                    onTap: () => _chooseLanguage(l),
                  ),
                  const SizedBox(height: 10),
                ],
                if (l != null && l.voice != null) ...[
                  _row(
                    'Voice',
                    voiceLabel(l.voice!),
                    onTap: () => _chooseVoice(l),
                  ),
                  Padding(
                    padding: const EdgeInsets.fromLTRB(14, 6, 14, 0),
                    child: Text(
                      l.tooSlow
                          ? 'The natural voices are slower than real time on '
                                'this device, so calls use the system voice.'
                          : 'Natural voices are made on this device. The first '
                                'call downloads about 330 MB and measures them.',
                      style: const TextStyle(color: ConnectoColors.muted),
                    ),
                  ),
                  const SizedBox(height: 10),
                ],
                if (l != null && l.all.isNotEmpty) ...[
                  SectionCard(
                    child: SwitchListTile.adaptive(
                      title: const Text('Show voice timings'),
                      subtitle: const Text(
                        'On the call screen: how fast replies are ready and '
                        'heard, and how fast the voice is made.',
                        style: TextStyle(color: ConnectoColors.muted),
                      ),
                      value: l.showTimings,
                      onChanged: (on) async {
                        await widget.voice.setShowTimings(on);
                        await _load();
                      },
                    ),
                  ),
                  const SizedBox(height: 10),
                ],
                if (phone.offered) ...[
                  SectionCard(
                    child: SwitchListTile.adaptive(
                      title: const Text('On-phone assistant (offline)'),
                      subtitle: Text(
                        _phoneNote,
                        style: const TextStyle(color: ConnectoColors.muted),
                      ),
                      value: phone.on,
                      onChanged: (on) async =>
                          _showPhone(await widget.voice.setPhoneModel(on)),
                    ),
                  ),
                  const SizedBox(height: 10),
                ],
                if (live case final l?) ...[
                  _liveSection(l),
                  const SizedBox(height: 10),
                ],
                _row('Version', version ?? '…'),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

extension on _SettingsPageState {
  Widget _liveSection(({bool hasKey, bool enabled}) l) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      const Padding(
        padding: EdgeInsets.fromLTRB(14, 6, 14, 6),
        child: Text(
          'Gemini Live',
          style: TextStyle(color: ConnectoColors.muted),
        ),
      ),
      _row(
        'API key',
        l.hasKey ? 'Saved in the Keychain' : 'Not set',
        onTap: _editKey,
      ),
      const SizedBox(height: 10),
      SectionCard(
        child: SwitchListTile.adaptive(
          key: const Key('live-enabled'),
          title: const Text('Use Gemini Live for calls'),
          subtitle: const Text(
            "Gemini holds the conversation and asks the project's agent when "
            'it needs the code. Audio goes to Google; usage is billed to the '
            'key. Off, calls stay on the device.',
            style: TextStyle(color: ConnectoColors.muted),
          ),
          value: l.enabled && l.hasKey,
          onChanged: l.hasKey
              ? (on) async {
                  await widget.live.setEnabled(on);
                  await _loadLive();
                }
              : null,
        ),
      ),
      if (l.hasKey) ...[
        const SizedBox(height: 10),
        _row(
          testing ? 'Testing the key…' : 'Test the key',
          null,
          onTap: testing ? null : _testKey,
        ),
      ],
      if (liveTest case final t?)
        Padding(
          padding: const EdgeInsets.fromLTRB(14, 6, 14, 0),
          child: Text(
            t,
            key: const Key('live-test'),
            style: const TextStyle(color: ConnectoColors.muted),
          ),
        ),
    ],
  );
}

/// How a voice choice reads: "Giovanni", or "System voice".
String voiceLabel(String voice) => voice == systemVoice
    ? 'System voice'
    : voice.isEmpty
    ? voice
    : '${voice[0].toUpperCase()}${voice.substring(1)}';

/// The name of the language calls use now.
String languageName(VoiceLanguages l) {
  for (final language in l.all) {
    if (language.id == l.current) return language.name;
  }
  return l.current;
}

/// Picks the language calls speak: the device's own, or any the device can
/// recognize. Pops `(id,)`, with a null id for the device's language.
class LanguageSheet extends StatefulWidget {
  const LanguageSheet({super.key, required this.languages});
  final VoiceLanguages languages;
  @override
  State<LanguageSheet> createState() => _LanguageSheetState();
}

class _LanguageSheetState extends State<LanguageSheet> {
  final filter = TextEditingController();

  @override
  void dispose() {
    filter.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final l = widget.languages;
    final query = filter.text.trim().toLowerCase();
    final shown = [
      for (final language in l.all)
        if (query.isEmpty ||
            language.name.toLowerCase().contains(query) ||
            language.id.toLowerCase().contains(query))
          language,
    ];
    Widget? mark(bool on) =>
        on ? const Icon(Icons.check, color: ConnectoColors.accent) : null;
    return SafeArea(
      child: SizedBox(
        height: MediaQuery.sizeOf(context).height * 0.7,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 0, 16, 8),
              child: Text(
                'Call language',
                style: Theme.of(context).textTheme.titleMedium,
              ),
            ),
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 16),
              child: TextField(
                controller: filter,
                onChanged: (_) => setState(() {}),
                decoration: const InputDecoration(
                  hintText: 'Search languages',
                  prefixIcon: Icon(Icons.search),
                  isDense: true,
                ),
              ),
            ),
            const SizedBox(height: 8),
            Expanded(
              child: ListView(
                children: [
                  if (query.isEmpty)
                    ListTile(
                      title: const Text('Device language'),
                      trailing: mark(l.selected == null),
                      onTap: () => Navigator.pop(context, (null,)),
                    ),
                  for (final language in shown)
                    ListTile(
                      title: Text(language.name),
                      subtitle: language.installed
                          ? null
                          : const Text('Downloads when chosen'),
                      trailing:
                          mark(l.selected == language.id) ??
                          (language.installed
                              ? null
                              : const Icon(
                                  Icons.download_outlined,
                                  color: ConnectoColors.faint,
                                )),
                      onTap: () => Navigator.pop(context, (language.id,)),
                    ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
