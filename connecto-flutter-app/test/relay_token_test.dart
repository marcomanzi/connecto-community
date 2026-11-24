import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

import '../hook/relay_token.dart';

void main() {
  late Directory tmp;

  setUp(() => tmp = Directory.systemTemp.createTempSync('relay_token_test'));
  tearDown(() => tmp.deleteSync(recursive: true));

  String real(Directory d) => d.resolveSymbolicLinksSync();

  test('a plain checkout reads only its own token file', () {
    final repo = Directory('${tmp.path}/repo')..createSync();
    Directory('${repo.path}/.git').createSync();
    final files = relayTokenFiles(repo);
    expect(files.map((f) => f.path), ['${repo.path}/$relayTokenPath']);
  });

  test('a git worktree also reads the main checkout token file', () {
    final main = Directory('${tmp.path}/main')..createSync();
    final wtGit = Directory('${main.path}/.git/worktrees/feature')
      ..createSync(recursive: true);
    File('${wtGit.path}/commondir').writeAsStringSync('../..\n');
    final wt = Directory('${tmp.path}/feature')..createSync();
    File('${wt.path}/.git').writeAsStringSync('gitdir: ${wtGit.path}\n');

    final files = relayTokenFiles(wt);
    expect(files, hasLength(2));
    expect(files[0].path, '${wt.path}/$relayTokenPath');
    expect(files[1].path, '${real(main)}/$relayTokenPath');
  });

  test('a .git file without a commondir is not followed', () {
    final repo = Directory('${tmp.path}/sub')..createSync();
    File('${repo.path}/.git').writeAsStringSync('gitdir: ${tmp.path}/nowhere\n');
    expect(relayTokenFiles(repo), hasLength(1));
  });
}
