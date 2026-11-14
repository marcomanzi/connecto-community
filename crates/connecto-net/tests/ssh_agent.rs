//! Signing through the real local ssh-agent. Ignored by default: it needs an
//! agent, and a key already in it. It never adds a key and never reads
//! `~/.ssh`.
//!
//! Run it with
//! `SSH_AUTH_SOCK=... cargo test -p connecto-net --test ssh_agent -- --ignored --nocapture`.
//! With an agent that holds no keys it proves only that the agent answers.
//!
//! On Windows, with `SSH_AUTH_SOCK` unset, it uses the Windows OpenSSH agent's
//! pipe `\\.\pipe\openssh-ssh-agent`. Its service is disabled by default; in
//! an administrator PowerShell:
//! `Set-Service ssh-agent -StartupType Manual; Start-Service ssh-agent`, then,
//! as the user, `ssh-add` (a key in `~\.ssh`), then
//! `Remove-Item Env:SSH_AUTH_SOCK -ErrorAction SilentlyContinue; cargo test -p connecto-net --test ssh_agent -- --ignored --nocapture`.
//! Without the service the test fails with "the agent did not answer ... is
//! the "OpenSSH Authentication Agent" service running?" (seen on 2026-11-13).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::sync::Arc;

use connecto_net::auth::{AgentSigner, AuthorizedKeySet, HostAuth, Signer, client_response};
use connecto_net::wire::Message;
use ssh_key::PublicKey;

#[test]
#[ignore = "needs a running ssh-agent that already holds a key"]
fn the_agent_signs_a_handshake_the_host_accepts() {
    let socket = AgentSigner::socket_path(None).expect("SSH_AUTH_SOCK is not set");
    let keys = AgentSigner::list(Some(&socket)).expect("the agent did not answer");
    eprintln!("the agent answered and holds {} key(s)", keys.len());
    if keys.is_empty() {
        eprintln!("no key to sign with: signing was NOT exercised");
        return;
    }
    let mut accepted = 0;
    for key in keys {
        let signer = AgentSigner::new(socket.clone(), key.clone());
        let allowed: Vec<PublicKey> = vec![key.clone()];
        let text: String = allowed
            .iter()
            .map(|k| format!("{}\n", k.to_openssh().unwrap()))
            .collect();
        let set = Arc::new(AuthorizedKeySet::parse(&text).unwrap());
        let (client, host, binding) = ([1_u8; 32], [2_u8; 32], [3_u8; 32]);
        let mut auth = HostAuth::new(host, client, binding, set);
        let Message::Challenge { nonce } = auth.on_hello(1).unwrap() else {
            panic!("no challenge");
        };
        match client_response(&signer, &nonce, &client, &host, &binding) {
            Ok(Message::AuthResponse {
                public_key,
                signature,
            }) => {
                let authed = auth.on_response(&public_key, &signature).unwrap();
                eprintln!(
                    "accepted {} ({})",
                    authed.fingerprint,
                    signer.public_key().algorithm()
                );
                accepted += 1;
            }
            Ok(_) => panic!("not a response"),
            // A key type SSHSIG over the agent does not cover (such as a
            // hardware key) is reported, not hidden.
            Err(err) => eprintln!(
                "could not sign with {}: {err}",
                signer.public_key().algorithm()
            ),
        }
    }
    assert!(
        accepted > 0,
        "no key in the agent produced an accepted signature"
    );
}
