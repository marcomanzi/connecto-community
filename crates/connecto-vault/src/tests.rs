#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::mpsc;

use connecto_core::{
    AskAnswer, AskKind, AskRequest, Binding, ProviderScope, SecretCaller, VaultBridge, VaultControl,
};
use connecto_secrets::{MemoryItems, Secrets};

use super::*;

const VALUE: &str = "ghp_examplevalue_0123456789";
const PROJECT: &str = "0f8bb1bcbbc9990b";

fn memory() -> (Arc<MemoryItems>, Arc<Secrets>) {
    let items = Arc::new(MemoryItems::default());
    let secrets = Arc::new(Secrets::new(Arc::clone(&items) as _, None));
    (items, secrets)
}

fn key() -> Arc<dyn MasterKey> {
    Arc::new(FixedKey([3; 32]))
}

fn open(secrets: Arc<Secrets>, dir: Option<&Path>) -> Vault {
    Vault::open(secrets, key(), dir)
}

fn project() -> ProviderScope {
    ProviderScope::Project(PROJECT.into())
}

fn gh_binding() -> Binding {
    Binding::Http {
        base_url: "https://api.github.com".into(),
        header: "Authorization".into(),
        prefix: "Bearer ".into(),
    }
}

/// A terminal of the project whose processes are this test's.
fn caller(folder: Option<&Path>) -> SecretCaller {
    let me = i32::try_from(std::process::id()).unwrap();
    SecretCaller {
        terminal: "claude-1".into(),
        project: Some(PROJECT.into()),
        project_folder: folder.map(Path::to_path_buf),
        owns_pid: Arc::new(move |pid| pid == me),
    }
}

#[test]
fn a_put_keeps_the_value_encrypted_and_only_the_name_in_the_view() {
    let (_, secrets) = memory();
    let vault = open(Arc::clone(&secrets), None);
    vault
        .put(
            &ProviderScope::Global,
            "GITHUB",
            "for gh",
            &format!("{VALUE}\n"),
        )
        .unwrap();
    let stored = secrets.get("user.global.GITHUB").unwrap().unwrap();
    assert!(stored.starts_with("vault:v1:"));
    assert!(!stored.contains(VALUE));
    let view = vault.view();
    assert_eq!(view.entries.len(), 1);
    assert!(!format!("{view:?}").contains(VALUE));
    let (_, value) = vault.value_for_use(None, "GITHUB").unwrap();
    assert_eq!(value, VALUE);
}

#[test]
fn a_value_that_does_not_open_is_said_and_not_used() {
    let (_, secrets) = memory();
    let dir = tempfile::tempdir().unwrap();
    let first = Vault::open(Arc::clone(&secrets), key(), Some(dir.path()));
    first
        .put(&ProviderScope::Global, "GITHUB", "", VALUE)
        .unwrap();
    // The same entries with another key: as if the Keychain item were lost.
    let other = Vault::open(secrets, Arc::new(FixedKey([9; 32])), Some(dir.path()));
    assert!(other.view().problem.as_ref().unwrap().contains("GITHUB"));
    assert!(other.value_for_use(None, "GITHUB").is_err());
    assert!(other.known().is_empty());
}

#[test]
fn the_bridge_lists_names_and_hides_every_value_form() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    vault.put(&project(), "GITHUB", "", VALUE).unwrap();
    let visible = vault.visible(Some(PROJECT));
    assert_eq!(visible.len(), 1);
    assert!(!format!("{visible:?}").contains(VALUE));
    assert!(vault.visible(Some("another")).is_empty());
    let known = vault.known();
    assert!(known.iter().any(|form| form == VALUE));
    assert!(known.len() > 1, "encoded forms are hidden too");
}

#[test]
fn a_bad_name_or_value_is_refused_and_nothing_is_stored() {
    let (items, secrets) = memory();
    let vault = open(secrets, None);
    assert!(
        vault
            .put(&ProviderScope::Global, "bad name", "", VALUE)
            .is_err()
    );
    assert!(
        vault
            .put(&ProviderScope::Global, "OK", "", "short")
            .is_err()
    );
    assert!(
        vault
            .put(&ProviderScope::Project("../x".into()), "OK", "", VALUE)
            .is_err()
    );
    assert_eq!(
        items.writes_of(connecto_secrets::SERVICE, connecto_secrets::ACCOUNT),
        0
    );
    assert!(vault.view().entries.is_empty());
}

#[test]
fn a_delete_removes_entry_and_value_and_stops_hiding_it() {
    let (_, secrets) = memory();
    let vault = open(Arc::clone(&secrets), None);
    vault
        .put(&ProviderScope::Global, "GITHUB", "", VALUE)
        .unwrap();
    vault.delete(&ProviderScope::Global, "GITHUB").unwrap();
    assert!(vault.view().entries.is_empty());
    assert_eq!(secrets.get("user.global.GITHUB").unwrap(), None);
    assert!(vault.known().is_empty());
    vault.delete(&ProviderScope::Global, "GITHUB").unwrap();
}

#[test]
fn bindings_are_checked_and_kept() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    vault
        .put(&ProviderScope::Global, "GITHUB", "", VALUE)
        .unwrap();
    let shell = Binding::Command {
        programs: vec!["/bin/sh".into()],
        env: "GH_TOKEN".into(),
    };
    assert!(
        vault
            .set_bindings(&ProviderScope::Global, "GITHUB", vec![shell])
            .is_err()
    );
    // Not an SSH key: it cannot be used as one.
    assert!(
        vault
            .set_bindings(&ProviderScope::Global, "GITHUB", vec![Binding::Ssh])
            .is_err()
    );
    assert!(vault.view().entries[0].bindings.is_empty());
    vault
        .set_bindings(&ProviderScope::Global, "GITHUB", vec![gh_binding()])
        .unwrap();
    assert_eq!(vault.view().entries[0].bindings, [gh_binding()]);
}

#[test]
fn the_metadata_survives_a_restart_and_holds_no_value() {
    let dir = tempfile::tempdir().unwrap();
    let (_, secrets) = memory();
    {
        let vault = open(Arc::clone(&secrets), Some(dir.path()));
        vault.put(&project(), "GITHUB", "for gh", VALUE).unwrap();
        vault
            .set_bindings(&project(), "GITHUB", vec![gh_binding()])
            .unwrap();
    }
    let meta = meta_file(dir.path());
    let text = std::fs::read_to_string(&meta).unwrap();
    assert!(!text.contains(VALUE));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&meta).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let again = open(secrets, Some(dir.path()));
    let entries = again.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].bindings, [gh_binding()]);
    assert!(again.known().iter().any(|form| form == VALUE));
}

/// A terminal cannot propose a program path the person's form would turn
/// into other paths (one per line, trimmed).
#[test]
fn a_request_cannot_propose_a_path_the_form_would_split() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    for path in [
        "/opt/homebrew/bin/gh\n/opt/tools/deploy",
        "/opt/homebrew/bin/gh ",
    ] {
        let request = AskRequest {
            proposed: vec![Binding::Command {
                programs: vec![path.into()],
                env: "GH_TOKEN".into(),
            }],
            ..plain_request("GH", CLEAR)
        };
        assert!(
            vault.ask(&caller(None), request, Box::new(|_| {})).is_err(),
            "{path:?}"
        );
    }
    assert!(vault.view().asks.is_empty());
}

/// A use saved by an earlier version, with a program path the request
/// form would refuse today (two spaces side by side), still loads, and so
/// does every other secret.
#[test]
fn a_saved_use_from_before_the_form_rule_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let (_, secrets) = memory();
    let tool = |path: &str| Binding::Command {
        programs: vec![path.to_owned()],
        env: "TOOL_TOKEN".into(),
    };
    {
        let vault = open(Arc::clone(&secrets), Some(dir.path()));
        vault.put(&project(), "TOOL", "", VALUE).unwrap();
        vault
            .set_bindings(
                &project(),
                "TOOL",
                vec![tool("/Applications/My Tool/bin/x")],
            )
            .unwrap();
        vault
            .put(&project(), "GITHUB", "", "another-value-1")
            .unwrap();
    }
    let meta = meta_file(dir.path());
    let text = std::fs::read_to_string(&meta).unwrap();
    std::fs::write(&meta, text.replace("My Tool", "My  Tool")).unwrap();
    let again = open(secrets, Some(dir.path()));
    assert!(again.view().problem.is_none(), "{:?}", again.view().problem);
    let entries = again.entries();
    assert_eq!(entries.len(), 2);
    let saved = entries.iter().find(|e| e.name == "TOOL").unwrap();
    assert_eq!(saved.bindings, [tool("/Applications/My  Tool/bin/x")]);
    assert!(again.value_for_use(Some(PROJECT), "GITHUB").is_ok());
}

#[test]
fn an_unreadable_metadata_file_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta_file(dir.path());
    std::fs::create_dir_all(meta.parent().unwrap()).unwrap();
    std::fs::write(&meta, "{\"version\": 9}").unwrap();
    let (_, secrets) = memory();
    let vault = open(secrets, Some(dir.path()));
    assert!(vault.view().problem.is_some());
    assert!(vault.put(&ProviderScope::Global, "A", "", VALUE).is_err());
    assert_eq!(std::fs::read_to_string(&meta).unwrap(), "{\"version\": 9}");
}

#[test]
fn a_project_secret_overrides_the_app_wide_one_of_its_name_for_use() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    vault
        .put(&ProviderScope::Global, "TOKEN", "", "global-value-1")
        .unwrap();
    vault
        .put(&project(), "TOKEN", "", "project-value-1")
        .unwrap();
    let (entry, value) = vault.value_for_use(Some(PROJECT), "TOKEN").unwrap();
    assert_eq!(entry.scope, project());
    assert_eq!(value, "project-value-1");
    let (_, value) = vault.value_for_use(Some("other"), "TOKEN").unwrap();
    assert_eq!(value, "global-value-1");
    assert!(vault.value_for_use(Some("other"), "NOPE").is_err());
}

// ---------- requests from terminals

fn ask(vault: &Vault, name: &str, kind: AskKind) -> (u64, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let id = vault
        .ask(
            &caller(None),
            AskRequest {
                name: name.into(),
                kind,
                reason: "to deploy\x1b[31m the site to Fly.io with flyctl".into(),
                proposed: vec![gh_binding()],
            },
            Box::new(move |text| {
                let _ = tx.send(text);
            }),
        )
        .unwrap();
    (id, rx)
}

#[test]
fn a_request_waits_for_the_person_and_the_terminal_learns_how_to_use_it() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    let (id, told) = ask(&vault, "GITHUB", AskKind::Token);
    let view = vault.view();
    assert_eq!(view.asks.len(), 1);
    assert!(!view.asks[0].reason.contains('\x1b'));
    assert!(told.try_recv().is_err(), "nothing is said before an answer");
    // Not into another project's secrets.
    assert!(
        vault
            .answer(
                id,
                AskAnswer::Provide {
                    scope: ProviderScope::Project("other".into()),
                    value: VALUE.into(),
                    bindings: vec![gh_binding()],
                }
            )
            .is_err()
    );
    vault
        .answer(
            id,
            AskAnswer::Provide {
                scope: project(),
                value: VALUE.into(),
                bindings: vec![gh_binding()],
            },
        )
        .unwrap();
    let text = told.recv().unwrap();
    assert!(
        text.contains("GITHUB") && text.contains("connecto secret url"),
        "{text}"
    );
    assert!(!text.contains(VALUE));
    assert!(vault.view().asks.is_empty());
    assert_eq!(
        vault.value_for_use(Some(PROJECT), "GITHUB").unwrap().1,
        VALUE
    );
    assert!(
        vault.answer(id, AskAnswer::Decline).is_err(),
        "answered once"
    );
}

#[test]
fn a_declined_request_says_so_and_keeps_nothing() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    let (id, told) = ask(&vault, "GITHUB", AskKind::Token);
    vault.answer(id, AskAnswer::Decline).unwrap();
    assert!(told.recv().unwrap().contains("declined"));
    assert!(vault.view().entries.is_empty());
}

/// A reason the person can judge.
const CLEAR: &str = "to open pull requests on github.com/acme/web";

fn plain_request(name: &str, reason: &str) -> AskRequest {
    AskRequest {
        name: name.into(),
        kind: AskKind::Token,
        reason: reason.into(),
        proposed: Vec::new(),
    }
}

#[test]
fn requests_are_limited_and_checked() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    let _a = ask(&vault, "A", AskKind::Token);
    assert!(
        vault
            .ask(&caller(None), plain_request("A", CLEAR), Box::new(|_| {}))
            .is_err(),
        "the same name twice"
    );
    let _b = ask(&vault, "B", AskKind::Token);
    let _c = ask(&vault, "C", AskKind::Token);
    assert!(
        vault
            .ask(&caller(None), plain_request("D", CLEAR), Box::new(|_| {}))
            .is_err(),
        "three per terminal"
    );
    let other = SecretCaller {
        terminal: "other".into(),
        ..caller(None)
    };
    assert!(
        vault
            .ask(&other, plain_request("E", "  "), Box::new(|_| {}))
            .is_err(),
        "a reason is needed"
    );
    for vague in [
        "x",
        "deploy",
        "needed for the task",
        "I need the E token please",
        "I need it for normal operations",
        "Mi serve per questo lavoro",
    ] {
        let why = vault
            .ask(&other, plain_request("E", vague), Box::new(|_| {}))
            .unwrap_err();
        assert!(why.contains("--reason"), "{vague:?}: {why}");
        assert!(
            vague.len() < 3 || !why.contains(vague),
            "not repeated: {why}"
        );
    }
    assert!(vault.view().asks.iter().all(|ask| ask.name != "E"));
    // Short but specific; shown on one line, its quotes unable to break out.
    vault
        .ask(
            &other,
            plain_request("F", "Restart staging \u{201d}PostgreSQL\u{2028}now"),
            Box::new(|_| {}),
        )
        .unwrap();
    let view = vault.view();
    let shown = view.asks.iter().find(|a| a.name == "F").unwrap();
    assert_eq!(shown.reason, "Restart staging 'PostgreSQL now");
    assert!(
        vault
            .ask(&other, plain_request("bad name", CLEAR), Box::new(|_| {}))
            .is_err()
    );
}

// ---------- SSH

fn brokers(vault: &Arc<Vault>) -> tempfile::TempDir {
    // Short: a Unix socket path has a length limit.
    let dir = tempfile::Builder::new()
        .prefix("cv")
        .tempdir_in("/tmp")
        .unwrap();
    vault.start_brokers(dir.path().join("a")).unwrap();
    dir
}

#[test]
fn a_generated_ssh_key_is_kept_and_signs_through_the_agent() {
    let (_, secrets) = memory();
    let vault = Arc::new(open(secrets, None));
    let _dir = brokers(&vault);
    let (id, told) = ask(&vault, "DEPLOY_KEY", AskKind::SshKey);
    let made = vault.generate_ssh_key("deploy").unwrap();
    vault
        .answer(
            id,
            AskAnswer::Provide {
                scope: project(),
                value: made.private_openssh.clone(),
                bindings: vec![Binding::Ssh],
            },
        )
        .unwrap();
    assert!(told.recv().unwrap().contains("connecto secret ssh"));
    let access = vault.ssh(&caller(None), "DEPLOY_KEY").unwrap();
    assert_eq!(access.public_openssh, made.public_openssh);

    let public = ssh_key::PublicKey::from_openssh(&made.public_openssh).unwrap();
    let mut client = ssh_agent_client_rs::Client::connect(&access.socket).unwrap();
    let keys = client.list_all_identities().unwrap();
    assert_eq!(keys.len(), 1);
    assert!(
        matches!(&keys[0], ssh_agent_client_rs::Identity::PublicKey(key) if key.key_data() == public.key_data())
    );
    let signature = client.sign(public.clone(), b"session data").unwrap();
    signature::Verifier::verify(&public, b"session data", &signature).unwrap();

    // Removing the key closes its socket.
    vault.delete(&project(), "DEPLOY_KEY").unwrap();
    assert!(!access.socket.exists());
}

#[test]
fn an_ssh_socket_serves_no_process_but_its_terminal() {
    let (_, secrets) = memory();
    let vault = Arc::new(open(secrets, None));
    let _dir = brokers(&vault);
    let made = vault.generate_ssh_key("k").unwrap();
    vault
        .put(&project(), "KEY", "", &made.private_openssh)
        .unwrap();
    vault
        .set_bindings(&project(), "KEY", vec![Binding::Ssh])
        .unwrap();
    let stranger = SecretCaller {
        owns_pid: Arc::new(|_| false),
        ..caller(None)
    };
    let access = vault.ssh(&stranger, "KEY").unwrap();
    let mut client = ssh_agent_client_rs::Client::connect(&access.socket).unwrap();
    assert!(
        client.list_all_identities().is_err(),
        "the connection is closed"
    );
}

// ---------- the web proxy

/// A server that answers one request with what it was sent.
fn echo_server() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line);
        }
        let body = format!("you sent:\n{head}");
        let _ = tx.send(head);
        write!(
            writer,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    (port, rx)
}

fn get(url: &str, extra: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let mut stream = std::net::TcpStream::connect(host).unwrap();
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n"
    )
    .unwrap();
    let mut out = String::new();
    stream.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn the_proxy_adds_the_header_upstream_and_hides_the_value_coming_back() {
    let (port, sent) = echo_server();
    let (_, secrets) = memory();
    let vault = Arc::new(open(secrets, None));
    let _dir = brokers(&vault);
    vault.put(&project(), "API", "", VALUE).unwrap();
    vault
        .set_bindings(
            &project(),
            "API",
            vec![Binding::Http {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                header: "Authorization".into(),
                prefix: "Bearer ".into(),
            }],
        )
        .unwrap();
    let url = vault.http_url(&caller(None), "API").unwrap();
    assert!(!url.contains(VALUE));
    let answer = get(
        &format!("{url}items?x=1"),
        "Authorization: Bearer forged\r\n",
    );
    let upstream = sent.recv().unwrap();
    assert!(upstream.starts_with("GET /v1/items?x=1 "), "{upstream}");
    assert!(upstream.contains(&format!("Bearer {VALUE}")), "{upstream}");
    assert!(
        !upstream.contains("forged"),
        "the terminal's own header is dropped"
    );
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(!answer.contains(VALUE), "{answer}");
    assert!(answer.contains("[REDACTED]"), "{answer}");
}

#[test]
fn the_proxy_refuses_a_wrong_token_and_a_secret_not_for_the_web() {
    let (_, secrets) = memory();
    let vault = Arc::new(open(secrets, None));
    let _dir = brokers(&vault);
    vault.put(&project(), "API", "", VALUE).unwrap();
    assert!(
        vault.http_url(&caller(None), "API").is_err(),
        "no web use set"
    );
    vault
        .set_bindings(&project(), "API", vec![gh_binding()])
        .unwrap();
    let url = vault.http_url(&caller(None), "API").unwrap();
    let port = url.split(':').nth(2).unwrap().split('/').next().unwrap();
    let answer = get(&format!("http://127.0.0.1:{port}/nottoken/API/x"), "");
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    assert!(!answer.contains(VALUE));
}

// ---------- programs

fn command_secret(vault: &Vault, program: &str) {
    vault.put(&project(), "TOKEN", "", VALUE).unwrap();
    vault
        .set_bindings(
            &project(),
            "TOKEN",
            vec![Binding::Command {
                programs: vec![program.into()],
                env: "MY_TOKEN".into(),
            }],
        )
        .unwrap();
}

#[test]
fn an_allowed_program_runs_with_the_secret_and_its_output_hides_it() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    let project_dir = tempfile::tempdir().unwrap();
    command_secret(&vault, "/usr/bin/printenv");
    let who = caller(Some(project_dir.path()));
    let out = vault
        .run(
            &who,
            "TOKEN",
            "/usr/bin/printenv",
            &["MY_TOKEN".into()],
            None,
        )
        .unwrap();
    assert_eq!(out.code, Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text.trim(),
        "[REDACTED]",
        "the program had it; the terminal does not"
    );
    assert!(vault.run(&who, "TOKEN", "/usr/bin/env", &[], None).is_err());
    assert!(vault.run(&who, "TOKEN", "printenv", &[], None).is_err());
}

#[test]
fn a_program_past_its_time_is_stopped() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    let project_dir = tempfile::tempdir().unwrap();
    command_secret(&vault, "/bin/sleep");
    let out = run::run_with_limit(
        &vault,
        &caller(Some(project_dir.path())),
        "TOKEN",
        "/bin/sleep",
        &["30".into()],
        None,
        std::time::Duration::from_millis(300),
    )
    .unwrap();
    assert!(out.timed_out);
    assert_eq!(out.code, None);
}

#[test]
fn every_use_is_logged_by_name_without_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let (_, secrets) = memory();
    let vault = open(secrets, Some(dir.path()));
    command_secret(&vault, "/usr/bin/true");
    vault
        .run(
            &caller(Some(dir.path())),
            "TOKEN",
            "/usr/bin/true",
            &[],
            None,
        )
        .unwrap();
    let log = std::fs::read_to_string(uses_file(dir.path())).unwrap();
    assert!(log.contains("\"name\":\"TOKEN\""), "{log}");
    assert!(log.contains("claude-1"));
    assert!(!log.contains(VALUE));
}

#[test]
fn works_binding_is_exclusive_project_scoped_and_invisible_to_terminals() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    for scope in [ProviderScope::Global, project()] {
        vault.put(&scope, "DB", "Queue", VALUE).unwrap();
    }
    assert!(
        vault
            .set_bindings(&ProviderScope::Global, "DB", vec![Binding::AgenticWorks])
            .is_err()
    );
    assert!(
        vault
            .set_bindings(&project(), "DB", vec![Binding::AgenticWorks, gh_binding()])
            .is_err()
    );
    vault
        .set_bindings(&project(), "DB", vec![Binding::AgenticWorks])
        .unwrap();
    assert_eq!(vault.works_credential(PROJECT, "DB").unwrap(), VALUE);
    assert!(vault.works_credential("another", "DB").is_err());
    assert!(
        vault
            .visible(Some(PROJECT))
            .iter()
            .all(|entry| entry.scope != project())
    );
    assert!(
        vault
            .value_for_use(Some(PROJECT), "DB")
            .is_ok_and(|(e, _)| e.scope == ProviderScope::Global)
    );
}

#[test]
fn service_account_keys_are_checked_and_kept_host_only() {
    let key = r#"{"type":"service_account","client_email":"sa@p.iam.gserviceaccount.com","private_key":"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n","token_uri":"https://oauth2.googleapis.com/token"}"#;
    assert_eq!(
        crate::check_service_account_key(key).unwrap(),
        "sa@p.iam.gserviceaccount.com"
    );
    let mut bad: serde_json::Value = serde_json::from_str(key).unwrap();
    for (field, value) in [
        ("type", serde_json::json!("authorized_user")),
        ("client_email", serde_json::json!("")),
        ("private_key", serde_json::json!("nope")),
        ("token_uri", serde_json::json!("http://evil.example/token")),
        (
            "token_uri",
            serde_json::json!("http://127.0.0.1.evil.example/token"),
        ),
    ] {
        let saved = bad[field].clone();
        bad[field] = value;
        let why = crate::check_service_account_key(&bad.to_string()).unwrap_err();
        assert!(!why.contains("PRIVATE KEY"), "{why}");
        bad[field] = saved;
    }
    assert!(crate::check_service_account_key("not json").is_err());
    assert!(crate::google_token_uri_ok(
        "https://accounts.google.com/o/oauth2/token"
    ));
    assert!(!crate::google_token_uri_ok("https://evil.example/token"));
    assert!(!crate::google_token_uri_ok("http://127.0.0.1:8080/token"));

    let (_, secrets) = memory();
    let vault = open(secrets, None);
    assert!(
        vault
            .put_host_secret(PROJECT, "KEY", "", "{}", Binding::GoogleServiceAccount)
            .is_err()
    );
    vault
        .put_host_secret(PROJECT, "KEY", "sa", key, Binding::GoogleServiceAccount)
        .unwrap();
    assert_eq!(vault.google_key(PROJECT, "KEY").unwrap(), key);
    assert!(vault.google_key("another", "KEY").is_err());
    assert!(vault.works_credential(PROJECT, "KEY").is_err());
    assert_eq!(
        vault.host_secrets(PROJECT, &Binding::GoogleServiceAccount),
        ["KEY"]
    );
    // No terminal sees it, and it cannot be used for anything else too.
    assert!(vault.visible(Some(PROJECT)).iter().all(|e| e.name != "KEY"));
    assert!(
        vault
            .set_bindings(
                &project(),
                "KEY",
                vec![Binding::GoogleServiceAccount, Binding::Ssh]
            )
            .is_err()
    );
    vault
        .put(&project(), "NOTKEY", "", "plain-value-123")
        .unwrap();
    assert!(
        vault
            .set_bindings(&project(), "NOTKEY", vec![Binding::GoogleServiceAccount])
            .is_err()
    );
    vault
        .set_bindings(&project(), "NOTKEY", vec![gh_binding()])
        .unwrap();
    // A host-only value never lands behind a terminal's use.
    assert!(
        vault
            .put_host_secret(PROJECT, "NOTKEY", "", key, Binding::GoogleServiceAccount)
            .is_err()
    );
    assert_eq!(
        vault
            .lock()
            .values
            .get(&store_key(&project(), "NOTKEY"))
            .map(String::as_str),
        Some("plain-value-123")
    );
}

// ---------- changes from a paired machine

/// A refused answer writes nothing: the secret keeps its value and use, and
/// the request still waits.
#[test]
fn a_refused_answer_leaves_the_secret_as_it_was() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    vault.put(&project(), "GITHUB", "", VALUE).unwrap();
    vault
        .set_bindings(&project(), "GITHUB", vec![gh_binding()])
        .unwrap();
    let (id, told) = ask(&vault, "GITHUB", AskKind::Token);
    let refused = vault.answer(
        id,
        AskAnswer::Provide {
            scope: project(),
            value: "ghp_replacement_value_999".into(),
            bindings: vec![Binding::Http {
                base_url: "ftp://elsewhere".into(),
                header: "Authorization".into(),
                prefix: String::new(),
            }],
        },
    );
    assert!(refused.is_err());
    assert!(vault.known().iter().any(|k| k == VALUE), "the value kept");
    assert!(
        !vault
            .known()
            .iter()
            .any(|k| k == "ghp_replacement_value_999")
    );
    let entry = vault.visible(Some(PROJECT))[0].clone();
    assert_eq!(entry.bindings, vec![gh_binding()], "and its use");
    assert_eq!(vault.view().asks.len(), 1, "the request still waits");
    assert!(told.try_recv().is_err());
}

/// A key generated over a secret used for web requests replaces the value
/// and the use at once: the key is never sent as a web header.
#[test]
fn a_key_generated_over_a_web_secret_takes_its_place_and_its_use_at_once() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    vault.put(&project(), "GITHUB", "", VALUE).unwrap();
    vault
        .set_bindings(&project(), "GITHUB", vec![gh_binding()])
        .unwrap();
    vault.put_generated_key(&project(), "GITHUB", "").unwrap();
    let entry = vault.visible(Some(PROJECT))[0].clone();
    assert_eq!(entry.bindings, vec![Binding::Ssh]);
    assert!(!vault.known().iter().any(|k| k == VALUE));
    assert!(vault.public_key(&project(), "GITHUB").is_some());
}

/// Removing one project's key closes the sockets of that project's
/// terminals only; another project's key of the same name still signs.
#[cfg(unix)]
#[test]
fn removing_a_projects_key_leaves_another_projects_key_of_that_name_open() {
    let (_, secrets) = memory();
    let vault = Arc::new(open(secrets, None));
    let _dir = brokers(&vault);
    const OTHER: &str = "1f8bb1bcbbc9990b";
    let other = ProviderScope::Project(OTHER.into());
    vault.put_generated_key(&project(), "DEPLOY", "").unwrap();
    vault.put_generated_key(&other, "DEPLOY", "").unwrap();
    let mine = vault.ssh(&caller(None), "DEPLOY").unwrap();
    let theirs = vault
        .ssh(
            &SecretCaller {
                terminal: "claude-2".into(),
                project: Some(OTHER.into()),
                ..caller(None)
            },
            "DEPLOY",
        )
        .unwrap();
    vault.delete(&project(), "DEPLOY").unwrap();
    assert!(!mine.socket.exists(), "the removed key's socket is closed");
    assert!(theirs.socket.exists(), "the other project's stays");
    let mut client = ssh_agent_client_rs::Client::connect(&theirs.socket).unwrap();
    assert_eq!(client.list_all_identities().unwrap().len(), 1);
}

/// Changes from paired machines are kept until the window has taken them,
/// however many come before; numbers never repeat.
#[test]
fn changes_from_paired_machines_wait_until_the_window_takes_them() {
    let (_, secrets) = memory();
    let vault = open(secrets, None);
    for n in 0..1500 {
        vault.note_remote_change(format!("change {n}"));
    }
    let changes = vault.view().remote_changes.clone();
    assert_eq!(changes.len(), 1500, "none dropped");
    assert_eq!(changes[0].1, "change 0");
    vault.ack_remote_changes(1500);
    assert!(vault.view().remote_changes.is_empty());
    vault.note_remote_change("later".into());
    assert_eq!(
        vault.view().remote_changes,
        vec![(1501, "later".to_owned())]
    );
}

/// A key generated over a web secret, with the list's write failing at
/// either step: once reopened, the key is never under the web use. The
/// first write failing leaves the secret as it was; the second leaves the
/// new key usable by no terminal.
#[test]
fn a_failed_write_never_leaves_a_new_key_under_the_old_use() {
    let (_, secrets) = memory();
    let dir = tempfile::tempdir().unwrap();
    let vault = open(Arc::clone(&secrets), Some(dir.path()));
    vault.put(&project(), "GITHUB", "", VALUE).unwrap();
    vault
        .set_bindings(&project(), "GITHUB", vec![gh_binding()])
        .unwrap();
    let reopened = || {
        let vault = open(Arc::clone(&secrets), Some(dir.path()));
        let entry = vault.visible(Some(PROJECT))[0].clone();
        let old_value = vault.known().iter().any(|k| k == VALUE);
        (entry.bindings, old_value)
    };

    vault.fail_save(1);
    assert!(vault.put_generated_key(&project(), "GITHUB", "").is_err());
    assert_eq!(reopened(), (vec![gh_binding()], true), "as it was");
    assert_eq!(vault.visible(Some(PROJECT))[0].bindings, vec![gh_binding()]);

    vault.fail_save(2);
    assert!(vault.put_generated_key(&project(), "GITHUB", "").is_err());
    assert_eq!(reopened(), (Vec::new(), false), "the new key, with no use");
    assert!(
        vault.visible(Some(PROJECT))[0].bindings.is_empty(),
        "and so in memory"
    );
}

/// A failed save never leaves a use in force that the disk does not have:
/// an existing secret with no use keeps none, a new secret is not there,
/// before and after reopening.
#[test]
fn a_failed_save_restores_the_last_saved_uses() {
    let (_, secrets) = memory();
    let dir = tempfile::tempdir().unwrap();
    let vault = open(Arc::clone(&secrets), Some(dir.path()));
    vault.put(&project(), "GITHUB", "", VALUE).unwrap();
    let reopened = || open(Arc::clone(&secrets), Some(dir.path())).visible(Some(PROJECT));

    // An existing secret with no use, given one with a request's answer.
    let (id, _told) = ask(&vault, "GITHUB", AskKind::Token);
    vault.fail_save(1);
    let refused = vault.answer(
        id,
        AskAnswer::Provide {
            scope: project(),
            value: "ghp_new_value_0123456789".into(),
            bindings: vec![gh_binding()],
        },
    );
    assert!(refused.is_err());
    assert!(
        vault.visible(Some(PROJECT))[0].bindings.is_empty(),
        "no use here"
    );
    assert!(reopened()[0].bindings.is_empty(), "nor on disk");

    // A new secret whose first save fails is not there.
    vault.fail_save(1);
    assert!(vault.put_generated_key(&project(), "DEPLOY", "").is_err());
    assert!(
        !vault
            .visible(Some(PROJECT))
            .iter()
            .any(|e| e.name == "DEPLOY")
    );
    assert!(!reopened().iter().any(|e| e.name == "DEPLOY"));
}

/// A refusal is told apart where it happens: one of the change itself, or
/// the vault's; and a change that wrote its value before failing says so,
/// whatever its time says.
#[test]
fn a_refusal_says_what_it_is_and_whether_anything_was_written() {
    let (_, secrets) = memory();
    let dir = tempfile::tempdir().unwrap();
    let vault = open(secrets, Some(dir.path()));
    vault.put(&project(), "GITHUB", "", VALUE).unwrap();
    let put = |value: &str| VaultChange::Put {
        scope: project(),
        name: "GITHUB".into(),
        description: String::new(),
        value: value.into(),
    };
    let short = vault.change(put("short")).unwrap_err();
    assert!(short.invalid && !short.partial, "{short:?}");
    vault.fail_save(1);
    let failed = vault.change(put("ghp_other_value_0123456789")).unwrap_err();
    assert!(!failed.invalid, "the vault's own failure");
    assert!(failed.partial, "the value was written");
    vault.fail_save(1);
    let nothing = vault
        .change(VaultChange::SetBindings {
            scope: project(),
            name: "GITHUB".into(),
            bindings: vec![gh_binding()],
        })
        .unwrap_err();
    assert!(!nothing.invalid && !nothing.partial, "{nothing:?}");
}
