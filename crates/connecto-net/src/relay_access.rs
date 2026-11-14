//! Which access token a relay gets.
//!
//! The relay asks for a shared token. A token the user stored always wins.
//! Otherwise the token built into the app (`CONNECTO_RELAY_TOKEN`, read at
//! compile time) is used, and only for the default relay: it is never sent to
//! an address the user typed that names a different host.
//!
//! Nothing here prints a token. [`RelayAccess`] has a `Debug` that hides it.

use std::str::FromStr;

use connecto_secrets::STORE_NAME;
use iroh::RelayUrl;

use crate::manager::DEFAULT_RELAY_URL;
use crate::secrets::{SecretStore, relay_token};

/// The token built into this binary. Unset or empty at build time means none:
/// dev builds and tests have no built-in token.
pub const BUILT_IN_TOKEN: Option<&str> = built_in_from(option_env!("CONNECTO_RELAY_TOKEN"));

const fn built_in_from(value: Option<&'static str>) -> Option<&'static str> {
    match value {
        Some(text) if !text.is_empty() => Some(text),
        _ => None,
    }
}

/// The message when this machine has no token for the default relay.
pub const NO_TOKEN: &str =
    "This machine has no access token for the relay. Add it in Settings, Remote Access.";

/// The token an endpoint for one relay is given.
#[derive(Clone, PartialEq, Eq)]
pub enum RelayAccess {
    /// The token the user stored.
    Stored(String),
    /// The token built into the app, for the default relay.
    BuiltIn(String),
    /// No token: a custom relay that needs none, no relay, or a build without
    /// a built-in token.
    None,
}

impl RelayAccess {
    /// The token to hand the endpoint, if there is one.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        match self {
            Self::Stored(token) | Self::BuiltIn(token) => Some(token.as_str()),
            Self::None => None,
        }
    }
}

impl std::fmt::Debug for RelayAccess {
    /// The token is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stored(_) => f.write_str("Stored(<set>)"),
            Self::BuiltIn(_) => f.write_str("BuiltIn(<set>)"),
            Self::None => f.write_str("None"),
        }
    }
}

/// Whether `url` names the default relay: the same scheme, host and port. The
/// host ignores case, and a trailing slash or a path does not matter.
#[must_use]
pub fn is_default_relay(url: &str) -> bool {
    same_relay(url, DEFAULT_RELAY_URL)
}

fn same_relay(a: &str, b: &str) -> bool {
    let (Some(a), Some(b)) = (parse(a), parse(b)) else {
        return false;
    };
    let host = |u: &RelayUrl| u.host_str().map(str::to_ascii_lowercase);
    a.scheme() == b.scheme()
        && host(&a).is_some()
        && host(&a) == host(&b)
        && a.port_or_known_default() == b.port_or_known_default()
}

fn parse(text: &str) -> Option<RelayUrl> {
    RelayUrl::from_str(text.trim()).ok()
}

/// The token for a relay, given what is stored and what is built in. Pure, so
/// a test can pass any built-in value.
#[must_use]
pub fn resolve(
    stored: Option<String>,
    relay_url: Option<&str>,
    built_in: Option<&str>,
) -> RelayAccess {
    if let Some(token) = stored.filter(|t| !t.is_empty()) {
        return RelayAccess::Stored(token);
    }
    match (relay_url, built_in.filter(|b| !b.is_empty())) {
        (Some(url), Some(built)) if is_default_relay(url) => RelayAccess::BuiltIn(built.to_owned()),
        _ => RelayAccess::None,
    }
}

/// The token an endpoint for `relay_url` is given, from the store and the
/// build. With no relay, nothing is read or needed.
///
/// # Errors
/// When the store cannot be read. That is not the same as no token, and it is
/// never taken for one.
pub fn for_relay(store: &dyn SecretStore, relay_url: Option<&str>) -> Result<RelayAccess, String> {
    if relay_url.is_none() {
        return Ok(RelayAccess::None);
    }
    Ok(resolve(relay_token(store)?, relay_url, BUILT_IN_TOKEN))
}

/// Whether to give up at once: the default relay needs a token, so with none
/// there is no point waiting for it to come online.
#[must_use]
pub fn lacks_required_token(relay_url: Option<&str>, access: &RelayAccess) -> bool {
    relay_url.is_some_and(is_default_relay) && access.token().is_none()
}

/// The message when the stored token cannot be read.
#[must_use]
pub fn unreadable(why: &str) -> String {
    format!("Could not read the relay access token from {STORE_NAME} ({why}).")
}

/// The message when the relay refused this endpoint or could not be reached.
/// `problem` is the relay's own reason, if it gave one.
#[must_use]
pub fn unreachable(problem: Option<String>, access: &RelayAccess, relay_url: &str) -> String {
    let why = problem.map_or_else(String::new, |why| format!(" ({why})"));
    let mut text = format!(
        "Could not reach the relay, so the other machine cannot find this one{why}. Check Settings, Relay."
    );
    if matches!(access, RelayAccess::Stored(_)) && is_default_relay(relay_url) {
        text.push_str(" The access token may be wrong: check it in Settings, Remote Access.");
    }
    text
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::secrets::{MemorySecrets, RELAY_TOKEN};

    /// A store that cannot be read, as a denied Keychain is.
    struct Denied;

    impl SecretStore for Denied {
        fn get(&self, _name: &str) -> Result<Option<Vec<u8>>, String> {
            Err("the Keychain denied access".to_owned())
        }
        fn set(&self, _name: &str, _value: &[u8]) -> Result<(), String> {
            Err("the Keychain denied access".to_owned())
        }
        fn clear(&self, _name: &str) -> Result<(), String> {
            Err("the Keychain denied access".to_owned())
        }
    }

    const DEFAULT: &str = "https://relay.connecto.studio";
    const CUSTOM: &str = "https://relay.example:8443";
    const BUILT: &str = "built-in-token-for-tests";

    #[test]
    fn the_default_relay_is_recognised_however_it_is_written() {
        for same in [
            "https://relay.connecto.studio",
            "https://relay.connecto.studio/",
            "https://RELAY.Connecto.Studio/",
            "https://relay.connecto.studio:443",
            "https://relay.connecto.studio:443/",
            "  https://relay.connecto.studio  ",
        ] {
            assert!(is_default_relay(same), "{same} is the default relay");
        }
    }

    #[test]
    fn any_other_host_scheme_or_port_is_not_the_default_relay() {
        for other in [
            "http://relay.connecto.studio",
            "https://relay.connecto.studio:8443",
            "https://relay.connecto.studio.evil.example",
            "https://evil.example/relay.connecto.studio",
            "https://connecto.studio",
            "https://xrelay.connecto.studio",
            "https://relay.example",
            "not a url",
            "",
        ] {
            assert!(!is_default_relay(other), "{other} is not the default relay");
        }
    }

    #[test]
    fn a_stored_token_wins_on_the_default_relay_and_on_any_other() {
        let stored = Some("mine".to_owned());
        assert_eq!(
            resolve(stored.clone(), Some(DEFAULT), Some(BUILT)),
            RelayAccess::Stored("mine".into())
        );
        assert_eq!(
            resolve(stored, Some(CUSTOM), Some(BUILT)),
            RelayAccess::Stored("mine".into())
        );
    }

    #[test]
    fn the_built_in_token_is_used_only_for_the_default_relay_without_a_stored_one() {
        assert_eq!(
            resolve(None, Some(DEFAULT), Some(BUILT)),
            RelayAccess::BuiltIn(BUILT.into())
        );
        assert_eq!(
            resolve(None, Some("https://RELAY.connecto.studio/"), Some(BUILT)),
            RelayAccess::BuiltIn(BUILT.into())
        );
    }

    #[test]
    fn the_built_in_token_never_goes_to_another_relay_or_to_no_relay() {
        assert_eq!(resolve(None, Some(CUSTOM), Some(BUILT)), RelayAccess::None);
        assert_eq!(
            resolve(None, Some("http://relay.connecto.studio"), Some(BUILT)),
            RelayAccess::None
        );
        assert_eq!(
            resolve(
                None,
                Some("https://relay.connecto.studio.evil.example"),
                Some(BUILT)
            ),
            RelayAccess::None
        );
        assert_eq!(resolve(None, None, Some(BUILT)), RelayAccess::None);
    }

    #[test]
    fn no_built_in_token_means_none_on_the_default_relay() {
        assert_eq!(resolve(None, Some(DEFAULT), None), RelayAccess::None);
        assert_eq!(resolve(None, Some(DEFAULT), Some("")), RelayAccess::None);
    }

    #[test]
    fn an_empty_stored_token_is_no_token() {
        assert_eq!(
            resolve(Some(String::new()), Some(DEFAULT), Some(BUILT)),
            RelayAccess::BuiltIn(BUILT.into())
        );
        assert_eq!(
            resolve(Some(String::new()), Some(CUSTOM), None),
            RelayAccess::None
        );
    }

    #[test]
    fn the_debug_text_never_shows_a_token() {
        let stored = RelayAccess::Stored("s3cret-value".into());
        let built = RelayAccess::BuiltIn("s3cret-value".into());
        assert!(!format!("{stored:?}").contains("s3cret-value"));
        assert!(!format!("{built:?}").contains("s3cret-value"));
        assert_eq!(format!("{:?}", RelayAccess::None), "None");
    }

    #[test]
    fn the_build_carries_the_token_only_when_the_environment_did() {
        let set = option_env!("CONNECTO_RELAY_TOKEN").is_some_and(|t| !t.is_empty());
        assert_eq!(BUILT_IN_TOKEN.is_some(), set);
        assert_eq!(built_in_from(Some("")), None);
        assert_eq!(built_in_from(Some("x")), Some("x"));
        assert_eq!(built_in_from(None), None);
    }

    #[test]
    fn for_relay_uses_the_build_value_on_the_default_relay_only() {
        let store = MemorySecrets::default();
        let expected = match BUILT_IN_TOKEN {
            Some(built) => RelayAccess::BuiltIn(built.to_owned()),
            None => RelayAccess::None,
        };
        assert_eq!(for_relay(&store, Some(DEFAULT)).unwrap(), expected);
        assert_eq!(for_relay(&store, Some(CUSTOM)).unwrap(), RelayAccess::None);
        assert_eq!(for_relay(&store, None).unwrap(), RelayAccess::None);
    }

    #[test]
    fn for_relay_prefers_the_stored_token() {
        let store = MemorySecrets::default();
        store.set(RELAY_TOKEN, b"mine").unwrap();
        assert_eq!(
            for_relay(&store, Some(DEFAULT)).unwrap(),
            RelayAccess::Stored("mine".into())
        );
        store.clear(RELAY_TOKEN).unwrap();
        assert_ne!(
            for_relay(&store, Some(DEFAULT)).unwrap(),
            RelayAccess::Stored("mine".into())
        );
    }

    #[test]
    fn an_unreadable_store_is_an_error_not_no_token() {
        let err = for_relay(&Denied, Some(DEFAULT)).unwrap_err();
        assert!(err.contains("denied"));
        assert!(for_relay(&Denied, Some(CUSTOM)).is_err());
        // No relay needs nothing from the store.
        assert_eq!(for_relay(&Denied, None).unwrap(), RelayAccess::None);
        assert!(unreadable(&err).contains("Could not read the relay access token"));
        assert!(unreadable(&err).contains("the Keychain denied access"));
    }

    #[test]
    fn the_default_relay_with_no_token_fails_at_once_and_a_custom_one_does_not() {
        let none = RelayAccess::None;
        assert!(lacks_required_token(Some(DEFAULT), &none));
        assert!(!lacks_required_token(
            Some(DEFAULT),
            &RelayAccess::BuiltIn(BUILT.into())
        ));
        assert!(!lacks_required_token(
            Some(DEFAULT),
            &RelayAccess::Stored("t".into())
        ));
        assert!(!lacks_required_token(Some(CUSTOM), &none));
        assert!(!lacks_required_token(None, &none));
    }

    #[test]
    fn the_unreachable_message_gives_the_relays_reason_and_hints_at_a_wrong_token() {
        let text = unreachable(
            Some("401 unauthorized".into()),
            &RelayAccess::Stored("t".into()),
            DEFAULT,
        );
        assert!(text.starts_with("Could not reach the relay"));
        assert!(text.contains("(401 unauthorized)"));
        assert!(text.contains("The access token may be wrong"));
        assert!(text.contains("Settings, Remote Access"));
    }

    #[test]
    fn the_unreachable_message_has_no_token_hint_for_other_cases() {
        let plain = unreachable(None, &RelayAccess::Stored("t".into()), CUSTOM);
        assert_eq!(
            plain,
            "Could not reach the relay, so the other machine cannot find this one. Check Settings, Relay."
        );
        let built = unreachable(
            Some("refused".into()),
            &RelayAccess::BuiltIn(BUILT.into()),
            DEFAULT,
        );
        assert!(built.contains("(refused)"));
        assert!(!built.contains("access token may be wrong"));
        assert!(!unreachable(None, &RelayAccess::None, DEFAULT).contains("access token"));
    }

    #[test]
    fn the_no_token_message_says_where_to_add_it() {
        assert!(NO_TOKEN.contains("no access token for the relay"));
        assert!(NO_TOKEN.contains("Settings, Remote Access"));
    }
}
