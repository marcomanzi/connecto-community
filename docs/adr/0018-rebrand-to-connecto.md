# 0018. Rebrand from Magneto to Connecto

Status: Accepted. Site: https://connecto.studio. Relay: https://relay.connecto.studio.

## Decision

Everything named Magneto is named Connecto: crates (`connecto-*`), the app
(`connecto-app`), the hook helper and the shell command (`connecto-hook`, with the
`connecto` link in agent terminals), environment variables (`CONNECTO_*`), the
data folder (`~/Library/Application Support/connecto`), per-project `.connecto/`,
the built-in package id (`connecto`), Keychain services (`connecto`,
`connecto-remote`) and the remote-session wire identifiers (ALPNs
`connecto/remote/1`, the SSHSIG namespace `connecto-remote-auth@v1`, the ticket
prefix `connecto:`, the payload tag and binding label, the pairing key-derivation
domain). No `magneto` command alias is installed.

No remote-session build had been released, so the wire identifiers changed with no
compatibility path: **both peers must run a Connecto build**; a Magneto build and a
Connecto build cannot connect or pair.

## Existing installs

An install under the old name keeps working. Nothing is deleted, and nothing
outside the new names is changed. All of it runs by itself, and none of it runs
when `CONNECTO_DATA_DIR` (or the old `MAGNETO_DATA_DIR`) is set, which keeps a
trial build away from the real folders.

- **Data folder.** The copy of `magneto/` to `connecto/` that once ran at startup,
  and the importer for the later history of Magneto's conversations, have been
  removed: Connecto is the only build in use. Profiles that were already copied
  keep working: the startup repairs still read the marker `.migrated-from-magneto`
  and each profile's `.secure-storage-dir` note, so their Claude sign-in stays.
- **Keychain.** A secret (the Jev key, the device key, the relay token) is read
  under the new service name; if absent, it is read under the old one and copied
  to the new one. The old entry is left alone, except that "clear" removes both, so a
  cleared key does not come back. macOS may ask once to allow access to the old
  entry.
- **Projects.** A lockfile at `.magneto/packages.lock` is read when there is no
  `.connecto/packages.lock`, with the package id brought up to date; the next
  install writes the new lockfile and removes the old one in the same step, so
  the two never disagree. Files installed by the old built-in package are updated in
  place if the user has not edited them. Other files in `.magneto/` are left.
- **Settings and ids.** `magneto_home` in `settings.json`, and the package id
  `magneto` in a project's settings or a package's `extends`, are read as the new
  names.
- **Guides.** A guide block marked `magneto-automations` or `magneto-packages` in a
  `CLAUDE.md` or `AGENTS.md` is replaced by the new block, not left beside it.
- **Hooks.** The app never writes hooks into the user's `~/.claude` or `~/.codex`;
  they are passed to each terminal at launch. So there are no old hooks to replace,
  and terminals started by the new build only ever get the new ones.

## What keeps the old name, on purpose

The code that reads old names (Keychain, lockfile, guide markers) and its tests; the git history and the repository folder on
disk; and whatever the user wrote themselves (project folder names, automation
texts). Conversations already open in an agent that was told about a `magneto`
command still mention it; after a restart, the briefing names `connecto`.
