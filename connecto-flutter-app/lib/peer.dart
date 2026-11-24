import 'dart:convert';
import 'dart:ffi';
import 'dart:isolate';

import 'package:ffi/ffi.dart';

@Native<Pointer<Utf8> Function(Pointer<Utf8>)>(symbol: 'connecto_call')
external Pointer<Utf8> connectoCall(Pointer<Utf8> request);

@Native<Void Function(Pointer<Utf8>)>(symbol: 'connecto_free')
external void connectoFree(Pointer<Utf8> result);

typedef Data = Map<String, dynamic>;

Future<dynamic> peer(String operation, [Data fields = const {}]) {
  final request = jsonEncode({'op': operation, ...fields});
  return Isolate.run(() {
    final input = request.toNativeUtf8();
    try {
      final output = connectoCall(input);
      if (output == nullptr) {
        throw StateError('Peer returned an empty response');
      }
      try {
        final response = jsonDecode(output.toDartString()) as Data;
        if (response['error'] case final String reason) {
          throw PeerError(reason);
        }
        return response['data'];
      } finally {
        connectoFree(output);
      }
    } finally {
      malloc.free(input);
    }
  });
}

class PeerError implements Exception {
  final String reason;
  const PeerError(this.reason);
  @override
  String toString() => reason;
}
