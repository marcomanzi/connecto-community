//! The real Windows Credential Manager: write, read, overwrite, delete, the
//! split of a value larger than one credential, and the size limit. Every
//! credential here has a throwaway target name made of this process's id, the
//! time and the test's name, and is removed when the test ends, passed or not
//! (with every part a test could have made). No other credential is read or
//! touched; in particular never `connecto/secrets`.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::panic)]

use connecto_secrets::{CredentialManagerItems, Items, MAX_ITEM_BYTES, MAX_PARTS, MAX_VALUE_BYTES};

/// A throwaway item, deleted on drop with its parts.
struct Throwaway {
    service: String,
    account: &'static str,
}

impl Throwaway {
    fn new(test: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let service = format!(
            "connecto-secrets-test-{}-{nanos}-{test}",
            std::process::id()
        );
        assert!(service.starts_with("connecto-secrets-test-"));
        Self {
            service,
            account: "item",
        }
    }

    /// A part credential `<service>/item.<generation>.<n>`, read as one item.
    fn part(&self, generation: u64, n: usize) -> Option<Vec<u8>> {
        CredentialManagerItems
            .read(&self.service, &format!("{}.{generation}.{n}", self.account))
            .unwrap()
    }
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = CredentialManagerItems.delete(&self.service, self.account);
        // Whatever a failed test left: the generations these tests reach.
        for generation in 1..=4 {
            for n in 0..MAX_PARTS {
                let _ = CredentialManagerItems
                    .delete(&self.service, &format!("{}.{generation}.{n}", self.account));
            }
        }
    }
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 251) as u8) ^ seed).collect()
}

#[test]
fn write_read_overwrite_delete() {
    let item = Throwaway::new("cycle");
    let (s, a) = (item.service.as_str(), item.account);
    let items = CredentialManagerItems;

    assert_eq!(items.read(s, a).unwrap(), None);
    items.delete(s, a).unwrap(); // absent: not an error

    items.write(s, a, br#"{"version":1,"secrets":{}}"#).unwrap();
    assert_eq!(
        items.read(s, a).unwrap().as_deref(),
        Some(&br#"{"version":1,"secrets":{}}"#[..])
    );

    // A shorter value replaces the whole of the longer one.
    items.write(s, a, b"\x00\x01binary\xff").unwrap();
    assert_eq!(
        items.read(s, a).unwrap().as_deref(),
        Some(&b"\x00\x01binary\xff"[..])
    );

    items.delete(s, a).unwrap();
    assert_eq!(items.read(s, a).unwrap(), None);
    items.delete(s, a).unwrap();
}

#[test]
fn a_value_larger_than_one_credential_is_split_and_comes_back_whole() {
    let item = Throwaway::new("split");
    let (s, a) = (item.service.as_str(), item.account);
    let items = CredentialManagerItems;
    assert_eq!(MAX_ITEM_BYTES, 2560);

    // Exactly one credential: kept in the item itself, as every build did.
    let full = bytes(MAX_ITEM_BYTES, 1);
    items.write(s, a, &full).unwrap();
    assert_eq!(items.read(s, a).unwrap().as_deref(), Some(&full[..]));
    assert_eq!(item.part(1, 0), None);

    // One byte more: two parts of generation 1.
    let more = bytes(MAX_ITEM_BYTES + 1, 2);
    items.write(s, a, &more).unwrap();
    assert_eq!(items.read(s, a).unwrap().as_deref(), Some(&more[..]));
    assert_eq!(item.part(1, 0).unwrap().len(), MAX_ITEM_BYTES);
    assert_eq!(item.part(1, 1).unwrap().len(), 1);

    // The largest value: generation 2, and generation 1 is gone.
    let most = bytes(MAX_VALUE_BYTES, 3);
    items.write(s, a, &most).unwrap();
    assert_eq!(items.read(s, a).unwrap().as_deref(), Some(&most[..]));
    assert_eq!(item.part(1, 0), None);
    assert_eq!(item.part(1, 1), None);
    assert!(item.part(2, MAX_PARTS - 1).is_some());

    // Back to one credential, then removed: no part is left.
    items.write(s, a, b"small").unwrap();
    assert_eq!(items.read(s, a).unwrap().as_deref(), Some(&b"small"[..]));
    assert_eq!(item.part(2, 0), None);
    items.write(s, a, &more).unwrap();
    items.delete(s, a).unwrap();
    assert_eq!(items.read(s, a).unwrap(), None);
    for generation in 1..=3 {
        assert_eq!(item.part(generation, 0), None, "generation {generation}");
    }
}

#[test]
fn the_limit_is_kept_exactly_and_a_larger_value_is_refused_whole() {
    let item = Throwaway::new("limit");
    let (s, a) = (item.service.as_str(), item.account);
    let items = CredentialManagerItems;
    assert_eq!(MAX_VALUE_BYTES, MAX_PARTS * 2560);

    let kept = bytes(MAX_ITEM_BYTES * 2, 4);
    items.write(s, a, &kept).unwrap();

    // One byte more than the limit: refused, and what was there is left as it was.
    let secret_marker = "do-not-echo-this-secret";
    let mut over = secret_marker.repeat(MAX_VALUE_BYTES / secret_marker.len() + 1);
    over.truncate(MAX_VALUE_BYTES + 1);
    let err = items.write(s, a, over.as_bytes()).unwrap_err();
    assert!(err.contains(&MAX_VALUE_BYTES.to_string()), "{err}");
    assert!(!err.contains(secret_marker), "the error holds the secret");
    assert_eq!(items.read(s, a).unwrap().as_deref(), Some(&kept[..]));
    assert_eq!(item.part(2, 0), None, "no part of a new generation");

    // Never created: refused before anything is written.
    let fresh = Throwaway::new("limit-fresh");
    assert!(
        items
            .write(&fresh.service, fresh.account, over.as_bytes())
            .is_err()
    );
    assert_eq!(items.read(&fresh.service, fresh.account).unwrap(), None);
    assert_eq!(fresh.part(1, 0), None);
}
