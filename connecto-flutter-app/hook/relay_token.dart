import 'dart:io';

/// Where the untracked relay token file lives, relative to a checkout's root.
const relayTokenPath = 'scripts/release/relay-token.local';

/// The relay token files a build of the app in [repoRoot] may read, best
/// first: the checkout's own, then, when [repoRoot] is a git worktree (its
/// `.git` is a file), the one in the main checkout. The token file is
/// untracked, so a worktree never has its own copy unless someone put it there.
List<File> relayTokenFiles(Directory repoRoot) {
  final files = [File('${repoRoot.path}/$relayTokenPath')];
  final main = _mainCheckout(repoRoot);
  if (main != null) files.add(File('${main.path}/$relayTokenPath'));
  return files;
}

/// The main checkout of the worktree at [repoRoot], or null when [repoRoot]
/// is not a linked worktree.
Directory? _mainCheckout(Directory repoRoot) {
  final dotGit = File('${repoRoot.path}/.git');
  if (!dotGit.existsSync()) return null;
  final line = dotGit.readAsStringSync().trim();
  if (!line.startsWith('gitdir:')) return null;
  var gitDir = line.substring('gitdir:'.length).trim();
  if (!gitDir.startsWith('/')) gitDir = '${repoRoot.path}/$gitDir';
  final commonFile = File('$gitDir/commondir');
  if (!commonFile.existsSync()) return null;
  var common = commonFile.readAsStringSync().trim();
  if (!common.startsWith('/')) common = '$gitDir/$common';
  // The common dir is the main checkout's `.git`.
  final commonDir = Directory(common).absolute;
  final resolved = Directory(commonDir.resolveSymbolicLinksSync());
  return resolved.parent;
}
