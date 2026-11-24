//! The vault: secrets the person keeps in Connecto for terminals to *use*.
//!
//! A vault secret is stored with Connecto's own secrets (`connecto-secrets`),
//! app-wide or for one project. Terminals never get its value: they see its
//! name, and they use it only through a binding the person chose, which the
//! app carries out itself, outside the terminal's processes. The value is
//! hidden wherever a terminal's text goes (`connecto read`, history, memory,
//! audit), in its exact and commonly encoded forms.
//!
//! This module holds the plain types and the two seams: [`VaultControl`] for
//! the window, [`VaultBridge`] for the terminals' bridge.

use std::sync::Arc;

use crate::ProviderScope;

/// Longest secret name.
pub const MAX_NAME: usize = 64;
/// Longest value, in bytes.
pub const MAX_VALUE: usize = 16 * 1024;
/// Longest description.
pub const MAX_DESCRIPTION: usize = 200;

/// A name: a letter, then letters, digits, `_` or `-`, at most [`MAX_NAME`].
/// Names are what terminals see, so they are kept plain.
///
/// # Errors
/// What is wrong with it, for the person.
pub fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("A secret needs a name.".to_owned());
    };
    if !first.is_ascii_alphabetic() {
        return Err("A secret's name begins with a letter.".to_owned());
    }
    if name.len() > MAX_NAME {
        return Err(format!("A secret's name is at most {MAX_NAME} characters."));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')) {
        return Err("A secret's name has only letters, digits, `_` and `-`.".to_owned());
    }
    Ok(())
}

/// Longest reason, in characters as shown (after [`display_line`]): a
/// longer one is refused, never cut, so the person sees all that was checked.
pub const MAX_REASON: usize = 300;

/// Fewest different words in a reason that say something of their own:
/// not filler ([`VAGUE_WORDS`]) and not shorter than three letters. One of
/// them may be part of the secret's name, not all.
const MIN_REASON_CONTENT: usize = 2;

/// Words that, alone, tell the person nothing about what a secret is for,
/// in English and Italian. A reason made only of these is refused.
const VAGUE_WORDS: &[&str] = &[
    // English
    "the",
    "and",
    "for",
    "with",
    "this",
    "that",
    "these",
    "those",
    "need",
    "needs",
    "needed",
    "require",
    "required",
    "requires",
    "necessary",
    "use",
    "uses",
    "using",
    "used",
    "access",
    "accessing",
    "secret",
    "secrets",
    "token",
    "tokens",
    "key",
    "keys",
    "password",
    "passwords",
    "credential",
    "credentials",
    "api",
    "auth",
    "authentication",
    "authenticate",
    "please",
    "test",
    "testing",
    "tests",
    "task",
    "tasks",
    "work",
    "working",
    "job",
    "jobs",
    "stuff",
    "thing",
    "things",
    "something",
    "some",
    "get",
    "got",
    "set",
    "setup",
    "value",
    "values",
    "connecto",
    "agent",
    "terminal",
    "request",
    "requested",
    "can",
    "will",
    "would",
    "should",
    "could",
    "must",
    "have",
    "has",
    "had",
    "was",
    "were",
    "are",
    "its",
    "our",
    "your",
    "you",
    "from",
    "into",
    "just",
    "also",
    "now",
    "then",
    "here",
    "there",
    "because",
    "since",
    "purpose",
    "purposes",
    "reason",
    "why",
    "what",
    "all",
    "any",
    "other",
    "more",
    "etc",
    "continue",
    "proceed",
    "doing",
    "does",
    "did",
    "make",
    "makes",
    "user",
    "person",
    "project",
    "app",
    "normal",
    "normally",
    "usual",
    "regular",
    "general",
    "generic",
    "various",
    "operation",
    "operations",
    "functionality",
    "feature",
    "features",
    "everything",
    "anything",
    "integration",
    "integrations",
    "service",
    "services",
    "system",
    "systems",
    "stuff",
    "code",
    "tool",
    "tools",
    "process",
    "workflow",
    "data",
    "development",
    "dev",
    "future",
    "later",
    "later",
    "current",
    "needful",
    "help",
    "able",
    "case",
    "order",
    "way",
    "run",
    "running",
    "things",
    "ongoing",
    "again",
    "one",
    // Italian
    "per",
    "questo",
    "questa",
    "questi",
    "queste",
    "quello",
    "quella",
    "quelli",
    "serve",
    "servono",
    "servire",
    "servirebbe",
    "bisogno",
    "necessario",
    "necessaria",
    "necessari",
    "lavoro",
    "lavori",
    "compito",
    "compiti",
    "attività",
    "chiave",
    "chiavi",
    "segreto",
    "segreti",
    "accesso",
    "accedere",
    "usare",
    "uso",
    "utilizzare",
    "utilizzo",
    "fare",
    "funzionare",
    "funzionamento",
    "normale",
    "normali",
    "operazione",
    "operazioni",
    "varie",
    "vari",
    "cose",
    "cosa",
    "progetto",
    "prova",
    "prove",
    "favore",
    "che",
    "come",
    "sono",
    "essere",
    "ora",
    "adesso",
    "continuare",
    "procedere",
    "autenticazione",
    "credenziali",
    "credenziale",
    "del",
    "della",
    "dello",
    "dei",
    "delle",
    "degli",
    "con",
    "una",
    "uno",
    "gli",
    "nel",
    "nella",
    "sul",
    "sulla",
    "mio",
    "mia",
    "tuo",
    "tua",
    "suo",
    "sua",
    "ho",
    "devo",
    "posso",
    "poter",
    "potere",
    "terminale",
    "agente",
    "utente",
    "tutto",
    "tutti",
    "qualcosa",
    "altro",
    "altri",
    "generale",
    "solito",
    "solite",
    "richiesta",
    "motivo",
];

/// A guard against an empty, plainly generic or overlong reason for a
/// request: at most [`MAX_REASON`] characters as shown, and at least
/// [`MIN_REASON_CONTENT`] different words that are not filler, in English
/// or Italian, one of them not part of the secret's name. It cannot tell a true reason from a
/// false one: the person still judges it.
///
/// # Errors
/// What to write instead, for the asking agent. It never repeats the
/// reason, which may hold something pasted by mistake.
pub fn check_reason(reason: &str, name: &str) -> Result<(), String> {
    // Checked as the person will see it, all of it: never cut afterwards.
    let shown = display_line(reason, usize::MAX);
    if shown.chars().count() > MAX_REASON {
        return Err(format!(
            "The reason is over {MAX_REASON} characters, more than the person is shown. Please shorten it to one plain sentence saying what you will do with {name} and where."
        ));
    }
    let own: Vec<String> = name
        .split(['_', '-'])
        .map(str::to_lowercase)
        .filter(|part| !part.is_empty())
        .collect();
    // Each word once. The secret's own name may be one of them ("Deploy to
    // Vercel" for VERCEL_TOKEN), so long as another says something too.
    let content: std::collections::BTreeSet<String> = shown
        .split(|ch: char| !ch.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|word| word.chars().count() >= 3)
        .filter(|word| !VAGUE_WORDS.contains(&word.as_str()))
        .collect();
    let beyond_name = content.iter().any(|word| !own.contains(word));
    if content.len() >= MIN_REASON_CONTENT && beyond_name {
        return Ok(());
    }
    let what = if shown.is_empty() {
        "No reason was given"
    } else {
        "The reason is too generic for the person to judge"
    };
    Err(format!(
        "{what}. In --reason, say in plain words what you will do with {name} and where, such as --reason 'Open pull requests on github.com/acme/web through the GitHub API'. The person reads it to decide."
    ))
}

/// A value of a use (an address, a header, a program's path...) shown so
/// that different values never look the same: in double quotes, with plain
/// printable ASCII as it is, `\\` and `"` escaped by a backslash, and every
/// other character (a control, a space other than U+0020, a formatting or
/// a non-ASCII one) written as `\u{hex}`. It reads back to the value.
#[must_use]
pub fn display_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            ' '..='~' => out.push(c),
            c => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
        }
    }
    out.push('"');
    out
}

/// Whether a use a terminal proposes reaches the person's form as it is.
/// The form keeps a command's programs one per line, trimmed, so a path
/// with a line break, a control character, or space at an end would become
/// other paths. Checked where a proposal enters the form (a request here,
/// or one a paired host sends), never on saved uses, which stay readable.
///
/// # Errors
/// Which path, and why.
pub fn check_proposed(binding: &Binding) -> Result<(), String> {
    if let Binding::Command { programs, .. } = binding {
        for program in programs {
            let breaks = program
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{85}'));
            if program.is_empty() || breaks || program.trim() != program {
                return Err(format!(
                    "The program path {} cannot be one line as it is: no line breaks, control characters, or spaces at its ends.",
                    display_value(program)
                ));
            }
        }
    }
    Ok(())
}

/// Text from a terminal or a paired machine, made safe to show on one line
/// inside quotes: control characters, line and paragraph separators,
/// bidirectional and zero-width formatting become spaces, double quotes
/// become single ones (so it cannot close the quotes around it), runs of
/// space become one, and it is cut to `max` characters.
#[must_use]
pub fn display_line(raw: &str, max: usize) -> String {
    raw.chars()
        .map(|c| match c {
            '"' | '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' | '\u{00ab}' | '\u{00bb}'
            | '\u{2033}' | '\u{ff02}' => '\'',
            '\u{2028}'
            | '\u{2029}'
            | '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// A value as typed, trimmed of the line ending a paste brings. The value
/// is never part of the message.
///
/// # Errors
/// Why it cannot be kept, for the person.
pub fn clean_value(typed: &str) -> Result<String, String> {
    let value = typed.trim_end_matches(['\n', '\r']);
    if value.trim().is_empty() {
        return Err("The secret is empty.".to_owned());
    }
    if value.len() < MIN_VALUE {
        return Err(format!(
            "A secret is at least {MIN_VALUE} characters, so that it can be hidden in what terminals show."
        ));
    }
    if value.len() > MAX_VALUE {
        return Err(format!("A secret is at most {} KB.", MAX_VALUE / 1024));
    }
    if value.contains('\0') {
        return Err("A secret cannot hold a NUL character.".to_owned());
    }
    Ok(value.to_owned())
}

/// Shortest value: shorter values cannot be hidden without hiding ordinary
/// text too (the same as `connecto_redact::MIN_KNOWN`).
pub const MIN_VALUE: usize = 8;

/// How terminals may use a secret. The person chooses it; a secret with no
/// binding cannot be used by any terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// Requests through the app to `base_url` get the header `header`, whose
    /// value is `prefix` and then the secret (`Authorization: Bearer …`).
    Http {
        base_url: String,
        header: String,
        prefix: String,
    },
    /// One of `programs` (by absolute path) is run by the app with the secret
    /// in the environment variable `env`.
    Command { programs: Vec<String>, env: String },
    /// The secret is an SSH private key. Terminals sign with it through an
    /// ssh-agent the app serves, and get its public key; never the key.
    Ssh,
    /// Project-only credential for the host PostgreSQL queue.
    AgenticWorks,
    /// Project-only Google Cloud service account key (its JSON) that
    /// authorises Agentic Works' Cloud SQL connections. Only Connecto uses it.
    GoogleServiceAccount,
}

impl Binding {
    /// One word for the kind, as `connecto secret list` prints it.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Http { .. } => "http",
            Self::Command { .. } => "command",
            Self::Ssh => "ssh",
            Self::AgenticWorks => "agentic_works",
            Self::GoogleServiceAccount => "google_service_account",
        }
    }

    /// Whether only Connecto itself uses it, for one project, and no
    /// terminal ever sees it.
    #[must_use]
    pub const fn host_only(&self) -> bool {
        matches!(self, Self::AgenticWorks | Self::GoogleServiceAccount)
    }

    /// Where the secret goes when used so, in a sentence for the person.
    #[must_use]
    pub fn plain_use(&self) -> String {
        match self {
            Self::Http {
                base_url,
                header,
                prefix,
            } => {
                // Every value as it is, quoted and escaped: a terminal chose
                // them, and different ones never look the same.
                let before = if prefix.is_empty() {
                    "on its own".to_owned()
                } else {
                    format!("after {}", display_value(prefix))
                };
                format!(
                    "Sent only to {} (and addresses under it), by Connecto, in the header {}, {before}.",
                    display_value(base_url),
                    display_value(header),
                )
            }
            Self::Command { programs, env } => {
                let shown: Vec<String> = programs.iter().map(|p| display_value(p)).collect();
                let (one, list) = match shown.as_slice() {
                    [program] => ("the program", program.clone()),
                    _ => ("one of the programs", shown.join(", ")),
                };
                format!(
                    "Given only to {one} {list}, run by Connecto, in the environment variable {}.",
                    display_value(env)
                )
            }
            Self::Ssh => "Used only as an SSH key: the terminal gets its public key and an ssh-agent that signs with it, never the key itself.".to_owned(),
            Self::AgenticWorks => "Used only by Connecto, to connect to this project's Agentic Works database. No terminal sees it.".to_owned(),
            Self::GoogleServiceAccount => "Used only by Connecto, as this project's Google Cloud service account for Agentic Works. No terminal sees it.".to_owned(),
        }
    }
}

/// One secret, without its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEntry {
    pub name: String,
    pub scope: ProviderScope,
    pub description: String,
    pub bindings: Vec<Binding>,
    /// When the value was last set, in seconds since the epoch.
    pub updated_unix: u64,
    /// When a terminal last used it.
    pub last_used_unix: Option<u64>,
    /// An SSH key's public half, when the list was made by another machine
    /// that keeps the key (a remote project's): what the person gives the
    /// server. `None` otherwise.
    pub public_key: Option<String>,
}

/// What the window shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultView {
    pub entries: Vec<SecretEntry>,
    /// Why the vault cannot be read or changed now, if so.
    pub problem: Option<String>,
    /// Terminals waiting for the person to give them a secret, oldest first.
    pub asks: Vec<SecretAsk>,
    /// Where the vault's values are protected at rest, in words.
    pub protection: String,
    /// Changes paired machines made, newest last, each with its number: the
    /// window tells the person of each one it has not shown yet.
    pub remote_changes: Vec<(u64, String)>,
    /// Scopes whose list is shown in part (a paired host's project keeping
    /// more than is read at once): where it begins and where it goes on.
    pub pages: Vec<SecretsPage>,
}

/// Where a list shown in part is ([`VaultView::pages`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretsPage {
    pub scope: ProviderScope,
    /// It begins after this name; `None`: from the first.
    pub from: Option<String>,
    /// More follow after the last shown: the name to go on from.
    pub next: Option<String>,
}

/// What kind of secret a terminal asks for: it decides what the dialog
/// offers (an SSH key can be generated there).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AskKind {
    /// An API key or token.
    #[default]
    Token,
    Password,
    /// An SSH private key, pasted or generated.
    SshKey,
}

impl AskKind {
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "token" | "api-key" | "key" => Some(Self::Token),
            "password" => Some(Self::Password),
            "ssh-key" | "ssh" => Some(Self::SshKey),
            _ => None,
        }
    }

    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Token => "an API key or token",
            Self::Password => "a password",
            Self::SshKey => "an SSH key",
        }
    }
}

/// A terminal asks the person for a secret (`connecto secret request`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretAsk {
    pub id: u64,
    /// The terminal that asked, by id, and its project.
    pub terminal: String,
    pub project: Option<String>,
    pub name: String,
    pub kind: AskKind,
    /// Why, in the terminal's words (sanitized, short). Untrusted.
    pub reason: String,
    /// How the terminal proposes to use it; the person may change it.
    pub proposed: Vec<Binding>,
    /// The proposed use was too long to be sent to this machine (a paired
    /// host's request): `proposed` is empty, and the person chooses a use.
    pub proposed_omitted: bool,
}

/// The person's answer to a [`SecretAsk`]. Its debug form hides the value.
#[derive(Clone, PartialEq, Eq)]
pub enum AskAnswer {
    /// Keep `value` as the secret, in `scope`, used as `bindings`.
    Provide {
        scope: ProviderScope,
        value: String,
        bindings: Vec<Binding>,
    },
    /// Make a new SSH key pair and keep it, in `scope`, used as `bindings`:
    /// the private half is made where the vault is and never leaves it.
    /// Only through the ssh-agent ([`Binding::Ssh`]), whatever else the
    /// request proposed: a private key is never a web header or a
    /// program's variable.
    GenerateSshKey {
        scope: ProviderScope,
    },
    Decline,
}

/// A change to the vault, as one call ([`VaultControl::change`]). It holds
/// a value, so it has no debug form.
#[derive(Clone, PartialEq, Eq)]
pub enum VaultChange {
    Put {
        scope: ProviderScope,
        name: String,
        description: String,
        value: String,
    },
    SetBindings {
        scope: ProviderScope,
        name: String,
        bindings: Vec<Binding>,
    },
    Delete {
        scope: ProviderScope,
        name: String,
    },
    GenerateKey {
        scope: ProviderScope,
        name: String,
        description: String,
    },
    Answer {
        ask: u64,
        answer: AskAnswer,
    },
}

/// Why a change was not made, told apart where it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeError {
    /// Something wrong with the change itself (its name, value or use, a
    /// request no longer waiting): its words name only what the change
    /// named. `false`: the vault or its storage failed, and its words are
    /// for this machine's person only (they may name other secrets or
    /// files).
    pub invalid: bool,
    pub why: String,
    /// Something was written before it stopped (the value, or the old use
    /// taken off): what is kept is no longer what it was.
    pub partial: bool,
}

impl ChangeError {
    #[must_use]
    pub const fn invalid(why: String) -> Self {
        Self {
            invalid: true,
            why,
            partial: false,
        }
    }

    #[must_use]
    pub const fn storage(why: String, partial: bool) -> Self {
        Self {
            invalid: false,
            why,
            partial,
        }
    }
}

impl From<ChangeError> for String {
    fn from(error: ChangeError) -> Self {
        error.why
    }
}

/// What an answer sent to another machine came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerState {
    /// On its way, or not answered yet.
    Waiting,
    /// Taken: the request leaves.
    Taken,
    /// Refused, and why (never a value).
    Refused(String),
}

/// What a debug form shows in place of a secret's value.
pub const HIDDEN: &str = "…";

impl std::fmt::Debug for AskAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provide {
                scope, bindings, ..
            } => f
                .debug_struct("Provide")
                .field("scope", scope)
                .field("value", &HIDDEN)
                .field("bindings", bindings)
                .finish(),
            Self::GenerateSshKey { scope } => f
                .debug_struct("GenerateSshKey")
                .field("scope", scope)
                .finish(),
            Self::Decline => f.write_str("Decline"),
        }
    }
}

impl std::fmt::Debug for GeneratedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedKey")
            .field("private_openssh", &HIDDEN)
            .field("public_openssh", &self.public_openssh)
            .finish()
    }
}

/// A key pair made for the person to look at before approving. Its debug
/// form hides the private half.
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedKey {
    /// OpenSSH private key text.
    pub private_openssh: String,
    /// `ssh-ed25519 AAAA… comment`.
    pub public_openssh: String,
}

/// Who uses a secret: the terminal the bridge identified from the process
/// tree, never from what the terminal says.
#[derive(Clone)]
pub struct SecretCaller {
    pub terminal: String,
    pub project: Option<String>,
    /// The project's folder: a program runs inside it.
    pub project_folder: Option<std::path::PathBuf>,
    /// Whether a process (by pid) belongs to this terminal.
    pub owns_pid: Arc<dyn Fn(i32) -> bool + Send + Sync>,
}

impl std::fmt::Debug for SecretCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretCaller")
            .field("terminal", &self.terminal)
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

/// A terminal's request for a secret, as `connecto secret request` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    pub name: String,
    pub kind: AskKind,
    pub reason: String,
    pub proposed: Vec<Binding>,
}

/// Called once with the sentence to tell the asking terminal when the person
/// answers. Never holds a value.
pub type AskReply = Box<dyn FnOnce(String) + Send>;

/// What a program run with a secret gave back, with every vault value hidden.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOutput {
    /// `None` when it was stopped (time limit) or ended by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// How a terminal reaches an SSH key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshAccess {
    /// Set `SSH_AUTH_SOCK` to this.
    pub socket: std::path::PathBuf,
    pub public_openssh: String,
}

impl VaultView {
    /// The secrets a terminal of `project` sees: the project's own, and the
    /// app-wide ones it does not override by name. Sorted by name.
    #[must_use]
    pub fn visible_to(&self, project: Option<&str>) -> Vec<&SecretEntry> {
        let own = |entry: &&SecretEntry| match (&entry.scope, project) {
            (ProviderScope::Project(id), Some(project)) => id == project,
            _ => false,
        };
        let mut out: Vec<&SecretEntry> = self
            .entries
            .iter()
            .filter(own)
            .filter(|e| !e.bindings.iter().any(Binding::host_only))
            .collect();
        for entry in &self.entries {
            if entry.scope == ProviderScope::Global
                && !out.iter().any(|seen| seen.name == entry.name)
            {
                out.push(entry);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

/// The window's commands. Values go in and never come back out.
pub trait VaultControl: Send + Sync {
    fn view(&self) -> Arc<VaultView>;

    /// Add a secret or replace its value (and description).
    ///
    /// # Errors
    /// Why it was not kept, for the person; never the value.
    fn put(
        &self,
        scope: &ProviderScope,
        name: &str,
        description: &str,
        value: &str,
    ) -> Result<(), String>;

    /// Set how terminals may use it.
    ///
    /// # Errors
    /// Why not, for the person.
    fn set_bindings(
        &self,
        scope: &ProviderScope,
        name: &str,
        bindings: Vec<Binding>,
    ) -> Result<(), String>;

    /// Forget it. Removing what is not there is not an error.
    ///
    /// # Errors
    /// Why not, for the person.
    fn delete(&self, scope: &ProviderScope, name: &str) -> Result<(), String>;

    /// Answer a terminal's request. The terminal is told the outcome, never
    /// the value.
    ///
    /// # Errors
    /// Why not (the request is gone, the value is refused), for the person.
    fn answer(&self, ask: u64, answer: AskAnswer) -> Result<(), String> {
        let _ = (ask, answer);
        Err(NO_VAULT.to_owned())
    }

    /// A new Ed25519 key pair, for the person to look at and approve.
    ///
    /// # Errors
    /// When none could be made.
    fn generate_ssh_key(&self, comment: &str) -> Result<GeneratedKey, String> {
        let _ = comment;
        Err(NO_VAULT.to_owned())
    }

    /// Make a new SSH key pair and keep it as `name`, used through the
    /// ssh-agent: the private half never leaves where the vault is. Another
    /// machine's vault (a remote project's) shows the public half in its
    /// list ([`SecretEntry::public_key`]).
    ///
    /// # Errors
    /// Why not, for the person.
    fn put_generated_key(
        &self,
        scope: &ProviderScope,
        name: &str,
        description: &str,
    ) -> Result<(), String> {
        check_name(name)?;
        let made = self.generate_ssh_key(&format!("connecto {name}"))?;
        self.put(scope, name, description, &made.private_openssh)?;
        self.set_bindings(scope, name, vec![Binding::Ssh])
    }

    /// The public half of the SSH key kept as `name`, if it is one.
    fn public_key(&self, scope: &ProviderScope, name: &str) -> Option<String> {
        let _ = (scope, name);
        None
    }

    /// Tell the person a paired machine changed a secret: `what` says which
    /// and how, never a value. Shown in [`VaultView::remote_changes`].
    fn note_remote_change(&self, what: String) {
        let _ = what;
    }

    /// Make `change`, telling apart a refusal of the change itself from a
    /// failure of the vault, and saying whether anything was written. By
    /// default through the calls above, whose refusals are all taken as the
    /// vault's own and nothing as written; the vault itself knows better.
    ///
    /// # Errors
    /// Why not, classified.
    fn change(&self, change: VaultChange) -> Result<(), ChangeError> {
        match change {
            VaultChange::Put {
                scope,
                name,
                description,
                value,
            } => self.put(&scope, &name, &description, &value),
            VaultChange::SetBindings {
                scope,
                name,
                bindings,
            } => self.set_bindings(&scope, &name, bindings),
            VaultChange::Delete { scope, name } => self.delete(&scope, &name),
            VaultChange::GenerateKey {
                scope,
                name,
                description,
            } => self.put_generated_key(&scope, &name, &description),
            VaultChange::Answer { ask, answer } => self.answer(ask, answer),
        }
        .map_err(|why| ChangeError::storage(why, false))
    }

    /// Show `scope`'s secrets after the name `from` (`None`: from the
    /// first), for a list shown in part ([`VaultView::pages`]).
    fn show_secrets_from(&self, scope: &ProviderScope, from: Option<String>) {
        let _ = (scope, from);
    }

    /// What the last answer given to request `ask` came to, when it went to
    /// another machine (a remote project's vault). `None`: none was sent
    /// from here, or this vault answers at once.
    fn answer_state(&self, ask: u64) -> Option<AnswerState> {
        let _ = ask;
        None
    }

    /// The window has told the person of the changes up to number `upto`:
    /// they are no longer kept. Until then none is dropped (up to a bound).
    fn ack_remote_changes(&self, upto: u64) {
        let _ = upto;
    }
}

/// What the terminals' bridge asks of the vault.
pub trait VaultBridge: Send + Sync {
    /// The secrets a terminal of `project` sees (see [`VaultView::visible_to`]).
    fn visible(&self, project: Option<&str>) -> Vec<SecretEntry>;

    /// Every vault value in every form to hide (`connecto_redact::known_set`),
    /// longest first. Cheap: kept until the vault changes.
    fn known(&self) -> Arc<[String]>;

    /// Ask the person for a secret. Returns the request's id at once;
    /// `reply` is called when the person answers.
    ///
    /// # Errors
    /// Why the request was refused (a bad name, too many waiting).
    fn ask(
        &self,
        caller: &SecretCaller,
        request: AskRequest,
        reply: AskReply,
    ) -> Result<u64, String> {
        let _ = (caller, request, reply);
        Err(NO_VAULT.to_owned())
    }

    /// The address through which `caller` sends web requests with `name`.
    ///
    /// # Errors
    /// When there is no such secret, or it is not for web requests.
    fn http_url(&self, caller: &SecretCaller, name: &str) -> Result<String, String> {
        let _ = (caller, name);
        Err(NO_VAULT.to_owned())
    }

    /// Run `program` (an absolute path) with `args` in `cwd` and the secret
    /// in its variable, as the secret's command binding allows. Blocks until
    /// it ends or its time is up.
    ///
    /// # Errors
    /// When there is no such secret, or the program is not allowed.
    fn run(
        &self,
        caller: &SecretCaller,
        name: &str,
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<RunOutput, String> {
        let _ = (caller, name, program, args, cwd);
        Err(NO_VAULT.to_owned())
    }

    /// An ssh-agent socket for `caller` that signs with the key `name`.
    ///
    /// # Errors
    /// When there is no such secret, or it is not an SSH key.
    fn ssh(&self, caller: &SecretCaller, name: &str) -> Result<SshAccess, String> {
        let _ = (caller, name);
        Err(NO_VAULT.to_owned())
    }
}

/// No vault: nothing to list and nothing to hide.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoVault;

impl VaultBridge for NoVault {
    fn visible(&self, _project: Option<&str>) -> Vec<SecretEntry> {
        Vec::new()
    }

    fn known(&self) -> Arc<[String]> {
        Arc::from(Vec::new())
    }
}

const NO_VAULT: &str = "Secrets are not available.";

impl VaultControl for NoVault {
    fn view(&self) -> Arc<VaultView> {
        Arc::default()
    }

    fn put(&self, _: &ProviderScope, _: &str, _: &str, _: &str) -> Result<(), String> {
        Err(NO_VAULT.to_owned())
    }

    fn set_bindings(&self, _: &ProviderScope, _: &str, _: Vec<Binding>) -> Result<(), String> {
        Err(NO_VAULT.to_owned())
    }

    fn delete(&self, _: &ProviderScope, _: &str) -> Result<(), String> {
        Err(NO_VAULT.to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(name: &str, scope: ProviderScope) -> SecretEntry {
        SecretEntry {
            name: name.to_owned(),
            scope,
            description: String::new(),
            bindings: Vec::new(),
            updated_unix: 0,
            last_used_unix: None,
            public_key: None,
        }
    }

    #[test]
    fn names_are_plain() {
        assert!(check_name("GITHUB_TOKEN").is_ok());
        assert!(check_name("aws-prod").is_ok());
        for bad in ["", "1abc", "a b", "a.b", "a/b", "ä", &"a".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_empty_or_generic_reason_is_refused_in_english_and_italian() {
        for good in [
            "Open pull requests on github.com/acme/web through the GitHub API",
            "PostgreSQL connection string for the Agentic Works queue",
            "Restart staging PostgreSQL",
            "to reach the staging server over SSH",
            "Per pubblicare il sito su Fly.io con flyctl",
            "Riavviare il database di staging",
        ] {
            assert!(check_reason(good, "GITHUB_TOKEN").is_ok(), "{good:?}");
        }
        for vague in [
            "",
            "   ",
            "x",
            "deploy",
            "needed for the task",
            "This token is needed to do the task",
            "I need the API key for authentication please",
            "Needs the GITHUB_TOKEN token to access the API",
            "I need it for normal operations",
            "For various things in this project",
            "Mi serve per questo lavoro",
            "Ho bisogno della chiave per il progetto",
            "Serve per le normali operazioni",
        ] {
            let why = check_reason(vague, "GITHUB_TOKEN").unwrap_err();
            assert!(why.contains("--reason"), "{vague:?}: {why}");
        }
    }

    /// Words are counted once, and the secret's own name may be one of
    /// them when another says something too.
    #[test]
    fn a_reason_counts_distinct_words_and_may_name_the_service() {
        assert!(check_reason("Deploy to Vercel", "VERCEL_TOKEN").is_ok());
        assert!(check_reason("Push images to the GitHub registry", "GITHUB_TOKEN").is_ok());
        for vague in [
            "deploy deploy",
            "Vercel vercel VERCEL",
            "token for Vercel",
            "deploy DEPLOY",
        ] {
            assert!(check_reason(vague, "VERCEL_TOKEN").is_err(), "{vague:?}");
        }
    }

    /// The person sees exactly the reason that was checked: a longer one is
    /// refused, not cut.
    #[test]
    fn a_reason_longer_than_is_shown_is_refused() {
        let long = format!("{} then deploy to Fly.io", "please ".repeat(45));
        assert!(display_line(&long, usize::MAX).chars().count() > MAX_REASON);
        let why = check_reason(&long, "FLY").unwrap_err();
        assert!(why.contains("shorten") && why.contains("300"), "{why}");
        let fits = format!("{} deploy to Fly.io", "x".repeat(MAX_REASON - 18));
        assert!(check_reason(&fits, "FLY").is_ok());
    }

    /// Different destinations never look the same: each value is shown
    /// in quotes, every character that is not plain ASCII (or that would
    /// end the quotes) written out.
    #[test]
    fn a_uses_summary_tells_every_destination_apart() {
        let web = |url: &str, prefix: &str| {
            Binding::Http {
                base_url: url.into(),
                header: "Authorization".into(),
                prefix: prefix.into(),
            }
            .plain_use()
        };
        let pairs = [
            (
                web("https://h/release\"notes", ""),
                web("https://h/release'notes", ""),
            ),
            (
                web("https://h/a", "Bearer  "),
                web("https://h/a", "Bearer "),
            ),
            (web("https://h/\u{0430}", ""), web("https://h/a", "")),
            (web("https://h/\\u{61}", ""), web("https://h/a", "")),
        ];
        for (one, other) in &pairs {
            assert_ne!(one, other);
        }
        assert!(
            pairs[0].0.contains(r#""https://h/release\"notes""#),
            "{}",
            pairs[0].0
        );
        assert!(pairs[2].0.contains(r"\u{430}"), "{}", pairs[2].0);
        let two = |a: &str| {
            Binding::Command {
                programs: vec![a.into()],
                env: "GH_TOKEN".into(),
            }
            .plain_use()
        };
        assert_ne!(two("/opt/My  Tool/x"), two("/opt/My Tool/x"));
    }

    #[test]
    fn a_proposed_use_must_survive_the_form_one_path_per_line() {
        let command = |path: &str| Binding::Command {
            programs: vec![path.into()],
            env: "GH_TOKEN".into(),
        };
        for bad in [
            "/opt/gh\n/opt/deploy",
            "/opt/gh\r",
            " /opt/gh",
            "/opt/gh ",
            "/opt/g\u{2028}h",
            "",
        ] {
            assert!(check_proposed(&command(bad)).is_err(), "{bad:?}");
        }
        assert!(check_proposed(&command("/opt/My  Tool/x")).is_ok());
        assert!(check_proposed(&Binding::Ssh).is_ok());
    }

    #[test]
    fn a_uses_summary_cannot_be_reordered_or_broken() {
        let web = Binding::Http {
            base_url: "https://evil.example/\u{202e}moc.buhtig".into(),
            header: "Authorization".into(),
            prefix: "Bearer ".into(),
        }
        .plain_use();
        assert!(!web.contains('\u{202e}'), "{web:?}");
        let program = Binding::Command {
            programs: vec!["/opt/a\u{2028}b/gh".into()],
            env: "GH_TOKEN".into(),
        }
        .plain_use();
        assert!(!program.contains('\u{2028}'), "{program:?}");
    }

    #[test]
    fn a_refusal_never_repeats_the_reason() {
        let why = check_reason("token sk-abcdefgh1234", "KEY").unwrap_err();
        assert!(!why.contains("abcdefgh1234"), "{why}");
    }

    #[test]
    fn text_to_show_cannot_break_its_line_or_quotes() {
        assert_eq!(
            display_line("a\u{201d} b\u{2028}c\u{202e}d\n\"e\u{200b}f", 100),
            "a' b c d 'e f"
        );
        assert_eq!(display_line("abcdef", 3), "abc");
    }

    #[test]
    fn each_use_says_where_the_secret_goes() {
        let web = Binding::Http {
            base_url: "https://api.github.com".into(),
            header: "Authorization".into(),
            prefix: "Bearer ".into(),
        }
        .plain_use();
        assert!(
            web.contains(r#""https://api.github.com""#)
                && web.contains(r#"header "Authorization", after "Bearer ""#),
            "{web}"
        );
        let program = Binding::Command {
            programs: vec!["/opt/homebrew/bin/gh".into()],
            env: "GH_TOKEN".into(),
        }
        .plain_use();
        assert!(
            program.contains("/opt/homebrew/bin/gh") && program.contains("GH_TOKEN"),
            "{program}"
        );
        assert!(Binding::Ssh.plain_use().contains("SSH"));
    }

    #[test]
    fn values_are_trimmed_of_the_paste_line_ending_and_checked() {
        assert_eq!(clean_value("abcdefgh\n").unwrap(), "abcdefgh");
        assert_eq!(clean_value(" abcdefgh ").unwrap(), " abcdefgh ");
        assert!(clean_value("short").is_err());
        assert!(clean_value("   \n").is_err());
        assert!(clean_value("abcd\0efgh").is_err());
        assert!(clean_value(&"a".repeat(MAX_VALUE + 1)).is_err());
    }

    #[test]
    fn errors_never_hold_the_value() {
        let value = "hunter2\0hunter2";
        assert!(!clean_value(value).unwrap_err().contains("hunter2"));
    }

    #[test]
    fn a_terminal_sees_its_project_and_the_app_wide_ones() {
        let view = VaultView {
            entries: vec![
                entry("A", ProviderScope::Global),
                entry("B", ProviderScope::Global),
                entry("B", ProviderScope::Project("p".into())),
                entry("C", ProviderScope::Project("other".into())),
            ],
            ..VaultView::default()
        };
        let seen = view.visible_to(Some("p"));
        let names: Vec<(&str, &ProviderScope)> =
            seen.iter().map(|e| (e.name.as_str(), &e.scope)).collect();
        assert_eq!(
            names,
            [
                ("A", &ProviderScope::Global),
                ("B", &ProviderScope::Project("p".into())),
            ]
        );
        let none: Vec<&str> = view
            .visible_to(None)
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(none, ["A", "B"]);
    }
}
