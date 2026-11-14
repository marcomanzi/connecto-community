//! Release versions: `MAJOR.MINOR.PATCH`, compared as numbers.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// A release version. No pre-release or build parts: there is one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || format!("{text:?} is not a version (MAJOR.MINOR.PATCH)");
        let mut parts = text.trim().split('.');
        let mut next = || -> Result<u64, String> {
            let part = parts.next().ok_or_else(bad)?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) || part.len() > 9 {
                return Err(bad());
            }
            part.parse().map_err(|_| bad())
        };
        let version = Self::new(next()?, next()?, next()?);
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(version)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        text.parse().unwrap()
    }

    #[test]
    fn versions_compare_as_numbers_not_text() {
        assert!(v("0.1.10") > v("0.1.9"));
        assert!(v("0.2.0") > v("0.1.99"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("0.1.8"), v(" 0.1.8 "));
        assert_eq!(v("0.1.8").to_string(), "0.1.8");
    }

    #[test]
    fn anything_but_three_plain_numbers_is_refused() {
        for bad in [
            "",
            "0.1",
            "0.1.8.1",
            "0.1.x",
            "v0.1.8",
            "0.1.8-beta",
            "0..8",
            "-1.0.0",
            "+1.0.0",
            "1.0.0 2",
            "9999999999.0.0",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad:?}");
        }
    }
}
