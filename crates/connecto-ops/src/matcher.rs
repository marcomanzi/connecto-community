//! The hook's free first pass (spec §6.1): which operations a prompt may be
//! asking for, from the words of their phrases. No model, no network.

/// A prompt longer than this, in words, is never matched.
pub const MAX_PROMPT_WORDS: usize = 60;
/// A score from here makes a candidate (Jev is asked about it).
pub const CANDIDATE: f64 = 0.6;
/// Without Jev, a score from here is suggested.
pub const SUGGEST_ALONE: f64 = 0.85;
/// The longest prompt, in words, the hook may run an operation in place of.
pub const INTERCEPT_MAX_WORDS: usize = 12;
/// Letters two longer words must share to count as the same word
/// (`deploy` / `deploying`).
const STEM: usize = 5;

/// Words that say nothing about which operation is meant.
const FILLER: [&str; 34] = [
    "the", "a", "an", "to", "and", "of", "on", "in", "for", "please", "it", "my", "me", "our",
    "this", "that", "can", "you", "now", "just", "pls", "il", "la", "lo", "gli", "le", "di", "da",
    "per", "un", "una", "fai", "puoi", "ora",
];

/// The prompt's words, lowercased, without punctuation and filler.
#[must_use]
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !FILLER.contains(&w.as_str()))
        .collect()
}

/// How many words a prompt has, filler included.
#[must_use]
pub fn word_count(text: &str) -> usize {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .count()
}

fn same_word(a: &str, b: &str) -> bool {
    let (na, nb) = (a.chars().count(), b.chars().count());
    if na >= STEM && nb >= STEM {
        a.chars().take(STEM).eq(b.chars().take(STEM))
    } else {
        a == b
    }
}

/// The share of the phrase's words found in the prompt, 0 to 1.
#[must_use]
pub fn phrase_score(prompt: &[String], phrase: &str) -> f64 {
    let wanted = words(phrase);
    if wanted.is_empty() {
        return 0.0;
    }
    let found = wanted
        .iter()
        .filter(|w| prompt.iter().any(|p| same_word(p, w)))
        .count();
    found as f64 / wanted.len() as f64
}

/// One operation that may be meant.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub slug: String,
    pub score: f64,
}

/// The operations whose best phrase scores at least [`CANDIDATE`], best
/// first. `ops` is each operation's slug and phrases.
#[must_use]
pub fn candidates(prompt: &str, ops: &[(String, Vec<String>)]) -> Vec<Candidate> {
    if word_count(prompt) > MAX_PROMPT_WORDS {
        return Vec::new();
    }
    let prompt = words(prompt);
    if prompt.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Candidate> = ops
        .iter()
        .filter_map(|(slug, phrases)| {
            let best = phrases
                .iter()
                .map(|p| phrase_score(&prompt, p))
                .fold(0.0_f64, f64::max);
            (best >= CANDIDATE).then(|| Candidate {
                slug: slug.clone(),
                score: best,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ops() -> Vec<(String, Vec<String>)> {
        vec![
            (
                "deploy".into(),
                vec!["deploy".into(), "ship it to production".into()],
            ),
            (
                "tests".into(),
                vec!["run the tests".into(), "lancia i test".into()],
            ),
            ("server".into(), vec!["start the dev server".into()]),
        ]
    }

    #[test]
    fn plain_requests_match() {
        let found = candidates("deploy please", &ops());
        assert_eq!(found[0].slug, "deploy");
        assert!((found[0].score - 1.0).abs() < 1e-9);
        assert_eq!(candidates("Run the tests!", &ops())[0].slug, "tests");
        assert_eq!(candidates("lancia i test", &ops())[0].slug, "tests");
        assert_eq!(candidates("start the server", &ops())[0].slug, "server");
        assert_eq!(candidates("we are deploying now", &ops())[0].slug, "deploy");
    }

    #[test]
    fn unrelated_or_long_prompts_do_not() {
        assert!(candidates("why is the login page slow?", &ops()).is_empty());
        assert!(candidates("", &ops()).is_empty());
        let long = format!("deploy {}", "word ".repeat(MAX_PROMPT_WORDS));
        assert!(candidates(&long, &ops()).is_empty());
    }

    #[test]
    fn short_words_must_be_equal() {
        // `test` and `tests` are short words: not the same.
        assert!((phrase_score(&words("test"), "tests") - 0.0).abs() < 1e-9);
        assert!(same_word("deployment", "deploying"));
        assert!(!same_word("depl", "deploy"));
    }

    #[test]
    fn counts_include_filler() {
        assert_eq!(word_count("please run the tests"), 4);
        assert_eq!(words("please run the tests"), ["run", "tests"]);
    }
}
