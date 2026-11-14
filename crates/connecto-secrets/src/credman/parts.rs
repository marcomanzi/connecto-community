//! One value across several credentials, for values larger than one credential
//! holds (`MAX_ITEM_BYTES`).
//!
//! The item `<service>/<account>` (the *head*) holds either
//!
//! - the value itself, when it fits and does not begin with [`MAGIC`]: the
//!   layout every earlier build wrote, so an old single-item store reads as it
//!   is, and a small store stays readable by an earlier build; or
//! - [`MAGIC`] and a header `1 <generation> <parts> <length> <fnv-1a 64>`, and
//!   the value in `<parts>` credentials `<service>/<account>.<generation>.<n>`
//!   (n from 0), each at most `MAX_ITEM_BYTES`.
//!
//! A write puts every part of a new generation first and the head last, so the
//! head names either the complete old value or the complete new one, never a
//! half: one credential write is all or nothing. When a part or the head
//! cannot be written, the new parts are removed again and the old value stays.
//! Only after the head was written are the old generation's parts removed (a
//! removal that fails leaves an unused credential behind, never a wrong value).
//! A read checks the length and checksum of what the parts add up to; a part
//! that is missing or does not match is an error, never a shorter value. An
//! earlier build reads the head of a split value as a document it does not
//! know, and so leaves it alone.

use crate::Items;

use super::MAX_ITEM_BYTES;

/// The head of a split value begins with this (a stored document begins with
/// `{`).
pub(super) const MAGIC: &[u8] = b"\0connecto-parts\0";

/// The most credentials one value is split into.
pub const MAX_PARTS: usize = 16;

/// The largest value: [`MAX_PARTS`] credentials of `MAX_ITEM_BYTES` each.
pub const MAX_VALUE_BYTES: usize = MAX_PARTS * MAX_ITEM_BYTES;

/// What the head of a split value says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    generation: u64,
    parts: usize,
    length: usize,
    checksum: u64,
}

impl Header {
    fn of(generation: u64, value: &[u8]) -> Self {
        Self {
            generation,
            parts: value.len().div_ceil(MAX_ITEM_BYTES),
            length: value.len(),
            checksum: fnv1a(value),
        }
    }

    fn to_bytes(self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(
            format!(
                "1 {} {} {} {:016x}",
                self.generation, self.parts, self.length, self.checksum
            )
            .as_bytes(),
        );
        out
    }

    /// `None`: not a split value. `Err`: a head that says it is one but cannot
    /// be read (a later layout, or damage).
    fn parse(head: &[u8]) -> Result<Option<Self>, String> {
        let Some(rest) = head.strip_prefix(MAGIC) else {
            return Ok(None);
        };
        let bad = || "its head is not a layout this version can read".to_owned();
        let text = std::str::from_utf8(rest).map_err(|_| bad())?;
        let fields: Vec<&str> = text.split(' ').collect();
        let [layout, generation, parts, length, checksum] = fields.as_slice() else {
            return Err(bad());
        };
        if *layout != "1" {
            return Err(bad());
        }
        let header = Self {
            generation: generation.parse().map_err(|_| bad())?,
            parts: parts.parse().map_err(|_| bad())?,
            length: length.parse().map_err(|_| bad())?,
            checksum: u64::from_str_radix(checksum, 16).map_err(|_| bad())?,
        };
        if header.parts > MAX_PARTS || header.parts != header.length.div_ceil(MAX_ITEM_BYTES) {
            return Err(bad());
        }
        Ok(Some(header))
    }
}

/// FNV-1a, 64 bits: catches a part of another generation or a damaged one (it
/// is no protection against someone who can write the credentials).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The account of part `n` of a generation.
fn part(account: &str, generation: u64, n: usize) -> String {
    format!("{account}.{generation}.{n}")
}

fn damaged(service: &str, account: &str, why: &str) -> String {
    format!("Credential Manager: the item {service}/{account} cannot be read ({why})")
}

/// The whole value, from the head and, for a split value, its parts.
pub(super) fn read(
    one: &dyn Items,
    service: &str,
    account: &str,
) -> Result<Option<Vec<u8>>, String> {
    let Some(bytes) = one.read(service, account)? else {
        return Ok(None);
    };
    let Some(header) = Header::parse(&bytes).map_err(|why| damaged(service, account, &why))? else {
        return Ok(Some(bytes));
    };
    let mut value = Vec::with_capacity(header.length);
    for n in 0..header.parts {
        match one.read(service, &part(account, header.generation, n))? {
            Some(piece) => value.extend_from_slice(&piece),
            None => return Err(damaged(service, account, &format!("part {n} is missing"))),
        }
    }
    if value.len() != header.length || fnv1a(&value) != header.checksum {
        return Err(damaged(service, account, "its parts do not match its head"));
    }
    Ok(Some(value))
}

/// Remove the parts of one generation; a failure only leaves an unused
/// credential behind.
fn remove_parts(one: &dyn Items, service: &str, account: &str, generation: u64, parts: usize) {
    for n in 0..parts {
        let _ = one.delete(service, &part(account, generation, n));
    }
}

/// Store `value` whole: in the head alone when it fits, else split (see the
/// module). Larger than [`MAX_VALUE_BYTES`]: refused before anything is written.
pub(super) fn write(
    one: &dyn Items,
    service: &str,
    account: &str,
    value: &[u8],
) -> Result<(), String> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(format!(
            "Credential Manager: the item {service}/{account} would be {} bytes, more than the {MAX_VALUE_BYTES} Connecto keeps in {MAX_PARTS} credentials of {MAX_ITEM_BYTES} bytes; nothing was written",
            value.len()
        ));
    }
    // What the head names now: its parts go once the new value is in place. A
    // head that cannot be understood is replaced (whatever it named is left).
    let old = match one.read(service, account)? {
        Some(bytes) => Header::parse(&bytes).ok().flatten(),
        None => None,
    };
    if value.len() <= MAX_ITEM_BYTES && !value.starts_with(MAGIC) {
        one.write(service, account, value)?;
    } else {
        let generation = old.map_or(1, |h| h.generation.wrapping_add(1).max(1));
        let header = Header::of(generation, value);
        for (n, piece) in value.chunks(MAX_ITEM_BYTES).enumerate() {
            if let Err(err) = one.write(service, &part(account, generation, n), piece) {
                remove_parts(one, service, account, generation, n + 1);
                return Err(err);
            }
        }
        if let Err(err) = one.write(service, account, &header.to_bytes()) {
            remove_parts(one, service, account, generation, header.parts);
            return Err(err);
        }
    }
    if let Some(old) = old {
        remove_parts(one, service, account, old.generation, old.parts);
    }
    Ok(())
}

/// Remove the value: the head first (then nothing reads it), then its parts.
pub(super) fn delete(one: &dyn Items, service: &str, account: &str) -> Result<(), String> {
    let old = one
        .read(service, account)
        .ok()
        .flatten()
        .and_then(|bytes| Header::parse(&bytes).ok().flatten());
    one.delete(service, account)?;
    if let Some(old) = old {
        remove_parts(one, service, account, old.generation, old.parts);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    /// Credentials in memory that keep at most `MAX_ITEM_BYTES` each, as the
    /// Credential Manager does; the n-th write from now (0-based) can be made
    /// to fail.
    #[derive(Default)]
    struct Fake {
        items: Mutex<BTreeMap<String, Vec<u8>>>,
        fail_write: Mutex<Option<usize>>,
        writes: Mutex<usize>,
    }

    impl Fake {
        fn names(&self) -> Vec<String> {
            self.items.lock().unwrap().keys().cloned().collect()
        }
        fn raw(&self, name: &str) -> Option<Vec<u8>> {
            self.items.lock().unwrap().get(name).cloned()
        }
        fn put(&self, name: &str, value: &[u8]) {
            self.items
                .lock()
                .unwrap()
                .insert(name.to_owned(), value.to_vec());
        }
        fn fail_write_after(&self, n: usize) {
            *self.writes.lock().unwrap() = 0;
            *self.fail_write.lock().unwrap() = Some(n);
        }
    }

    impl Items for Fake {
        fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.raw(&format!("{service}/{account}")))
        }
        fn write(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
            assert!(value.len() <= MAX_ITEM_BYTES, "one credential overfilled");
            let mut writes = self.writes.lock().unwrap();
            let now = *writes;
            *writes += 1;
            if *self.fail_write.lock().unwrap() == Some(now) {
                return Err("Credential Manager: cannot write".to_owned());
            }
            self.put(&format!("{service}/{account}"), value);
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> Result<(), String> {
            self.items
                .lock()
                .unwrap()
                .remove(&format!("{service}/{account}"));
            Ok(())
        }
    }

    fn bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 ^ seed).collect()
    }

    #[test]
    fn a_value_that_fits_is_stored_as_earlier_builds_stored_it() {
        let fake = Fake::default();
        let doc = br#"{"version":1,"secrets":{}}"#;
        write(&fake, "s", "a", doc).unwrap();
        assert_eq!(fake.names(), ["s/a"]);
        assert_eq!(fake.raw("s/a").unwrap(), doc);
        assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), doc);

        let full = bytes(MAX_ITEM_BYTES, 1);
        write(&fake, "s", "a", &full).unwrap();
        assert_eq!(fake.names(), ["s/a"], "exactly the limit still fits one");
        assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), full);
    }

    #[test]
    fn an_old_single_item_store_reads_as_it_is() {
        let fake = Fake::default();
        fake.put(
            "connecto/secrets",
            br#"{"version":1,"secrets":{"jev.api-key":"k"}}"#,
        );
        assert_eq!(
            read(&fake, "connecto", "secrets").unwrap().unwrap(),
            br#"{"version":1,"secrets":{"jev.api-key":"k"}}"#
        );
        assert_eq!(read(&fake, "connecto", "other").unwrap(), None);
    }

    #[test]
    fn a_larger_value_is_split_and_read_back_whole() {
        let fake = Fake::default();
        let big = bytes(MAX_ITEM_BYTES * 2 + 7, 2);
        write(&fake, "s", "a", &big).unwrap();
        assert_eq!(fake.names(), ["s/a", "s/a.1.0", "s/a.1.1", "s/a.1.2"]);
        assert!(fake.raw("s/a").unwrap().starts_with(MAGIC));
        assert_eq!(fake.raw("s/a.1.2").unwrap().len(), 7);
        assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), big);

        let most = bytes(MAX_VALUE_BYTES, 3);
        write(&fake, "s", "a", &most).unwrap();
        assert_eq!(
            fake.names().len(),
            1 + MAX_PARTS,
            "the old generation is gone"
        );
        assert!(
            fake.names()
                .iter()
                .all(|n| n == "s/a" || n.starts_with("s/a.2."))
        );
        assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), most);
    }

    #[test]
    fn a_small_value_that_looks_like_a_head_is_split_too() {
        let fake = Fake::default();
        let mut tricky = MAGIC.to_vec();
        tricky.extend_from_slice(b"1 9 1 3 0000000000000000");
        write(&fake, "s", "a", &tricky).unwrap();
        assert_eq!(fake.names(), ["s/a", "s/a.1.0"]);
        assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), tricky);
    }

    #[test]
    fn too_large_is_refused_before_anything_is_written() {
        let fake = Fake::default();
        write(&fake, "s", "a", b"kept").unwrap();
        let secret = "do-not-echo-this-secret";
        let mut over = secret
            .repeat(MAX_VALUE_BYTES / secret.len() + 1)
            .into_bytes();
        over.truncate(MAX_VALUE_BYTES + 1);
        let err = write(&fake, "s", "a", &over).unwrap_err();
        assert!(err.contains(&MAX_VALUE_BYTES.to_string()), "{err}");
        assert!(!err.contains(secret), "the error holds the secret");
        assert_eq!(fake.names(), ["s/a"]);
        assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), b"kept");
    }

    #[test]
    fn a_failed_part_or_head_keeps_the_old_value_and_leaves_no_new_part() {
        for old in [bytes(10, 4), bytes(MAX_ITEM_BYTES * 3, 5)] {
            // Writes: the parts 0..3 of the new value, then the head.
            for fail in 0..=3 {
                let fake = Fake::default();
                write(&fake, "s", "a", &old).unwrap();
                let before = fake.names();
                fake.fail_write_after(fail);
                let new = bytes(MAX_ITEM_BYTES * 2 + 1, 6);
                assert!(write(&fake, "s", "a", &new).is_err());
                assert_eq!(fake.names(), before, "fail at write {fail}");
                assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), old);
            }
        }
    }

    #[test]
    fn going_back_to_one_item_and_deleting_remove_the_parts() {
        let fake = Fake::default();
        write(&fake, "s", "a", &bytes(MAX_ITEM_BYTES + 1, 7)).unwrap();
        write(&fake, "s", "a", b"small").unwrap();
        assert_eq!(fake.names(), ["s/a"]);
        assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), b"small");

        write(&fake, "s", "a", &bytes(MAX_ITEM_BYTES * 4, 8)).unwrap();
        delete(&fake, "s", "a").unwrap();
        assert!(fake.names().is_empty());
        assert_eq!(read(&fake, "s", "a").unwrap(), None);
        delete(&fake, "s", "a").unwrap();
    }

    #[test]
    fn a_missing_or_wrong_part_or_an_unknown_head_is_an_error_not_a_shorter_value() {
        let fake = Fake::default();
        let big = bytes(MAX_ITEM_BYTES * 2, 9);
        write(&fake, "s", "a", &big).unwrap();

        let part1 = fake.raw("s/a.1.1").unwrap();
        fake.put("s/a.1.1", &bytes(MAX_ITEM_BYTES, 10));
        assert!(read(&fake, "s", "a").unwrap_err().contains("do not match"));
        fake.put("s/a.1.1", &part1[..10]);
        assert!(read(&fake, "s", "a").is_err());
        fake.items.lock().unwrap().remove("s/a.1.1");
        assert!(
            read(&fake, "s", "a")
                .unwrap_err()
                .contains("part 1 is missing")
        );
        fake.put("s/a.1.1", &part1);
        assert_eq!(read(&fake, "s", "a").unwrap().unwrap(), big);

        for head in [&b"2 1 1 1 0"[..], b"1 1 99 5 0", b"1 x 1 1 0", b"1 1 1 1"] {
            let mut bad = MAGIC.to_vec();
            bad.extend_from_slice(head);
            fake.put("s/a", &bad);
            assert!(read(&fake, "s", "a").is_err(), "{head:?}");
        }
    }

    /// The Credential Manager as `CredentialManagerItems` uses it, over the fake.
    struct Split(std::sync::Arc<Fake>);

    impl Items for Split {
        fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
            read(&*self.0, service, account)
        }
        fn write(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
            write(&*self.0, service, account, value)
        }
        fn delete(&self, service: &str, account: &str) -> Result<(), String> {
            delete(&*self.0, service, account)
        }
    }

    #[test]
    fn the_secrets_of_many_paired_machines_fit_and_a_new_launch_reads_them() {
        let fake = std::sync::Arc::new(Fake::default());
        // An Ed25519 key in OpenSSH text is about 400 bytes, as pairing stores it.
        let key = |n: usize| {
            format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
                format!("{n:04}").repeat(85)
            )
        };
        let secrets = crate::Secrets::new(std::sync::Arc::new(Split(fake.clone())), None);
        secrets
            .set_bytes(crate::REMOTE_DEVICE_KEY, &[7; 32])
            .unwrap();
        for n in 0..40 {
            secrets
                .set(&format!("remote.pair.{n:064x}"), &key(n))
                .unwrap();
        }
        assert!(fake.raw("connecto/secrets").unwrap().starts_with(MAGIC));
        assert!(fake.names().len() > 2, "split across parts");

        let next = crate::Secrets::new(std::sync::Arc::new(Split(fake.clone())), None);
        assert_eq!(
            next.get_bytes(crate::REMOTE_DEVICE_KEY).unwrap().unwrap(),
            [7; 32]
        );
        for n in 0..40 {
            assert_eq!(
                next.get(&format!("remote.pair.{n:064x}")).unwrap().unwrap(),
                key(n)
            );
        }
        // Unpairing them all goes back to one credential.
        for n in 0..40 {
            next.delete(&format!("remote.pair.{n:064x}")).unwrap();
        }
        assert_eq!(fake.names(), ["connecto/secrets"]);
    }

    #[test]
    fn the_header_round_trips() {
        let header = Header::of(42, &bytes(MAX_ITEM_BYTES * 3 + 1, 11));
        assert_eq!(header.parts, 4);
        assert_eq!(Header::parse(&header.to_bytes()).unwrap(), Some(header));
        assert_eq!(Header::parse(b"{\"version\":1}").unwrap(), None);
        assert!(header.to_bytes().len() < MAX_ITEM_BYTES);
    }
}
