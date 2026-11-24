//! The hook's second pass (spec §6.2): one Jev `choice` question about the
//! local match's candidates, and what its answer allows.

use serde_json::{Map, Value, json};

/// The Jev model asked.
pub const JEV_MODEL: &str = "jev-1.14.0";
/// The question's name in the request.
pub const QUESTION: &str = "operation";
/// The choice that says the prompt asks for none of them.
pub const NONE: &str = "none";
/// Most candidates in one question.
pub const MAX_CANDIDATES: usize = 8;
/// Jev's confidence from which a pick is suggested.
pub const SUGGEST: f64 = 0.6;
/// Jev's confidence from which a pick may run in place of the prompt.
pub const INTERCEPT: f64 = 0.85;

/// What Jev is told about one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub phrases: Vec<String>,
}

/// Each choice name with the slug it stands for.
pub type ChoiceNames = Vec<(String, String)>;

/// The question, and which choice name stands for which slug (choice names
/// are Connecto's own, `op_1`..., never an operation's text).
#[must_use]
pub fn question(candidates: &[Candidate]) -> (Vec<(String, Value)>, ChoiceNames) {
    let mut criteria = Map::new();
    let mut names = Vec::new();
    for (i, c) in candidates.iter().take(MAX_CANDIDATES).enumerate() {
        let choice = format!("op_{}", i + 1);
        criteria.insert(
            choice.clone(),
            json!(format!(
                "The message asks only for this: {} ({}). People ask for it as: {}.",
                c.name,
                c.description,
                c.phrases.join("; ")
            )),
        );
        names.push((choice, c.slug.clone()));
    }
    criteria.insert(
        NONE.to_owned(),
        json!("Anything else: the message asks for something different, asks a question about it, asks for more than that alone, or adds conditions or changes to it."),
    );
    let q = json!({
        "type": "choice",
        "instructions": "The text is a message a person typed to a coding agent in their project. Decide whether it asks for exactly one of these routine operations, as it stands, and nothing more.",
        "criteria": criteria,
    });
    (vec![(QUESTION.to_owned(), q)], names)
}

/// What Jev's answer allows.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// It picked this operation, with this confidence.
    Picked(String, f64),
    /// It said the prompt asks for something else.
    NoneOfThem,
    /// No usable answer.
    Unknown,
}

/// Read Jev's pick back to a slug.
#[must_use]
pub fn verdict(choice: &str, confidence: f64, names: &[(String, String)]) -> Verdict {
    if choice == NONE {
        return Verdict::NoneOfThem;
    }
    names
        .iter()
        .find(|(name, _)| name == choice)
        .map_or(Verdict::Unknown, |(_, slug)| {
            Verdict::Picked(slug.clone(), confidence)
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn candidate(slug: &str) -> Candidate {
        Candidate {
            slug: slug.into(),
            name: slug.to_uppercase(),
            description: "Does it.".into(),
            phrases: vec![slug.into()],
        }
    }

    #[test]
    fn the_question_names_choices_by_number_and_has_none() {
        let (questions, names) = question(&[candidate("run-tests"), candidate("deploy")]);
        let q = &questions[0].1;
        assert_eq!(questions[0].0, QUESTION);
        assert_eq!(q["type"], "choice");
        let criteria = q["criteria"].as_object().unwrap();
        assert_eq!(criteria.len(), 3);
        assert!(criteria.contains_key("op_1") && criteria.contains_key(NONE));
        assert_eq!(names[1], ("op_2".to_owned(), "deploy".to_owned()));
        let many: Vec<Candidate> = (0..20).map(|i| candidate(&format!("o{i}"))).collect();
        assert_eq!(question(&many).1.len(), MAX_CANDIDATES);
    }

    #[test]
    fn answers_read_back() {
        let names = vec![("op_1".to_owned(), "tests".to_owned())];
        assert_eq!(
            verdict("op_1", 0.9, &names),
            Verdict::Picked("tests".into(), 0.9)
        );
        assert_eq!(verdict("none", 0.9, &names), Verdict::NoneOfThem);
        assert_eq!(verdict("op_7", 0.9, &names), Verdict::Unknown);
    }
}
