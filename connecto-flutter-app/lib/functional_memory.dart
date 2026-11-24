import 'dart:convert';

import 'peer.dart';

/// One file of the project's functional memory.
typedef MemoryFile = ({String path, String text});

/// The project's functional memory (`.functionalMemory/`, through the
/// host's `functional_memory` op): what the project does, for the call's
/// models to talk about it. Empty when there is none, or the host is too old
/// to say.
Future<(List<MemoryFile>, bool)> functionalMemory(
  Future<dynamic> Function(String, Data) call,
  Data target,
) async {
  try {
    final data = await call('functional_memory', target);
    if (data is! Map) return (const <MemoryFile>[], false);
    return (
      [
        for (final f in data['files'] as List? ?? const [])
          if (f is Map)
            (
              path: '${f['path'] ?? f['name'] ?? ''}',
              text: '${f['text'] ?? f['content'] ?? ''}',
            ),
      ],
      data['truncated'] == true,
    );
  } catch (_) {
    return (const <MemoryFile>[], false);
  }
}

/// The most of the functional memory put in a large model's instructions
/// (Gemini Live, the Mac's model), in UTF-8 bytes: the peer link takes at
/// most 64 KiB per message (`MAX_COMPLETION_TEXT`), instructions included.
const memoryBudget = 48 * 1024;

/// The most of it a small model on the phone is given: README.md and
/// overview.md only, so its instructions, the conversation and the answer
/// still fit its few thousand tokens.
const memoryExcerptBudget = 3000;

/// The files a small model is given, in this order.
const excerptFiles = ['readme.md', 'overview.md'];

/// [memory] as one text of `## path` sections, whole files only, at most
/// [budget] UTF-8 bytes; and whether some of it was left out (or already
/// was: [truncated]). Empty when there is no memory.
(String, bool) memoryDigest(
  List<MemoryFile> memory, {
  bool truncated = false,
  int budget = memoryBudget,
}) {
  final buffer = StringBuffer();
  var cut = truncated;
  var bytes = 0;
  for (final f in memory) {
    final part = '## ${f.path}\n${f.text.trim()}\n\n';
    final size = utf8.encode(part).length;
    if (bytes + size > budget) {
      cut = true;
      break;
    }
    buffer.write(part);
    bytes += size;
  }
  return (buffer.toString(), cut);
}

/// The short part of [memory] for a small model: README.md, then
/// overview.md, at most [budget] characters. A file too long is cut at a
/// line (or at the budget) rather than left out: its start says the most.
String memoryExcerpt(
  List<MemoryFile> memory, {
  int budget = memoryExcerptBudget,
}) {
  String name(MemoryFile f) => f.path.split('/').last.toLowerCase();
  final buffer = StringBuffer();
  for (final wanted in excerptFiles) {
    for (final f in memory) {
      if (name(f) != wanted) continue;
      final part = '## ${f.path}\n${f.text.trim()}\n\n';
      final room = budget - buffer.length;
      if (part.length <= room) {
        buffer.write(part);
      } else if (room > 200) {
        var end = room - 1;
        // Never half of a character made of two code units (an emoji).
        final unit = part.codeUnitAt(end - 1);
        if (unit >= 0xD800 && unit <= 0xDBFF) end--;
        var head = part.substring(0, end);
        final line = head.lastIndexOf('\n');
        if (line > room ~/ 2) head = head.substring(0, line);
        buffer.write('${head.trimRight()}…\n\n');
      }
    }
  }
  return buffer.toString().trim();
}
