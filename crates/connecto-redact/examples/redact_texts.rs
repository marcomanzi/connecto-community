//! Redact texts with Connecto's own redactor, for scripts that must not send
//! secrets (scripts/topic-routing-eval/jev_eval.py).
//!
//! Reads a JSON array of strings on stdin and prints a JSON array of the same
//! length: each text sanitized and redacted, or `null` where redaction could
//! not run (the caller must then not send that text).

#![allow(clippy::print_stdout)]

use std::io::Read;

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        std::process::exit(1);
    }
    let Ok(texts) = serde_json::from_str::<Vec<String>>(&input) else {
        std::process::exit(2);
    };
    let out: Vec<Option<String>> = texts
        .iter()
        .map(|text| {
            let clean = connecto_redact::sanitize(text);
            connecto_redact::redact(&clean, &[])
                .ok()
                .map(|done| done.get().clone())
        })
        .collect();
    println!("{}", serde_json::Value::from(out));
}
