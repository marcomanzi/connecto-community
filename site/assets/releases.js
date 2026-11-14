// The downloads shown on the landing page and in the docs. Version 0.1.9 is the
// current build (0.1.0 to 0.1.8 are left out of this list; their files may stay on the server). The files are not in this repository: put each one in
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
    version: "0.1.9",
    file: "downloads/connecto-0.1.9-windows-x86_64.zip",
    size: "16.1 MB",
    sha256: "56b7d7d1edb9d7b76460a66ed07a27b9b0127128f0fa67c0262d69f8b938f4bf",
    note: "Windows 10 (1809) or later"
  },
  {
    title: "macOS · Apple silicon",
    version: "0.1.9",
    file: "downloads/connecto-0.1.9-macos-aarch64.dmg",
    size: "16.9 MB",
    sha256: "3761fc824c2015db7d1ff8ee5591801fa26f90416429c4718c422d3a119fdeca",
    note: "macOS 11 or later"
  },
  {
    title: "macOS · Intel",
    version: "0.1.9",
    file: "downloads/connecto-0.1.9-macos-x86_64.dmg",
    size: "18.6 MB",
    sha256: "f9c75a229180059afb5007912eb705ddd25f8b242d16b1284685c772be441103",
    note: "macOS 10.15 or later"
  },
  {
    title: "macOS · Universal (both)",
    version: "0.1.9",
    file: "downloads/connecto-0.1.9-macos-universal.dmg",
    size: "36.7 MB",
    sha256: "b1e77c2128ea3f6eb9a1fb0a6cdd13fa9717d694eefa2c68899c9ce40f576ba0",
    note: "macOS 10.15 or later"
  }
];
