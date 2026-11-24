import 'dart:convert';

/// What the Mac's Jev made of a turn: how likely the person has finished,
/// and (asked with a context) how likely it fits the call.
typedef TurnScores = ({double complete, double? fits});

/// Asks the host whether what the person has said so far in a call is a
/// finished turn and, given the call so far, whether it is meant for the
/// call: the Mac's Jev judges it, with the key in Connecto's settings (the
/// phone has none). It only says whether to stop listening and whether to
/// ask first; the words go to the agent as always. A host without a key (or
/// too old) says so once, and is not asked again this call.
class TurnJudge {
  TurnJudge(
    this.op,
    this.host, {
    this.deadline = const Duration(milliseconds: 1200),
  });

  final Future<dynamic> Function(String, Map<String, Object?>) op;
  final String host;

  /// How long one answer is waited for: it runs at every pause, and a late
  /// one is worth nothing (the turn ends as without Jev).
  final Duration deadline;

  /// How much longer than [deadline] the peer may take to say it gave up.
  static const grace = Duration(milliseconds: 300);

  /// The longest transcript, and the longest context, in UTF-8 bytes (the
  /// peer link's limits). A longer transcript is not asked about and a
  /// context is made of whole pieces that fit ([contextOf]): neither is ever
  /// cut, since only the Mac redacts them and a cut could split a secret.
  static const maxBytes = 4096;

  bool _off = false;

  /// Whether the host may still judge (it has not said it cannot).
  bool get available => !_off;

  /// The call's context for Jev: [head] (the project, the task) and the
  /// latest of [lines] (the conversation, oldest first) that fit, whole.
  /// A piece too long to fit ends what is taken; null when even [head]
  /// does not fit.
  static String? contextOf(List<String> head, List<String> lines) {
    int bytes(String s) => utf8.encode(s).length + 1;
    var used = head.fold(0, (n, h) => n + bytes(h));
    if (used > maxBytes) return null;
    final taken = <String>[];
    for (final line in lines.reversed) {
      if (used + bytes(line) > maxBytes) break;
      used += bytes(line);
      taken.insert(0, line);
    }
    return [...head, ...taken].join('\n');
  }

  /// What Jev made of [text] (with [context], also whether it fits), or
  /// null: no answer in time, an answer that is not one, or the host cannot
  /// judge.
  Future<TurnScores?> judge(String text, {String context = ''}) async {
    final said = text.trim();
    if (_off ||
        said.isEmpty ||
        utf8.encode(said).length > maxBytes ||
        utf8.encode(context).length > maxBytes) {
      return null;
    }
    try {
      final reply = await op('turnCheck', {
        'host': host,
        'text': said,
        'context': context,
        'timeoutMs': deadline.inMilliseconds,
      }).timeout(deadline + grace);
      if (reply is Map && reply['complete'] is num) {
        final complete = (reply['complete'] as num).toDouble();
        final fits = reply['fits'];
        // Anything but a probability is no answer.
        bool ok(double p) => p >= 0 && p <= 1;
        if (!ok(complete)) return null;
        if (fits == null) return (complete: complete, fits: null);
        if (fits is! num || !ok(fits.toDouble())) return null;
        return (complete: complete, fits: fits.toDouble());
      }
      if (reply is Map && reply['unavailable'] == true) _off = true;
    } catch (_) {
      // Too slow, or the link failed: this pause goes on without it.
    }
    return null;
  }

  /// How likely [text] is a finished turn (0..1), or null ([judge]).
  Future<double?> complete(String text) async => (await judge(text))?.complete;
}
