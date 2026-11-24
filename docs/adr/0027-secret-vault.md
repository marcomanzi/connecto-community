# 0027: A vault of the person's secrets, used by terminals but never read by them

Status: Accepted and implemented: `connecto-vault` (store, encryption, web
proxy, program runner, ssh-agent, requests), `connecto-core/src/vault.rs`
(types and seams), `connecto-redact/src/known.rs` (hiding values),
`connecto-session/src/bridge/vault.rs` (`connecto secret`) and
`connecto-ui/src/vault_ui.rs` (Settings > Secrets, the request dialog). The
ssh-agent is macOS and Linux only. A paired machine keeps a host project's
secrets as described in "From a paired machine" below.

## Context

People want terminals to deploy, call APIs, run cloud CLIs and reach servers
over SSH, which needs tokens and keys. The only way so far was an
environment variable (the Pi and Together keys), which the agent can print,
and which then travels: `connecto read` returned raw scrollback, and
history, memory and audit only caught secrets of known shapes. There is no
sandbox (ADR 0013): anything placed in an agent's environment or files can be
printed, encoded or sent anywhere, and an agent runs as the person, so it can
read any file they can.

## Decision

1. **Storage, like IntelliJ's KeePass store.** Values go in Connecto's one
   secrets document (`connecto-secrets`), as `user.global.<name>` or
   `user.project.<project id>.<name>`, each encrypted (AES-256-GCM, a fresh
   nonce, the store key as associated data) under a master key kept apart:
   the macOS Keychain item `connecto`/`vault-key`, the Windows Credential
   Manager credential of that name, or `<data>/secrets/vault.key` (0600) on
   Linux and in builds with a data folder of their own. Names, scopes,
   descriptions, bindings and times go in `<data>/secrets/vault.json` (0600):
   listing never needs a value. A file this version cannot read is never
   written. Settings says which protection is in use.
2. **Scope.** A secret is app-wide or one project's. A terminal sees its
   project's secrets and the app-wide ones it does not override by name.
3. **Brokered use only.** A vault value never enters a terminal's process
   tree: no environment variable, no file. A terminal uses a secret only
   through a *binding* the person sets, which the app carries out itself.
   Setting the binding is the consent; there is no prompt per use. Every use
   is written to `<data>/secrets/uses.jsonl` by name, terminal and target.
   - *Web requests*: `connecto secret url NAME` gives
     `http://127.0.0.1:<port>/<terminal token>/NAME/`. The proxy sends a
     request on to the bound address (`https://`, or `http://` to this
     machine) with the bound header, drops the terminal's own header of that
     name, follows no redirect, asks for no compression, and removes the
     value in every form from the response.
   - *One program*: `connecto secret run NAME -- PROGRAM ARGS`. The app runs
     the program, if its resolved path is one the binding names, in the
     project folder, with a short environment plus the value in one
     variable, no input, and a 10-minute limit; output comes back with every
     vault value hidden. Shells and interpreters cannot be named, nor
     variables that steer how programs start.
   - *SSH*: `connecto secret ssh NAME` gives an ssh-agent socket (in a 0700
     folder) and the public key. The agent serves only processes of the
     terminal it was made for, answers only "list" and "sign", and reads the
     key from the vault for each request.
4. **Terminals can ask.** `connecto secret request NAME --reason '...'
   [--kind token|password|ssh-key] [--http ...] [--command ... --env ...]
   [--ssh]` puts a request in the window: a dialog over everything shows the
   terminal, its reason (as its words), where the secret will be kept, the
   use it proposes (the person may change it) and a field for the value. For
   an SSH key the dialog generates an Ed25519 pair and shows both halves.
   Approve keeps it; Decline does not. Either way the terminal gets a
   `[connecto notice]` with the outcome and how to use the secret, never the
   value. Three requests per terminal and twenty in all may wait.
5. **No op returns a value.** Identity is the bridge's (process ancestry),
   as for every op. Asking for or using a secret is refused under Pause.
6. **Hidden everywhere.** The app registers the vault with
   `connecto_redact::set_known_source`, so every `redact`/`prepare` in the
   process removes vault values first, in their exact form and as base64
   (both alphabets, every offset), hex and percent encoding. `connecto read`,
   which shows a screen as it is, removes only those (`hide_known`). The
   memory host adds them to its known secrets.
7. **Minimum length** of a value is 8 characters, so hiding it does not hide
   ordinary text.
8. **A request the person can judge (2026-11-24).** A guard against an
   empty, plainly generic or overlong reason, not a judge of it: the reason,
   as it will be shown, is at most 300 characters (a longer one is refused
   with a request to shorten it, never cut, so the person sees all that was
   checked), and has at least two different words of three letters or more
   that are not filler, in English or Italian ("needed", "token", "access",
   "task", "operations", "serve", "lavoro"...), one of which is not part of
   the secret's name (`vault::check_reason`). "Restart staging PostgreSQL"
   and "Deploy to Vercel" (for `VERCEL_TOKEN`) pass; "deploy deploy", "Mi
   serve per questo lavoro" and "I need it for normal operations" do not.
   The refusal gives an example and never repeats the reason (it may hold
   something pasted by mistake). The person still decides whether the
   reason is true. Reasons, terminal ids and names are shown on one line
   with control, separator, bidirectional and zero-width characters made
   spaces and double quotes made single (`vault::display_line`), so none
   can break out of its quotes. The dialog first says what Connecto knows:
   the terminal's id, its kind of agent and its project (with the machine,
   for a paired host's). The terminal's name, which an agent may have
   chosen, is shown apart as an unchecked label. The reason is shown large
   under "What it is for". "Where it will be used" says, in a sentence per
   use, exactly what Approve grants (`Binding::plain_use`: the web address
   and header, the program's path and variable, SSH, or Connecto only).
   Each of those values is shown as it is, in double quotes, with `\` and
   `"` escaped and every character that is not printable ASCII written as
   `\u{hex}` (`vault::display_value`), so two different destinations never
   look the same. When what Approve grants differs from the terminal's
   proposal (of several proposed uses the form keeps one), it says so and
   lists the proposal. "Generate a new SSH key on <host> and approve" is
   offered only while the use chosen is SSH, which is all a key made there
   is ever given.
   A proposed program path with a line break, a control character or a
   space at either end is refused where a proposal enters the form: a
   terminal's request here, and a paired host's request on arrival (that
   request is then not shown on the peer, and is answered on the host)
   (`vault::check_proposed`). The uses editor keeps one path per line,
   trimmed, so it never turns one proposed path into others. Saved uses are
   not checked by this rule, so a vault saved by an earlier version still
   opens whole.
   A paired host's reason is checked again on arrival by the same rule:
   one with no reason, a generic one or an overlong one (from a Connecto
   older than this check) is not shown on the peer, and is answered on the
   host itself. One this machine accepted reaches the peer exactly as it
   was shown here.

## Consequences

- On macOS an agent cannot get a vault value with Connecto's help, nor by
  copying the secrets file (ciphertext) without the Keychain asking the
  person. On Windows the master key is readable by any program running as the
  person, and on Linux it sits beside the file: there the encryption only
  keeps values out of a plain read of the document. Connecto's own keys
  (device, relay, Jev) are not covered by this change.
- A program named in a command binding can still reveal the value in a form
  the redactor does not know; the Settings and dialog wording says so. Web
  and SSH bindings never put the value where a terminal can see it.
- The proxy's token is per terminal and lasts for the app's run. Any process
  that learns it (another terminal of the same person, told it) can use that
  terminal's web secrets; it cannot see them.
- Remote viewing (ADR 0017) streams raw output to the person's own paired
  machines and is not redacted.
- The Pi and Together keys still go to terminals as environment variables.
  Moving them to web-request bindings is follow-up work.
- A new request wakes the window (the vault calls the app's wake), so an idle
  window still never repaints on its own.

## From a paired machine (2026-11-22)

The person keeps a host project's secrets, and answers its terminals'
requests, from a paired Connecto (ADR 0017) as on the host itself.
`connecto-net/src/remote_secrets.rs`; the window's side is
`connecto-ui/src/remote_vaults.rs`.

1. **That project's own secrets only.** `ProjectAsk::Secrets { after }`
   lists them in name order, one page (at most 1 MiB as sent) after
   `after`, until the last: names, descriptions, uses, times, and the
   public half of an SSH key. A peer reads at most 64 MiB of them at once.
   Past that it keeps what it read, and Settings offers "Next secrets",
   which reads on from the last name, and "First secrets". So every secret
   is reachable within that bound on memory (`VaultView::pages`,
   `show_secrets_from`). Whatever the vault says of its problems may name
   other secrets, so a peer is told only that there is a problem.
   A refusal of a change is classified where it happens
   (`VaultControl::change`, `ChangeError`). A refusal of the change itself
   (its name, value or use, a request no longer waiting) is told as it is.
   The vault's own failure (storage, a frozen list) reaches the peer in
   fixed words. So a reply never depends on other secrets.
   `ProjectAsk::SecretsAct` adds, replaces, removes, changes the use of,
   or generates one, or answers a request, always in `Project(<that
   project>)`. App-wide secrets and other projects' are never listed or
   touched. An answer to a request of another project's terminal is
   refused ("That request is no longer waiting.").
2. **Full control, as on the host.** The peer may add, replace, remove and
   change how a secret is used. The host checks each change as it checks
   its own person's (name, value length, bindings). A value and its use
   change together, checked together first. When the use changes too, the
   old use is taken off on disk before the new value is written. So a
   failed write or a crash leaves either the secret as it was or the new
   value usable by no terminal, never the new value under the old use (a
   key sent as a web header). Whenever a save of the list fails, the list
   in memory goes back to what is on disk: no use, and no new secret, is in
   force that the disk does not have. A refused answer writes nothing. The
   vault says, from inside the change, whether anything was written, and a
   change that failed part way is told to the host's person, like any
   other. A key
   generated for a request is used only through the ssh-agent, whatever
   the request proposed. Removing a
   project's SSH key closes only the sockets of that project's terminals
   (or, for an app-wide key, those of terminals that used it).
3. **The host's person is told.** Every change from a paired machine is
   added to `VaultView::remote_changes` (`note_remote_change`), and kept
   until the window has taken it (`ack_remote_changes`), however many. The
   host's window shows them all in a notice that scrolls: what changed and
   in which project, never a value.
4. **The value goes one way.** It travels inside the peer link, which is
   QUIC with TLS 1.3 between the two machines' keys. The relay forwards
   packets it cannot read. Nothing sent back holds a value: the list has no
   values, and an action's answer is empty or the vault's refusal, which
   never names one. `SecretValue` prints as `…`. The guard's audit records
   `project-secret-<verb>` on `<project>/secret:<name>`. An answer is
   recorded by the name of the secret the host's request asks for. A value
   the window here sends is not kept: the form is cleared once it is sent.
   Nothing that holds a value prints it in debug output: the wire's
   encoded ask (`AskBytes`), `AskAnswer`, `GeneratedKey`, and the window's
   forms and actions, and the frame decoder's buffer. The value fields'
   edit history (egui's undo) is cleared once a value is sent, when the
   place or the request changes, and once neither Settings nor a request
   is open.
5. **SSH keys are made on the host.** "Generate" for a remote project, and
   for a remote request, asks the host to make the key and keep it
   (`put_generated_key`, `AskAnswer::GenerateSshKey`). The private half
   never leaves the host, and the peer shows the public half from the list.
   The window decides this by the chosen project, not by whether its
   machine is connected. With that machine gone, no key is made at all,
   and a key made here is dropped when the person picks another machine's
   project.
6. **Requests show on both machines.** A peer asks for the waiting requests
   at each heartbeat (`Message::SecretAsksProbe`). It gets only those of the
   projects its latest list holds, and again at once after an answer it
   sent. The host keeps at most 20 waiting requests, and a request's
   proposed uses are left out when longer than 4 KiB, so all of them fit
   in one answer (192 KiB). The request then says so
   (`SecretAsk::proposed_omitted`), and the dialog asks the person to
   choose a use before Approve. A request's number is random, not a count, so
   an answer from before the host started again never meets a new request. The request dialog opens on every machine showing that project.
   The first answer wins; the request then leaves the host's vault, and
   every other dialog closes when it is next asked. A late answer is
   refused. The window follows each answer it sent by the command that
   carried it (`VaultControl::answer_state`), so a refusal opens the dialog
   again even when it says the same as the last one.
7. **Pause and the guard.** Every secret ask passes the host's guard as the
   `remote` actor, and Pause refuses them all. Pause also refuses the
   probe of requests: a request names a secret, a terminal and its reason.
8. **Older hosts.** A host from before this answers that it does not know
   the ask (or refuses the stream). The section then says "The Connecto on
   that machine is too old to keep this project's secrets from here.
   Update Connecto there…", and every change is refused here. Nothing is
   ever put in this machine's vault instead: a remote project's scope
   always goes to its machine, and is refused when that machine is not
   connected.

Consequences: a paired machine already had full use of the host's
terminals; it can now also set where a project's secret is sent (a web
binding's address) or which program gets it. The host's notice is how the
person there learns of it. The probe adds one small stream per host each
heartbeat (3 s).
