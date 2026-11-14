use super::*;

fn store() -> (Arc<MemoryItems>, Secrets) {
    let items = Arc::new(MemoryItems::default());
    let secrets = Secrets::new(items.clone(), None);
    (items, secrets)
}

fn document(items: &MemoryItems) -> Value {
    serde_json::from_slice(&items.peek(SERVICE, ACCOUNT).unwrap()).unwrap()
}

fn put(items: &MemoryItems, value: &[u8]) {
    items.put(SERVICE, ACCOUNT, value);
}

fn doc_with(secrets: &str) -> Vec<u8> {
    format!(r#"{{"version":1,"secrets":{secrets}}}"#).into_bytes()
}

/// A folder of its own for one test, under the system temp folder.
fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "connecto-secrets-{name}-{}-{}",
        std::process::id(),
        new_stamp()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---- the document

#[test]
fn text_and_binary_round_trip_and_the_item_is_the_documented_json() {
    let (items, secrets) = store();
    secrets.set(JEV_API_KEY, "jev-123").unwrap();
    secrets
        .set_bytes(REMOTE_DEVICE_KEY, &[0, 1, 2, 250, 251, 252, 255])
        .unwrap();
    assert_eq!(
        secrets.get(JEV_API_KEY).unwrap().as_deref(),
        Some("jev-123")
    );
    assert_eq!(
        secrets.get_bytes(REMOTE_DEVICE_KEY).unwrap().unwrap(),
        [0, 1, 2, 250, 251, 252, 255]
    );
    assert_eq!(secrets.get("nothing.here").unwrap(), None);
    let doc = document(&items);
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["secrets"]["jev.api-key"], "jev-123");
    assert_eq!(doc["secrets"]["remote.device-key"], "AAEC+vv8/w==");
    let again = Secrets::new(items, None);
    assert_eq!(again.get(JEV_API_KEY).unwrap().as_deref(), Some("jev-123"));
}

#[test]
fn base64_matches_the_standard_and_refuses_damage() {
    for (raw, text) in [
        (&b""[..], ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
    ] {
        assert_eq!(b64_encode(raw), text);
        assert_eq!(b64_decode(text).unwrap(), raw);
    }
    assert!(b64_decode("Zg=").is_none());
    assert!(b64_decode("Z$==").is_none());
}

#[test]
fn the_document_is_read_once_per_process_however_many_secrets_are_asked_for() {
    let (items, secrets) = store();
    put(&items, &doc_with(r#"{"a":"1","b":"2"}"#));
    for key in ["a", "b", "a", "c"] {
        let _ = secrets.get(key).unwrap();
    }
    assert_eq!(items.reads(), 1);
    assert_eq!(items.writes_of(SERVICE, ACCOUNT), 0);
}

#[test]
fn concurrent_first_callers_share_one_read() {
    let (items, secrets) = store();
    put(&items, &doc_with(r#"{"jev.api-key":"k"}"#));
    items.on_read(|_, _| std::thread::sleep(std::time::Duration::from_millis(100)));
    let secrets = Arc::new(secrets);
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let (secrets, barrier) = (Arc::clone(&secrets), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                secrets.get(JEV_API_KEY).unwrap()
            })
        })
        .collect();
    for t in threads {
        assert_eq!(t.join().unwrap().as_deref(), Some("k"));
    }
    assert_eq!(items.reads(), 1);
}

#[test]
fn the_shared_store_is_one_instance_per_process() {
    assert!(Arc::ptr_eq(&Secrets::system(), &Secrets::system()));
}

#[test]
fn nothing_is_read_until_a_secret_is_asked_for() {
    let (items, _secrets) = store();
    assert_eq!(items.reads(), 0);
}

#[test]
fn unknown_keys_and_fields_survive_a_rewrite() {
    let (items, secrets) = store();
    put(
        &items,
        br#"{"version":1,"future":{"x":[1,2]},"secrets":{"ssh.example":"pem","remote.relay-token":"t"},"migrated":["jev.api-key"]}"#,
    );
    secrets.set(JEV_API_KEY, "new").unwrap();
    secrets.delete(REMOTE_RELAY_TOKEN).unwrap();
    let doc = document(&items);
    assert_eq!(doc["future"], json!({"x": [1, 2]}));
    assert_eq!(doc["migrated"], json!(["jev.api-key"]));
    assert_eq!(doc["secrets"]["ssh.example"], "pem");
    assert_eq!(doc["secrets"]["jev.api-key"], "new");
    assert!(doc["secrets"].get("remote.relay-token").is_none());
}

#[test]
fn delete_removes_the_key_and_is_fine_when_it_is_not_there() {
    let (items, secrets) = store();
    secrets.set("k", "v").unwrap();
    secrets.delete("k").unwrap();
    secrets.delete("k").unwrap();
    assert_eq!(secrets.get("k").unwrap(), None);
    assert!(document(&items)["secrets"].get("k").is_none());
}

#[test]
fn a_change_that_changes_nothing_writes_nothing_and_reads_nothing() {
    let (items, secrets) = store();
    put(&items, &doc_with(r#"{"a":"1"}"#));
    assert_eq!(secrets.get("a").unwrap().as_deref(), Some("1"));
    secrets.delete("remote.staged.00ff").unwrap();
    secrets.set("a", "1").unwrap();
    assert_eq!(items.reads(), 1);
    assert_eq!(items.writes_of(SERVICE, ACCOUNT), 0);
}

#[test]
fn a_corrupt_document_is_never_overwritten() {
    for bad in [
        &b"not json at all"[..],
        b"[1,2]",
        br#"{"version":1}"#,
        br#"{"secrets":[]}"#,
        br#"{"secrets":{}}"#,
        br#"{"version":"1","secrets":{}}"#,
        br#"{"version":0,"secrets":{}}"#,
        br#"{"version":1.5,"secrets":{}}"#,
    ] {
        let (items, secrets) = store();
        put(&items, bad);
        let err = secrets.get("k").unwrap_err();
        assert!(err.contains("left as they are"), "{err}");
        assert!(secrets.set("k", "v").is_err());
        assert!(secrets.delete("k").is_err());
        assert_eq!(items.peek(SERVICE, ACCOUNT).unwrap(), bad);
        assert!(Secrets::new(items.clone(), None).set("k", "v").is_err());
        assert_eq!(items.peek(SERVICE, ACCOUNT).unwrap(), bad);
    }
}

#[test]
fn a_failed_read_is_an_error_not_an_empty_store() {
    let (items, secrets) = store();
    put(&items, &doc_with(r#"{"jev.api-key":"k"}"#));
    items.deny(SERVICE, ACCOUNT, true);
    assert!(secrets.get(JEV_API_KEY).is_err());
    assert!(secrets.set(JEV_API_KEY, "other").is_err());
    // Not asked again in this run.
    assert_eq!(items.reads(), 1);
    items.deny(SERVICE, ACCOUNT, false);
    assert_eq!(document(&items)["secrets"]["jev.api-key"], "k");
}

#[test]
fn a_secret_that_is_not_text_is_an_error_and_is_never_replaced() {
    let (items, secrets) = store();
    let body = br#"{"version":1,"secrets":{"remote.device-key":123,"other":"fine"}}"#;
    put(&items, body);
    assert!(
        secrets
            .get_bytes(REMOTE_DEVICE_KEY)
            .unwrap_err()
            .contains("not text")
    );
    assert!(secrets.set_bytes(REMOTE_DEVICE_KEY, &[1; 32]).is_err());
    assert!(secrets.delete(REMOTE_DEVICE_KEY).is_err());
    assert_eq!(secrets.get("other").unwrap().as_deref(), Some("fine"));
    assert_eq!(items.peek(SERVICE, ACCOUNT).unwrap(), body);
    secrets.set("another", "x").unwrap();
    assert_eq!(document(&items)["secrets"]["remote.device-key"], 123);
}

#[test]
fn a_binary_secret_that_is_not_base64_is_an_error_not_an_absence() {
    let (items, secrets) = store();
    put(
        &items,
        br#"{"version":1,"secrets":{"remote.device-key":"not base64!"}}"#,
    );
    assert!(
        secrets
            .get_bytes(REMOTE_DEVICE_KEY)
            .unwrap_err()
            .contains("base64")
    );
}

#[test]
fn a_document_of_a_newer_version_is_read_for_what_is_known_and_never_written() {
    let (items, secrets) = store();
    let body = br#"{"version":2,"secrets":{"jev.api-key":"k"},"extra":{"a":1}}"#;
    put(&items, body);
    assert_eq!(secrets.get(JEV_API_KEY).unwrap().as_deref(), Some("k"));
    assert!(secrets.set("x", "y").unwrap_err().contains("newer"));
    assert!(secrets.delete(JEV_API_KEY).is_err());
    assert_eq!(items.peek(SERVICE, ACCOUNT).unwrap(), body);
}

// ---- more than one process

#[test]
fn two_handles_setting_different_keys_at_once_both_keep_theirs() {
    let items = Arc::new(MemoryItems::default());
    let dir = temp("twohandles");
    let lock = dir.join(".secrets.lock");
    let a = Arc::new(Secrets::new(items.clone(), Some(lock.clone())));
    let b = Arc::new(Secrets::new(items.clone(), Some(lock)));
    assert_eq!(a.get("x").unwrap(), None);
    assert_eq!(b.get("x").unwrap(), None);
    let threads: Vec<_> = (0..20)
        .map(|n| {
            let handle = if n % 2 == 0 {
                Arc::clone(&a)
            } else {
                Arc::clone(&b)
            };
            std::thread::spawn(move || handle.set(&format!("key.{n}"), &format!("v{n}")).unwrap())
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let fresh = Secrets::new(items, None);
    for n in 0..20 {
        assert_eq!(
            fresh.get(&format!("key.{n}")).unwrap().as_deref(),
            Some(format!("v{n}").as_str()),
            "key.{n}"
        );
    }
}

#[test]
fn a_set_is_one_write_when_nobody_else_wrote_since() {
    let items = Arc::new(MemoryItems::default());
    put(&items, &doc_with("{}"));
    let dir = temp("onewrite");
    let secrets = Secrets::new(items.clone(), Some(dir.join(".secrets.lock")));
    assert_eq!(secrets.get("x").unwrap(), None);
    // The first change reads again: there is no stamp yet.
    secrets.set("a", "1").unwrap();
    let reads = items.reads();
    secrets.set("b", "2").unwrap();
    secrets.delete("a").unwrap();
    assert_eq!(items.reads(), reads, "no read before the writes");
    assert_eq!(items.writes_of(SERVICE, ACCOUNT), 3);
    let doc = document(&items);
    assert_eq!(doc["secrets"]["b"], "2");
    assert!(doc["secrets"].get("a").is_none());
}

#[test]
fn a_write_by_another_process_is_read_before_the_next_change() {
    let items = Arc::new(MemoryItems::default());
    let dir = temp("other");
    let lock = dir.join(".secrets.lock");
    let a = Secrets::new(items.clone(), Some(lock.clone()));
    let b = Secrets::new(items.clone(), Some(lock));
    a.set("from.a", "1").unwrap();
    assert_eq!(b.get("from.a").unwrap().as_deref(), Some("1"));
    a.set("again.a", "2").unwrap();
    b.set("from.b", "3").unwrap();
    let doc = document(&items);
    assert_eq!(doc["secrets"]["from.a"], "1");
    assert_eq!(doc["secrets"]["again.a"], "2");
    assert_eq!(doc["secrets"]["from.b"], "3");
}

// ---- the file

#[test]
fn the_file_is_written_whole_and_private() {
    let dir = temp("file");
    let path = secrets_file(&dir);
    let secrets = Secrets::new(
        Arc::new(FileItems::new(path.clone())),
        Some(dir.join(".secrets.lock")),
    );
    assert_eq!(secrets.get(JEV_API_KEY).unwrap(), None);
    assert!(!path.exists(), "nothing written for a read");
    secrets.set(JEV_API_KEY, "k").unwrap();
    secrets.set_bytes(REMOTE_DEVICE_KEY, &[9; 32]).unwrap();
    let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(doc["secrets"]["jev.api-key"], "k");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "the file");
        assert_eq!(mode(path.parent().unwrap()), 0o700, "its folder");
    }
    // No temporary file is left beside it.
    let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [FILE]);
    let again = Secrets::new(Arc::new(FileItems::new(path)), None);
    assert_eq!(
        again.get_bytes(REMOTE_DEVICE_KEY).unwrap().unwrap(),
        [9; 32]
    );
}

#[test]
fn the_file_keeps_only_the_document() {
    let dir = temp("fileonly");
    let file = FileItems::new(secrets_file(&dir));
    assert_eq!(file.read("other", "item").unwrap(), None);
    assert!(file.write("other", "item", b"x").is_err());
    file.delete("other", "item").unwrap();
    file.delete(SERVICE, ACCOUNT).unwrap();
}

// ---- the move from the other place

fn moving() -> (Arc<MemoryItems>, Arc<MemoryItems>, Secrets) {
    let target = Arc::new(MemoryItems::default());
    let previous = Arc::new(MemoryItems::default());
    let secrets = Secrets::moving_from(target.clone(), Some(previous.clone()), None);
    (target, previous, secrets)
}

#[test]
fn the_old_document_is_moved_once_and_then_removed() {
    let (target, previous, secrets) = moving();
    let old =
        br#"{"version":1,"secrets":{"remote.device-key":"BwcHBw==","x":"y"},"migrated":["a"]}"#;
    previous.put(SERVICE, ACCOUNT, old);
    assert_eq!(
        secrets.get_bytes(REMOTE_DEVICE_KEY).unwrap().unwrap(),
        [7; 4]
    );
    assert_eq!(
        target.peek(SERVICE, ACCOUNT).unwrap(),
        old,
        "moved as it was"
    );
    assert!(previous.peek(SERVICE, ACCOUNT).is_none(), "removed");
    // The next launch reads the new place only.
    let reads = previous.reads();
    let next = Secrets::moving_from(target.clone(), Some(previous.clone()), None);
    assert_eq!(next.get("x").unwrap().as_deref(), Some("y"));
    assert_eq!(previous.reads(), reads);
}

#[test]
fn with_nothing_to_move_an_empty_document_is_written_so_the_other_place_is_not_asked_again() {
    let (target, previous, secrets) = moving();
    assert_eq!(secrets.get(JEV_API_KEY).unwrap(), None);
    assert_eq!(previous.reads(), 1);
    assert!(target.peek(SERVICE, ACCOUNT).is_some());
    let next = Secrets::moving_from(target, Some(previous.clone()), None);
    assert_eq!(next.get(JEV_API_KEY).unwrap(), None);
    assert_eq!(previous.reads(), 1, "never again");
}

#[test]
fn a_refused_old_document_is_tried_once_and_the_store_starts_empty() {
    let (target, previous, secrets) = moving();
    previous.put(SERVICE, ACCOUNT, &doc_with(r#"{"jev.api-key":"k"}"#));
    previous.deny(SERVICE, ACCOUNT, true);
    assert_eq!(secrets.get(JEV_API_KEY).unwrap(), None);
    let doc = document(&target);
    assert!(doc["moved"].as_str().unwrap().starts_with("not moved"));
    assert_eq!(doc["secrets"], json!({}));
    // Left where it was, and never asked for again.
    assert!(previous.peek(SERVICE, ACCOUNT).is_some());
    previous.deny(SERVICE, ACCOUNT, false);
    let next = Secrets::moving_from(target, Some(previous.clone()), None);
    assert_eq!(next.get(JEV_API_KEY).unwrap(), None);
    assert_eq!(previous.reads(), 1);
    // New secrets go to the new place as usual.
    next.set(JEV_API_KEY, "new").unwrap();
    assert_eq!(next.get(JEV_API_KEY).unwrap().as_deref(), Some("new"));
}

#[test]
fn an_old_document_that_cannot_be_read_is_left_and_not_moved() {
    let (target, previous, secrets) = moving();
    previous.put(SERVICE, ACCOUNT, b"garbage");
    assert_eq!(secrets.get(JEV_API_KEY).unwrap(), None);
    assert_eq!(previous.peek(SERVICE, ACCOUNT).unwrap(), b"garbage");
    assert!(document(&target)["moved"].is_string());
}

#[test]
fn a_move_whose_write_fails_changes_nothing_and_is_tried_at_the_next_launch() {
    let (target, previous, secrets) = moving();
    previous.put(SERVICE, ACCOUNT, &doc_with(r#"{"jev.api-key":"k"}"#));
    target.fail_writes(true);
    assert!(secrets.get(JEV_API_KEY).is_err());
    assert!(previous.peek(SERVICE, ACCOUNT).is_some(), "not removed");
    target.fail_writes(false);
    let next = Secrets::moving_from(target, Some(previous.clone()), None);
    assert_eq!(next.get(JEV_API_KEY).unwrap().as_deref(), Some("k"));
    assert!(previous.peek(SERVICE, ACCOUNT).is_none());
}

#[test]
fn a_document_already_in_the_new_place_wins_and_the_old_place_is_not_read() {
    let (target, previous, secrets) = moving();
    target.put(SERVICE, ACCOUNT, &doc_with(r#"{"jev.api-key":"new"}"#));
    previous.put(SERVICE, ACCOUNT, &doc_with(r#"{"jev.api-key":"old"}"#));
    assert_eq!(secrets.get(JEV_API_KEY).unwrap().as_deref(), Some("new"));
    assert_eq!(previous.reads(), 0);
}

#[test]
fn the_move_into_a_file_works_end_to_end() {
    let dir = temp("movefile");
    let previous = Arc::new(MemoryItems::default());
    previous.put(SERVICE, ACCOUNT, &doc_with(r#"{"remote.relay-token":"t"}"#));
    let path = secrets_file(&dir);
    let secrets = Secrets::moving_from(
        Arc::new(FileItems::new(path.clone())),
        Some(previous.clone()),
        Some(dir.join(".secrets.lock")),
    );
    assert_eq!(
        secrets.get(REMOTE_RELAY_TOKEN).unwrap().as_deref(),
        Some("t")
    );
    assert!(path.is_file());
    assert!(previous.peek(SERVICE, ACCOUNT).is_none());
    // And the next change needs no read: the move left a stamp.
    secrets.set("k", "v").unwrap();
    assert_eq!(previous.reads(), 1);
}
