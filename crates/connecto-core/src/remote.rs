//! The contract between the UI and remote sessions: which hosts this Connecto
//! knows, whether this machine lets others in, and the keys that may.
//!
//! Every method returns promptly. Slow work (dialing, saving, the Keychain)
//! happens elsewhere and shows up in [`RemoteView`].

use std::sync::Arc;

use crate::TerminalsControl;

/// How many digits a pairing code has. The one place this is set: the net
/// crate generates, checks and derives from it, and the UI groups and counts
/// the digits with it. Eight digits is about 2^26.6 values; see ADR 0022 for
/// the search cost this buys.
pub const PAIRING_CODE_DIGITS: usize = 8;

/// The display name that always means the machine the app runs on.
pub const LOCALHOST: &str = "localhost";

/// A remote peer, as the host sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePeer {
    /// `SHA256:...` of the SSH key that authenticated; empty before auth.
    pub fingerprint: String,
    pub endpoint: [u8; 32],
}

/// Why the host's policy refused a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardDenied {
    /// Pause is on.
    Paused,
    /// Anything else, including an audit log that could not take the record.
    Refused,
}

/// The host's policy for remote peers. The implementation passes every action
/// through the Gate as the `remote` actor (ADR 0017); the transport crate sees
/// only this trait.
pub trait RemoteGuard: Send + Sync {
    /// Whether hosting is on. Off, a connection is dropped unanswered.
    fn enabled(&self) -> bool;
    fn set_enabled(&self, on: bool);
    /// Record how a handshake ended; `Err` carries the reason shown to no one
    /// but the log.
    fn connected(&self, peer: &RemotePeer, outcome: Result<(), &str>);
    /// A peer wants to look at (and then type into) a terminal.
    fn attach(&self, peer: &RemotePeer, terminal: &str, attempt: u64) -> Result<(), GuardDenied>;
    /// A peer wants to type `bytes` into a terminal.
    fn input(
        &self,
        peer: &RemotePeer,
        terminal: &str,
        attempt: u64,
        bytes: &[u8],
    ) -> Result<(), GuardDenied>;
    /// A peer wants to resize a terminal.
    fn resize(&self, peer: &RemotePeer, terminal: &str, attempt: u64) -> Result<(), GuardDenied>;
    /// A peer wants to change the host's terminals: `action` is `add`,
    /// `close`, `restart` or `rename`, and `terminal` names the target (empty
    /// for `add`). Pause refuses it.
    fn manage(
        &self,
        peer: &RemotePeer,
        action: &'static str,
        terminal: &str,
        attempt: u64,
    ) -> Result<(), GuardDenied>;
    /// A peer's stream ended; write out anything held back.
    fn detach(&self, peer: &RemotePeer, terminal: &str, attempt: u64);
}

/// How this Connecto stands with one saved host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLink {
    Disconnected,
    Connecting,
    /// Connected, and the traffic goes straight to the host.
    Direct,
    /// Connected through the relay.
    Relayed,
    /// Connected; the path is not known yet.
    Connected,
    /// The link dropped; the next try is in `retry_in_secs`.
    Reconnecting {
        retry_in_secs: u64,
    },
    /// Refused for a reason that waiting will not fix (the host does not allow
    /// this machine's keys, or another machine answered).
    Refused(String),
}

/// One saved host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRow {
    pub name: String,
    /// The host's identity as shown for comparison, `xxxx-xxxx-...`.
    pub fingerprint: String,
    pub link: HostLink,
    pub terminals: usize,
}

/// A host the user has named but not yet confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHost {
    pub name: String,
    pub fingerprint: String,
    pub address: String,
}

/// A key the host lets in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedKeyRow {
    /// `SHA256:...`
    pub fingerprint: String,
    pub algorithm: String,
    pub comment: String,
    /// Added by pairing a device (not by hand), so it can be revoked there.
    pub paired: bool,
}

/// One machine this Connecto has paired with, as either side sees it. A
/// machine is one record: its keys, its address and its name are added and
/// removed together (`disconnect_machine`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRow {
    /// Stable and opaque: the other machine's endpoint id as 64 hex digits, or
    /// `legacy:SHA256:...` for a device paired by the older one-sided flow,
    /// whose endpoint id this machine never learned.
    pub id: String,
    /// The name this machine chose for the other one (for a legacy device, the
    /// name it was paired under).
    pub name: String,
    /// `YYYY-MM-DD`, or empty when unknown.
    pub paired_at: String,
    /// Whether the other machine is connected to this one now, and how.
    /// `Disconnected` means it is not.
    pub inbound: HostLink,
    /// Whether this machine is connected to the other one now, and how.
    pub outbound: HostLink,
}

/// Where a pairing with another machine stands on this machine. Both machines
/// run the same flow: each shows its own digits, and each types the
/// other's. Order does not matter.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum PairingPhase {
    /// Nothing in progress.
    #[default]
    Idle,
    /// Getting ready: turning the two codes into the connection's key (slow on
    /// purpose) and reaching the relay.
    Preparing,
    /// This machine's code is on screen; the other machine's has not been
    /// entered here yet. `expires_in_secs` counts down to the code's end.
    ShowingCode {
        /// `123 456`
        my_code: String,
        expires_in_secs: u64,
    },
    /// Both codes are entered here, and this machine is waiting for the other
    /// one to reach it. The other machine may still be entering this code.
    WaitingForPeer {
        my_code: String,
        expires_in_secs: u64,
    },
    /// The other machine is connected; keys are being exchanged.
    Connecting,
    /// Connected. Call `finish_pairing` with the name for the other machine.
    NameNeeded { suggested_name: String },
    /// This machine has its name for the other one and is waiting for the
    /// other machine to confirm it too. A cancel still stops this (nothing is
    /// kept by either side).
    Confirming,
    /// Both machines have confirmed and this one has sent its final word: the
    /// pairing is being kept. A cancel is no longer taken; this resolves within
    /// a short wait, to `Done` or `Failed`.
    Keeping,
    /// Paired. `machine_id` is the new record's [`MachineRow::id`].
    Done { machine_id: String },
    /// Stopped. Nothing was saved by this pairing.
    Failed { message: String },
}

impl std::fmt::Debug for PairingPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The codes are never printed.
        match self {
            Self::Idle => f.write_str("Idle"),
            Self::Preparing => f.write_str("Preparing"),
            Self::ShowingCode {
                expires_in_secs, ..
            } => f
                .debug_struct("ShowingCode")
                .field("my_code", &"<hidden>")
                .field("expires_in_secs", expires_in_secs)
                .finish(),
            Self::WaitingForPeer {
                expires_in_secs, ..
            } => f
                .debug_struct("WaitingForPeer")
                .field("my_code", &"<hidden>")
                .field("expires_in_secs", expires_in_secs)
                .finish(),
            Self::Connecting => f.write_str("Connecting"),
            Self::NameNeeded { .. } => f.write_str("NameNeeded"),
            Self::Confirming => f.write_str("Confirming"),
            Self::Keeping => f.write_str("Keeping"),
            Self::Done { .. } => f.write_str("Done"),
            Self::Failed { message } => f.debug_struct("Failed").field("message", message).finish(),
        }
    }
}

/// Whether this machine is accepting remote peers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HostingStatus {
    #[default]
    Off,
    Starting,
    Listening,
    /// Hosting is on, but cannot work, and why.
    Error(String),
}

/// Everything the remote screens draw.
#[derive(Debug, Clone, Default)]
pub struct RemoteView {
    pub hosting_enabled: bool,
    pub hosting: HostingStatus,
    /// A remote peer is connected to this machine now, by key fingerprint.
    pub peer: Option<String>,
    /// Every machine this one is paired with, with its live links.
    pub machines: Vec<MachineRow>,
    /// This machine's identity, for sharing.
    pub my_fingerprint: String,
    /// What to give someone who wants to add this machine.
    pub my_address: String,
    pub relay_url: Option<String>,
    /// The user stored an access token for the relay.
    pub relay_token_set: bool,
    /// No token is stored, and Connecto's built-in access is used (the default relay only).
    pub relay_builtin_token: bool,
    /// The stored access token could not be read (the Keychain refused). It is
    /// neither set nor absent: the UI must not say "No access token" then.
    pub relay_token_unreadable: bool,
    /// Keys added by hand (`add_authorized_key`). Keys that belong to a machine
    /// are in `machines` and are removed with `disconnect_machine`.
    pub authorized: Vec<AuthorizedKeyRow>,
    pub authorized_path: String,
    /// Lines of the file that carry options and so let nobody in.
    pub authorized_ignored: usize,
    pub hosts: Vec<HostRow>,
    pub pending: Option<PendingHost>,
    /// The pairing with another machine, if one is under way or just ended.
    pub pairing: PairingPhase,
    pub last_error: Option<String>,
}

/// User commands on remote sessions.
pub trait RemoteControl: Send + Sync {
    /// The latest state. Never blocks.
    fn view(&self) -> Arc<RemoteView>;

    /// Turn hosting on or off. Off by default; saved.
    fn set_hosting(&self, enabled: bool);

    /// Set the relay: an address (`None` for direct connections only) and,
    /// when `token` is `Some`, its access token (empty clears it).
    fn set_relay(&self, url: Option<String>, token: Option<String>) -> Result<(), String>;

    /// Add one `authorized_keys` line.
    fn add_authorized_key(&self, line: &str) -> Result<(), String>;

    /// Remove every key with this fingerprint.
    fn remove_authorized_key(&self, fingerprint: &str) -> Result<(), String>;

    /// Name a host and give its address. Nothing is saved or dialed yet: the
    /// view shows [`RemoteView::pending`] with the host's fingerprint to
    /// confirm.
    fn add_host(&self, name: &str, address: &str) -> Result<(), String>;

    /// Start pairing with another machine. Both machines do this: each shows
    /// its own code (in [`RemoteView::pairing`]), and each enters the
    /// other's with [`RemoteControl::submit_peer_code`]. Pairing needs hosting,
    /// so this turns hosting on (and leaves it on).
    ///
    /// # Errors
    /// When a pairing is already under way, or it is asked for too often.
    fn start_mutual_pairing(&self) -> Result<(), String>;

    /// Stop the pairing, whatever its phase. Nothing is saved by a pairing
    /// that did not finish.
    fn cancel_pairing(&self);

    /// Enter the other machine's code. Order does not matter: entering it
    /// before or after the other machine enters this one's is fine, and the
    /// view says what to wait for. Entering a different code again replaces
    /// the first (up to five times per pairing).
    ///
    /// # Errors
    /// At once: "Enter the eight digits shown on the other machine.", "That is
    /// this machine's own code...", or that no pairing is waiting for a code.
    fn submit_peer_code(&self, code: &str) -> Result<(), String>;

    /// Name the other machine, once the view shows [`PairingPhase::NameNeeded`].
    /// The name is this machine's alone. Trimmed; at most 40 characters; no
    /// control characters; not `localhost`; not the name of another machine.
    ///
    /// # Errors
    /// At once, with the reason the name is not usable.
    fn finish_pairing(&self, name: &str) -> Result<(), String>;

    /// The machine's live connections in both directions, and its record.
    /// Disconnecting it closes any live session with it at once, removes its
    /// authorized key, its saved host and its Keychain key, so its next attempt
    /// is refused. The other machine is not told: its record stays until it
    /// removes this machine, and its next attempt shows as refused.
    ///
    /// # Errors
    /// When `id` is not a machine, or a file or the Keychain could not be
    /// written. Calling it again finishes the job.
    fn disconnect_machine(&self, id: &str) -> Result<(), String>;

    fn confirm_host(&self);

    fn cancel_host(&self);

    fn connect(&self, name: &str);

    fn disconnect(&self, name: &str);

    fn rename_host(&self, from: &str, to: &str) -> Result<(), String>;

    /// Forget a host and drop its connection.
    fn forget_host(&self, name: &str) -> Result<(), String>;

    /// The host's terminals, as a registry the window can draw like a
    /// project's. `None` for an unknown host.
    fn terminals(&self, name: &str) -> Option<Arc<dyn TerminalsControl>>;
}

/// A build without remote sessions.
pub struct NoRemote;

impl RemoteControl for NoRemote {
    fn view(&self) -> Arc<RemoteView> {
        Arc::new(RemoteView::default())
    }
    fn set_hosting(&self, _enabled: bool) {}
    fn set_relay(&self, _url: Option<String>, _token: Option<String>) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn add_authorized_key(&self, _line: &str) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn remove_authorized_key(&self, _fingerprint: &str) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn add_host(&self, _name: &str, _address: &str) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn start_mutual_pairing(&self) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn cancel_pairing(&self) {}
    fn submit_peer_code(&self, _code: &str) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn finish_pairing(&self, _name: &str) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn disconnect_machine(&self, _id: &str) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn confirm_host(&self) {}
    fn cancel_host(&self) {}
    fn connect(&self, _name: &str) {}
    fn disconnect(&self, _name: &str) {}
    fn rename_host(&self, _from: &str, _to: &str) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn forget_host(&self, _name: &str) -> Result<(), String> {
        Err("remote sessions are not available".to_owned())
    }
    fn terminals(&self, _name: &str) -> Option<Arc<dyn TerminalsControl>> {
        None
    }
}
