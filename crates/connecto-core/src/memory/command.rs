//! `connecto memory <verb> [args]` (spec §6), parsed once for both sides of
//! the bridge: `connecto-hook` reads the command line, `connecto-memory`
//! answers it. The wire carries the verb word, the arguments as given and the
//! stdin text; both ends call [`MemoryCommand::parse`].
//!
//! Errors are fixed sentences: they never repeat what the agent typed.

use super::MemoryError;
use super::ids::{Lookup, MemoryId, Room};
use super::record::{AnnotationKind, Scope, check_note_text};

word_enum! {
    pub enum Verb {
        Brief => "brief", "Brief";
        Search => "search", "Search";
        Read => "read", "Read";
        Room => "room", "Room";
        Propose => "propose", "Propose";
        Flag => "flag", "Flag";
        Status => "status", "Status";
    }
}

impl Verb {
    /// The default output budget, in estimated tokens (characters / 4);
    /// `None` for the verbs that write.
    #[must_use]
    pub const fn default_budget(self) -> Option<u32> {
        match self {
            Self::Brief => Some(600),
            Self::Search | Self::Room => Some(800),
            Self::Read => Some(1500),
            Self::Status => Some(200),
            Self::Propose | Self::Flag => None,
        }
    }

    /// Whether the verb takes text on stdin (and requires it).
    #[must_use]
    pub const fn takes_text(self) -> bool {
        matches!(self, Self::Propose | Self::Flag)
    }
}

/// The usage lines, one per verb.
pub const USAGE: &str = "\
connecto memory brief [--budget N]
connecto memory search <text> [--room R] [--scope S] [--all] [--budget N]
connecto memory read <id|room/slug> [--budget N]
connecto memory room <room> [--budget N]
connecto memory propose [--type integrazione|correzione|contraddizione|task] [--about <id>] [--room R] [--scope S]  (text on stdin)
connecto memory flag <id> [--type contraddizione|correzione]  (reason on stdin)
connecto memory status [--budget N]";

/// Smallest and largest `--budget`, in estimated tokens.
pub const MIN_BUDGET: u32 = 50;
pub const MAX_BUDGET: u32 = 20_000;

/// A parsed `connecto memory` command. `budget` is `None` when not given:
/// use [`Verb::default_budget`] ([`MemoryCommand::budget`] does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryCommand {
    Brief {
        budget: Option<u32>,
    },
    Search {
        query: String,
        room: Option<Room>,
        scope: Option<Scope>,
        /// Include the archive.
        all: bool,
        budget: Option<u32>,
    },
    Read {
        target: Lookup,
        budget: Option<u32>,
    },
    /// The entrance is accepted: it shows the brief.
    Room {
        room: Room,
        budget: Option<u32>,
    },
    Propose {
        kind: AnnotationKind,
        /// The memory it is about; required for `correzione` and
        /// `contraddizione`.
        about: Option<MemoryId>,
        /// The room suggested for a new note (not with `about`).
        room: Option<Room>,
        scope: Option<Scope>,
    },
    Flag {
        target: MemoryId,
        /// `contraddizione` (default) or `correzione`.
        kind: AnnotationKind,
    },
    Status {
        budget: Option<u32>,
    },
}

/// Why a command line was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("unknown memory command; one of: brief, search, read, room, propose, flag, status")]
    UnknownVerb,
    #[error("unknown option for this command")]
    UnknownOption,
    #[error("an option was given twice")]
    Repeated,
    #[error("missing {0}")]
    Missing(&'static str),
    #[error("unexpected argument")]
    Unexpected,
    #[error("--budget must be a number from 50 to 20000")]
    BadBudget,
    #[error(
        "--type must be integrazione, correzione, contraddizione or task (flag: contraddizione or correzione)"
    )]
    BadType,
    #[error("a correction or contradiction needs --about <id>")]
    AboutRequired,
    #[error("use --about or --room, not both")]
    AboutAndRoom,
    #[error("{0}")]
    Value(MemoryError),
    #[error("this command takes no text on stdin")]
    TextNotTaken,
}

impl From<MemoryError> for CommandError {
    fn from(error: MemoryError) -> Self {
        Self::Value(error)
    }
}

/// Options, read in any order, as `--name value` or `--name=value`.
#[derive(Default)]
struct Options {
    budget: Option<u32>,
    room: Option<Room>,
    scope: Option<Scope>,
    all: bool,
    kind: Option<AnnotationKind>,
    about: Option<MemoryId>,
    positional: Vec<String>,
}

fn set<T>(slot: &mut Option<T>, value: T) -> Result<(), CommandError> {
    if slot.is_some() {
        return Err(CommandError::Repeated);
    }
    *slot = Some(value);
    Ok(())
}

impl Options {
    fn read(args: &[String], allowed: &[&str]) -> Result<Self, CommandError> {
        let mut options = Self::default();
        let mut rest = args.iter();
        let mut literal = false;
        while let Some(arg) = rest.next() {
            if literal || !arg.starts_with("--") {
                options.positional.push(arg.clone());
                continue;
            }
            if arg == "--" {
                literal = true;
                continue;
            }
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            let name = name.trim_start_matches("--");
            if !allowed.contains(&name) {
                return Err(CommandError::UnknownOption);
            }
            if name == "all" {
                if inline.is_some() || options.all {
                    return Err(CommandError::Unexpected);
                }
                options.all = true;
                continue;
            }
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or(CommandError::Missing("an option value"))?,
            };
            match name {
                "budget" => {
                    let budget = value
                        .parse::<u32>()
                        .ok()
                        .filter(|n| (MIN_BUDGET..=MAX_BUDGET).contains(n))
                        .ok_or(CommandError::BadBudget)?;
                    set(&mut options.budget, budget)?;
                }
                "room" => set(
                    &mut options.room,
                    Room::from_word(&value).ok_or(MemoryError::BadRoom)?,
                )?,
                "scope" => set(&mut options.scope, Scope::parse(&value)?)?,
                "type" => set(
                    &mut options.kind,
                    AnnotationKind::from_word(&value).ok_or(CommandError::BadType)?,
                )?,
                "about" => set(&mut options.about, MemoryId::parse(&value)?)?,
                _ => return Err(CommandError::UnknownOption),
            }
        }
        Ok(options)
    }

    fn no_positional(&self) -> Result<(), CommandError> {
        if self.positional.is_empty() {
            Ok(())
        } else {
            Err(CommandError::Unexpected)
        }
    }

    /// Exactly one positional argument.
    fn one(&self, what: &'static str) -> Result<&str, CommandError> {
        match self.positional.as_slice() {
            [one] => Ok(one),
            [] => Err(CommandError::Missing(what)),
            _ => Err(CommandError::Unexpected),
        }
    }
}

impl MemoryCommand {
    /// Parse the verb word and the arguments after it.
    pub fn parse(verb: &str, args: &[String]) -> Result<Self, CommandError> {
        let verb = Verb::from_word(verb).ok_or(CommandError::UnknownVerb)?;
        match verb {
            Verb::Brief | Verb::Status => {
                let o = Options::read(args, &["budget"])?;
                o.no_positional()?;
                Ok(if verb == Verb::Brief {
                    Self::Brief { budget: o.budget }
                } else {
                    Self::Status { budget: o.budget }
                })
            }
            Verb::Search => {
                let o = Options::read(args, &["room", "scope", "all", "budget"])?;
                let query = o.positional.join(" ");
                if query.trim().is_empty() {
                    return Err(CommandError::Missing("the text to search for"));
                }
                Ok(Self::Search {
                    query,
                    room: o.room,
                    scope: o.scope,
                    all: o.all,
                    budget: o.budget,
                })
            }
            Verb::Read => {
                let o = Options::read(args, &["budget"])?;
                let target = Lookup::parse(o.one("an id or address")?)?;
                Ok(Self::Read {
                    target,
                    budget: o.budget,
                })
            }
            Verb::Room => {
                let o = Options::read(args, &["budget"])?;
                let room = Room::from_word(o.one("a room")?).ok_or(MemoryError::BadRoom)?;
                Ok(Self::Room {
                    room,
                    budget: o.budget,
                })
            }
            Verb::Propose => {
                let o = Options::read(args, &["type", "about", "room", "scope"])?;
                o.no_positional()?;
                let kind = o.kind.unwrap_or_default();
                if o.about.is_some() && o.room.is_some() {
                    return Err(CommandError::AboutAndRoom);
                }
                if kind.needs_target() && o.about.is_none() {
                    return Err(CommandError::AboutRequired);
                }
                if let Some(room) = o.room
                    && !room.holds_memories()
                {
                    return Err(MemoryError::EntranceHoldsNothing.into());
                }
                Ok(Self::Propose {
                    kind,
                    about: o.about,
                    room: o.room,
                    scope: o.scope,
                })
            }
            Verb::Flag => {
                let o = Options::read(args, &["type"])?;
                let target = MemoryId::parse(o.one("the id of the memory")?)?;
                let kind = match o.kind {
                    None | Some(AnnotationKind::Contradiction) => AnnotationKind::Contradiction,
                    Some(AnnotationKind::Correction) => AnnotationKind::Correction,
                    Some(_) => return Err(CommandError::BadType),
                };
                Ok(Self::Flag { target, kind })
            }
        }
    }

    #[must_use]
    pub const fn verb(&self) -> Verb {
        match self {
            Self::Brief { .. } => Verb::Brief,
            Self::Search { .. } => Verb::Search,
            Self::Read { .. } => Verb::Read,
            Self::Room { .. } => Verb::Room,
            Self::Propose { .. } => Verb::Propose,
            Self::Flag { .. } => Verb::Flag,
            Self::Status { .. } => Verb::Status,
        }
    }

    /// The budget to render with: the one given, or the verb's default.
    /// `None` for the verbs that write.
    #[must_use]
    pub const fn budget(&self) -> Option<u32> {
        match self {
            Self::Brief { budget }
            | Self::Search { budget, .. }
            | Self::Read { budget, .. }
            | Self::Room { budget, .. }
            | Self::Status { budget } => match budget {
                Some(b) => Some(*b),
                None => self.verb().default_budget(),
            },
            Self::Propose { .. } | Self::Flag { .. } => None,
        }
    }

    /// Check the stdin text against the verb: `propose` and `flag` need a
    /// text that is not blank and at most 8 KiB (refused, never cut); the
    /// others take none.
    pub fn check_text(&self, text: Option<&str>) -> Result<(), CommandError> {
        if self.verb().takes_text() {
            check_note_text(text.unwrap_or_default()).map_err(CommandError::from)
        } else if text.is_some_and(|t| !t.is_empty()) {
            Err(CommandError::TextNotTaken)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::memory::MAX_NOTE_BYTES;

    fn parse(line: &str) -> Result<MemoryCommand, CommandError> {
        let mut words = line.split_whitespace();
        let verb = words.next().unwrap_or_default();
        let args: Vec<String> = words.map(str::to_owned).collect();
        MemoryCommand::parse(verb, &args)
    }

    #[test]
    fn verbs_and_default_budgets() {
        assert_eq!(parse("brief").unwrap().budget(), Some(600));
        assert_eq!(parse("search x").unwrap().budget(), Some(800));
        assert_eq!(parse("read m-abcdefgh").unwrap().budget(), Some(1500));
        assert_eq!(parse("room officina").unwrap().budget(), Some(800));
        assert_eq!(parse("status").unwrap().budget(), Some(200));
        assert_eq!(parse("propose").unwrap().budget(), None);
        assert_eq!(parse("brief --budget 300").unwrap().budget(), Some(300));
        assert_eq!(parse("brief --budget=300").unwrap().budget(), Some(300));
        for verb in Verb::ALL {
            assert_eq!(Verb::from_word(verb.word()), Some(*verb));
        }
        assert_eq!(parse("forget"), Err(CommandError::UnknownVerb));
    }

    #[test]
    fn search_takes_free_text_and_filters() {
        assert_eq!(
            parse("search release macos --room officina --scope branch:main --all --budget 100"),
            Ok(MemoryCommand::Search {
                query: "release macos".to_owned(),
                room: Some(Room::Workshop),
                scope: Some(Scope::Branch("main".to_owned())),
                all: true,
                budget: Some(100),
            })
        );
        let literal =
            MemoryCommand::parse("search", &["--".to_owned(), "--all".to_owned()]).unwrap();
        assert!(
            matches!(literal, MemoryCommand::Search { ref query, all: false, .. } if query == "--all")
        );
        assert_eq!(
            parse("search"),
            Err(CommandError::Missing("the text to search for"))
        );
        assert_eq!(parse("search x --all --all"), Err(CommandError::Unexpected));
        assert_eq!(
            parse("search x --room cantina"),
            Err(MemoryError::BadRoom.into())
        );
    }

    #[test]
    fn refusals() {
        let table: &[(&str, CommandError)] = &[
            ("brief extra", CommandError::Unexpected),
            ("brief --room officina", CommandError::UnknownOption),
            ("brief --budget", CommandError::Missing("an option value")),
            ("brief --budget 10", CommandError::BadBudget),
            ("brief --budget 20001", CommandError::BadBudget),
            ("brief --budget x", CommandError::BadBudget),
            ("brief --budget 100 --budget 200", CommandError::Repeated),
            ("read", CommandError::Missing("an id or address")),
            ("read a b", CommandError::Unexpected),
            ("read sqlite", CommandError::Value(MemoryError::BadLookup)),
            ("room", CommandError::Missing("a room")),
            ("propose --type correzione", CommandError::AboutRequired),
            ("propose --type contraddizione", CommandError::AboutRequired),
            ("propose --type idea", CommandError::BadType),
            (
                "propose --about m-abcdefgh --room officina",
                CommandError::AboutAndRoom,
            ),
            (
                "propose --room ingresso",
                CommandError::Value(MemoryError::EntranceHoldsNothing),
            ),
            (
                "propose --about nope",
                CommandError::Value(MemoryError::BadMemoryId),
            ),
            ("propose words", CommandError::Unexpected),
            ("flag", CommandError::Missing("the id of the memory")),
            ("flag m-abcdefgh --type task", CommandError::BadType),
            ("flag m-abcdefgh --budget 100", CommandError::UnknownOption),
        ];
        for (line, error) in table {
            assert_eq!(parse(line), Err(*error), "{line}");
        }
    }

    #[test]
    fn propose_and_flag() {
        assert_eq!(
            parse("propose"),
            Ok(MemoryCommand::Propose {
                kind: AnnotationKind::Integration,
                about: None,
                room: None,
                scope: None,
            })
        );
        assert_eq!(
            parse("propose --type task --room lavori-aperti --scope project"),
            Ok(MemoryCommand::Propose {
                kind: AnnotationKind::Task,
                about: None,
                room: Some(Room::OpenWork),
                scope: Some(Scope::Project),
            })
        );
        assert!(parse("propose --type correzione --about m-abcdefgh").is_ok());
        assert_eq!(
            parse("flag m-abcdefgh"),
            Ok(MemoryCommand::Flag {
                target: MemoryId::parse("m-abcdefgh").unwrap(),
                kind: AnnotationKind::Contradiction,
            })
        );
        assert!(matches!(
            parse("flag m-abcdefgh --type correzione"),
            Ok(MemoryCommand::Flag {
                kind: AnnotationKind::Correction,
                ..
            })
        ));
    }

    #[test]
    fn text_rules() {
        let propose = parse("propose").unwrap();
        assert_eq!(propose.check_text(Some("We chose SQLite.")), Ok(()));
        assert_eq!(propose.check_text(None), Err(MemoryError::EmptyText.into()));
        assert_eq!(
            propose.check_text(Some("  ")),
            Err(MemoryError::EmptyText.into())
        );
        let long = "x".repeat(MAX_NOTE_BYTES + 1);
        assert_eq!(
            propose.check_text(Some(&long)),
            Err(MemoryError::TextTooLong.into())
        );
        let brief = parse("brief").unwrap();
        assert_eq!(brief.check_text(None), Ok(()));
        assert_eq!(brief.check_text(Some("")), Ok(()));
        assert_eq!(brief.check_text(Some("x")), Err(CommandError::TextNotTaken));
    }

    #[test]
    fn the_entrance_room_is_readable() {
        assert!(matches!(
            parse("room ingresso"),
            Ok(MemoryCommand::Room {
                room: Room::Entrance,
                ..
            })
        ));
    }
}
