import 'dart:io';

import 'package:hooks/hooks.dart';
import 'package:native_toolchain_rust/native_toolchain_rust.dart';

import 'relay_token.dart';

void main(List<String> args) async {
  await build(args, (input, output) async {
    // Flutter's native build hook runs with a filtered environment, so an
    // exported CONNECTO_RELAY_TOKEN usually doesn't reach it: the untracked
    // token file is what normally carries the token in. A git worktree has
    // no copy of its own, so the main checkout's is read too.
    var token = Platform.environment['CONNECTO_RELAY_TOKEN']?.trim();
    final repoRoot = Directory.fromUri(input.packageRoot.resolve('..'));
    for (final file in relayTokenFiles(repoRoot)) {
      output.dependencies.add(file.absolute.uri);
      if ((token == null || token.isEmpty) && await file.exists()) {
        token = (await file.readAsString()).trim();
      }
    }
    await RustBuilder(
      assetName: 'peer.dart',
      extraCargoEnvironmentVariables: {
        if (token != null && token.isNotEmpty) 'CONNECTO_RELAY_TOKEN': token,
      },
    ).run(input: input, output: output);
  });
}
