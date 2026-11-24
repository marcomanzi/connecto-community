## Choosing the agents

Every role runs in its own Connecto terminal, started with `connecto add`. A role is either a coder, which writes, or a reviewer, which checks what a coder wrote. Which coding agent runs each depends on which ones this project can start.

Before you start the first terminal, run:

```bash
connecto agents --roles
```

It lists the coding CLIs the project allows, whether each is ready (installed, offered and signed in), and the exact command that starts each ready one. Below that, it says which CLI takes each role, and the command that starts it, Pi and OpenCode on the model their user configured. Start the coder from its `coder` line and every reviewer from its `reviewer` line. Tell the user in one line which agents you chose. If the user asks for other agents or models, use those.

- If the `reviewer` line says `(same CLI)`, no other coding CLI is ready, so the reviewer is a fresh session of the coder's CLI. Tell the user so in one line, and say "same CLI" wherever you pass on its verdict.
- If a line says `(unverified)`, Connecto couldn't check that CLI's sign-in before it starts. If its terminal stops at a sign-in screen, handle it as a failed start, below. If it says `via <provider>`, that CLI runs the provider's own model, not the usual one: name it that way to the user.
- If a line says `none`, or an agent it would prefer shows `checking`, run the command again in a moment. If no CLI is ready, tell the user which ones are unusable and why, as the list says, and wait.
- If `connecto agents` fails with `unknown command "agents"`, or answers that this Connecto app is older than the command, Connecto is too old for it: start every role on the CLI you're running in, and say "same CLI" for the reviewer as above: Codex with `connecto add codex --model gpt-6.2-astra --effort <effort>`, Antigravity with `connecto add antigravity --model gemini-4-neon-<effort>`, Pi with `connecto add pi --effort <effort>`, OpenCode with `connecto add opencode`.

**Effort.** Every command has an `<effort>` slot. Before each `connecto add`, fill it by how hard the task looks to you now. Decide by feel; don't read code to decide:

- `low`: a trivial edit, wording, docs, or a config value
- `medium`: a normal feature or fix
- `high`: work across modules or crates, concurrency, security, or a risk of losing data
- `xhigh`: only a rare, very hard problem

The reviewer gets the same effort as the coder. Antigravity's command carries the effort in its model name (`gemini-4-neon-<effort>`); OpenCode's has no slot.

If a new terminal exits, or reports that its agent isn't installed or needs a login, before its first reply, close it with `connecto close <id>`, count that agent as unusable for this session, tell the user, and start the role on another ready CLI from the list, keeping the reviewer on a different CLI from the coder while one is ready. If `connecto add` itself is refused, no terminal was made. If the reason is the terminal cap (16 terminals, stopped ones included), close your own finished or stopped terminals with `connecto close <id>`, such as an earlier team's coder or reviewer, and try again. Never close a terminal you didn't start. If none of yours can go, or the reason is another, such as Connecto being paused, tell the user the reason and wait. If it timed out instead, check `connecto list` for a new terminal of yours before trying again, since it may still have been made.

The commands in this skill are for a POSIX shell. In PowerShell (Connecto's default on Windows), send a brief by piping a here-string into the command: a line with `@'`, the brief, a line with `'@ | connecto send <id> --ask`.

`connecto add` prints the new terminal's id, such as `codex-2 starting`. Note which id has which role, and use only the terminals you started. Never send work to terminals you didn't start, because they belong to the user or to other teams. The `connecto` commands work in this project only; never reach another project's terminals with `--project` unless the user explicitly asks for it.
