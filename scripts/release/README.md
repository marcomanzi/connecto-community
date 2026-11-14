# Release builds

Everything lands in `dist/` (git-ignored). Website: https://connecto.studio

## Artifact names

`VERSION` is the workspace version in the root `Cargo.toml`
(`[workspace.package] version`). The names are stable, so a site can link them.

| Platform | Artifact | Built by |
| --- | --- | --- |
| macOS, Apple Silicon | `connecto-VERSION-macos-aarch64.dmg` | `scripts/release/build-macos.sh aarch64` |
| macOS, Intel | `connecto-VERSION-macos-x86_64.dmg` | `scripts/release/build-macos.sh x86_64` |
| macOS, both in one app | `connecto-VERSION-macos-universal.dmg` | `scripts/release/build-macos.sh universal` |
| Linux, x86_64 | `connecto-VERSION-linux-x86_64.tar.gz` | `scripts/release/build-linux.sh x86_64` |
| Linux, aarch64 | `connecto-VERSION-linux-aarch64.tar.gz` | `scripts/release/build-linux.sh aarch64` |
| Windows, x86_64 | `connecto-VERSION-windows-x86_64.zip` | `scripts/release/build-windows.ps1` |
| Checksums | `SHA256SUMS` (`shasum -a 256 -c SHA256SUMS`) | every script adds its line |

`scripts/release/build-macos-all.sh` builds the three macOS files.

Each archive holds what ships together: `connecto-app` (the app), `connecto-hook`
(the helper) and `connecto` (the command; the same program as `connecto-hook` under
another name, a symlink on macOS and Linux, a copy on Windows) plus `README.txt`. On
macOS they sit in `Connecto.app/Contents/MacOS/` beside each other, which is where
the app looks for its helper. The `.dmg` also holds an `Applications` link.

To bump the version, edit `version` under `[workspace.package]` in `Cargo.toml` and
run `cargo update -w` (it refreshes `Cargo.lock`); the scripts read it from there.
`CONNECTO_VERSION` overrides it for a one-off build.

## What was built and tested

On an Apple Silicon Mac (macOS) and on a Windows 11 Pro x86_64 PC (Windows):

| Target | State |
| --- | --- |
| aarch64-apple-darwin | built, the app ran (isolated data folder) |
| x86_64-apple-darwin | built (minimum macOS 10.15, and a build with 10.13 linked too), the app ran under Rosetta |
| universal | built (`lipo`), both slices verified |
| x86_64-pc-windows-msvc | built with `build-windows.ps1` (2026-11-13, Rust 1.99.1, VS 2022 Build Tools, Windows SDK 10.0.26100, Windows PowerShell 5.1 and PowerShell 7); the zip unpacked into a fresh folder, the app ran (isolated data folder) and installed its helper; `cargo test --workspace` 1392 passed, 0 failed, 8 ignored |
| x86_64 / aarch64 Linux | NOT ported: not built since the Windows port, expected to fail, see below |
| aarch64 Windows (ARM64) | not supported, see below |

The macOS minimum: Rust's own floor is 11.0 on arm64 and 10.12 on Intel; the
dependencies (eframe/wgpu, iroh) link fine against 10.13. The default for Intel is
10.15; change it with `CONNECTO_MIN_MACOS`. Note that Claude Code itself needs a
newer macOS than that, so an old Intel Mac can run Connecto but not the agents.

## Windows

The zip holds `connecto-app.exe`, `connecto-hook.exe`, `connecto.exe`
(a copy of the helper) and a `README.txt` for the user. The app copies its helper
from beside itself into `%APPDATA%\connecto\cli\bin` when it starts.

**What the build needs** (x86_64 Windows 10 1809 or later, or Windows 11):
rustup with the stable MSVC toolchain (`x86_64-pc-windows-msvc`, at least the
`rust-version` in `Cargo.toml`), Visual Studio 2022 Build Tools with the C++
workload (`cl.exe`, `link.exe`) and a Windows 10/11 SDK. `scripts\windows\bootstrap.ps1`
checks a machine (`-CheckOnly`) and installs what is missing with winget. Not
needed: NASM, Perl, cmake, clang (ring's crates.io package ships its assembly, and
no locked crate builds with cmake; `libproc`, the only `bindgen` user, is macOS-only).

**Build:**

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\release\build-windows.ps1
```

It works in Windows PowerShell 5.1 and PowerShell 7, runs the same release feature
check as `check-release-features.sh`, builds with `--locked`, honours
`CARGO_TARGET_DIR` and `CONNECTO_DIST`, and writes `dist\connecto-VERSION-windows-x86_64.zip`
(entries with `/`, one top folder `Connecto`, so unzipping it into
`%LOCALAPPDATA%\Programs` gives the install folder the updater uses) and its line in `dist\SHA256SUMS` in the same
format as the bash scripts (lowercase hex, two spaces, name, LF, no BOM), so
`shasum -a 256 -c SHA256SUMS` works on a Mac. The relay token rules are the same
as below (`$env:CONNECTO_RELAY_TOKEN`, `scripts\release\relay-token.local`, or
`$env:CONNECTO_NO_RELAY_TOKEN = '1'`).

**What is different on Windows:**

- Secrets: by default the same file as on macOS, `%APPDATA%\connecto\secrets\secrets.json`
  (private to the user through `%APPDATA%`). With the system store chosen in
  Settings, Windows Credential Manager holds the document (`connecto/secrets`)
  where a Mac would use the Keychain. One credential holds 2560 bytes; a larger document
  is split across up to 16 credentials `connecto/secrets.<generation>.<n>`, written
  so the item always names a whole document (40960 bytes in all, about 75 paired
  machines; more is refused, never cut short).
- Agent trust: Authenticode signature plus publisher check of the Claude and Codex
  programs (PowerShell's `Get-AuthenticodeSignature`), instead of `codesign` and
  Apple team ids. One documented exception: Codex's bundled `codex-path\rg.exe`
  is unsigned and is accepted as the single, switchable exception
  (`connecto-store` `ACCEPT_UNSIGNED_CODEX_RG`).
- The hook and bridge servers are named pipes (`\\.\pipe\connecto-hook-…`,
  `\\.\pipe\connecto-bridge-…`, this user only, no remote clients); the caller's
  pid comes from the pipe and must descend from the terminal's root process.
- Terminals are PowerShell in ConPTY: PowerShell 7 (`pwsh`) when present (setup
  offers it), else Windows PowerShell 5.1. The built-in status lines run as
  PowerShell twins of the `.sh` scripts, so no `bash` or `jq` is needed.
- The agents are installed with their official Windows installers
  (`https://claude.ai/install.ps1`, `https://chatgpt.com/codex/install.ps1`).
- The menu is inside the window (Windows has no system menu bar).
- SSH: with `SSH_AUTH_SOCK` unset, the Windows OpenSSH agent's pipe
  `\\.\pipe\openssh-ssh-agent`. Implemented and unit-tested, but not yet tried
  against a real agent.

**Unsigned unless set up.** Without signing variables the Windows build is not
signed: Windows SmartScreen warns the first time it is started on a machine ("More
info", then "Run anyway"), and the README in the zip says so. See "Signing
(Windows)" below for the variables that sign it.

**ARM64 Windows is not supported:** there ring compiles C with clang, which the
bootstrap does not set up, and no ARM64 build or test has been made.

## Linux is not ported

The Linux script and CI job are in place so they can be run on a native machine,
but the build has not been made since the Windows port and is expected to fail. By
reading the code, what stands in the way:

- The secret store: the default secrets file works on Linux too (0600 in a 0700
  folder); there is no system-store option there (the Keychain and the Credential
  Manager are the macOS and Windows ones). (`security-framework` is a dependency of
  `connecto-secrets` only, for macOS.)
- Agent trust: `connecto-store` checks Apple signatures (`codesign`) on macOS and
  Authenticode on Windows; Linux has no equivalent.
- Peer credentials: the hook and bridge sockets read the caller's pid with
  `LOCAL_PEERPID` (`nix`'s `LocalPeerPid`, macOS only); Linux needs `SO_PEERCRED`.
- Unix-only code: 36 files mention `std::os::unix`, now behind `cfg` gates, with
  macOS and Windows branches written and tested; the Linux branches are not.
  (`libproc` is a macOS crate, used by `connecto-session` only; other platforms
  read the process tree with `sysinfo`.)
- Paths: on every Unix the data folder is `~/Library/Application Support/connecto`,
  the macOS folder; Linux wants an XDG folder (`~/.local/share`).
- Tools: the computer's name comes from `scutil` (macOS); elsewhere on Unix only
  `HOSTNAME` is tried.

Rough size: a week or two of focused work (secret store, trust model, peer
credentials, paths), much of it smaller now that the per-platform modules exist.

## Relay access token (required)

A fresh install talks to the default relay (`https://relay.connecto.studio`), which
asks for an access token. Release builds carry that token, so a new install works
without setup. Every build script reads it from `CONNECTO_RELAY_TOKEN`, or else
from `scripts/release/relay-token.local` (one line, git-ignored). Without either,
the release build fails. To build on purpose with no built-in token, set
`CONNECTO_NO_RELAY_TOKEN=1`. The scripts never print the token.

The app uses the built-in token only for the default relay, and only when the user
has not stored a token of their own. A token inside a shipped binary can be
extracted, so it only keeps casual traffic off the relay; it is not a secret.

## Updates (the in-app updater)

Connecto updates itself from `https://connecto.studio/downloads/update.json`, a
feed signed with ed25519 (`crates/connecto-update`; the user's side is
`site/docs/updates.html`). Each release adds, beside the usual downloads:

| File | Made by | Installed on |
| --- | --- | --- |
| `connecto-VERSION-macos-aarch64.app.zip`, `connecto-VERSION-macos-x86_64.app.zip` | `build-macos.sh aarch64` / `x86_64` (the app, zipped with `ditto`, stapled when notarized) | Macs of that architecture |
| `connecto-VERSION-windows-x86_64.zip` | `build-windows.ps1` (the usual zip; its top folder is `Connecto`) | Windows, into `%LOCALAPPDATA%\Programs\Connecto` |
| `update.json` | `make-update-feed.sh` | read by every install |

**The key pair, once.** Make it on the release Mac:

```sh
cargo run -q -p connecto-update --bin connecto-update-feed -- keygen scripts/release/update-signing-key.local
```

It writes the private key to `scripts/release/update-signing-key.local` (0600,
git-ignored: never commit it, keep a copy in your password manager) and prints
the public key. Put the public key in `crates/connecto-update/update-public-key.txt`
and commit that file: builds made from then on check feeds against it. A build
with that file empty has updates turned off ("This build has no update key").
For CI, add the private key text as the repository secret
`CONNECTO_UPDATE_SIGNING_KEY`. Losing the private key means existing installs
can no longer update themselves (a new key needs a new build, downloaded by hand).

**Each release**, after the builds:

```sh
scripts/release/make-update-feed.sh "One line of release notes"
```

It finds the update archives of this version in `dist/`, and writes
`dist/update.json` with each one's URL, size and SHA-256, signed. The key comes
from `CONNECTO_UPDATE_SIGNING_KEY`, `CONNECTO_UPDATE_SIGNING_KEY_FILE`, or
`scripts/release/update-signing-key.local`. Upload the archives to `downloads/`
on the server first and `update.json` last, so no install reads a feed whose
files are not there yet. `connecto-update-feed verify dist/update.json` checks a
feed against the public key built into this checkout.

In CI, the `checksums` job makes `update.json` (artifact `update-feed`) when the
secret is set.

The updater sends the user agent `Connecto-Updater/<version>`; the site's
download count (`scripts/site/download-stats`) leaves those downloads out of
the total and counts them apart as `updates`.

## Signing and notarization (macOS, optional)

All driven by environment variables; nothing is stored in the repository, and the
scripts never look for identities on their own. Without them the build is unsigned
(only the ad-hoc signature Apple Silicon needs) and the script says so.

| Variable | What to supply |
| --- | --- |
| `CONNECTO_SIGN_IDENTITY` | the name or hash of a `Developer ID Application` certificate in the keychain |
| `CONNECTO_NOTARY_PROFILE` | a profile made once with `xcrun notarytool store-credentials` (Apple ID, team id, app-specific password) |
| `CONNECTO_ENTITLEMENTS` | optional entitlements plist for the hardened runtime |

With the identity the app and helpers are signed with the hardened runtime and
the `.dmg` is signed. With the profile too it is notarized and stapled.
`CONNECTO_BUNDLE_ID` (default `studio.connecto.app`) changes the bundle identifier.
Windows: see the next section.

## Signing (Windows, optional)

`build-windows.ps1` signs `connecto-app.exe` and `connecto-hook.exe` with
Authenticode after the build and before the zip (`connecto.exe` is copied from the
signed helper), timestamps them (RFC 3161, SHA-256) and checks each signature with
`signtool verify /pa`. It uses `signtool.exe` from the newest Windows 10/11 SDK
(or `CONNECTO_SIGNTOOL`). Everything comes from environment variables; nothing is
stored in the repository and nothing secret is printed. The first match wins:

| Way | Variables |
| --- | --- |
| Azure Artifact Signing (formerly Trusted Signing) | `CONNECTO_WIN_ARTIFACT_SIGNING_ENDPOINT` (the account's regional endpoint, such as `https://weu.codesigning.azure.net`), `CONNECTO_WIN_ARTIFACT_SIGNING_ACCOUNT`, `CONNECTO_WIN_ARTIFACT_SIGNING_PROFILE` (the certificate profile), `CONNECTO_WIN_ARTIFACT_SIGNING_DLIB` (the x64 `Azure.CodeSigning.Dlib.dll` from the NuGet package `Microsoft.ArtifactSigning.Client`, which needs the .NET 8 runtime); Azure sign-in through `DefaultAzureCredential`, for example `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` of an app registration that has the role "Artifact Signing Certificate Profile Signer" |
| A certificate in the Windows certificate store (an OV or EV certificate on a USB token, or in a cloud HSM whose key storage provider is installed) | `CONNECTO_WIN_CERT_SHA1` (its thumbprint) |
| A `.pfx` file | `CONNECTO_WIN_PFX`, `CONNECTO_WIN_PFX_PASSWORD` (new public certificates are issued only on hardware since June 2023, so this is mainly for older or test certificates) |

`CONNECTO_WIN_TIMESTAMP_URL` names the timestamp server: Artifact Signing defaults
to `http://timestamp.acs.microsoft.com`; with a certificate it is required (the CA
names its own). `CONNECTO_WIN_REQUIRE_SIGNED=1` makes the build fail instead of
producing an unsigned zip; set it for releases once signing works. The summary at
the end says `signed: yes (<way>)` or `signed: no`, and the README in the zip
follows.

Not yet tried: no certificate or Artifact Signing account exists yet, so the
signing path has not run. The first signed build should be checked on a clean
Windows PC (Properties, Digital Signatures; and `Get-AuthenticodeSignature`).

**What the options cost and give** (prices change; check before buying):

- **Azure Artifact Signing:** a monthly Azure subscription fee (Basic tier), no
  hardware, works from CI, short-lived certificates renewed by Microsoft. Public
  trust is open to organizations in the US, Canada, the EU, the UK and a few other
  countries, but to individual developers only in the US and Canada; identity
  validation takes 1 to 20 business days. It issues no EV certificates.
- **OV certificate from a CA** (Sectigo, DigiCert, SSL.com, Certum, ...): about a
  year at a time, issued to a person or a company; the key must be on a hardware
  token or in the CA's cloud HSM. Signing in CI needs the cloud HSM option.
- **EV certificate:** more expensive and for registered organizations only. Since
  2024 Windows no longer gives EV certificates instant SmartScreen reputation, so
  it buys little over OV here.

With any of them SmartScreen may still warn for a while on a new release, until
downloads build reputation; a signature makes the publisher name show instead of
"Unknown publisher".

## CI

`.github/workflows/release-build.yml` runs the same scripts on native runners. It
starts only by hand (Actions, "Run workflow"), only uploads the files as run
artifacts and never publishes a release. For signed macOS builds add the repository
secrets `CONNECTO_SIGN_IDENTITY` and `CONNECTO_NOTARY_PROFILE` (a CI runner also
needs the certificate and notary profile installed, which is a setup of its own).
The `windows` job (`windows-2022`) adds the `x86_64-pc-windows-msvc` target, as
the macOS job adds its targets, runs `build-windows.ps1` and uploads the zip as
the artifact `windows-x86_64`. It ran on GitHub for the first time on 2026-11-13
(about 16 minutes) and its zip started on a Windows 11 PC. The `checksums` job puts
every artifact's line into one `SHA256SUMS`.

The `windows` job signs with Azure Artifact Signing when the repository variable
`CONNECTO_WIN_ARTIFACT_SIGNING_ACCOUNT` is set: it installs the
`Microsoft.ArtifactSigning.Client` package with `nuget` and passes the repository
variables `CONNECTO_WIN_ARTIFACT_SIGNING_ENDPOINT`, `..._ACCOUNT` and `..._PROFILE`
and the secrets `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and `AZURE_CLIENT_SECRET` to
the build. Without the variable the zip is unsigned, as before. A certificate on a
USB token cannot be used from GitHub's runners.

"Run workflow" has two inputs. `only` limits the run to one platform (`macos`,
`linux` or `windows`; the default `all` builds every platform). `no_relay_token`
makes a test build with no built-in relay access token (the scripts'
`CONNECTO_NO_RELAY_TOKEN=1`); never publish such a build.

There is no test CI (no `cargo test` on push or pull request) for any platform.
Recommended: one workflow that runs `cargo clippy --workspace --all-targets` and
`cargo test --workspace` on `macos-14` and `windows-2022`.

The build steps need one more repository secret, `CONNECTO_RELAY_TOKEN`: the relay
access token built into the app (see above). Without it the macOS, Linux and
Windows build steps fail with "no relay access token". The workflow passes the
secret only to those build steps, as an environment variable, and nothing prints it.
Add it under Settings, Secrets and variables, Actions.

## Publishing a build on the site

The site (`site/`, see its README) lists the downloads in `site/assets/releases.js`;
the files themselves go to `downloads/` on the server, never into this repository.
For each file: upload it under its exact name, then add or change its entry
(`title`, `version`, `file`, `size`, `sha256`, `note`) with the size and the
SHA-256 of that very file.

The Windows zip, from the Mac (the Windows texts of the landing page and of
`docs/install.html` appear by themselves once the entry is there):

1. Get a zip built **with** the relay token, from the same commit as the macOS
   files: either the artifact `windows-x86_64` of a manual run of the
   `release-build` workflow (with the `CONNECTO_RELAY_TOKEN` secret set; its
   `SHA256SUMS` artifact has the line), or a run of `build-windows.ps1` on a
   Windows machine with `CONNECTO_RELAY_TOKEN` or `relay-token.local`, copying
   the zip and the zip's line of `dist\SHA256SUMS`. A zip built with
   `CONNECTO_NO_RELAY_TOKEN=1` is for testing only: never publish it.
2. Check it on the Mac, in the folder that holds the zip:
   `shasum -a 256 connecto-VERSION-windows-x86_64.zip` must print the line from
   that `SHA256SUMS` (or put the line in a file and run `shasum -a 256 -c` on it).
   Add the line to the Mac's `dist/SHA256SUMS` if you publish that file.
3. Size: `stat -f%z connecto-VERSION-windows-x86_64.zip` gives the bytes; write
   it as the macOS entries are written (MB, one decimal).
4. Upload the zip to `downloads/` on the server as
   `connecto-VERSION-windows-x86_64.zip`.
5. In `site/assets/releases.js` add the entry its header comment shows:

   ```js
   {
     title: "Windows · x86_64",
     version: "VERSION",
     file: "downloads/connecto-VERSION-windows-x86_64.zip",
     size: "NN.N MB",
     sha256: "<the 64 hex digits from SHA256SUMS>",
     note: "Windows 10 (1809) or later"
   },
   ```

6. In `site/docs/install.html`, the first note names the current version and the
   macOS files; bump it as usual (the Windows file name in it is filled in from
   the entry).
7. Preview: `python3 -m http.server 8080 --directory site`, then check the
   landing page (a Windows card with its SHA-256 and the SmartScreen line; the
   Windows texts under the cards) and `docs/install.html#windows`.