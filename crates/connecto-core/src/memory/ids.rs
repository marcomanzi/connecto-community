//! Rooms, identifiers and addresses (spec §4.1, §4.2).

use std::fmt;
use std::str::FromStr;

use super::MemoryError;

word_enum! {
    /// The fixed rooms. None is created or renamed in this version.
    pub enum Room {
        /// The brief. A generated view, not a store: no memory lives here.
        Entrance => "ingresso", "Entrance";
        /// Structure, components, boundaries, flows.
        Architecture => "architettura", "Architecture";
        /// Procedures: build, test, release, diagnose.
        Workshop => "officina", "Workshop";
        /// Choices with their reasons.
        Decisions => "decisioni", "Decisions";
        /// Known defects, traps, recurring mistakes and how to avoid them.
        Problems => "problemi", "Problems";
        /// Work in progress and on hold.
        OpenWork => "lavori-aperti", "Open work";
        /// Style and working rules.
        Conventions => "convenzioni", "Conventions";
        /// Superseded or inactive memories. Readable; left out of the brief and,
        /// by default, of search.
        Archive => "archivio", "Archive";
    }
}

impl Room {
    /// Whether memories can live in this room (every room but the entrance).
    #[must_use]
    pub const fn holds_memories(self) -> bool {
        !matches!(self, Self::Entrance)
    }

    /// The room for `slug`, refusing the entrance as a place for memories.
    pub fn for_memories(slug: &str) -> Result<Self, MemoryError> {
        let room = Self::from_word(slug).ok_or(MemoryError::BadRoom)?;
        if room.holds_memories() {
            Ok(room)
        } else {
            Err(MemoryError::EntranceHoldsNothing)
        }
    }
}

/// RFC 4648 base32, lowercase.
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const RANDOM_CHARS: usize = 8;

fn encode_base32(bytes: [u8; 5]) -> String {
    let mut bits = bytes
        .iter()
        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
    let mut out = [0u8; RANDOM_CHARS];
    for slot in out.iter_mut().rev() {
        // The index is five bits: always within the alphabet.
        *slot = BASE32.get((bits & 0x1f) as usize).copied().unwrap_or(b'a');
        bits >>= 5;
    }
    out.iter().map(|b| char::from(*b)).collect()
}

fn valid_random_id(text: &str, prefix: &str) -> bool {
    text.strip_prefix(prefix)
        .is_some_and(|rest| rest.len() == RANDOM_CHARS && rest.bytes().all(|b| BASE32.contains(&b)))
}

macro_rules! random_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal, $error:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// The prefix every id of this type starts with.
            pub const PREFIX: &'static str = $prefix;

            /// A new id from 40 random bits. The caller supplies the randomness
            /// (tests pass fixed bytes) and checks the store for a collision:
            /// an id is never reused.
            #[must_use]
            pub fn from_entropy(bytes: [u8; 5]) -> Self {
                Self(format!("{}{}", $prefix, encode_base32(bytes)))
            }

            /// Read an id: the prefix and exactly 8 lowercase base32
            /// characters.
            pub fn parse(text: &str) -> Result<Self, MemoryError> {
                if valid_random_id(text, $prefix) {
                    Ok(Self(text.to_owned()))
                } else {
                    Err(MemoryError::$error)
                }
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = MemoryError;
            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::parse(text)
            }
        }
    };
}

random_id!(
    /// A memory: `m-` and 8 base32 characters, made by the host.
    MemoryId,
    "m-",
    BadMemoryId
);
random_id!(
    /// An annotation (postilla): `p-` and 8 base32 characters.
    AnnotationId,
    "p-",
    BadAnnotationId
);

/// A piece of evidence: `e-` and the sequence number of the event it came
/// from, in decimal without leading zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EvidenceId(u64);

impl EvidenceId {
    pub const PREFIX: &'static str = "e-";

    #[must_use]
    pub const fn new(seq: u64) -> Self {
        Self(seq)
    }

    #[must_use]
    pub const fn seq(self) -> u64 {
        self.0
    }

    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        let digits = text
            .strip_prefix(Self::PREFIX)
            .ok_or(MemoryError::BadEvidenceId)?;
        let canonical = !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'));
        if !canonical {
            return Err(MemoryError::BadEvidenceId);
        }
        digits
            .parse()
            .map(Self)
            .map_err(|_| MemoryError::BadEvidenceId)
    }
}

impl fmt::Display for EvidenceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", Self::PREFIX, self.0)
    }
}

impl FromStr for EvidenceId {
    type Err = MemoryError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// Something `connecto memory` can show a session, and so something evidence
/// can have been exposed to (`Evidence::exposed_to`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Subject {
    Memory(MemoryId),
    Annotation(AnnotationId),
}

impl Subject {
    /// A memory or annotation id, told apart by its prefix.
    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        if text.starts_with(MemoryId::PREFIX) {
            MemoryId::parse(text).map(Self::Memory)
        } else if text.starts_with(AnnotationId::PREFIX) {
            AnnotationId::parse(text).map(Self::Annotation)
        } else {
            Err(MemoryError::BadLookup)
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory(id) => id.fmt(f),
            Self::Annotation(id) => id.fmt(f),
        }
    }
}

/// A persistent address `room/slug`, such as `officina/rilascio-macos`. A move
/// leaves a redirect from the old address to the new one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address {
    room: Room,
    slug: String,
}

impl Address {
    /// Longest slug, in bytes.
    pub const MAX_SLUG: usize = 64;

    /// `room` must hold memories; `slug` must be [`Address::is_valid_slug`].
    pub fn new(room: Room, slug: &str) -> Result<Self, MemoryError> {
        if !room.holds_memories() {
            return Err(MemoryError::EntranceHoldsNothing);
        }
        if !Self::is_valid_slug(slug) {
            return Err(MemoryError::BadSlug);
        }
        Ok(Self {
            room,
            slug: slug.to_owned(),
        })
    }

    /// Read `room/slug`.
    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        let (room, slug) = text.split_once('/').ok_or(MemoryError::BadLookup)?;
        Self::new(Room::for_memories(room)?, slug)
    }

    /// 1 to 64 lowercase ASCII letters and digits in groups joined by single
    /// hyphens: `rilascio-macos`.
    #[must_use]
    pub fn is_valid_slug(slug: &str) -> bool {
        !slug.is_empty()
            && slug.len() <= Self::MAX_SLUG
            && slug.split('-').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            })
    }

    /// A slug made from free text: lowercase ASCII letters and digits, every
    /// other run of characters a single hyphen, cut at a hyphen to fit.
    /// Accented letters lose their accent when it is a common Latin one and
    /// are dropped otherwise. `None` when nothing usable is left.
    #[must_use]
    pub fn slugify(text: &str) -> Option<String> {
        let mut slug = String::new();
        let mut gap = false;
        for c in text.chars().flat_map(char::to_lowercase) {
            let c = fold_accent(c);
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                if gap && !slug.is_empty() {
                    slug.push('-');
                }
                gap = false;
                slug.push(c);
            } else {
                gap = true;
            }
        }
        while slug.len() > Self::MAX_SLUG {
            match slug.rfind('-') {
                Some(cut) => slug.truncate(cut),
                None => slug.truncate(Self::MAX_SLUG),
            }
        }
        Self::is_valid_slug(&slug).then_some(slug)
    }

    #[must_use]
    pub const fn room(&self) -> Room {
        self.room
    }

    #[must_use]
    pub fn slug(&self) -> &str {
        &self.slug
    }
}

fn fold_accent(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => 'a',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ò' | 'ó' | 'ô' | 'ö' | 'õ' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        other => other,
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.room.word(), self.slug)
    }
}

impl FromStr for Address {
    type Err = MemoryError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// What `connecto memory read` (and the UI) can be asked to open: an id, or an
/// address (which may be a redirect).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Lookup {
    Memory(MemoryId),
    Annotation(AnnotationId),
    Address(Address),
}

impl Lookup {
    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        if text.contains('/') {
            return Address::parse(text).map(Self::Address);
        }
        match Subject::parse(text)? {
            Subject::Memory(id) => Ok(Self::Memory(id)),
            Subject::Annotation(id) => Ok(Self::Annotation(id)),
        }
    }
}

impl fmt::Display for Lookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory(id) => id.fmt(f),
            Self::Annotation(id) => id.fmt(f),
            Self::Address(address) => address.fmt(f),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rooms_have_stable_slugs_and_the_entrance_holds_nothing() {
        let slugs: Vec<_> = Room::ALL.iter().map(|r| r.word()).collect();
        assert_eq!(
            slugs,
            [
                "ingresso",
                "architettura",
                "officina",
                "decisioni",
                "problemi",
                "lavori-aperti",
                "convenzioni",
                "archivio"
            ]
        );
        for room in Room::ALL {
            assert_eq!(Room::from_word(room.word()), Some(*room));
        }
        assert_eq!(
            Room::for_memories("ingresso"),
            Err(MemoryError::EntranceHoldsNothing)
        );
        assert_eq!(Room::for_memories("cantina"), Err(MemoryError::BadRoom));
        assert_eq!(Room::for_memories("officina"), Ok(Room::Workshop));
    }

    #[test]
    fn random_ids_are_prefix_and_eight_base32_characters() {
        let id = MemoryId::from_entropy([0, 0, 0, 0, 0]);
        assert_eq!(id.as_str(), "m-aaaaaaaa");
        let id = MemoryId::from_entropy([0xff; 5]);
        assert_eq!(id.as_str(), "m-77777777");
        let id = AnnotationId::from_entropy([0x12, 0x34, 0x56, 0x78, 0x9a]);
        assert_eq!(AnnotationId::parse(id.as_str()), Ok(id.clone()));
        assert!(id.as_str().starts_with("p-"));
        // Distinct bytes, distinct ids.
        assert_ne!(
            MemoryId::from_entropy([1, 2, 3, 4, 5]),
            MemoryId::from_entropy([1, 2, 3, 4, 6])
        );
    }

    #[test]
    fn id_parsing_is_strict() {
        for bad in [
            "",
            "m-",
            "m-abc",
            "m-abcdefgh1",
            "m-ABCDEFGH",
            "m-abcdefg1",
            "p-abcdefgh",
            "m_abcdefgh",
            " m-abcdefgh",
        ] {
            assert_eq!(MemoryId::parse(bad), Err(MemoryError::BadMemoryId), "{bad}");
        }
        assert!(MemoryId::parse("m-abcdefg2").is_ok());
        assert_eq!(
            AnnotationId::parse("m-abcdefgh"),
            Err(MemoryError::BadAnnotationId)
        );
    }

    #[test]
    fn evidence_ids_are_canonical_numbers() {
        assert_eq!(EvidenceId::parse("e-42"), Ok(EvidenceId::new(42)));
        assert_eq!(EvidenceId::new(7).to_string(), "e-7");
        assert_eq!(EvidenceId::parse("e-0"), Ok(EvidenceId::new(0)));
        for bad in [
            "e-",
            "e-01",
            "e--1",
            "e-1a",
            "42",
            "e-99999999999999999999999",
        ] {
            assert_eq!(
                EvidenceId::parse(bad),
                Err(MemoryError::BadEvidenceId),
                "{bad}"
            );
        }
    }

    #[test]
    fn addresses_parse_and_print() {
        let a = Address::parse("officina/rilascio-macos").unwrap();
        assert_eq!(a.room(), Room::Workshop);
        assert_eq!(a.slug(), "rilascio-macos");
        assert_eq!(a.to_string(), "officina/rilascio-macos");
        assert!(Address::parse("archivio/vecchio-2").is_ok());
        assert_eq!(
            Address::parse("ingresso/brief"),
            Err(MemoryError::EntranceHoldsNothing)
        );
        for bad in [
            "officina/",
            "officina/Rilascio",
            "officina/a--b",
            "officina/-a",
            "officina/a-",
            "officina/a/b",
            "officina/a b",
        ] {
            assert!(Address::parse(bad).is_err(), "{bad}");
        }
        assert!(Address::parse(&format!("officina/{}", "a".repeat(65))).is_err());
    }

    #[test]
    fn slugify_makes_valid_slugs() {
        assert_eq!(
            Address::slugify("Rilascio macOS: firma & notarizzazione").as_deref(),
            Some("rilascio-macos-firma-notarizzazione")
        );
        assert_eq!(
            Address::slugify("Perché è così").as_deref(),
            Some("perche-e-cosi")
        );
        assert_eq!(Address::slugify("!!!"), None);
        let long = Address::slugify(&"parola ".repeat(30)).unwrap();
        assert!(Address::is_valid_slug(&long));
        assert!(long.len() <= Address::MAX_SLUG);
    }

    #[test]
    fn lookups_tell_ids_from_addresses() {
        assert!(matches!(Lookup::parse("m-abcdefgh"), Ok(Lookup::Memory(_))));
        assert!(matches!(
            Lookup::parse("p-abcdefgh"),
            Ok(Lookup::Annotation(_))
        ));
        assert!(matches!(
            Lookup::parse("decisioni/sqlite"),
            Ok(Lookup::Address(_))
        ));
        assert_eq!(Lookup::parse("sqlite"), Err(MemoryError::BadLookup));
        assert_eq!(
            Lookup::parse("m-abcdefgh").unwrap().to_string(),
            "m-abcdefgh"
        );
    }
}
