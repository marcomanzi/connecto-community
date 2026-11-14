# ADR 0009: unsandboxed code never reads session-writable state

**Context.** A session may write its own profile. ADR 0008 ran `login` outside the sandbox with that profile as both config and cwd, and D3 also ran `auth status` there, automatically, before every Start. The PM's exception covers only an interactive login started by the user. A probe (d3-review/PROBE-LOGIN-PROFILE.md) measured that an agent's `settings.json` changes what unsandboxed Claude status reports. None of the tried keys ran code, but the login was not completed and only a short list of keys was tried, so that is no proof.

**Decision.**
1. **Status runs inside the sandbox**, under the generated profile, after the canary, with cwd in the session temp folder. No automatic step runs a CLI outside the sandbox.
2. **Every login gets a new profile path.** Profiles are generations: `profiles/<tool>/g<N>/`, with the current number in an app-owned file no session can write. Log in creates `g<N+1>`, empty, and login and all later sessions use it. A sandbox's rules are fixed at launch and name only its own generation, so no earlier session or surviving process can write the new path. The old generation is deleted as far as possible, unread, and never restored. Login is refused while that tool's session is live.
3. **Login's cwd is the run folder,** which no session can write. It was the profile.

**Consequences.** **Log in again** resets that tool's history and settings for the project. The first login loses nothing. Status writes into the profile, so the app cannot tell the two apart, and every Log in takes a confirming second click. Status now reports what the session itself can read, which is the useful answer. An agent can still forge "logged in" with its own settings: the session starts and then fails, inside the sandbox, so nothing is gained.

**Why not empty the same path.** That was the first version of this decision. The Tester showed a detached child of an ended session writing a file into the emptied profile while the unsandboxed login ran (T-001). Rules by path cannot stop that for the same path, and macOS gives no reliable way to find a detached process. A new path removes the race instead of narrowing it.

**Residual, stated plainly.** Each re-login may leave the previous generation's Claude Keychain item behind, unmeasured until a real login. An old generation that could not be fully deleted stays where no CLI looks, and the pane warns because it may hold a credential. Status inside the sandbox is unmeasured under a real login: if it reads logged out straight after a good login, the session could not authenticate either, and that is reported, not worked around.

**Reverse if** a CLI gains a way to take its config and credentials from separate paths, which would allow a login profile no session can write.
