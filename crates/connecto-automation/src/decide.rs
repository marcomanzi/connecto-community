//! The pure parts of the engine: cleaning a screen, and what an answer means.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use connecto_hook::automation::{Definition, Question};

use crate::jev::Answer;

/// Lines of screen text sent to Jev, counted from the bottom.
pub const SCREEN_LINES: usize = 60;
/// Characters sent, counted from the end.
pub const SCREEN_CHARS: usize = 6000;

/// What Jev's answer means for one automation.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// At or above the threshold: type the keys.
    Fire { confidence: f64 },
    /// The match is the top answer but below the threshold: ask the user.
    Ask { confidence: f64 },
    /// The answer is something else. Carries what Jev answered.
    NoMatch {
        answer: Option<String>,
        confidence: Option<f64>,
    },
}

impl Decision {
    #[must_use]
    pub const fn confidence(&self) -> Option<f64> {
        match self {
            Self::Fire { confidence } | Self::Ask { confidence } => Some(*confidence),
            Self::NoMatch { confidence, .. } => *confidence,
        }
    }
}

/// Apply the match/threshold rules.
///
/// - choice: the answer equals `match`: confidence >= threshold fires; lower
///   asks (it is still the top answer).
/// - noul: the answer is the probability of yes: >= threshold fires; from 0.5
///   (yes is the likelier answer) up to the threshold asks; lower is no match.
#[must_use]
pub fn decide(def: &Definition, answer: Option<&Answer>) -> Decision {
    let no_match =
        |answer: Option<String>, confidence: Option<f64>| Decision::NoMatch { answer, confidence };
    match (&def.question, answer) {
        (Question::Choice { .. }, Some(Answer::Choice { choice, confidence })) => {
            if *choice != def.matches {
                no_match(Some(choice.clone()), Some(*confidence))
            } else if *confidence >= def.threshold {
                Decision::Fire {
                    confidence: *confidence,
                }
            } else {
                Decision::Ask {
                    confidence: *confidence,
                }
            }
        }
        (Question::Noul { .. }, Some(Answer::Noul(p))) => {
            if *p >= def.threshold {
                Decision::Fire { confidence: *p }
            } else if *p >= 0.5 {
                Decision::Ask { confidence: *p }
            } else {
                no_match(Some("false".to_owned()), Some(1.0 - *p))
            }
        }
        _ => no_match(None, None),
    }
}

fn border_only(line: &str) -> bool {
    line.chars().all(|c| {
        c.is_whitespace()
            || ('\u{2500}'..='\u{257f}').contains(&c)
            || matches!(c, '-' | '=' | '_' | '|' | '+')
    })
}

/// The screen as Jev should see it: no blank lines, no border-only lines, the
/// side borders of boxes removed, only the last [`SCREEN_LINES`] lines. The
/// selection marker and option numbers stay, because they are what a prompt is
/// made of.
#[must_use]
pub fn clean_screen(raw: &str) -> String {
    let lines: Vec<String> = raw
        .lines()
        .map(|line| {
            let line = line.trim();
            let line = line
                .trim_start_matches(['\u{2502}', '\u{2503}', '\u{2551}', '|'])
                .trim_start();
            line.trim_end_matches(['\u{2502}', '\u{2503}', '\u{2551}', '|'])
                .trim_end()
                .to_owned()
        })
        .filter(|line| !line.is_empty() && !border_only(line))
        .collect();
    let tail = &lines[lines.len().saturating_sub(SCREEN_LINES)..];
    let text = tail.join("\n");
    let skip = text.chars().count().saturating_sub(SCREEN_CHARS);
    text.chars().skip(skip).collect()
}

#[must_use]
pub fn hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn choice(threshold: f64) -> Definition {
        Definition::from_value(&json!({
            "version": 1, "name": "n", "agent": "claude", "model": "m",
            "question": {"type": "choice", "instructions": "i", "criteria": {"yes": "y", "other": "o"}},
            "match": "yes", "keys": ["enter"], "threshold": threshold,
            "trigger": ["some phrase"]
        }))
        .unwrap()
    }

    fn noul() -> Definition {
        Definition::from_value(&json!({
            "version": 1, "name": "n", "agent": "claude", "model": "m",
            "question": {"type": "noul", "instructions": "i"},
            "match": "true", "keys": ["enter"], "trigger": ["some phrase"]
        }))
        .unwrap()
    }

    fn pick(choice: &str, confidence: f64) -> Answer {
        Answer::Choice {
            choice: choice.to_owned(),
            confidence,
        }
    }

    #[test]
    fn a_choice_fires_at_the_threshold_asks_below_it_and_ignores_other_answers() {
        let def = choice(0.8);
        assert_eq!(
            decide(&def, Some(&pick("yes", 0.8))),
            Decision::Fire { confidence: 0.8 }
        );
        assert_eq!(
            decide(&def, Some(&pick("yes", 0.79))),
            Decision::Ask { confidence: 0.79 }
        );
        assert!(matches!(
            decide(&def, Some(&pick("other", 0.99))),
            Decision::NoMatch { .. }
        ));
        assert!(matches!(
            decide(&def, None),
            Decision::NoMatch { answer: None, .. }
        ));
        assert!(matches!(
            decide(&def, Some(&Answer::Noul(0.9))),
            Decision::NoMatch { .. }
        ));
    }

    #[test]
    fn a_noul_uses_the_probability_of_yes() {
        let def = noul();
        assert!(matches!(
            decide(&def, Some(&Answer::Noul(0.95))),
            Decision::Fire { .. }
        ));
        assert!(matches!(
            decide(&def, Some(&Answer::Noul(0.6))),
            Decision::Ask { .. }
        ));
        assert!(matches!(
            decide(&def, Some(&Answer::Noul(0.3))),
            Decision::NoMatch { .. }
        ));
    }

    #[test]
    fn screens_lose_borders_and_blank_lines_but_keep_the_prompt() {
        let raw = "\n  ╭────────╮\n  │ Do you want to proceed? │\n  │ ❯ 1. Yes │\n  │   2. No │\n  ╰────────╯\n\n";
        assert_eq!(
            clean_screen(raw),
            "Do you want to proceed?\n❯ 1. Yes\n2. No"
        );
        let long: String = (0..200).map(|n| format!("line {n}\n")).collect();
        let cleaned = clean_screen(&long);
        assert_eq!(cleaned.lines().count(), SCREEN_LINES);
        assert!(cleaned.ends_with("line 199"));
        assert_eq!(hash("a"), hash("a"));
        assert_ne!(hash("a"), hash("b"));
    }
}
