//! The model providers Pi terminals are given, as Settings shows them.
//!
//! Pi is not tied to one vendor: it talks to whichever providers it has
//! credentials for. Connecto keeps those credentials (in its secret store) and
//! hands them to every Pi terminal at launch, and tells Pi where a local LM
//! Studio server is. Which model a terminal uses is then chosen inside Pi
//! (`/model`), from every model those providers offer.

/// A hosted provider Pi reaches with an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PiProvider {
    OpenRouter,
    Together,
}

impl PiProvider {
    pub const ALL: [Self; 2] = [Self::OpenRouter, Self::Together];

    /// The name people know it by.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::Together => "Together AI",
        }
    }

    /// Pi's own id for the provider, as in `--model openrouter/<model>`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::Together => "together",
        }
    }

    /// The variable Pi reads this provider's key from.
    #[must_use]
    pub const fn env_var(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Together => "TOGETHER_API_KEY",
        }
    }

    /// Where a key is made, for the hint under the field.
    #[must_use]
    pub const fn keys_page(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter.ai/keys",
            Self::Together => "api.together.ai/settings/api-keys",
        }
    }
}

/// Where LM Studio's local server listens unless it was told otherwise.
pub const DEFAULT_LMSTUDIO_URL: &str = "http://localhost:1234";

/// The longest server address or key that is taken.
const MAX_FIELD: usize = 512;

/// Whether the local LM Studio server answers at the address set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PiLocalState {
    /// No address is set: the local server is not used.
    Off,
    /// Being asked now.
    #[default]
    Checking,
    /// It answers, with this many chat models.
    Working { models: usize },
    /// Nothing answers there, or what answers is not LM Studio's server.
    NotAnswering,
}

/// What Settings shows about Pi's providers. Never a key itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiProvidersView {
    /// The providers that have a key saved.
    pub keys_set: Vec<PiProvider>,
    /// The local LM Studio server. Empty when it is not used.
    pub lmstudio_url: String,
    /// Whether that server answers.
    pub lmstudio: PiLocalState,
    /// The secret store could not be read: saved keys are unknown.
    pub keys_unreadable: bool,
    /// Every model Pi can use now, as `provider/model`, as Pi itself lists
    /// them with these providers. Empty until it has been asked.
    pub models: Vec<String>,
    /// The list is being asked for.
    pub models_loading: bool,
    /// The model a new Pi terminal starts on when none is chosen for it.
    /// `None` leaves the choice to Pi.
    pub default_model: Option<String>,
    /// For a project: it uses providers of its own instead of the app-wide
    /// ones. Always true app-wide.
    pub own: bool,
}

impl Default for PiProvidersView {
    fn default() -> Self {
        Self {
            keys_set: Vec::new(),
            lmstudio_url: DEFAULT_LMSTUDIO_URL.to_owned(),
            lmstudio: PiLocalState::Checking,
            keys_unreadable: false,
            models: Vec::new(),
            models_loading: false,
            default_model: None,
            own: false,
        }
    }
}

impl PiProvidersView {
    #[must_use]
    pub fn has_key(&self, provider: PiProvider) -> bool {
        self.keys_set.contains(&provider)
    }

    /// Whether Pi has a model to talk to: a key for a hosted provider, or a
    /// local server that answers with at least one chat model. Without one a
    /// Pi terminal can only say that it has no model, so Pi is not offered.
    #[must_use]
    pub fn usable(&self) -> bool {
        !self.keys_set.is_empty()
            || matches!(self.lmstudio, PiLocalState::Working { models } if models > 0)
    }

    /// Whether it is too early to say (the local server is still being asked
    /// and there is no key).
    #[must_use]
    pub fn undecided(&self) -> bool {
        self.keys_set.is_empty() && self.lmstudio == PiLocalState::Checking
    }
}

/// A local server address as typed: empty means "not used"; otherwise an
/// `http://` or `https://` address, without a path's trailing `/` or `/v1`.
///
/// # Errors
/// Says what is wrong with the address, for the person who typed it.
pub fn clean_lmstudio_url(typed: &str) -> Result<String, String> {
    let url = typed.trim();
    if url.is_empty() {
        return Ok(String::new());
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or("The address starts with http:// or https://, such as http://localhost:1234.")?;
    let plain = url.len() <= MAX_FIELD
        && !rest.is_empty()
        && !rest.starts_with('/')
        && url
            .chars()
            .all(|ch| ch.is_ascii_graphic() && !matches!(ch, '"' | '\'' | '\\' | '<' | '>'));
    if !plain {
        return Err("That is not a server address. An example: http://localhost:1234.".to_owned());
    }
    let url = url.trim_end_matches('/');
    Ok(url.strip_suffix("/v1").unwrap_or(url).to_owned())
}

/// The models whose name holds every word of `typed`, in any case, at most
/// `limit` of them. Nothing typed gives the first ones.
#[must_use]
pub fn matching_models<'a>(models: &'a [String], typed: &str, limit: usize) -> Vec<&'a str> {
    let words: Vec<String> = typed.split_whitespace().map(str::to_lowercase).collect();
    models
        .iter()
        .filter(|model| {
            let name = model.to_lowercase();
            words.iter().all(|word| name.contains(word))
        })
        .take(limit)
        .map(String::as_str)
        .collect()
}

/// A key as typed: trimmed, on one line.
///
/// # Errors
/// Says what is wrong with the key, never the key.
pub fn clean_api_key(typed: &str) -> Result<String, String> {
    let key = typed.trim();
    if key.is_empty() {
        return Err("Paste the key first.".to_owned());
    }
    if key.len() > MAX_FIELD || !key.chars().all(|ch| ch.is_ascii_graphic()) {
        return Err(
            "That does not look like an API key: it is one word, with no spaces.".to_owned(),
        );
    }
    Ok(key.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_server_address_is_cleaned_and_odd_ones_are_refused() {
        assert_eq!(clean_lmstudio_url("  ").as_deref(), Ok(""));
        for (typed, clean) in [
            ("http://localhost:1235", "http://localhost:1235"),
            (" http://localhost:1234/ ", "http://localhost:1234"),
            ("http://127.0.0.1:1234/v1", "http://127.0.0.1:1234"),
            (
                "https://studio.example.com/v1/",
                "https://studio.example.com",
            ),
        ] {
            assert_eq!(clean_lmstudio_url(typed).as_deref(), Ok(clean), "{typed}");
        }
        for bad in [
            "localhost:1234",
            "ftp://localhost",
            "http://",
            "http:///x",
            "http://local host:1234",
            "http://a\"b",
        ] {
            assert!(clean_lmstudio_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn pi_is_usable_with_a_key_or_a_local_server_that_has_a_model() {
        let mut view = PiProvidersView::default();
        assert!(!view.usable() && view.undecided());
        view.lmstudio = PiLocalState::NotAnswering;
        assert!(!view.usable() && !view.undecided());
        view.lmstudio = PiLocalState::Working { models: 0 };
        assert!(!view.usable());
        view.lmstudio = PiLocalState::Working { models: 2 };
        assert!(view.usable());
        view.lmstudio = PiLocalState::Off;
        view.keys_set.push(PiProvider::Together);
        assert!(view.usable());
    }

    #[test]
    fn models_are_found_by_any_words_of_their_name() {
        let models: Vec<String> = [
            "lmstudio/google/gemma-4-e2b",
            "openrouter/google/gemini-3-pro",
            "openrouter/anthropic/claude-opus-5.5",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(matching_models(&models, "", 2).len(), 2);
        assert_eq!(
            matching_models(&models, "Google router", 9),
            ["openrouter/google/gemini-3-pro"]
        );
        assert!(matching_models(&models, "llama", 9).is_empty());
    }

    #[test]
    fn a_key_is_one_word_and_an_error_never_repeats_it() {
        assert_eq!(
            clean_api_key(" sk-or-v1-abc \n").as_deref(),
            Ok("sk-or-v1-abc")
        );
        assert!(clean_api_key("").is_err());
        let refused = clean_api_key("two words").unwrap_err();
        assert!(!refused.contains("two words"));
    }

    #[test]
    fn each_provider_names_the_variable_pi_reads() {
        assert_eq!(PiProvider::OpenRouter.env_var(), "OPENROUTER_API_KEY");
        assert_eq!(PiProvider::Together.env_var(), "TOGETHER_API_KEY");
        assert_eq!(PiProvider::Together.id(), "together");
    }
}
