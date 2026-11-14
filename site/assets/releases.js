// The downloads shown on the landing page and in the docs. Version 0.1.8 is the
// current build (0.1.0 to 0.1.7 are left out of this list; their files may stay on the server). The files are not in this repository: put each one in
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
    version: "0.1.8",
    file: "downloads/connecto-0.1.8-windows-x86_64.zip",
    size: "16.1 MB",
    sha256: "3a393dc6a655a2fde1b01eecd6d42f5d8c65722105ab3c498061ed932af2a95c",
    note: "Windows 10 (1809) or later"
  },
  {
    title: "macOS · Apple silicon",
    version: "0.1.8",
    file: "downloads/connecto-0.1.8-macos-aarch64.dmg",
    size: "16.9 MB",
    sha256: "6746ff561adfbb4ccf2b0a0a84ea0685c9ab3ddbf45d72211667f13e1f7b1bb0",
    note: "macOS 11 or later"
  },
  {
    title: "macOS · Intel",
    version: "0.1.8",
    file: "downloads/connecto-0.1.8-macos-x86_64.dmg",
    size: "18.6 MB",
    sha256: "b1f26513206b0e50db7d35d6b106d066f8f2750b8d2d1737745e7f60c1c68ba0",
    note: "macOS 10.15 or later"
  },
  {
    title: "macOS · Universal (both)",
    version: "0.1.8",
    file: "downloads/connecto-0.1.8-macos-universal.dmg",
    size: "36.7 MB",
    sha256: "3021f75c8baf41308e163c7e00556587a335dbd0aebb93155a43258d1d543daa",
    note: "macOS 10.15 or later"
  }
];
