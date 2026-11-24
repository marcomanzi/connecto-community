import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'peer.dart';
import 'theme.dart';

/// What the host accepts, from the chat response's `attach` field.
class AttachLimits {
  const AttachLimits({required this.maxBytes, required this.maxFiles});
  final int maxBytes, maxFiles;

  static AttachLimits? from(Object? value) => switch (value) {
    {'maxBytes': final int bytes, 'maxFiles': final int files}
        when bytes > 0 && files > 0 =>
      AttachLimits(maxBytes: bytes, maxFiles: files),
    _ => null,
  };

  String get summary =>
      'Up to $maxFiles ${maxFiles == 1 ? 'file' : 'files'}, '
      '${formatBytes(maxBytes)} in total';
}

String formatBytes(int bytes) {
  if (bytes < 1024) return '$bytes B';
  if (bytes < 1024 * 1024) return '${(bytes / 1024).round()} KiB';
  final mib = bytes / (1024 * 1024);
  return '${mib >= 10 || mib == mib.roundToDouble() ? mib.round() : mib.toStringAsFixed(1)} MiB';
}

/// One file the user picked or dropped.
class PendingFile {
  PendingFile(this.path, this.session)
    : name = path.split(Platform.pathSeparator).last;
  final String path;

  /// The app session the file is staged for; its token is only valid there.
  final String? session;
  String name;
  int? bytes;
  String? token, error;
  bool get staging => token == null && error == null;
}

/// Why attaching is not possible right now, or null when it is.
String? attachBlocker({
  required AttachLimits? limits,
  required bool canSend,
  required bool answering,
}) {
  if (limits == null) {
    return "This host can't receive files. Update Connecto on the host.";
  }
  if (answering) return 'Answer the question before attaching files.';
  if (!canSend) return 'Files can be attached when the agent is ready.';
  return null;
}

class Composer extends StatefulWidget {
  const Composer({
    super.key,
    required this.draft,
    required this.focusNode,
    required this.canSend,
    required this.busy,
    required this.hint,
    required this.limits,
    required this.answering,
    required this.session,
    required this.onSend,
    required this.stage,
    required this.discard,
    required this.pickFiles,
  });

  final TextEditingController draft;
  final FocusNode focusNode;
  final bool canSend, busy, answering;
  final String? session;
  final String hint;
  final AttachLimits? limits;

  /// Sends the draft with these staged tokens; true when it went through.
  final Future<bool> Function(List<String> tokens) onSend;
  final Future<Data> Function(String path, String? session) stage;
  final Future<void> Function(String token, String? session) discard;
  final Future<List<String>> Function() pickFiles;

  @override
  State<Composer> createState() => ComposerState();
}

class ComposerState extends State<Composer> {
  final files = <PendingFile>[];
  String? notice;
  bool picking = false;

  String? get blocker => attachBlocker(
    limits: widget.limits,
    canSend: widget.canSend,
    answering: widget.answering,
  );

  bool get desktop => switch (Theme.of(context).platform) {
    TargetPlatform.macOS ||
    TargetPlatform.windows ||
    TargetPlatform.linux => true,
    _ => false,
  };

  /// While a question is answered only the typed text is sent; queued files
  /// stay for the next message.
  bool get ready =>
      widget.canSend &&
      !widget.busy &&
      (widget.answering
          ? widget.draft.text.trim().isNotEmpty
          : !files.any((f) => f.staging || f.error != null) &&
                (widget.draft.text.trim().isNotEmpty || files.isNotEmpty));

  void discard(PendingFile file) {
    if (file.token case final token?) {
      unawaited(widget.discard(token, file.session));
    }
  }

  @override
  void didUpdateWidget(Composer oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.session != widget.session && files.isNotEmpty) {
      // Tokens belong to the replaced session: drop them there.
      files.forEach(discard);
      files.clear();
      notice = 'The session changed. Attach the files again.';
    }
  }

  @override
  void dispose() {
    files.forEach(discard);
    super.dispose();
  }

  /// Opens the system picker.
  Future<void> pick() async {
    if (picking) return;
    if (blocker case final reason?) {
      setState(() => notice = reason);
      return;
    }
    picking = true;
    try {
      addPaths(await widget.pickFiles());
    } catch (e) {
      if (mounted) setState(() => notice = 'Could not open files: $e');
    } finally {
      picking = false;
    }
  }

  /// Adds picked or dropped paths: folders, duplicates and files over the
  /// count are refused here; the rest are staged.
  void addPaths(List<String> paths) {
    if (paths.isEmpty || !mounted) return;
    if (blocker case final reason?) {
      setState(() => notice = reason);
      return;
    }
    final limits = widget.limits!;
    final problems = <String>[];
    final added = <PendingFile>[];
    for (final path in paths) {
      if (files.any((f) => f.path == path) ||
          added.any((f) => f.path == path)) {
        continue;
      }
      if (FileSystemEntity.isDirectorySync(path)) {
        problems.add("Folders can't be attached.");
        continue;
      }
      if (files.length + added.length >= limits.maxFiles) {
        problems.add('You can attach up to ${limits.maxFiles} files.');
        break;
      }
      added.add(PendingFile(path, widget.session));
    }
    setState(() {
      files.addAll(added);
      notice = problems.isEmpty ? null : problems.toSet().join(' ');
    });
    for (final file in added) {
      stageFile(file);
    }
  }

  Future<void> stageFile(PendingFile file) async {
    setState(() => file.error = null);
    try {
      final staged = await widget.stage(file.path, file.session);
      if (!mounted || !files.contains(file)) {
        if (staged['token'] case final String token) {
          unawaited(widget.discard(token, file.session));
        }
        return;
      }
      setState(() {
        file.token = staged['token'] as String;
        file.name = staged['name'] as String? ?? file.name;
        file.bytes = (staged['bytes'] as num?)?.toInt();
      });
    } catch (e) {
      if (mounted) setState(() => file.error = e.toString());
    }
  }

  void remove(PendingFile file) {
    setState(() {
      files.remove(file);
      notice = null;
    });
    discard(file);
    widget.focusNode.requestFocus();
  }

  Future<void> send() async {
    if (!ready) {
      if (widget.canSend && files.any((f) => f.error != null)) {
        setState(() => notice = 'Remove or retry the file that failed.');
      }
      return;
    }
    if (widget.answering) {
      await widget.onSend(const []);
      return;
    }
    final sending = List.of(files);
    final ok = await widget.onSend([for (final f in sending) f.token!]);
    if (ok && mounted) {
      setState(() {
        files.removeWhere(sending.contains);
        notice = null;
      });
    }
  }

  KeyEventResult onKey(FocusNode _, KeyEvent event) {
    final keyboard = HardwareKeyboard.instance;
    final composing = widget.draft.value.composing;
    if (!desktop ||
        !widget.canSend ||
        event is! KeyDownEvent ||
        (event.logicalKey != LogicalKeyboardKey.enter &&
            event.logicalKey != LogicalKeyboardKey.numpadEnter) ||
        keyboard.isControlPressed ||
        keyboard.isMetaPressed ||
        keyboard.isAltPressed ||
        (composing.isValid && !composing.isCollapsed)) {
      return KeyEventResult.ignored;
    }
    if (keyboard.isShiftPressed) {
      final draft = widget.draft;
      final selection = draft.selection;
      final start = selection.isValid ? selection.start : draft.text.length;
      final end = selection.isValid ? selection.end : draft.text.length;
      draft.value = TextEditingValue(
        text: draft.text.replaceRange(start, end, '\n'),
        selection: TextSelection.collapsed(offset: start + 1),
      );
    } else {
      send();
    }
    return KeyEventResult.handled;
  }

  @override
  Widget build(BuildContext context) {
    final control = desktop ? 36.0 : 40.0;
    final reason = blocker;
    final showAttach = !widget.answering && widget.limits != null;
    return Focus(
      canRequestFocus: false,
      skipTraversal: true,
      onKeyEvent: onKey,
      child: ListenableBuilder(
        listenable: Listenable.merge([widget.focusNode, widget.draft]),
        builder: (context, _) {
          final focused = widget.focusNode.hasFocus;
          return AnimatedContainer(
            duration: MediaQuery.disableAnimationsOf(context)
                ? Duration.zero
                : const Duration(milliseconds: 120),
            decoration: BoxDecoration(
              color: ConnectoColors.raised,
              border: Border.all(
                color: focused
                    ? ConnectoColors.accent.withValues(alpha: 0.85)
                    : ConnectoColors.borderStrong,
              ),
              borderRadius: BorderRadius.circular(14),
              boxShadow: [
                if (focused)
                  BoxShadow(
                    color: ConnectoColors.accent.withValues(alpha: 0.14),
                    spreadRadius: 3,
                  ),
              ],
            ),
            padding: const EdgeInsets.all(6),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                if (files.isNotEmpty)
                  Padding(
                    padding: const EdgeInsets.fromLTRB(2, 2, 2, 6),
                    child: LayoutBuilder(
                      builder: (context, constraints) =>
                          constraints.maxWidth < 520
                          // Narrow: one scrolling row keeps the field in view.
                          ? SingleChildScrollView(
                              scrollDirection: Axis.horizontal,
                              child: Row(
                                children: [
                                  for (final file in files) ...[
                                    chip(file),
                                    if (file != files.last)
                                      const SizedBox(width: 6),
                                  ],
                                ],
                              ),
                            )
                          : Wrap(
                              spacing: 6,
                              runSpacing: 6,
                              children: [for (final file in files) chip(file)],
                            ),
                    ),
                  ),
                if (notice case final message?)
                  Padding(
                    padding: const EdgeInsets.fromLTRB(8, 0, 2, 4),
                    child: Row(
                      children: [
                        const Icon(
                          Icons.error_outline,
                          size: 14,
                          color: ConnectoColors.coral,
                        ),
                        const SizedBox(width: 6),
                        Expanded(
                          child: Text(
                            message,
                            style: Theme.of(context).textTheme.bodySmall
                                ?.copyWith(color: ConnectoColors.coral),
                          ),
                        ),
                        IconButton(
                          tooltip: 'Dismiss',
                          visualDensity: VisualDensity.compact,
                          iconSize: 14,
                          constraints: const BoxConstraints.tightFor(
                            width: 28,
                            height: 28,
                          ),
                          padding: EdgeInsets.zero,
                          onPressed: () => setState(() => notice = null),
                          icon: const Icon(Icons.close),
                        ),
                      ],
                    ),
                  ),
                Row(
                  crossAxisAlignment: CrossAxisAlignment.end,
                  children: [
                    if (showAttach)
                      SizedBox.square(
                        dimension: control,
                        child: IconButton(
                          tooltip: reason ?? 'Attach files',
                          onPressed: reason == null && !widget.busy
                              ? pick
                              : null,
                          padding: EdgeInsets.zero,
                          style: IconButton.styleFrom(
                            foregroundColor: ConnectoColors.muted,
                            disabledForegroundColor: ConnectoColors.faint
                                .withValues(alpha: 0.6),
                            shape: RoundedRectangleBorder(
                              borderRadius: BorderRadius.circular(10),
                            ),
                          ),
                          icon: const Icon(Icons.attach_file, size: 20),
                        ),
                      )
                    else
                      const SizedBox(width: 6),
                    Expanded(
                      child: TextField(
                        controller: widget.draft,
                        focusNode: widget.focusNode,
                        maxLines: 8,
                        minLines: 1,
                        enabled: widget.canSend,
                        textAlignVertical: TextAlignVertical.center,
                        decoration: InputDecoration(
                          hintText: widget.hint,
                          isDense: true,
                          filled: false,
                          contentPadding: EdgeInsets.symmetric(
                            horizontal: 6,
                            vertical: (control - 20) / 2,
                          ),
                          border: InputBorder.none,
                          enabledBorder: InputBorder.none,
                          disabledBorder: InputBorder.none,
                          focusedBorder: InputBorder.none,
                        ),
                      ),
                    ),
                    const SizedBox(width: 6),
                    SizedBox.square(
                      dimension: control,
                      child: IconButton.filled(
                        tooltip: 'Send message',
                        onPressed: ready ? send : null,
                        padding: EdgeInsets.zero,
                        style: IconButton.styleFrom(
                          backgroundColor: ConnectoColors.accent,
                          foregroundColor: ConnectoColors.onAccent,
                          disabledBackgroundColor: ConnectoColors.hover,
                          disabledForegroundColor: ConnectoColors.faint,
                          shape: RoundedRectangleBorder(
                            borderRadius: BorderRadius.circular(10),
                          ),
                        ),
                        icon: widget.busy
                            ? const SizedBox.square(
                                dimension: 16,
                                child: CircularProgressIndicator(
                                  strokeWidth: 2,
                                ),
                              )
                            : const Icon(Icons.arrow_upward, size: 20),
                      ),
                    ),
                  ],
                ),
              ],
            ),
          );
        },
      ),
    );
  }

  Widget chip(PendingFile file) {
    final failed = file.error != null;
    final sending = widget.busy && file.token != null;
    final working = file.staging || sending;
    final detail = failed
        ? 'Failed'
        : file.staging
        ? 'Preparing…'
        : sending
        ? 'Sending…'
        : file.bytes == null
        ? null
        : formatBytes(file.bytes!);
    final body = Container(
      constraints: const BoxConstraints(minHeight: 32, maxWidth: 240),
      padding: const EdgeInsets.only(left: 9, right: 3),
      decoration: BoxDecoration(
        color: failed
            ? ConnectoColors.coral.withValues(alpha: 0.08)
            : ConnectoColors.hover,
        border: Border.all(
          color: failed
              ? ConnectoColors.coral.withValues(alpha: 0.55)
              : ConnectoColors.border,
        ),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          SizedBox.square(
            dimension: 16,
            child: working
                ? const Padding(
                    padding: EdgeInsets.all(1.5),
                    child: CircularProgressIndicator(strokeWidth: 1.6),
                  )
                : Icon(
                    failed ? Icons.error_outline : fileIcon(file.name),
                    size: 16,
                    color: failed ? ConnectoColors.coral : ConnectoColors.muted,
                  ),
          ),
          const SizedBox(width: 7),
          Flexible(
            child: Text(
              file.name,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(fontSize: 13, color: ConnectoColors.text),
            ),
          ),
          if (detail != null) ...[
            const SizedBox(width: 6),
            Text(
              detail,
              style: TextStyle(
                fontSize: 12,
                color: failed ? ConnectoColors.coral : ConnectoColors.faint,
              ),
            ),
          ],
          const SizedBox(width: 4),
          if (failed)
            chipButton(
              'Retry ${file.name}',
              Icons.refresh,
              widget.busy ? null : () => stageFile(file),
            ),
          chipButton(
            'Remove ${file.name}',
            Icons.close,
            widget.busy ? null : () => remove(file),
          ),
        ],
      ),
    );
    return Semantics(
      label: ['Attachment ${file.name}', ?detail, ?file.error].join(', '),
      child: failed ? Tooltip(message: file.error!, child: body) : body,
    );
  }

  Widget chipButton(String tooltip, IconData icon, VoidCallback? onPressed) =>
      IconButton(
        tooltip: tooltip,
        onPressed: onPressed,
        iconSize: 14,
        padding: EdgeInsets.zero,
        constraints: const BoxConstraints.tightFor(width: 26, height: 26),
        style: IconButton.styleFrom(
          minimumSize: const Size(26, 26),
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          visualDensity: VisualDensity.compact,
          foregroundColor: ConnectoColors.muted,
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(6)),
        ),
        icon: Icon(icon),
      );
}

IconData fileIcon(String name) {
  final dot = name.lastIndexOf('.');
  final ext = dot < 0 ? '' : name.substring(dot + 1).toLowerCase();
  return switch (ext) {
    'png' ||
    'jpg' ||
    'jpeg' ||
    'gif' ||
    'webp' ||
    'heic' ||
    'svg' => Icons.image_outlined,
    'pdf' => Icons.picture_as_pdf_outlined,
    'zip' || 'gz' || 'tgz' || 'tar' || '7z' => Icons.folder_zip_outlined,
    'txt' ||
    'md' ||
    'csv' ||
    'json' ||
    'log' ||
    'yaml' ||
    'yml' ||
    'toml' => Icons.description_outlined,
    _ => Icons.insert_drive_file_outlined,
  };
}

/// Shown over the chat while files are dragged over it.
class DropOverlay extends StatelessWidget {
  const DropOverlay({super.key, required this.limits, required this.blocker});
  final AttachLimits? limits;
  final String? blocker;

  @override
  Widget build(BuildContext context) {
    final ok = blocker == null;
    final color = ok ? ConnectoColors.accent : ConnectoColors.amber;
    return IgnorePointer(
      child: Container(
        margin: const EdgeInsets.all(8),
        decoration: BoxDecoration(
          color: ConnectoColors.bg.withValues(alpha: 0.88),
          border: Border.all(color: color.withValues(alpha: 0.7), width: 1.5),
          borderRadius: BorderRadius.circular(14),
        ),
        alignment: Alignment.center,
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(
                ok ? Icons.file_upload_outlined : Icons.block,
                size: 30,
                color: color,
              ),
              const SizedBox(height: 10),
              Text(
                ok ? 'Drop files to attach' : "Can't attach files here",
                style: Theme.of(context).textTheme.titleMedium,
                textAlign: TextAlign.center,
              ),
              const SizedBox(height: 4),
              Text(
                blocker ?? limits!.summary,
                style: Theme.of(context).textTheme.bodySmall,
                textAlign: TextAlign.center,
              ),
            ],
          ),
        ),
      ),
    );
  }
}
