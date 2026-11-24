//! The vault's values are hidden by every redaction once its source is set.
//! Its own test binary: the source is process-wide.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

const VALUE: &str = "vault-value-0123";

#[test]
fn vault_values_are_hidden_everywhere_once_the_source_is_set() {
    // Before: an ordinary-looking value goes through untouched.
    assert!(connecto_redact::hide_known(VALUE).contains(VALUE));

    connecto_redact::set_known_source(Arc::new(|| Arc::from(connecto_redact::known_set([VALUE]))));

    let screen = format!("$ echo $X\n{VALUE}\n$ ");
    let shown = connecto_redact::hide_known(&screen);
    assert!(!shown.contains(VALUE), "{shown}");
    assert!(shown.contains("$ echo $X"));

    let redacted = connecto_redact::redact(&screen, &[]).unwrap();
    assert!(!redacted.get().contains(VALUE));

    let hex: String = VALUE.bytes().map(|b| format!("{b:02x}")).collect();
    assert!(!connecto_redact::hide_known(&hex).contains(&hex));

    let body = connecto_redact::prepare(&format!("the key is {VALUE}"), &[]).unwrap();
    assert!(!body.get().contains(VALUE));
}
