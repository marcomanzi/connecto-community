// The downloads shown on the landing page and in the docs. Version 0.3.10 is the
// current build (0.1.0 to 0.3.9 are left out of this list; their files may stay on the server). The files are not in this repository: put each one in
// site/downloads/ on the server, under the name below.
//
// To publish another build, build it with scripts/release/ (see its README for
// the file names), copy the files to downloads/ on the server, and change the
// entries here: title, version, file, size, sha256 (from dist/SHA256SUMS) and
// an optional note. While the list is empty the page shows "coming soon" cards.
//
// Each entry's platform ("macos", "windows", "linux") is read off its file name,
// or set with `platform`. Once an entry for Windows is here, the landing page
// and the install page show their Windows texts too (assets/downloads.js), and
// a Windows visitor gets that card first. For the Windows zip, add (with the
// real size and the sha256 from dist\SHA256SUMS; see scripts/release/README.md):
//
//   {
//     title: "Windows · x86_64",
//     version: "0.1.6",
//     file: "downloads/connecto-0.1.6-windows-x86_64.zip",
//     size: "<NN.N> MB",
//     sha256: "<64 hex digits from SHA256SUMS>",
//     note: "Windows 10 (1809) or later"
//   },
window.CONNECTO_RELEASES = [
  {
    title: "Windows · x86_64",
    version: "0.3.10",
    file: "downloads/connecto-0.3.10-windows-x86_64.zip",
    size: "36.2 MB",
    sha256: "2820b87546c6624d31ac420d1041f89178da559f1787caf1dbd3cfdd267c334e",
    note: "Windows 10 (1809) or later"
  },
  {
    title: "macOS · Apple silicon",
    version: "0.3.10",
    file: "downloads/connecto-0.3.10-macos-aarch64.dmg",
    size: "26.8 MB",
    sha256: "bd2afecdef5176c17c641c8f179f6761588124e1c8d026c941cf3f070ed3c306",
    note: "macOS 11 or later"
  },
  {
    title: "macOS · Intel",
    version: "0.3.10",
    file: "downloads/connecto-0.3.10-macos-x86_64.dmg",
    size: "29.4 MB",
    sha256: "5f2c90b4617bde62a6172ad1bd4750d9dc2141f68879f8b071f1e53ae0799909",
    note: "macOS 10.15 or later"
  },
  {
    title: "macOS · Universal (both)",
    version: "0.3.10",
    file: "downloads/connecto-0.3.10-macos-universal.dmg",
    size: "55.5 MB",
    sha256: "a8aa26f93081baa265ef197211d13bfe48b4d65d34d2d29532675661f1f209f1",
    note: "macOS 10.15 or later"
  },
  {
    title: "Linux · x86_64 (experimental)",
    version: "0.3.10",
    file: "downloads/connecto-0.3.10-linux-x86_64.tar.gz",
    size: "30.4 MB",
    sha256: "073687f97dac4b38754f5fdaea874405c8cadc76324d0e66ab004d0e14a5ecb0",
    note: "Experimental · Debian 12 or compatible · agent provisioning is incomplete"
  },
  {
    title: "Linux · aarch64 (experimental)",
    version: "0.3.10",
    file: "downloads/connecto-0.3.10-linux-aarch64.tar.gz",
    size: "30.9 MB",
    sha256: "cf1cdbe617398eaefdb21d3e2a3570e4c0fb7deb94fc56d74a5daf7f2529c53d",
    note: "Experimental · Debian 12 or compatible · agent provisioning is incomplete"
  }
];

// Connecto Chat, the companion for a second computer: the same shape, shown in
// the Connecto Chat download dialog. Their checksums are published as .sha256
// files next to each download (`sha256File`) instead of inline.
window.CONNECTO_CHAT_RELEASES = [
  {
    title: "Windows · x86_64",
    version: "0.3.10",
    file: "downloads/connecto-0.3.10-chat-windows-x86_64.zip",
    sha256File: "downloads/connecto-0.3.10-chat-windows-x86_64.zip.sha256",
    note: "Unsigned · extract the ZIP and open connecto_flutter_app.exe"
  },
  {
    title: "macOS · Universal",
    version: "0.1.0",
    file: "downloads/connecto-chat-0.1.0-macos-universal.dmg",
    sha256File: "downloads/connecto-chat-0.1.0-macos-universal.dmg.sha256",
    note: "macOS 12 or later · Apple silicon and Intel"
  },
  {
    title: "Linux · x86_64",
    version: "0.1.0",
    file: "downloads/connecto-chat-0.1.0-linux-x64.tar.gz",
    sha256File: "downloads/connecto-chat-0.1.0-linux-x64.tar.gz.sha256",
    note: "Debian 12 or compatible with GTK 3 · run connecto_flutter_app in the extracted folder"
  },
  {
    title: "Linux · ARM64",
    version: "0.1.0",
    file: "downloads/connecto-chat-0.1.0-linux-arm64.tar.gz",
    sha256File: "downloads/connecto-chat-0.1.0-linux-arm64.tar.gz.sha256",
    note: "Debian 12 or compatible with GTK 3 · run connecto_flutter_app in the extracted folder"
  }
];
