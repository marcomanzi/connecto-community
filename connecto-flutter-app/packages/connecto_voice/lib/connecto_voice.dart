/// The device's voice: on-device speech recognition, speech and condensing a
/// reply, for the call mode. Apple platforms only (iOS and macOS 26 or later);
/// elsewhere [VoiceEngine.support] reports it unavailable.
library;

import 'dart:math';

import 'package:flutter/services.dart';

/// What the recognizer heard, or what happened to the call.
sealed class VoiceEvent {
  const VoiceEvent();
}

/// Words still being recognized; they may change.
class Heard extends VoiceEvent {
  final String text;
  final bool isFinal;

  /// The audio this covers, in seconds since the call started, when known.
  final double? start, end;
  const Heard(this.text, {required this.isFinal, this.start, this.end});
}

/// The microphone is open and recognition is running.
class Listening extends VoiceEvent {
  const Listening();
}

/// Progress worth showing, such as a model download.
class VoiceStatus extends VoiceEvent {
  final String text;
  const VoiceStatus(this.text);
}

/// A speech model is downloading for [language]; [fraction] is 0 to 1 when
/// known.
class VoiceDownload extends VoiceEvent {
  final String language;
  final double? fraction;
  const VoiceDownload(this.language, this.fraction);
}

/// The current utterance was first heard: its first audio was scheduled.
class FirstSound extends VoiceEvent {
  const FirstSound();
}

/// How the natural voice did on one utterance: its [speed] (audio seconds
/// made per second of work; under 1 it cannot keep up with playback), when
/// it was first heard after it was asked for, how much it said, and on
/// what engine and voice.
class VoiceTiming extends VoiceEvent {
  final double speed;
  final Duration? firstAudio;
  final double audioSeconds;
  final String engine, voice;
  const VoiceTiming({
    required this.speed,
    this.firstAudio,
    this.audioSeconds = 0,
    this.engine = '',
    this.voice = '',
  });
}

/// The choice of voice that means the system's, always.
const systemVoice = 'system';

/// Below this speed (measured on the device) the natural voice cannot keep
/// up with playback: calls use the system voice.
const naturalMinimumSpeed = 1.1;

/// The most audio held back before speech starts.
const maxPrebufferSeconds = 1.5;

/// The fewest characters a second of speech says, pauses included: speech
/// is assumed to last at most this long, so the audio held back is enough
/// however slowly a sentence is said (about 14 is usual).
const slowestCharsPerSecond = 8.0;

/// How many seconds of the natural voice's audio to hold back before [text]
/// starts playing so playback never runs dry: what synthesis at [speed]
/// falls behind over the whole of it, with a margin, and a little at least
/// for jitter. Not capped.
///
/// Playback of `A` seconds started after holding `B` seconds stays fed
/// iff `B >= A * (1 - speed)`. `A` is taken at its longest
/// ([slowestCharsPerSecond]) and speed 10% lower than measured, since a
/// device slows down when warm or busy. Measured below real time, or not
/// at all, it is the whole of it.
double holdSeconds({required double speed, required String text}) {
  const floor = 0.16; // two frames
  final audio = text.trim().length / slowestCharsPerSecond;
  if (speed < 1) return double.infinity;
  final effective = speed * 0.9;
  if (effective >= 1) return floor;
  return max(floor, audio * (1 - effective) * 1.1);
}

/// How an utterance went: [completed] when it was spoken to the end; else
/// [heard] is the start of its text the person heard before it stopped, as
/// the device counts the audio actually played back (it may run short,
/// never long).
class Spoken {
  final bool completed;
  final String heard;
  const Spoken({required this.completed, required this.heard});

  /// [text] spoken to the end.
  const Spoken.all(String text) : completed = true, heard = text;

  @override
  bool operator ==(Object other) =>
      other is Spoken && other.completed == completed && other.heard == heard;
  @override
  int get hashCode => Object.hash(completed, heard);
  @override
  String toString() => 'Spoken($completed, $heard)';
}

/// The device's answer to `speak` for [text]: `{completed, heard}`, or, from
/// an older device side, only whether it was completed (then nothing is
/// known to have been heard of an interrupted one). Null: spoken.
Spoken spokenFrom(Object? answer, String text) {
  if (answer is Map) {
    final completed = answer['completed'] != false;
    final heard = answer['heard'];
    return completed
        ? Spoken.all(text)
        : Spoken(completed: false, heard: heard is String ? heard : '');
  }
  return answer == false
      ? const Spoken(completed: false, heard: '')
      : Spoken.all(text);
}

/// Work that a newer turn made useless: whoever does it stops as soon as it
/// can once [cancel] is called.
class Cancellation {
  bool _cancelled = false;
  final _listeners = <void Function()>[];

  bool get cancelled => _cancelled;

  void cancel() {
    if (_cancelled) return;
    _cancelled = true;
    for (final listener in List.of(_listeners)) {
      listener();
    }
    _listeners.clear();
  }

  /// Calls [listener] once on [cancel] (at once if already cancelled);
  /// the returned function takes it back.
  void Function() onCancel(void Function() listener) {
    if (_cancelled) {
      listener();
      return () {};
    }
    _listeners.add(listener);
    return () => _listeners.remove(listener);
  }
}

/// One part of an utterance and the audio held back before it plays;
/// [hold] infinite means the whole part is made first.
class SpeechPart {
  final String text;
  final double hold;
  const SpeechPart(this.text, this.hold);

  @override
  bool operator ==(Object other) =>
      other is SpeechPart && other.text == text && other.hold == hold;
  @override
  int get hashCode => Object.hash(text, hold);
  @override
  String toString() => 'SpeechPart($text, $hold)';
}

/// How to say [text] at [speed] without ever pausing mid-sentence: in one
/// go when the audio to hold back is at most [maxPrebufferSeconds]; else
/// sentence by sentence, each held back as much as it needs, a sentence
/// that needs more than that being made whole before it plays. Pauses then
/// fall only between sentences.
List<SpeechPart> speechPlan({required double speed, required String text}) {
  final whole = holdSeconds(speed: speed, text: text);
  if (whole <= maxPrebufferSeconds) return [SpeechPart(text, whole)];
  return [
    for (final sentence in sentences(text))
      switch (holdSeconds(speed: speed, text: sentence)) {
        final hold when hold <= maxPrebufferSeconds => SpeechPart(
          sentence,
          hold,
        ),
        _ => SpeechPart(sentence, double.infinity),
      },
  ];
}

/// [text] split after each sentence's end (".", "!", "?", "…", ";"), a
/// very short one kept with the next.
List<String> sentences(String text) {
  final parts = text
      .trim()
      .split(RegExp(r'(?<=[.!?…;])\s+'))
      .where((s) => s.trim().isNotEmpty)
      .toList();
  final out = <String>[];
  var pending = '';
  for (final part in parts) {
    pending = pending.isEmpty ? part : '$pending $part';
    if (pending.length >= 24) {
      out.add(pending);
      pending = '';
    }
  }
  if (pending.isNotEmpty) {
    if (out.isEmpty) {
      out.add(pending);
    } else {
      out[out.length - 1] = '${out.last} $pending';
    }
  }
  return out;
}

/// The person's voice started or stopped, from the sound level on the
/// echo-cancelled microphone: sooner than recognized words, and blind to
/// what is said.
class VoiceActivity extends VoiceEvent {
  final bool speaking;
  const VoiceActivity(this.speaking);
}

/// How likely the person has finished their turn, from how the last few
/// seconds sounded (words, intonation, pace), estimated on the device when
/// they paused. [at] is where the pause began, in the same seconds as
/// [Heard.start] and [Heard.end], when known.
class TurnEstimate extends VoiceEvent {
  final double complete;
  final double? at;
  const TurnEstimate(this.complete, {this.at});
}

/// Recognition stopped and the call cannot go on.
class VoiceFailed extends VoiceEvent {
  final String message;
  const VoiceFailed(this.message);
}

/// The on-device model's next step in the conversation that works out a
/// task.
class Intake {
  /// What to say now, in the call's language: one or two short sentences.
  /// Empty when [check] is set: the app says its own phrase then.
  final String say;

  /// The request is nearly clear, or needs knowledge of the project's code:
  /// the project's agent should check it.
  final bool check;
  const Intake({this.say = '', this.check = false});
}

/// The on-phone assistant (a small model run on the device), as Settings
/// and a call show it.
class PhoneModelState {
  const PhoneModelState({
    this.on = false,
    this.ready = false,
    this.progress,
    this.bytes = 0,
    this.error,
    this.waiting = false,
  });

  /// Not on this platform: nothing to offer.
  static const none = PhoneModelState();

  /// Turned on in Settings, and downloaded and checked ([ready]).
  final bool on, ready;

  /// While downloading, how far, in [0, 1].
  final double? progress;

  /// Its size; 0 where it is not offered.
  final int bytes;

  /// Why the last download failed.
  final String? error;

  /// The download waits for Wi-Fi: it never uses cellular data.
  final bool waiting;

  bool get offered => bytes > 0;
}

class VoiceSupport {
  final bool available;

  /// On-device Foundation Models can shorten replies.
  final bool summarizer;
  final String? reason;
  const VoiceSupport({
    required this.available,
    this.summarizer = false,
    this.reason,
  });
  static const none = VoiceSupport(
    available: false,
    reason: 'Calls need iOS 26 or macOS 26 or later.',
  );
}

/// A language the device can recognize on the device.
class VoiceLanguage {
  /// A locale identifier such as `it-IT`.
  final String id;

  /// Its name in the device's own language, such as "Italian (Italy)".
  final String name;

  /// Its speech model is on the device; otherwise starting a call in it
  /// downloads the model first.
  final bool installed;
  const VoiceLanguage(this.id, this.name, {required this.installed});
}

/// The languages to choose from and the one calls use.
class VoiceLanguages {
  final List<VoiceLanguage> all;

  /// The chosen language, or null for the device's own.
  final String? selected;

  /// What a call uses now: [selected], or the device's language.
  final String current;

  /// The natural voices (PocketTTS) for [current], its native one first;
  /// empty where there is none.
  final List<String> voices;

  /// What calls in [current] speak with: one of [voices], or [systemVoice];
  /// null where there is no natural voice.
  final String? voice;

  /// The natural voice's speed here (audio seconds per second of work), and
  /// the engine it runs on, once measured.
  final double? speed;
  final String? engine;

  /// Measured too slow here: calls use the system voice whatever [voice] says.
  final bool tooSlow;

  /// The call screen shows how fast speech is made.
  final bool showTimings;

  const VoiceLanguages(
    this.all, {
    this.selected,
    required this.current,
    this.voices = const [],
    this.voice,
    this.speed,
    this.engine,
    this.tooSlow = false,
    this.showTimings = false,
  });

  /// Whether calls in [current] speak with a natural voice; null where none.
  bool? get natural => voice == null ? null : voice != systemVoice;
  static const none = VoiceLanguages([], current: '');
}

/// The calls the call mode makes; [PlatformVoice] is the real one, tests
/// supply their own.
abstract class VoiceEngine {
  Stream<VoiceEvent> get events;
  Future<VoiceSupport> support();

  /// Opens the microphone and starts recognizing in the chosen language,
  /// first downloading its model if needed ([VoiceDownload]); throws with a reason the
  /// person can act on (permission, language).
  Future<void> start();
  Future<void> stop();

  /// What can be recognized here, and what calls use.
  Future<VoiceLanguages> languages();

  /// Calls from now on recognize and speak [id]; null goes back to the
  /// device's language. Kept across launches.
  Future<void> setLanguage(String? id);

  /// What calls in the current language speak with: one of
  /// [VoiceLanguages.voices], or [systemVoice]. Kept across launches.
  Future<void> setVoice(String choice);

  /// Whether the call screen shows how fast speech is made.
  Future<void> setShowTimings(bool on);
  Future<void> mute(bool muted);

  /// Whether the natural voice could speak now, in this call. Asked once, at
  /// a call's first words: the call keeps that voice to the end.
  Future<bool> naturalReady();

  /// Resolves once [text] was spoken to the end, or stopped (barge-in,
  /// another utterance, hang-up): [Spoken] says which, and what of it was
  /// heard. The natural voice only when [natural] (and ready), said by the
  /// parts of [plan] (see [speechPlan]); without one, in one go at once.
  Future<Spoken> speak(
    String text, {
    bool natural = true,
    List<SpeechPart>? plan,
  });
  Future<void> stopSpeaking();

  /// Holds the speech where it is, to hear out the person who began talking
  /// over it; [resumeSpeaking] goes on from there. Cosmetic: they never throw.
  Future<void> pauseSpeaking();
  Future<void> resumeSpeaking();

  /// The person's turn was taken: what is heard from now on is a new turn
  /// for [TurnEstimate]. Cosmetic: never throws.
  Future<void> endTurn();

  /// A short tone that says the turn was heard and sent, played through the
  /// call's echo-cancelled output. Cosmetic: never throws.
  Future<void> cue();

  /// While [on], a soft tone plays every few seconds when nothing is being
  /// said: the call is on hold. Cosmetic: never throws.
  Future<void> hold(bool on);

  /// The next step in working out what the person wants, from the
  /// conversation so far (`(fromPerson, text)`), answered in [language] (a
  /// language code such as `it`), or null when no on-device model can.
  /// Called off through [cancel] (a newer turn made it useless), it stops
  /// and answers null. [memory], when given, is a short excerpt of what the
  /// project does (its functional memory) for the model to know; the model
  /// shortens it further to fit its context.
  Future<Intake?> clarify(
    List<(bool, String)> turns, {
    String language = 'en',
    Cancellation? cancel,
    String memory = '',
  });

  /// A short spoken version of [text] in [language], or null when no
  /// on-device model can.
  Future<String?> condense(String text, {String language = 'en'});

  /// The on-phone assistant: its state, and turning it on (which downloads
  /// it) or off (which deletes it).
  Future<PhoneModelState> phoneModel();
  Future<PhoneModelState> setPhoneModel(bool on);

  /// Loads the on-phone assistant ahead of need, when it is on and ready.
  /// Cosmetic: never throws.
  Future<void> prewarmPhoneModel();

  /// The on-phone assistant's answer to [messages] (`(role, text)`, roles
  /// `system`, `user`, `assistant`), or null: off, not downloaded, slower
  /// than [timeout], failed, or called off through [cancel] (it then stops
  /// at once, waiting in line or between tokens, and frees the model).
  Future<String?> localComplete(
    List<(String, String)> messages, {
    int maxTokens = 120,
    double temperature = 0.3,
    Duration timeout = const Duration(seconds: 10),
    Cancellation? cancel,
  });
}

class PlatformVoice implements VoiceEngine {
  static const _methods = MethodChannel('connecto/voice');
  static const _channel = EventChannel('connecto/voice/events');

  /// One platform subscription shared by every listener (the call and the
  /// language picker): the native side keeps a single sink.
  static final Stream<VoiceEvent> _events = _channel
      .receiveBroadcastStream()
      .map(_decode);
  const PlatformVoice();

  @override
  Stream<VoiceEvent> get events => _events;

  static VoiceEvent _decode(dynamic raw) {
    final event = Map<String, dynamic>.from(raw as Map);
    final text = event['text'] as String? ?? '';
    return switch (event['type']) {
      'partial' || 'final' => Heard(
        text,
        isFinal: event['type'] == 'final',
        start: (event['start'] as num?)?.toDouble(),
        end: (event['end'] as num?)?.toDouble(),
      ),
      'listening' => const Listening(),
      'activity' => VoiceActivity(event['speaking'] == true),
      'turn' => TurnEstimate(
        (event['complete'] as num?)?.toDouble() ?? 0,
        at: (event['at'] as num?)?.toDouble(),
      ),
      'status' => VoiceStatus(text),
      'firstSound' => const FirstSound(),
      'timing' => VoiceTiming(
        speed: (event['speed'] as num?)?.toDouble() ?? 0,
        firstAudio: switch (event['firstAudioMs']) {
          final num ms => Duration(milliseconds: ms.round()),
          _ => null,
        },
        audioSeconds: (event['audio'] as num?)?.toDouble() ?? 0,
        engine: event['engine'] as String? ?? '',
        voice: event['voice'] as String? ?? '',
      ),
      'download' => VoiceDownload(
        event['language'] as String? ?? '',
        (event['fraction'] as num?)?.toDouble(),
      ),
      _ => VoiceFailed(event['message'] as String? ?? 'The call stopped.'),
    };
  }

  @override
  Future<VoiceSupport> support() async {
    try {
      final raw = await _methods.invokeMapMethod<String, dynamic>('support');
      if (raw == null) return VoiceSupport.none;
      return VoiceSupport(
        available: raw['available'] == true,
        summarizer: raw['summarizer'] == true,
        reason: raw['reason'] as String?,
      );
    } on MissingPluginException {
      return VoiceSupport.none;
    } on PlatformException catch (e) {
      return VoiceSupport(available: false, reason: e.message);
    }
  }

  @override
  Future<void> start() => _run('start');
  @override
  Future<void> stop() => _run('stop');
  @override
  Future<VoiceLanguages> languages() async {
    try {
      final raw = await _methods.invokeMapMethod<String, dynamic>('languages');
      if (raw == null) return VoiceLanguages.none;
      return VoiceLanguages(
        [
          for (final l in raw['languages'] as List? ?? const [])
            VoiceLanguage(
              (l as Map)['id'] as String,
              l['name'] as String? ?? l['id'] as String,
              installed: l['installed'] == true,
            ),
        ],
        selected: raw['selected'] as String?,
        current: raw['current'] as String? ?? '',
        voices: [for (final v in raw['voices'] as List? ?? const []) '$v'],
        voice: raw['voice'] as String?,
        speed: (raw['speed'] as num?)?.toDouble(),
        engine: raw['engine'] as String?,
        tooSlow: raw['tooSlow'] == true,
        showTimings: raw['showTimings'] == true,
      );
    } on MissingPluginException {
      return VoiceLanguages.none;
    } on PlatformException {
      return VoiceLanguages.none;
    }
  }

  @override
  Future<void> setLanguage(String? id) => _run('setLanguage', {'language': id});
  @override
  Future<void> setVoice(String choice) => _run('setVoice', {'choice': choice});
  @override
  Future<void> setShowTimings(bool on) => _run('setShowTimings', {'on': on});
  @override
  Future<bool> naturalReady() async =>
      await _run<bool>('naturalReady').catchError((_) => false) ?? false;
  @override
  Future<void> mute(bool muted) => _run('mute', {'muted': muted});
  @override
  Future<Spoken> speak(
    String text, {
    bool natural = true,
    List<SpeechPart>? plan,
  }) async {
    final answer = await _run<Object>('speak', {
      'text': text,
      'natural': natural,
      // An infinite hold travels as -1: the whole part first.
      'plan': [
        for (final part in plan ?? [SpeechPart(text, 0)])
          [part.text, part.hold.isFinite ? part.hold : -1],
      ],
    });
    return spokenFrom(answer, text);
  }

  @override
  Future<void> stopSpeaking() => _run('stopSpeaking');
  @override
  Future<void> pauseSpeaking() => _quietly('pauseSpeaking');
  @override
  Future<void> resumeSpeaking() => _quietly('resumeSpeaking');
  @override
  Future<void> endTurn() => _quietly('endTurn');
  @override
  Future<void> cue() => _quietly('cue');

  /// Calls [method] where a failure changes nothing the call relies on.
  Future<void> _quietly(String method, [Map<String, dynamic>? args]) async {
    try {
      await _methods.invokeMethod<void>(method, args);
    } on PlatformException {
      // The call goes on without it.
    } on MissingPluginException {
      // Same.
    }
  }

  @override
  Future<void> hold(bool on) => _quietly('hold', {'on': on});

  @override
  Future<Intake?> clarify(
    List<(bool, String)> turns, {
    String language = 'en',
    Cancellation? cancel,
    String memory = '',
  }) async {
    if (cancel?.cancelled ?? false) return null;
    final request = ++_localRequests;
    final forget = cancel?.onCancel(
      () => _quietly('cancelClarify', {'request': request}),
    );
    try {
      final raw = await _methods.invokeMapMethod<String, dynamic>('clarify', {
        'turns': [
          for (final (mine, text) in turns) [mine, text],
        ],
        'language': language,
        'request': request,
        if (memory.isNotEmpty) 'memory': memory,
      });
      if (raw == null || (cancel?.cancelled ?? false)) return null;
      return Intake(
        say: raw['say'] as String? ?? '',
        check: raw['check'] == true,
      );
    } on PlatformException {
      return null;
    } on MissingPluginException {
      return null;
    } finally {
      forget?.call();
    }
  }

  @override
  Future<String?> condense(String text, {String language = 'en'}) async {
    try {
      return await _methods.invokeMethod<String>('condense', {
        'text': text,
        'language': language,
      });
    } on PlatformException {
      return null;
    } on MissingPluginException {
      return null;
    }
  }

  static PhoneModelState _phoneState(Map<String, dynamic>? raw) => raw == null
      ? PhoneModelState.none
      : PhoneModelState(
          on: raw['on'] == true,
          ready: raw['ready'] == true,
          progress: (raw['progress'] as num?)?.toDouble(),
          bytes: (raw['bytes'] as num?)?.toInt() ?? 0,
          error: raw['error'] as String?,
          waiting: raw['waiting'] == true,
        );

  @override
  Future<PhoneModelState> phoneModel() async {
    try {
      return _phoneState(
        await _methods.invokeMapMethod<String, dynamic>('phoneModel'),
      );
    } on PlatformException {
      return PhoneModelState.none;
    } on MissingPluginException {
      return PhoneModelState.none;
    }
  }

  @override
  Future<PhoneModelState> setPhoneModel(bool on) async {
    try {
      return _phoneState(
        await _methods.invokeMapMethod<String, dynamic>('setPhoneModel', {
          'on': on,
        }),
      );
    } on PlatformException {
      return PhoneModelState.none;
    } on MissingPluginException {
      return PhoneModelState.none;
    }
  }

  @override
  Future<void> prewarmPhoneModel() => _quietly('phoneModelPrewarm');

  /// Numbers the on-phone requests, so one can be called off by its number.
  static int _localRequests = 0;

  @override
  Future<String?> localComplete(
    List<(String, String)> messages, {
    int maxTokens = 120,
    double temperature = 0.3,
    Duration timeout = const Duration(seconds: 10),
    Cancellation? cancel,
  }) async {
    if (cancel?.cancelled ?? false) return null;
    final request = ++_localRequests;
    final forget = cancel?.onCancel(
      () => _quietly('cancelLocalComplete', {'request': request}),
    );
    try {
      return await _methods.invokeMethod<String>('localComplete', {
        'messages': [
          for (final (role, text) in messages) [role, text],
        ],
        'maxTokens': maxTokens,
        'temperature': temperature,
        'timeoutMs': timeout.inMilliseconds,
        'request': request,
      });
    } on PlatformException {
      return null;
    } on MissingPluginException {
      return null;
    } finally {
      forget?.call();
    }
  }

  Future<T?> _run<T>(String method, [Map<String, dynamic>? args]) async {
    try {
      return await _methods.invokeMethod<T>(method, args);
    } on PlatformException catch (e) {
      throw VoiceError(e.message ?? 'The call failed.');
    } on MissingPluginException {
      throw const VoiceError('Calls are not available on this device.');
    }
  }
}

class VoiceError implements Exception {
  final String message;
  const VoiceError(this.message);
  @override
  String toString() => message;
}

/// What the audio of a Gemini Live call reports (ADR 0034).
sealed class LiveAudioEvent {
  const LiveAudioEvent();
}

/// A chunk of the echo-cancelled microphone: 16 kHz 16-bit little-endian
/// mono PCM.
class MicAudio extends LiveAudioEvent {
  final Uint8List pcm;
  const MicAudio(this.pcm);
}

/// Gemini's audio started or stopped being heard.
class LivePlayback extends LiveAudioEvent {
  final bool playing;
  const LivePlayback(this.playing);
}

/// What the on-device recognizer heard while listening for an answer
/// ([LiveAudio.listenForAnswer]): in the microphone's order, unlike
/// Gemini's transcripts.
class LocalHeard extends LiveAudioEvent {
  final String text;
  final bool isFinal;

  /// The listening session it belongs to ([LiveAudio.listenForAnswer]).
  final int session;
  const LocalHeard(this.text, {required this.isFinal, required this.session});
}

/// The recognizer of [session] stopped on its own (it failed or ended).
class LocalHeardEnded extends LiveAudioEvent {
  final int session;
  const LocalHeardEnded(this.session);
}

class LiveAudioFailed extends LiveAudioEvent {
  final String message;
  const LiveAudioFailed(this.message);
}

/// The microphone and speaker of a Gemini Live call, on the same
/// voice-processing engine as on-device calls: echo cancellation lets the
/// person talk over Gemini.
abstract interface class LiveAudio {
  Stream<LiveAudioEvent> get events;
  Future<void> start();
  Future<void> stop();

  /// Plays 24 kHz 16-bit little-endian mono PCM after what is queued.
  Future<void> play(Uint8List pcm);

  /// Drops what is queued to play (barge-in).
  Future<void> flush();
  Future<void> mute(bool muted);

  /// Says [text] with the device's own voice in [language], through the
  /// same player as Gemini, after what is queued: the app's read-back of
  /// what a yes would send. True only once all of it played out; false
  /// when it was flushed (talked over), replaced by a newer one, or the
  /// call stopped.
  Future<bool> say(String text, String language);

  /// Recognizes what the person says on the device, in [language], as
  /// [LocalHeard] events tagged with [session], until [stopListening] or
  /// [LocalHeardEnded]. Throws when it cannot.
  Future<void> listenForAnswer(String language, int session);
  Future<void> stopListening();
}

/// The Gemini API key, kept in the Keychain, and whether calls use it.
abstract interface class LiveKeyStore {
  Future<({bool hasKey, bool enabled})> settings();
  Future<String?> key();

  /// Saves [key]; null or empty deletes it.
  Future<void> setKey(String? key);
  Future<void> setEnabled(bool on);
}

class PlatformLiveAudio implements LiveAudio, LiveKeyStore {
  static const _methods = MethodChannel('connecto/voice');
  static const _channel = EventChannel('connecto/voice/live');
  static final Stream<LiveAudioEvent> _events = _channel
      .receiveBroadcastStream()
      .map(_decode);
  const PlatformLiveAudio();

  static LiveAudioEvent _decode(dynamic raw) {
    final event = Map<String, dynamic>.from(raw as Map);
    return switch (event['type']) {
      'audio' => MicAudio(event['data'] as Uint8List),
      'playback' => LivePlayback(event['playing'] == true),
      'heard' => LocalHeard(
        event['text'] as String? ?? '',
        isFinal: event['final'] == true,
        session: (event['session'] as num?)?.toInt() ?? -1,
      ),
      'heardEnded' => LocalHeardEnded(
        (event['session'] as num?)?.toInt() ?? -1,
      ),
      _ => LiveAudioFailed(event['message'] as String? ?? 'The call stopped.'),
    };
  }

  @override
  Stream<LiveAudioEvent> get events => _events;
  @override
  Future<void> start() => _run('liveStart');
  @override
  Future<void> stop() => _run('liveStop');
  @override
  Future<void> play(Uint8List pcm) => _run('livePlay', {'data': pcm});
  @override
  Future<void> flush() => _run('liveFlush');
  @override
  Future<bool> say(String text, String language) async =>
      await _run<bool>('liveSay', {'text': text, 'language': language}) ??
      false;
  @override
  Future<void> mute(bool muted) => _run('liveMute', {'muted': muted});
  @override
  Future<void> listenForAnswer(String language, int session) =>
      _run('liveListen', {'language': language, 'session': session});
  @override
  Future<void> stopListening() => _run('liveStopListening');

  @override
  Future<({bool hasKey, bool enabled})> settings() async {
    try {
      final raw = await _methods.invokeMapMethod<String, dynamic>(
        'liveSettings',
      );
      return (hasKey: raw?['hasKey'] == true, enabled: raw?['enabled'] == true);
    } on MissingPluginException {
      return (hasKey: false, enabled: false);
    }
  }

  @override
  Future<String?> key() => _run<String>('liveKey');
  @override
  Future<void> setKey(String? key) => _run('setLiveKey', {'key': key});
  @override
  Future<void> setEnabled(bool on) => _run('setLiveEnabled', {'on': on});

  Future<T?> _run<T>(String method, [Map<String, dynamic>? args]) async {
    try {
      return await _methods.invokeMethod<T>(method, args);
    } on PlatformException catch (e) {
      throw VoiceError(e.message ?? 'The call failed.');
    } on MissingPluginException {
      throw const VoiceError('Gemini Live is not available on this device.');
    }
  }
}
