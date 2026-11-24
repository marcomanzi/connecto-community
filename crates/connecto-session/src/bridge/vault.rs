//! `connecto secret` on the bridge.
//!
//! The app holds one [`VaultBridge`], set once by `connecto-app` with
//! [`set_vault_bridge`] (until then [`NoVault`]: no secrets). A terminal sees
//! the secrets of its project and the app-wide ones, by name and by how it
//! may use them. No reply here ever holds a value: values are used by the
//! app itself, through the bindings the person chose.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use connecto_core::{
    AskKind, AskReply, AskRequest, Binding, NoVault, RunOutput, SecretCaller, SecretEntry,
    VaultBridge,
};
use connecto_hook::bridge::Reply;
use serde_json::json;

static VAULT: RwLock<Option<Arc<dyn VaultBridge>>> = RwLock::new(None);

/// Give the bridge the app's vault. Called once by `connecto-app` at startup.
pub fn set_vault_bridge(bridge: Arc<dyn VaultBridge>) {
    *VAULT.write().unwrap_or_else(PoisonError::into_inner) = Some(bridge);
}

/// The app's vault, or [`NoVault`].
pub(crate) fn vault_bridge() -> Arc<dyn VaultBridge> {
    VAULT
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::new(NoVault))
}

const USAGE: &str = "usage:
  connecto secret list
  connecto secret request <NAME> --reason '<why>' [--kind token|password|ssh-key]
      [--http <https://base> [--header H] [--prefix P]] [--command </path> --env VAR] [--ssh]
      [--agentic-works]   (a database connection string only Connecto uses)
  connecto secret url <NAME>
  connecto secret run <NAME> -- <program> [args]
  connecto secret ssh <NAME>
The person sees --reason, this terminal and the proposed use before they answer.
Write the reason as one plain sentence: what you will do with the secret and
where, such as --reason 'Open pull requests on github.com/acme/web through the
GitHub API'. An empty or generic reason (\"needed for the task\") is refused.";

/// What a terminal is told when it has none.
const NONE_YET: &str = "No secrets are set up for this terminal. Ask the user for one with `connecto secret request <NAME> --reason '<why>'`; they can also add them in Connecto's Settings > Secrets.\n";

/// Answer `connecto secret <verb> [args]` for `caller`. `notify` tells the
/// terminal the answer to a `request` later.
pub(crate) fn command(
    bridge: &dyn VaultBridge,
    caller: &SecretCaller,
    verb: &str,
    args: &[String],
    cwd: Option<&Path>,
    notify: AskReply,
) -> Reply {
    let refused = |why: String| Reply::refused("refused", &why, None);
    match (verb, args) {
        ("list", []) => list(&bridge.visible(caller.project.as_deref())),
        ("help" | "--help", _) => answer(&format!("{USAGE}\n")),
        ("request", [name, rest @ ..]) => match parse_request(name, rest) {
            Ok(request) => {
                let name = request.name.clone();
                match bridge.ask(caller, request, notify) {
                    Ok(_) => answer(&format!(
                        "Asked the user for {name}. End your turn now: a [connecto notice] tells you when they answer. Do not wait or poll.\n"
                    )),
                    Err(why) => refused(why),
                }
            }
            Err(why) => Reply::refused("bad_request", &format!("{why}\n{USAGE}"), None),
        },
        ("url", [name]) => match bridge.http_url(caller, name) {
            Ok(url) => answer(&format!(
                "{url}\nSend web requests under this address (add the API path after it). Connecto sends them on with the secret; do not add it yourself.\n"
            )),
            Err(why) => refused(why),
        },
        ("run", [name, dashes, program, program_args @ ..]) if dashes == "--" => {
            match bridge.run(caller, name, program, program_args, cwd) {
                Ok(out) => run_reply(out),
                Err(why) => refused(why),
            }
        }
        ("ssh", [name]) => match bridge.ssh(caller, name) {
            Ok(access) => {
                let socket = access.socket.display().to_string();
                answer(&format!(
                    "export SSH_AUTH_SOCK='{socket}'\n{public}\nUse it as: SSH_AUTH_SOCK='{socket}' ssh -o IdentitiesOnly=no user@host. The line above the address is the public key, to install in the server's authorized_keys.\n",
                    public = access.public_openssh,
                ))
            }
            Err(why) => refused(why),
        },
        ("list" | "request" | "url" | "run" | "ssh", _) => {
            Reply::refused("bad_request", &format!("wrong arguments\n{USAGE}"), None)
        }
        (other, _) => Reply::refused(
            "bad_request",
            &format!("unknown secret command {other:?}\n{USAGE}"),
            None,
        ),
    }
}

/// `request <NAME> --reason R [--kind K] [--http URL [--header H] [--prefix P]]
/// [--command PATH --env VAR] [--ssh]`.
fn parse_request(name: &str, rest: &[String]) -> Result<AskRequest, String> {
    let mut reason = None;
    let mut kind = None;
    let (mut http, mut header, mut prefix) = (None, None, None);
    let (mut program, mut env) = (None, None);
    let mut ssh = false;
    let mut works = false;
    let mut words = rest.iter();
    while let Some(flag) = words.next() {
        if flag == "--ssh" {
            ssh = true;
            continue;
        }
        if flag == "--agentic-works" {
            works = true;
            continue;
        }
        let slot = match flag.as_str() {
            "--reason" => &mut reason,
            "--kind" => &mut kind,
            "--http" => &mut http,
            "--header" => &mut header,
            "--prefix" => &mut prefix,
            "--command" => &mut program,
            "--env" => &mut env,
            other => return Err(format!("unknown option {other:?}")),
        };
        let value = words.next().ok_or(format!("{flag} needs a value"))?;
        if slot.replace(value.clone()).is_some() {
            return Err(format!("{flag} is given twice"));
        }
    }
    let reason = reason.ok_or("say why with --reason '<why>'")?;
    let kind = match kind {
        None => AskKind::Token,
        Some(word) => AskKind::from_word(&word).ok_or("--kind is token, password or ssh-key")?,
    };
    let mut proposed = Vec::new();
    if let Some(base_url) = http {
        proposed.push(Binding::Http {
            base_url,
            header: header.unwrap_or_else(|| "Authorization".to_owned()),
            prefix: prefix.unwrap_or_else(|| "Bearer ".to_owned()),
        });
    } else if header.is_some() || prefix.is_some() {
        return Err("--header and --prefix go with --http".to_owned());
    }
    match (program, env) {
        (Some(program), Some(env)) => proposed.push(Binding::Command {
            programs: vec![program],
            env,
        }),
        (None, None) => {}
        _ => return Err("--command and --env go together".to_owned()),
    }
    if ssh || kind == AskKind::SshKey {
        proposed.push(Binding::Ssh);
    }
    // Only Connecto uses it, for this project: no other use goes with it.
    if works {
        if !proposed.is_empty() {
            return Err("--agentic-works goes with no other use".to_owned());
        }
        proposed.push(Binding::AgenticWorks);
    }
    Ok(AskRequest {
        name: name.to_owned(),
        kind,
        reason,
        proposed,
    })
}

/// A program's output: stdout then stderr in the body, split by `stdout_len`.
fn run_reply(out: RunOutput) -> Reply {
    let mut body = out.stdout;
    let stdout_len = body.len();
    body.extend_from_slice(&out.stderr);
    Reply::ok(json!({
        "code": out.code,
        "timed_out": out.timed_out,
        "stdout_len": stdout_len,
    }))
    .with_body(body)
}

fn answer(text: &str) -> Reply {
    Reply::ok(json!({})).with_body(text.as_bytes().to_vec())
}

/// One line per secret: its name, whose it is, and how it may be used.
fn list(entries: &[SecretEntry]) -> Reply {
    if entries.is_empty() {
        return answer(NONE_YET);
    }
    let width = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let mut out = format!("{:<width$}  {:<7}  USE\n", "NAME", "SCOPE");
    for entry in entries {
        let scope = match entry.scope {
            connecto_core::ProviderScope::Global => "app",
            connecto_core::ProviderScope::Project(_) => "project",
        };
        let uses: Vec<&str> = entry
            .bindings
            .iter()
            .map(connecto_core::Binding::kind)
            .collect();
        let uses = if uses.is_empty() {
            "none yet (the person has not said how it may be used)".to_owned()
        } else {
            uses.join(", ")
        };
        let _ = writeln!(out, "{:<width$}  {scope:<7}  {uses}", entry.name);
        let description = connecto_redact::sanitize(&entry.description);
        let description = description.trim();
        if !description.is_empty() {
            let _ = writeln!(out, "{:<width$}  {description}", "");
        }
    }
    out.push_str("Values are never shown to terminals.\n");
    answer(&out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use connecto_core::{Binding, ProviderScope};

    struct Fake(Vec<SecretEntry>);

    impl VaultBridge for Fake {
        fn visible(&self, project: Option<&str>) -> Vec<SecretEntry> {
            self.0
                .iter()
                .filter(|e| match &e.scope {
                    ProviderScope::Global => true,
                    ProviderScope::Project(id) => Some(id.as_str()) == project,
                })
                .cloned()
                .collect()
        }
        fn known(&self) -> Arc<[String]> {
            Arc::from(Vec::new())
        }
    }

    fn entry(name: &str, scope: ProviderScope, bindings: Vec<Binding>) -> SecretEntry {
        SecretEntry {
            name: name.into(),
            scope,
            description: "for \x1b[31mgh".into(),
            bindings,
            updated_unix: 0,
            last_used_unix: None,
            public_key: None,
        }
    }

    fn who(project: Option<&str>) -> SecretCaller {
        SecretCaller {
            terminal: "t".into(),
            project: project.map(str::to_owned),
            project_folder: None,
            owns_pid: Arc::new(|_| false),
        }
    }

    #[test]
    fn a_request_reads_its_options_into_a_proposal() {
        let words = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        let request = parse_request(
            "GH",
            &words("--reason deploy --http https://api.github.com --prefix token_ --ssh"),
        )
        .unwrap();
        assert_eq!(request.reason, "deploy");
        assert_eq!(
            request.proposed,
            [
                Binding::Http {
                    base_url: "https://api.github.com".into(),
                    header: "Authorization".into(),
                    prefix: "token_".into(),
                },
                Binding::Ssh,
            ]
        );
        let key = parse_request("K", &words("--kind ssh-key --reason ssh")).unwrap();
        assert_eq!(key.kind, AskKind::SshKey);
        assert_eq!(key.proposed, [Binding::Ssh]);
        let db = parse_request(
            "DB",
            &words("--kind password --agentic-works --reason queue"),
        )
        .unwrap();
        assert_eq!(db.proposed, [Binding::AgenticWorks]);
        for bad in [
            "--reason a --agentic-works --ssh",
            "--reason a --agentic-works --http https://x",
            "--http x",
            "--reason a --reason b",
            "--reason a --env X",
            "--reason a --header H",
            "--reason a --kind banana",
            "--reason",
            "--reason a --what",
        ] {
            assert!(parse_request("K", &words(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn without_a_vault_every_use_is_refused_and_no_value_is_given() {
        for (verb, args) in [
            ("url", vec!["A".to_owned()]),
            ("ssh", vec!["A".to_owned()]),
            (
                "run",
                vec!["A".to_owned(), "--".to_owned(), "/bin/ls".to_owned()],
            ),
            (
                "request",
                vec!["A".to_owned(), "--reason".to_owned(), "x".to_owned()],
            ),
        ] {
            let reply = command(&NoVault, &who(None), verb, &args, None, Box::new(|_| {}));
            assert!(!reply.is_ok(), "{verb}");
        }
    }

    fn body(reply: &Reply) -> String {
        String::from_utf8(reply.body.clone()).unwrap()
    }

    #[test]
    fn list_shows_names_scopes_and_uses_of_this_project_only() {
        let fake = Fake(vec![
            entry(
                "GITHUB",
                ProviderScope::Global,
                vec![Binding::Http {
                    base_url: "https://api.github.com".into(),
                    header: "Authorization".into(),
                    prefix: "Bearer ".into(),
                }],
            ),
            entry("AWS", ProviderScope::Project("p".into()), Vec::new()),
            entry("OTHER", ProviderScope::Project("q".into()), Vec::new()),
        ]);
        let reply = command(&fake, &who(Some("p")), "list", &[], None, Box::new(|_| {}));
        assert!(reply.is_ok());
        let text = body(&reply);
        assert!(text.contains("GITHUB") && text.contains("http"), "{text}");
        assert!(text.contains("AWS") && text.contains("none yet"), "{text}");
        assert!(!text.contains("OTHER"), "{text}");
        assert!(!text.contains('\x1b'), "descriptions are sanitized");
    }

    #[test]
    fn an_empty_vault_says_where_secrets_are_added() {
        let reply = command(&NoVault, &who(None), "list", &[], None, Box::new(|_| {}));
        assert!(body(&reply).contains("Settings > Secrets"));
    }

    #[test]
    fn unknown_verbs_and_extra_arguments_are_refused() {
        assert!(
            !command(
                &NoVault,
                &who(None),
                "get",
                &["A".into()],
                None,
                Box::new(|_| {})
            )
            .is_ok()
        );
        assert!(
            !command(
                &NoVault,
                &who(None),
                "list",
                &["A".into()],
                None,
                Box::new(|_| {})
            )
            .is_ok()
        );
    }
}
