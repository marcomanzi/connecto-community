//! The policy: a pure function. Whatever it does not know, it denies.

use connecto_core::Readiness;

use crate::Denial;

/// Changes whenever a rule here changes. Recorded in every audit entry.
pub const POLICY_REVISION: u32 = 4;

/// Who is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// A click in the window.
    User,
    /// An agent in a Connecto terminal, through the bridge. It can never use the
    /// manual path: [`decide_deliver`] denies it in writing, and
    /// [`decide_agent`] is the only door it has (ADR 0015).
    Agent,
    /// A peer on another machine, authorized by an SSH key (ADR 0017). It has
    /// no delivery path either: it types into terminals through the remote
    /// door in [`crate::Gate::remote_submit`].
    Remote,
}

impl Actor {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Remote => "remote",
        }
    }
}

/// What the session layer knows about a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetState {
    pub attempt: u64,
    pub running: bool,
    pub readiness: Readiness,
    pub busy: bool,
    pub manual: bool,
    pub open_delivery: bool,
}

/// What an agent asks for through the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAction {
    /// Text pasted into a target, then Enter.
    Type,
    /// Keys pressed in a target.
    Keys,
    /// A notice typed into a not-busy watcher by Connecto itself.
    Wake,
    Add,
    Start,
    Close,
}

impl AgentAction {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Keys => "keys",
            Self::Wake => "wake",
            Self::Add => "add",
            Self::Start => "start",
            Self::Close => "close",
        }
    }
}

/// The policy's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(Denial),
}

/// Whether `actor` may deliver to a target in `state`. `own_reply` says the
/// body came from the target's own captured reply, through the user's Use click.
#[must_use]
pub const fn decide_deliver(
    actor: Actor,
    state: &TargetState,
    confirmed: bool,
    own_reply: bool,
) -> Decision {
    match actor {
        Actor::Agent | Actor::Remote => return Decision::Deny(Denial::NotUser),
        Actor::User => {}
    }
    if !state.running {
        return Decision::Deny(Denial::NotRunning);
    }
    if state.manual {
        return Decision::Deny(Denial::ManualMode);
    }
    if own_reply {
        return Decision::Deny(Denial::OwnReply);
    }
    let ready = matches!(state.readiness, Readiness::Ready);
    if confirmed || (ready && !state.busy) {
        Decision::Allow
    } else if state.busy {
        Decision::Deny(Denial::Busy)
    } else {
        Decision::Deny(Denial::NeedsConfirmation)
    }
}

/// Whether an agent's `action` may reach a target in `state`. Readiness and
/// busy are not consulted: text is typed at once (R3). Add, Start and Close are
/// process control, so they pass, on a bridge-off target too.
#[must_use]
pub const fn decide_agent(action: AgentAction, state: &TargetState) -> Decision {
    match action {
        AgentAction::Add | AgentAction::Start | AgentAction::Close => Decision::Allow,
        AgentAction::Type | AgentAction::Keys | AgentAction::Wake => {
            if !state.running {
                Decision::Deny(Denial::NotRunning)
            } else if state.manual {
                Decision::Deny(Denial::ManualMode)
            } else {
                Decision::Allow
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> TargetState {
        TargetState {
            attempt: 1,
            running: true,
            readiness: Readiness::Ready,
            busy: false,
            manual: false,
            open_delivery: false,
        }
    }

    #[test]
    fn a_ready_target_is_allowed() {
        assert_eq!(
            decide_deliver(Actor::User, &state(), false, false),
            Decision::Allow
        );
    }

    #[test]
    fn an_unready_target_needs_the_second_click() {
        for readiness in [Readiness::NotReady, Readiness::Unknown] {
            let state = TargetState {
                readiness,
                ..state()
            };
            assert_eq!(
                decide_deliver(Actor::User, &state, false, false),
                Decision::Deny(Denial::NeedsConfirmation)
            );
            assert_eq!(
                decide_deliver(Actor::User, &state, true, false),
                Decision::Allow
            );
        }
    }

    #[test]
    fn a_busy_target_needs_the_second_click_whatever_its_readiness() {
        for readiness in [Readiness::Ready, Readiness::NotReady, Readiness::Unknown] {
            let state = TargetState {
                busy: true,
                readiness,
                ..state()
            };
            assert_eq!(
                decide_deliver(Actor::User, &state, false, false),
                Decision::Deny(Denial::Busy)
            );
            assert_eq!(
                decide_deliver(Actor::User, &state, true, false),
                Decision::Allow
            );
        }
    }

    #[test]
    fn confirming_never_overrides_manual_mode_or_stopped() {
        for (state, denial) in [
            (
                TargetState {
                    manual: true,
                    ..state()
                },
                Denial::ManualMode,
            ),
            (
                TargetState {
                    running: false,
                    ..state()
                },
                Denial::NotRunning,
            ),
        ] {
            assert_eq!(
                decide_deliver(Actor::User, &state, true, false),
                Decision::Deny(denial)
            );
        }
    }

    #[test]
    fn a_session_is_never_fed_its_own_reply_even_on_a_second_click() {
        for confirmed in [false, true] {
            assert_eq!(
                decide_deliver(Actor::User, &state(), confirmed, true),
                Decision::Deny(Denial::OwnReply)
            );
        }
    }

    #[test]
    fn a_session_is_denied_in_every_state() {
        for confirmed in [false, true] {
            assert_eq!(
                decide_deliver(Actor::Agent, &state(), confirmed, false),
                Decision::Deny(Denial::NotUser)
            );
        }
    }

    #[test]
    fn an_agent_types_at_once_whatever_the_target_is_doing() {
        for readiness in [Readiness::Ready, Readiness::NotReady, Readiness::Unknown] {
            for busy in [false, true] {
                let state = TargetState {
                    readiness,
                    busy,
                    open_delivery: busy,
                    ..state()
                };
                for action in [AgentAction::Type, AgentAction::Keys, AgentAction::Wake] {
                    assert_eq!(decide_agent(action, &state), Decision::Allow);
                }
            }
        }
    }

    #[test]
    fn an_agent_cannot_type_into_a_stopped_or_manual_target_but_can_control_it() {
        let stopped = TargetState {
            running: false,
            ..state()
        };
        let manual = TargetState {
            manual: true,
            ..state()
        };
        for action in [AgentAction::Type, AgentAction::Keys, AgentAction::Wake] {
            assert_eq!(
                decide_agent(action, &stopped),
                Decision::Deny(Denial::NotRunning)
            );
            assert_eq!(
                decide_agent(action, &manual),
                Decision::Deny(Denial::ManualMode)
            );
        }
        for action in [AgentAction::Add, AgentAction::Start, AgentAction::Close] {
            assert_eq!(decide_agent(action, &stopped), Decision::Allow);
            assert_eq!(decide_agent(action, &manual), Decision::Allow);
        }
    }
}
