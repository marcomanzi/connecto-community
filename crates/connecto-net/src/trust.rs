//! The client's list of hosts it knows: a unique, renameable display name for
//! each, the host's pinned endpoint id, and what is needed to reach it again.
//!
//! The name `localhost` always means the machine the app runs on and cannot be
//! saved. The id is the pin, as in `known_hosts`: dialing a saved name dials
//! its id, and the transport authenticates that id, so a different machine
//! answering is refused rather than trusted.

use std::io::Write;
use std::path::Path;

use crate::transport::EndpointId;
use crate::wire::LOCALHOST;

/// The longest display name, in characters.
pub const MAX_HOST_NAME_CHARS: usize = 40;

/// One saved host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHost {
    pub name: String,
    /// The pinned identity.
    pub id: EndpointId,
    /// The relay URL the host was reached through, if any.
    pub relay: Option<String>,
    /// Direct addresses seen last time, as hints. They never replace the pin.
    pub addrs: Vec<String>,
    /// `YYYY-MM-DD` when the pairing was made; empty when not known.
    pub paired_at: String,
}

/// What a name or id found in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// The name is saved with this id.
    Match,
    /// The name is not saved.
    Unknown,
    /// The name is saved with a different id. Refuse.
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostsError {
    #[error("\"{LOCALHOST}\" is the machine you are on and cannot be used")]
    Reserved,
    #[error("a host needs a name")]
    Empty,
    #[error("a host name is at most {MAX_HOST_NAME_CHARS} characters")]
    TooLong,
    #[error("a host name cannot hold control characters or tabs")]
    BadChar,
    #[error("there is already a host called \"{0}\"")]
    NameTaken(String),
    #[error("that machine is already saved as \"{0}\"")]
    IdTaken(String),
    #[error("no host called \"{0}\"")]
    Unknown(String),
    #[error("a machine's {0} cannot be saved: it has a character the list cannot hold")]
    BadField(&'static str),
}

/// Whether a stored value can be one tab-separated field of the list: no
/// control characters, no tab, no separator of its own.
fn plain(text: &str) -> bool {
    text.chars().all(|c| !c.is_control() && c != '\t')
}

/// Every value of a host, checked before it is kept, so the file the list is
/// written to always reads back. The relay and the addresses are the parts a
/// peer sends, so they are checked strictly.
///
/// # Errors
/// Naming the first field that cannot be stored.
pub fn check_fields(host: &KnownHost) -> Result<(), HostsError> {
    if let Some(relay) = &host.relay {
        let usable = relay != "-"
            && relay.len() <= 512
            && (relay.starts_with("https://") || relay.starts_with("http://"))
            && relay.bytes().all(|b| b.is_ascii_graphic());
        if !usable {
            return Err(HostsError::BadField("relay"));
        }
    }
    for addr in &host.addrs {
        if addr.is_empty() || addr.contains(',') || !addr.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(HostsError::BadField("address"));
        }
    }
    if !plain(&host.paired_at) || host.paired_at.len() > 32 {
        return Err(HostsError::BadField("paired date"));
    }
    Ok(())
}

/// Trim a name and check it. Names compare without regard to case, so
/// `Build` and `build` cannot both exist.
///
/// # Errors
/// When the name is empty, too long, reserved or holds control characters.
pub fn clean_name(raw: &str) -> Result<String, HostsError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(HostsError::Empty);
    }
    if name.chars().count() > MAX_HOST_NAME_CHARS {
        return Err(HostsError::TooLong);
    }
    if name.chars().any(char::is_control) {
        return Err(HostsError::BadChar);
    }
    if name.eq_ignore_ascii_case(LOCALHOST) {
        return Err(HostsError::Reserved);
    }
    Ok(name.to_owned())
}

/// The saved hosts, in the order the user added them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownHosts {
    hosts: Vec<KnownHost>,
}

/// The endpoint id as 64 lowercase hex digits.
pub fn hex(id: &EndpointId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// The inverse of [`hex`].
pub fn unhex(s: &str) -> Option<EndpointId> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut id = [0_u8; 32];
    for (i, slot) in id.iter_mut().enumerate() {
        *slot = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(id)
}

impl KnownHosts {
    #[must_use]
    pub fn list(&self) -> &[KnownHost] {
        &self.hosts
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&KnownHost> {
        self.hosts
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name.trim()))
    }

    #[must_use]
    pub fn by_id(&self, id: &EndpointId) -> Option<&KnownHost> {
        self.hosts.iter().find(|h| &h.id == id)
    }

    /// Compare what the user dialed with what is pinned.
    #[must_use]
    pub fn check(&self, name: &str, id: &EndpointId) -> Pin {
        match self.get(name) {
            None => Pin::Unknown,
            Some(host) if &host.id == id => Pin::Match,
            Some(_) => Pin::Mismatch,
        }
    }

    /// Save a new host.
    ///
    /// # Errors
    /// When the name is unusable or taken, or the machine is saved already.
    pub fn add(&mut self, mut host: KnownHost) -> Result<(), HostsError> {
        check_fields(&host)?;
        host.name = clean_name(&host.name)?;
        if self.get(&host.name).is_some() {
            return Err(HostsError::NameTaken(host.name));
        }
        if let Some(other) = self.by_id(&host.id) {
            return Err(HostsError::IdTaken(other.name.clone()));
        }
        self.hosts.push(host);
        Ok(())
    }

    /// Give a host a new name. The pin and the address stay.
    ///
    /// # Errors
    /// When `from` is unknown or `to` is unusable or taken by another host.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), HostsError> {
        let to = clean_name(to)?;
        let Some(index) = self
            .hosts
            .iter()
            .position(|h| h.name.eq_ignore_ascii_case(from.trim()))
        else {
            return Err(HostsError::Unknown(from.to_owned()));
        };
        if self
            .hosts
            .iter()
            .enumerate()
            .any(|(i, h)| i != index && h.name.eq_ignore_ascii_case(&to))
        {
            return Err(HostsError::NameTaken(to));
        }
        if let Some(host) = self.hosts.get_mut(index) {
            host.name = to;
        }
        Ok(())
    }

    /// Forget a host.
    ///
    /// # Errors
    /// When the name is unknown.
    pub fn remove(&mut self, name: &str) -> Result<KnownHost, HostsError> {
        let index = self
            .hosts
            .iter()
            .position(|h| h.name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| HostsError::Unknown(name.to_owned()))?;
        Ok(self.hosts.remove(index))
    }

    /// Refresh how to reach a host, keeping its name and pin.
    ///
    /// # Errors
    /// When a value cannot be stored; the host is then left as it was.
    pub fn update_route(
        &mut self,
        name: &str,
        relay: Option<String>,
        addrs: Vec<String>,
    ) -> Result<(), HostsError> {
        let Some(host) = self
            .hosts
            .iter_mut()
            .find(|h| h.name.eq_ignore_ascii_case(name.trim()))
        else {
            return Ok(());
        };
        let candidate = KnownHost {
            relay,
            addrs,
            ..host.clone()
        };
        check_fields(&candidate)?;
        *host = candidate;
        Ok(())
    }

    /// One line per host: `name TAB id TAB relay-or-dash TAB addr,addr`.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::from(
            "# Connecto known hosts: name, endpoint id, relay, addresses, paired date\n",
        );
        for h in &self.hosts {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                h.name,
                hex(&h.id),
                h.relay.as_deref().unwrap_or("-"),
                h.addrs.join(","),
                h.paired_at
            ));
        }
        out
    }

    /// Read the text form. A bad line is an error, not skipped: a pin that is
    /// silently dropped would turn into trust on first use again.
    ///
    /// # Errors
    /// With the line number of the first bad line.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut hosts = Self::default();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let mut f = line.split('\t');
            let (Some(name), Some(id), Some(relay)) = (f.next(), f.next(), f.next()) else {
                return Err(format!("line {}: expected tab-separated fields", n + 1));
            };
            let id = unhex(id).ok_or_else(|| format!("line {}: bad endpoint id", n + 1))?;
            let addrs = f
                .next()
                .unwrap_or("")
                .split(',')
                .filter(|a| !a.is_empty())
                .map(str::to_owned)
                .collect();
            let paired_at = f.next().unwrap_or("").to_owned();
            hosts
                .add(KnownHost {
                    name: name.to_owned(),
                    id,
                    relay: (relay != "-").then(|| relay.to_owned()),
                    addrs,
                    paired_at,
                })
                .map_err(|err| format!("line {}: {err}", n + 1))?;
        }
        Ok(hosts)
    }

    /// Read the file. A missing file is an empty list.
    ///
    /// # Errors
    /// When the file exists and cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Write the file, owner-only, through a temporary file and a rename.
    ///
    /// # Errors
    /// On any I/O failure; the old file is untouched.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_private(path, self.to_text().as_bytes())
    }
}

/// Write `bytes` to `path` with mode 0600, atomically.
///
/// On Windows there is no mode: the file takes the folder's ACL, which for a
/// folder under the user profile is already private to the user.
///
/// # Errors
/// On any I/O failure.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    owner_only(&mut options);
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    // Closed before the rename: Windows cannot replace an open file.
    drop(file);
    std::fs::rename(&tmp, path)
}

/// New files made through `options` are readable by the owner only (0600).
#[cfg(unix)]
fn owner_only(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

/// No mode on Windows: the default ACL of the user's own folders applies.
#[cfg(not(unix))]
fn owner_only(_options: &mut std::fs::OpenOptions) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn host(name: &str, byte: u8) -> KnownHost {
        KnownHost {
            name: name.to_owned(),
            id: [byte; 32],
            relay: Some("https://staging.example/".into()),
            addrs: vec!["10.0.0.2:4000".into()],
            paired_at: "2026-11-12".into(),
        }
    }

    #[test]
    fn localhost_is_reserved_in_any_case() {
        let mut hosts = KnownHosts::default();
        assert_eq!(hosts.add(host("localhost", 1)), Err(HostsError::Reserved));
        assert_eq!(
            hosts.add(host("  LocalHost ", 1)),
            Err(HostsError::Reserved)
        );
    }

    #[test]
    fn names_are_unique_without_regard_to_case_and_so_are_machines() {
        let mut hosts = KnownHosts::default();
        hosts.add(host("Build box", 1)).unwrap();
        assert!(matches!(
            hosts.add(host("build BOX", 2)),
            Err(HostsError::NameTaken(_))
        ));
        assert!(matches!(
            hosts.add(host("other", 1)),
            Err(HostsError::IdTaken(name)) if name == "Build box"
        ));
        assert_eq!(hosts.add(host("", 3)), Err(HostsError::Empty));
        assert_eq!(hosts.add(host("a\tb", 3)), Err(HostsError::BadChar));
        assert_eq!(
            hosts.add(host(&"x".repeat(41), 3)),
            Err(HostsError::TooLong)
        );
    }

    #[test]
    fn rename_keeps_the_pin_and_refuses_taken_or_reserved_names() {
        let mut hosts = KnownHosts::default();
        hosts.add(host("a", 1)).unwrap();
        hosts.add(host("b", 2)).unwrap();
        assert!(matches!(
            hosts.rename("a", "B"),
            Err(HostsError::NameTaken(_))
        ));
        assert_eq!(hosts.rename("a", "localhost"), Err(HostsError::Reserved));
        assert!(matches!(
            hosts.rename("zzz", "c"),
            Err(HostsError::Unknown(_))
        ));
        hosts.rename("a", "Laptop").unwrap();
        assert_eq!(hosts.get("laptop").unwrap().id, [1; 32]);
        // Renaming to its own name in another case is allowed.
        hosts.rename("Laptop", "LAPTOP").unwrap();
        assert!(hosts.get("a").is_none());
    }

    #[test]
    fn check_tells_match_unknown_and_mismatch() {
        let mut hosts = KnownHosts::default();
        hosts.add(host("a", 1)).unwrap();
        assert_eq!(hosts.check("a", &[1; 32]), Pin::Match);
        assert_eq!(hosts.check("a", &[9; 32]), Pin::Mismatch);
        assert_eq!(hosts.check("b", &[1; 32]), Pin::Unknown);
    }

    #[test]
    fn the_file_round_trips_and_is_owner_only() {
        let mut hosts = KnownHosts::default();
        hosts.add(host("Build box", 1)).unwrap();
        hosts
            .add(KnownHost {
                name: "x".into(),
                id: [2; 32],
                relay: None,
                addrs: vec![],
                paired_at: String::new(),
            })
            .unwrap();
        let dir = std::env::temp_dir().join(format!("mg-net-hosts-{}", std::process::id()));
        let path = dir.join("known_hosts");
        hosts.save(&path).unwrap();
        assert_eq!(KnownHosts::load(&path).unwrap(), hosts);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Saving again replaces the file (a rename over an existing file).
        hosts.save(&path).unwrap();
        assert_eq!(KnownHosts::load(&path).unwrap(), hosts);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(KnownHosts::load(&path).unwrap(), KnownHosts::default());
    }

    #[test]
    fn a_bad_line_is_an_error_not_a_dropped_pin() {
        assert!(KnownHosts::parse("a\tnothex\t-\t\n").is_err());
        assert!(KnownHosts::parse("only-a-name\n").is_err());
        let dup = format!("a\t{}\t-\t\na\t{}\t-\t\n", "11".repeat(32), "22".repeat(32));
        assert!(KnownHosts::parse(&dup).is_err());
    }

    #[test]
    fn a_hostile_value_is_refused_and_never_reaches_the_file() {
        let mut hosts = KnownHosts::default();
        let mut evil = host("a", 1);
        evil.relay = Some("https://x.example\tb\t-".into());
        assert_eq!(hosts.add(evil.clone()), Err(HostsError::BadField("relay")));
        evil.relay = Some("-".into());
        assert_eq!(hosts.add(evil.clone()), Err(HostsError::BadField("relay")));
        evil.relay = None;
        evil.addrs = vec!["10.0.0.1:4000\nevil\t1".into()];
        assert_eq!(
            hosts.add(evil.clone()),
            Err(HostsError::BadField("address"))
        );
        evil.addrs = vec!["10.0.0.1:4000,10.0.0.2:1".into()];
        assert_eq!(
            hosts.add(evil.clone()),
            Err(HostsError::BadField("address"))
        );
        evil.addrs = vec![];
        evil.paired_at = "2026\n".into();
        assert_eq!(hosts.add(evil), Err(HostsError::BadField("paired date")));
        assert!(hosts.list().is_empty());
        // A failed route change keeps the old one.
        hosts.add(host("a", 1)).unwrap();
        assert!(
            hosts
                .update_route("a", Some("https://ok\tbad".into()), vec![])
                .is_err()
        );
        assert_eq!(
            hosts.get("a").unwrap().relay.as_deref(),
            Some("https://staging.example/")
        );
        // Whatever is kept reads back the same from the file.
        let text = hosts.to_text();
        assert_eq!(KnownHosts::parse(&text).unwrap(), hosts);
    }

    #[test]
    fn remove_and_update_route_work() {
        let mut hosts = KnownHosts::default();
        hosts.add(host("a", 1)).unwrap();
        hosts
            .update_route("A", None, vec!["1.2.3.4:5".into()])
            .unwrap();
        assert_eq!(hosts.get("a").unwrap().relay, None);
        assert_eq!(hosts.get("a").unwrap().id, [1; 32]);
        assert_eq!(hosts.remove("a").unwrap().name, "a");
        assert!(hosts.remove("a").is_err());
    }
}
