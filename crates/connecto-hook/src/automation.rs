//! The automation definition file (`automation.json`): its schema and offline
//! validation. Shared by the app's engine and by `connecto automation check`,
//! which the authoring agent runs on its own file.

use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::bridge::valid_key;

/// The only schema version.
pub const VERSION: u64 = 1;
/// The model the first release pins (docs/poc/ARCHITECTURE.md, JevRouter).
pub const DEFAULT_MODEL: &str = "jev-1.14.0";
pub const DEFAULT_THRESHOLD: f64 = 0.8;
/// The placeholders of [`Definition::template`], which the authoring agent
/// replaces. A file that still holds one is not finished.
const TODO_INSTRUCTIONS: &str =
    "TODO: describe the screen this automation recognizes, from its text.";
const TODO_MATCH: &str = "TODO: the screen to act on.";
const TODO_TRIGGER: &str = "TODO: a distinctive phrase from the screen";
const TODO_TEXTS: [&str; 3] = [TODO_INSTRUCTIONS, TODO_MATCH, TODO_TRIGGER];
pub const MAX_NAME_CHARS: usize = crate::name::MAX_NAME_CHARS;
pub const MAX_KEYS: usize = 20;
pub const MAX_INSTRUCTIONS: usize = 4000;
pub const MAX_CHOICES: usize = 32;
pub const MIN_TRIGGER: usize = 3;
pub const MAX_TRIGGER: usize = 200;
pub const MAX_TRIGGERS: usize = 5;
pub const FILE_NAME: &str = "automation.json";

const FIELDS: [&str; 10] = [
    "version",
    "name",
    "enabled",
    "agent",
    "model",
    "question",
    "match",
    "keys",
    "trigger",
    "threshold",
];

/// Which agent authors (and is resumed for) this automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// A Jev question.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    /// Pick one named choice; `criteria` maps each choice name to its description.
    Choice {
        instructions: String,
        criteria: Vec<(String, String)>,
    },
    /// Yes/no. The answer is the probability of yes.
    Noul {
        instructions: String,
        yes: Option<String>,
        no: Option<String>,
    },
}

/// A validated automation.
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    pub name: String,
    pub enabled: bool,
    pub agent: Agent,
    pub model: String,
    pub question: Question,
    /// For a choice: the choice name that fires. For noul: `"true"`.
    pub matches: String,
    pub keys: Vec<String>,
    /// Phrases that must all be on the screen (see [`trigger_matches`]) before
    /// Jev is asked. Empty when missing or invalid.
    pub trigger: Vec<String>,
    /// Why `trigger` is unusable, if it is: the engine never asks Jev about an
    /// automation with one.
    pub trigger_problem: Option<String>,
    pub threshold: f64,
}

impl Definition {
    /// Parse and validate. On failure, every problem found, one per entry.
    pub fn parse(text: &str) -> Result<Self, Vec<String>> {
        let value: Value =
            serde_json::from_str(text).map_err(|err| vec![format!("not valid JSON: {err}")])?;
        Self::from_value(&value)
    }

    /// As [`Definition::parse`], but a missing or invalid `trigger` does not
    /// fail the file: it loads with [`Definition::trigger_problem`] set, so it
    /// can be listed and fixed and is never sent to Jev.
    pub fn parse_lenient(text: &str) -> Result<Self, Vec<String>> {
        let value: Value =
            serde_json::from_str(text).map_err(|err| vec![format!("not valid JSON: {err}")])?;
        Self::lenient(&value)
    }

    /// Strict: every problem, the trigger's included, is an error.
    pub fn from_value(value: &Value) -> Result<Self, Vec<String>> {
        let def = Self::lenient(value)?;
        match def.trigger_problem {
            Some(problem) => Err(vec![problem]),
            None => Ok(def),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lenient(value: &Value) -> Result<Self, Vec<String>> {
        let Some(root) = value.as_object() else {
            return Err(vec!["the file must be a JSON object".to_owned()]);
        };
        let mut errors = Vec::new();
        for key in root.keys() {
            if !FIELDS.contains(&key.as_str()) {
                errors.push(format!("unknown field {key:?}"));
            }
        }
        if root.get("version").and_then(Value::as_u64) != Some(VERSION) {
            errors.push(format!("\"version\" must be the number {VERSION}"));
        }
        let name = match root.get("name").and_then(Value::as_str).map(str::trim) {
            Some(name)
                if !name.is_empty()
                    && name.chars().count() <= MAX_NAME_CHARS
                    && !name.chars().any(char::is_control) =>
            {
                name.to_owned()
            }
            _ => {
                errors.push(format!(
                    "\"name\" must be one line of 1 to {MAX_NAME_CHARS} characters"
                ));
                String::new()
            }
        };
        let enabled = match root.get("enabled") {
            None => true,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => {
                errors.push("\"enabled\" must be true or false".to_owned());
                false
            }
        };
        let agent = match root.get("agent").and_then(Value::as_str) {
            Some("claude") => Agent::Claude,
            Some("codex") => Agent::Codex,
            _ => {
                errors.push("\"agent\" must be \"claude\" or \"codex\"".to_owned());
                Agent::Claude
            }
        };
        let model = match root.get("model").and_then(Value::as_str) {
            Some(model) if !model.trim().is_empty() && !model.chars().any(char::is_whitespace) => {
                model.to_owned()
            }
            _ => {
                errors.push(format!(
                    "\"model\" must be a model name such as \"{DEFAULT_MODEL}\""
                ));
                String::new()
            }
        };
        let threshold = match root.get("threshold") {
            None => DEFAULT_THRESHOLD,
            Some(v) => match v.as_f64() {
                Some(t) if t > 0.5 && t <= 1.0 => t,
                _ => {
                    errors
                        .push("\"threshold\" must be a number above 0.5 and at most 1".to_owned());
                    DEFAULT_THRESHOLD
                }
            },
        };
        let keys = match root.get("keys").and_then(Value::as_array) {
            Some(list) if !list.is_empty() && list.len() <= MAX_KEYS => {
                let mut keys = Vec::new();
                for item in list {
                    match item.as_str() {
                        Some(key) if valid_key(key) => keys.push(key.to_owned()),
                        other => errors.push(format!(
                            "{} is not a key name `connecto keys` accepts",
                            other.map_or_else(|| item.to_string(), |s| format!("{s:?}"))
                        )),
                    }
                }
                keys
            }
            _ => {
                errors.push(format!(
                    "\"keys\" must be a list of 1 to {MAX_KEYS} key names"
                ));
                Vec::new()
            }
        };
        let (question, matches) = parse_question(root, &mut errors);
        let (trigger, trigger_problem) = match parse_trigger(root.get("trigger")) {
            Ok(trigger) => (trigger, None),
            Err(problem) => (Vec::new(), Some(problem)),
        };
        if !errors.is_empty() {
            // The other problems come first; the trigger's is reported too.
            errors.extend(trigger_problem);
            return Err(errors);
        }
        let Some(question) = question else {
            return Err(vec!["\"question\" is missing".to_owned()]);
        };
        Ok(Self {
            name,
            enabled,
            agent,
            model,
            question,
            matches,
            keys,
            trigger,
            trigger_problem,
            threshold,
        })
    }

    /// The file's JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let question = self.jev_question();
        json!({
            "version": VERSION,
            "name": self.name,
            "enabled": self.enabled,
            "agent": self.agent.word(),
            "model": self.model,
            "question": question,
            "match": self.matches,
            "keys": self.keys,
            "trigger": self.trigger,
            "threshold": self.threshold,
        })
    }

    /// The question as Jev's request wants it.
    #[must_use]
    pub fn jev_question(&self) -> Value {
        match &self.question {
            Question::Choice {
                instructions,
                criteria,
            } => {
                let map: Map<String, Value> = criteria
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect();
                json!({"type": "choice", "instructions": instructions, "criteria": map})
            }
            Question::Noul {
                instructions,
                yes,
                no,
            } => {
                let mut q = json!({"type": "noul", "instructions": instructions});
                let mut criteria = Map::new();
                if let Some(yes) = yes {
                    criteria.insert("true".into(), json!(yes));
                }
                if let Some(no) = no {
                    criteria.insert("false".into(), json!(no));
                }
                if !criteria.is_empty() {
                    q["criteria"] = Value::Object(criteria);
                }
                q
            }
        }
    }

    /// A starting file for a new automation: valid, disabled, with placeholders
    /// the authoring agent replaces.
    #[must_use]
    pub fn template(name: &str, agent: Agent) -> Self {
        Self {
            name: name.to_owned(),
            enabled: false,
            agent,
            model: DEFAULT_MODEL.to_owned(),
            question: Question::Choice {
                instructions: TODO_INSTRUCTIONS.to_owned(),
                criteria: vec![
                    ("match".to_owned(), TODO_MATCH.to_owned()),
                    ("other".to_owned(), "Any other screen.".to_owned()),
                ],
            },
            matches: "match".to_owned(),
            keys: vec!["enter".to_owned()],
            trigger: vec![TODO_TRIGGER.to_owned()],
            trigger_problem: None,
            threshold: DEFAULT_THRESHOLD,
        }
    }

    /// True while the file still holds one of the template's placeholders.
    /// Only those exact texts count: a finished automation may well mention
    /// TODO, for a screen such as "Edit TODO.md".
    #[must_use]
    pub fn has_todo(&self) -> bool {
        let placeholder = |text: &str| TODO_TEXTS.contains(&text.trim());
        let (instructions, criteria): (&str, &[(String, String)]) = match &self.question {
            Question::Choice {
                instructions,
                criteria,
            } => (instructions, criteria),
            Question::Noul { instructions, .. } => (instructions, &[]),
        };
        self.trigger.iter().any(|t| placeholder(t))
            || placeholder(instructions)
            || criteria.iter().any(|(_, v)| placeholder(v))
    }
}

/// 1 to 5 phrases, each 3 to 200 characters.
fn parse_trigger(value: Option<&Value>) -> Result<Vec<String>, String> {
    let bad = || {
        format!(
            "\"trigger\" is required: a list of 1 to {MAX_TRIGGERS} phrases copied from the screen, each {MIN_TRIGGER} to {MAX_TRIGGER} characters"
        )
    };
    let list = value.and_then(Value::as_array).ok_or_else(bad)?;
    if list.is_empty() || list.len() > MAX_TRIGGERS {
        return Err(bad());
    }
    let mut out = Vec::new();
    for item in list {
        let phrase = item.as_str().map(str::trim).ok_or_else(bad)?;
        let length = phrase.chars().count();
        if !(MIN_TRIGGER..=MAX_TRIGGER).contains(&length) || phrase.chars().any(char::is_control) {
            return Err(bad());
        }
        out.push(phrase.to_owned());
    }
    Ok(out)
}

/// Lowercase, with the box-drawing characters of terminal chrome and every
/// run of whitespace (line wraps included) turned into one space.
#[must_use]
pub fn normalize(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| {
            if ('\u{2500}'..='\u{257f}').contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .to_lowercase();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether every phrase of `trigger` is on the screen. Case, wrapping and
/// chrome do not matter; an empty trigger matches nothing.
#[must_use]
pub fn trigger_matches(trigger: &[String], screen: &str) -> bool {
    if trigger.is_empty() {
        return false;
    }
    let screen = normalize(screen);
    trigger.iter().all(|phrase| {
        let phrase = normalize(phrase);
        !phrase.is_empty() && screen.contains(&phrase)
    })
}

/// The report of `connecto automation check --screen`: which phrases are on the
/// saved screen text, with no call to Jev.
#[must_use]
pub fn trigger_report(path: &str, text: &str, screen: &str) -> (bool, String) {
    let def = match Definition::parse_lenient(text) {
        Ok(def) => def,
        Err(errors) => {
            return (
                false,
                errors.iter().map(|e| format!("{path}: {e}\n")).collect(),
            );
        }
    };
    if let Some(problem) = &def.trigger_problem {
        return (false, format!("{path}: {problem}\n"));
    }
    let seen = normalize(screen);
    let mut out = String::new();
    let mut all = true;
    for phrase in &def.trigger {
        let found = seen.contains(&normalize(phrase));
        all &= found;
        let _ = writeln!(
            out,
            "{path}: trigger {phrase:?}: {}",
            if found { "found" } else { "NOT on the screen" }
        );
    }
    let _ = writeln!(
        out,
        "{path}: the trigger {} this screen{}",
        if all { "matches" } else { "does not match" },
        if all {
            ", so Jev would be asked"
        } else {
            ", so Jev would not be called"
        }
    );
    (all, out)
}

fn parse_question(
    root: &Map<String, Value>,
    errors: &mut Vec<String>,
) -> (Option<Question>, String) {
    let Some(q) = root.get("question").and_then(Value::as_object) else {
        errors.push("\"question\" must be an object".to_owned());
        return (None, String::new());
    };
    let instructions = match q.get("instructions").and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() && text.chars().count() <= MAX_INSTRUCTIONS => {
            text.to_owned()
        }
        _ => {
            errors.push(format!(
                "\"question.instructions\" must be text of 1 to {MAX_INSTRUCTIONS} characters"
            ));
            String::new()
        }
    };
    let matched = root.get("match").and_then(Value::as_str);
    match q.get("type").and_then(Value::as_str) {
        Some("choice") => {
            let mut criteria = Vec::new();
            match q.get("criteria").and_then(Value::as_object) {
                Some(map) if (2..=MAX_CHOICES).contains(&map.len()) => {
                    for (name, desc) in map {
                        match desc.as_str() {
                            Some(desc) if !name.trim().is_empty() => {
                                criteria.push((name.clone(), desc.to_owned()));
                            }
                            _ => errors.push(format!(
                                "criteria entry {name:?} must map a non-empty name to a description string"
                            )),
                        }
                    }
                }
                _ => errors.push(format!(
                    "\"question.criteria\" must be an object of 2 to {MAX_CHOICES} choices (name: description)"
                )),
            }
            let matches = match matched {
                Some(m) if criteria.iter().any(|(name, _)| name == m) => m.to_owned(),
                _ => {
                    errors.push(
                        "\"match\" must be the name of one of the choices in \"question.criteria\""
                            .to_owned(),
                    );
                    String::new()
                }
            };
            (
                Some(Question::Choice {
                    instructions,
                    criteria,
                }),
                matches,
            )
        }
        Some("noul") => {
            let mut yes = None;
            let mut no = None;
            match q.get("criteria") {
                None => {}
                Some(Value::Object(map)) => {
                    for (k, v) in map {
                        match (k.as_str(), v.as_str()) {
                            ("true", Some(t)) => yes = Some(t.to_owned()),
                            ("false", Some(t)) => no = Some(t.to_owned()),
                            _ => errors.push(
                                "noul \"criteria\" takes only \"true\" and \"false\", each a description string"
                                    .to_owned(),
                            ),
                        }
                    }
                }
                Some(_) => errors.push("noul \"criteria\" must be an object".to_owned()),
            }
            if matched != Some("true") {
                errors.push("for a noul question \"match\" must be \"true\"".to_owned());
            }
            (
                Some(Question::Noul {
                    instructions,
                    yes,
                    no,
                }),
                "true".to_owned(),
            )
        }
        _ => {
            errors.push("\"question.type\" must be \"choice\" or \"noul\"".to_owned());
            (None, String::new())
        }
    }
}

/// The report `connecto automation check` prints, and its exit status.
#[must_use]
pub fn check_report(path: &str, text: Result<String, String>) -> (bool, String) {
    let mut out = String::new();
    let text = match text {
        Ok(text) => text,
        Err(err) => return (false, format!("{path}: cannot read: {err}\n")),
    };
    match Definition::parse(&text) {
        Ok(def) => {
            let _ = writeln!(out, "{path}: ok ({:?}, {} keys)", def.name, def.keys.len());
            let mut ok = true;
            if def.has_todo() {
                let _ = writeln!(out, "warning: the file still holds TODO placeholder text");
                ok = false;
            }
            if def.enabled {
                let _ = writeln!(out, "note: enabled, so Connecto will act on it right away");
            } else {
                let _ = writeln!(
                    out,
                    "note: \"enabled\" is false; the user turns it on in Connecto"
                );
            }
            (ok, out)
        }
        Err(errors) => {
            for error in errors {
                let _ = writeln!(out, "{path}: {error}");
            }
            (false, out)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn good() -> Value {
        json!({
            "version": 1, "name": "Approve", "enabled": true, "agent": "claude",
            "model": "jev-1.14.0",
            "question": {"type": "choice", "instructions": "Is this a permission prompt?",
                "criteria": {"proceed": "Do you want to proceed with options Yes/No", "other": "anything else"}},
            "match": "proceed", "keys": ["down", "enter"], "threshold": 0.9,
            "trigger": ["Do you want to proceed", "Yes, allow all"]
        })
    }

    #[test]
    fn the_trigger_is_required_and_bounded() {
        let mut v = good();
        for bad in [
            json!([]),
            json!(["ab"]),
            json!(["x".repeat(201)]),
            json!(["fine", 3]),
            json!(["a", "b", "c", "d", "e", "f"].map(|x| x.repeat(3))),
            json!("not a list"),
            json!(["bad\nline"]),
        ] {
            v["trigger"] = bad.clone();
            let errors = Definition::from_value(&v).unwrap_err();
            assert!(errors[0].contains("trigger"), "{bad}: {errors:?}");
            // Lenient: the file loads and says what is wrong with the trigger.
            let def = Definition::parse_lenient(&v.to_string()).unwrap();
            assert!(def.trigger.is_empty() && def.trigger_problem.is_some());
        }
        v.as_object_mut().unwrap().remove("trigger");
        assert!(Definition::parse(&v.to_string()).unwrap_err()[0].contains("required"));
        let lenient = Definition::parse_lenient(&v.to_string()).unwrap();
        assert!(lenient.trigger_problem.is_some() && lenient.trigger.is_empty());
        // 1 to 5 entries of 3 to 200 characters are fine, and trimmed.
        v["trigger"] = json!(["  abc  ", "x".repeat(200)]);
        assert_eq!(Definition::from_value(&v).unwrap().trigger[0], "abc");
        // The template's placeholder is valid but unfinished.
        let template = Definition::template("T", Agent::Claude);
        assert!(template.trigger_problem.is_none() && template.has_todo());
    }

    #[test]
    fn the_trigger_matches_by_case_whitespace_wrapping_and_chrome_and_needs_every_phrase() {
        let t = |phrases: &[&str]| phrases.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        let screen = "\u{256d}\u{2500}\u{2500}\u{2500}\u{256e}\n\u{2502} Claude in CHROME      \u{2502}\n\u{2502} extension   detected  \u{2502}\n\u{2570}\u{2500}\u{2500}\u{2500}\u{256f}\n  \u{276f} 1. Yes, allow\n  2. No";
        // Case, runs of spaces, a wrap across lines and box chrome do not matter.
        assert!(trigger_matches(
            &t(&["claude in chrome extension detected"]),
            screen
        ));
        assert!(trigger_matches(&t(&["CLAUDE IN\tCHROME"]), screen));
        assert!(trigger_matches(
            &t(&["extension detected", "1. yes, allow"]),
            screen
        ));
        // A phrase that itself carries chrome or odd spacing is normalized alike.
        assert!(trigger_matches(
            &t(&["\u{2502} claude in   chrome \u{2502}"]),
            screen
        ));
        // Every phrase must be there.
        assert!(!trigger_matches(
            &t(&["claude in chrome", "not on screen"]),
            screen
        ));
        // Nothing matches nothing, and words must be whole as typed.
        assert!(!trigger_matches(&[], screen));
        assert!(!trigger_matches(&t(&["extensiondetected"]), screen));
        assert_eq!(normalize("A\n\u{2502}  b\t\u{2500}c"), "a b c");
    }

    #[test]
    fn the_screen_check_reports_each_phrase_offline() {
        let text = good().to_string();
        let (ok, out) = trigger_report(
            "p",
            &text,
            "DO YOU WANT TO PROCEED?\n 1. Yes, allow all edits",
        );
        assert!(ok, "{out}");
        assert!(out.contains("matches") && out.contains("found"));
        let (ok, out) = trigger_report("p", &text, "Do you want to proceed?");
        assert!(
            !ok && out.contains("NOT on the screen") && out.contains("would not be called"),
            "{out}"
        );
        let mut v = good();
        v.as_object_mut().unwrap().remove("trigger");
        let (ok, out) = trigger_report("p", &v.to_string(), "anything");
        assert!(!ok && out.contains("required"));
    }

    #[test]
    fn a_good_file_parses_and_round_trips() {
        let def = Definition::from_value(&good()).unwrap();
        assert_eq!(def.keys, ["down", "enter"]);
        assert_eq!(Definition::from_value(&def.to_json()).unwrap(), def);
    }

    #[test]
    fn threshold_defaults_and_is_bounded() {
        let mut v = good();
        v.as_object_mut().unwrap().remove("threshold");
        assert!((Definition::from_value(&v).unwrap().threshold - 0.8).abs() < f64::EPSILON);
        v["threshold"] = json!(0.4);
        assert!(Definition::from_value(&v).is_err());
        v["threshold"] = json!(1.5);
        assert!(Definition::from_value(&v).is_err());
    }

    #[test]
    fn every_problem_is_reported() {
        let mut v = good();
        v["keys"] = json!(["down", "bogus-key"]);
        v["match"] = json!("nope");
        v["agent"] = json!("gemini");
        v["extra"] = json!(1);
        let errors = Definition::from_value(&v).unwrap_err();
        assert_eq!(errors.len(), 4, "{errors:?}");
    }

    #[test]
    fn noul_needs_match_true_and_choice_needs_two_choices() {
        let mut v = good();
        v["question"] =
            json!({"type": "noul", "instructions": "x", "criteria": {"true": "a", "false": "b"}});
        v["match"] = json!("true");
        assert!(Definition::from_value(&v).is_ok());
        v["match"] = json!("false");
        assert!(Definition::from_value(&v).is_err());
        let mut w = good();
        w["question"]["criteria"] = json!({"proceed": "x"});
        assert!(Definition::from_value(&w).is_err());
    }

    #[test]
    fn only_the_templates_own_placeholders_make_a_file_unfinished() {
        // A finished automation about a screen that says TODO.
        let mut v = good();
        v["trigger"] = json!(["Edit TODO.md"]);
        v["question"]["instructions"] = json!("Is Claude asking to edit TODO.md?");
        v["question"]["criteria"]["proceed"] = json!("A prompt about the TODO list");
        let def = Definition::from_value(&v).unwrap();
        assert!(!def.has_todo());
        assert!(check_report("p", Ok(v.to_string())).0);
        // Each placeholder left in place still flags it.
        let template = Definition::template("T", Agent::Claude).to_json();
        for field in ["trigger", "instructions", "criteria"] {
            let mut v = good();
            match field {
                "trigger" => v["trigger"] = template["trigger"].clone(),
                "instructions" => {
                    v["question"]["instructions"] = template["question"]["instructions"].clone();
                }
                _ => {
                    v["question"]["criteria"]["proceed"] =
                        template["question"]["criteria"]["match"].clone();
                }
            }
            assert!(Definition::from_value(&v).unwrap().has_todo(), "{field}");
        }
    }

    #[test]
    fn the_template_is_valid_but_flagged() {
        let def = Definition::template("T", Agent::Codex);
        let again = Definition::from_value(&def.to_json()).unwrap();
        assert!(again.has_todo() && !again.enabled);
        let (ok, text) = check_report("p", Ok(def.to_json().to_string()));
        assert!(!ok && text.contains("TODO"));
        let (ok, _) = check_report("p", Ok(good().to_string()));
        assert!(ok);
        assert!(!check_report("p", Ok("{".into())).0);
    }
}
