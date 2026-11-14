# ADR 0015: agents are an actor of their own, and may type at once

**Context.** ADR 0006 made every relay hop need a user click, and the Gate denied any non-user actor in writing. The bridge lets an agent in one terminal use the others without approval (R3), which the manual path cannot express: it allows one open delivery per target, asks a second click for a busy or unready target, and limits hops, rate and duplicates.

**Decision.** `Actor::Session` becomes `Actor::Agent`. `decide_deliver` still denies it with `NotUser`, so an agent can never use the manual path. The agent's only door is `Gate::agent_submit`, in this order: paused now, pause epoch equal to the epoch captured at admission, target attempt equal to the admitted one, `decide_agent`, durable audit intent. There is no call to the limits (R7, A16). `decide_agent` refuses text, keys and wakes to a target that is not running or is in manual mode, and consults neither readiness nor busy; add, start and close are process control and pass. `agent_note` records a read and never waits. Agent text is sanitized, never redacted, and the audit holds the redacted copy and the hash. `POLICY_REVISION` is 4.

`PauseSwitch::pause` now sets `paused` first and bumps the epoch second, so a reader that sees `paused` false took its epoch before the bump, and an admission taken during a Pause cannot become valid again at Resume. Session actors take agent writes from a queue of depth 16, one at a time, and a write waits for its Enter before the next is taken, so two pastes can never share one Enter. A Pause between the paste and Enter means no Enter, and the caller is told `paused_before_enter`.

**Supersedes** ADR 0006. A user click is still the only way into the manual path and its limits. Nothing an agent does can lift Pause or use Stop all (A21).

**Consequences.** Agents can cause model turns, and nothing limits them (L11). Pause stops them, `unwatch` ends a watch, and the ledger and tab strip show the activity.
