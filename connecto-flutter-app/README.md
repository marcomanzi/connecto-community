# Connecto Chat

A Flutter peer for iOS, Android, Windows, and Linux. The host runs the CLI. The app shows submitted messages, completed replies, activity, and interactive questions.

## Run

Use Flutter 3.47 or later, Rust via rustup, and the Flutter build tools for the target platform. Build from this checkout so the native library can use the same Connecto peer implementation as the host.

```sh
cd connecto-flutter-app
flutter pub get
flutter run
```

Run the desktop host built from this checkout. In its remote access settings start device pairing, then choose **Pair a host** in the app. Enter each device’s code on the other and confirm the name on both. Development builds using the default relay need its access token in the relay settings on each device, or supplied as `CONNECTO_RELAY_TOKEN` at build time.

Open a project on the host, connect to the host in the app, then choose the project, its agent (Claude, Codex, Pi or Antigravity) and the model, or **Default** for the agent's own. The host offers Pi's models from its providers, Codex's from its own model cache (`models_cache.json` in the Codex home the project uses), and a short list for the others; a host from before model choice offers only the default. The host creates an ordinary named terminal, `app-session`, on that model. Projects with an agent working, or with background work, are listed first. Opening that project again reuses the terminal. Closing the client leaves a live turn running. After a host restart, use **Start session** to resume.

With an attachment-capable host, pick files in the composer or drop them onto the desktop chat. A message can include up to four files and 10 MiB combined. Connecto uploads them before sending the prompt and gives the agent their paths on the host. The agent may ask for permission to read them, depending on its configured policy. The host keeps the files in its Connecto data folder under `chat-attachments/<project-id>`, outside the repository. A subsequent upload clears that project's files older than seven days.

Claude starts with `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=50`. Codex uses its built-in compaction. Pi respects its own auto-compaction setting, enabled by default. [Pi compaction reference](https://docs.pi.dev/latest/compaction)

The chat reads normalized prompt and final-reply events from Connecto’s host hooks. Recent messages are held on the host while the project stays open, bounded to 256 messages and 192 KiB. The CLI retains its own session according to its settings. The client uses the current CLI dialog to show interactive questions and permission choices. It can answer ordinary assistant questions in the composer.

## Calls

On iOS 26 and macOS 26 or later, the call button in a project's chat starts a hands-free call with its agent. What you say is recognized on the device and sent as a chat message once you pause. The reply is read aloud, and you can talk over it to cut it short. Long replies are shortened on the device by Apple's Foundation Models when they are available. Otherwise the call reads the first two sentences. Say "stop" while the agent works to interrupt it, or "hang up" to end the call. Choices the agent offers still need a tap on screen. Audio never leaves the device. See [ADR 0028](../docs/adr/0028-voice-calls-on-device.md).

## Verify

```sh
flutter analyze
flutter test
cargo test --manifest-path native/Cargo.toml
```

## Mobile stores

The iOS bundle ID is `studio.connecto.connectoFlutterApp` under Apple team
`Y6ZS3MW7G2`. The Google Play application ID is
`studio.connecto.connecto_flutter_app`. Both take their version and build number
from this app's `pubspec.yaml`. Increment the build number for subsequent uploads.

From the Flutter app directory on the release Mac, build with the default relay token in
`../scripts/release/relay-token.local`. The native build hook reads that file for each architecture:

```sh
flutter clean
flutter pub get
flutter build appbundle --release
flutter build ipa --release
```

Google Play uses the upload key configured in the ignored `android/key.properties`.
It contains `storeFile` (absolute path or relative to `android/`), `storePassword`,
`keyAlias`, and `keyPassword`. Back up the upload keystore and credentials privately.
The signed bundle is in `build/app/outputs/bundle/release/`.

For iOS, sign in to Xcode with the publisher account and enable automatic signing for
the Runner target. Upload the signed IPA from `build/ios/ipa/` to the matching
App Store Connect record. For app review, provide access to a dedicated Connecto
host and pairing instructions so the reviewer can open a project and exchange messages.
