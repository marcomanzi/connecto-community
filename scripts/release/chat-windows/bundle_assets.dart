import 'dart:io' as io;

import 'package:flutter_tools/src/build_info.dart';
import 'package:flutter_tools/src/build_system/build_system.dart';
import 'package:flutter_tools/src/build_system/targets/assets.dart';
import 'package:flutter_tools/src/cache.dart';
import 'package:flutter_tools/src/context_runner.dart';
import 'package:flutter_tools/src/globals.dart' as globals;
import 'package:flutter_tools/src/isolated/native_assets/dart_hook_result.dart';

Future<void> main(List<String> args) async {
  Cache.flutterRoot = io.Platform.environment['FLUTTER_ROOT']!;
  await runInContext(() async {
    final fs = globals.fs;
    final output = fs.directory(args.single).absolute
      ..createSync(recursive: true);
    final env = Environment(
      projectDir: fs.currentDirectory,
      packageConfigPath: fs
          .file('.dart_tool/package_config.json')
          .absolute
          .path,
      outputDir: output,
      cacheDir: globals.cache.getRoot(),
      flutterRootDir: fs.directory(Cache.flutterRoot),
      fileSystem: fs,
      logger: globals.logger,
      artifacts: globals.artifacts!,
      processManager: globals.processManager,
      platform: globals.platform,
      analytics: globals.analytics,
      generateDartPluginRegistry: true,
      defines: {
        kBuildMode: 'release',
        kTargetPlatform: 'windows-x64',
        kIconTreeShakerFlag: 'false',
      },
    );
    await copyAssets(
      env,
      output,
      dartHookResult: DartHooksResult.empty(),
      targetPlatform: TargetPlatform.windows_x64,
      buildMode: BuildMode.release,
    );
  });
}
