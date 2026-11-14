//! Two real iroh endpoints, authenticated with a real ssh-agent.
//!
//! The agent is a throwaway: `ssh-agent` is started for this test alone, on a
//! socket in a temp folder, holding a key generated into that folder. The
//! user's own agent (`SSH_AUTH_SOCK`) and `~/.ssh` are never read or written.
//! The test skips when the OpenSSH tools are not installed.
//!
//! Unix only: the Windows OpenSSH agent is a system service on one fixed
//! named pipe, not a throwaway process on a socket of the test's choosing.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Sink};
use connecto_core::{PauseSwitch, RemoteGuard};
use connecto_gate::Gate;
use connecto_net::auth::{AgentSigner, AuthorizedKeySet};
use connecto_net::client::RemoteClient;
use connecto_net::host::{FixedTerminals, HostService};
use connecto_net::iroh_transport::{IrohConfig, IrohNode, PeerAddr};
use connecto_session::RemoteGate;

struct Throwaway {
    dir: PathBuf,
    agent: Child,
    sock: PathBuf,
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = self.agent.kill();
        let _ = self.agent.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn throwaway() -> Throwaway {
    // A Unix socket path is limited to about 100 bytes, and a sandboxed temp
    // folder can be longer than that.
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() < 60 {
        base
    } else {
        PathBuf::from("/tmp")
    };
    let dir = base.join(format!("mg-ag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("k");
    run(Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "throwaway", "-f"])
        .arg(&key));
    let sock = dir.join("a.sock");
    let agent = Command::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&sock)
        .env_remove("SSH_AUTH_SOCK")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let guard = Throwaway { dir, agent, sock };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !guard.sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // The key goes into this agent only: the variable is set for this one
    // command, to this socket.
    run(Command::new("ssh-add")
        .arg(&key)
        .env("SSH_AUTH_SOCK", &guard.sock));
    guard
}

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl Sink for Lines {
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.0.lock().unwrap().push(line.to_owned());
        Ok(())
    }
}

fn cfg(seed: u8) -> IrohConfig {
    IrohConfig {
        device_key: [seed; 32],
        relay_url: None,
        relay_token: None,
        bind: Some("127.0.0.1:0".parse().unwrap()),
        listen: true,
        relay_only: false,
        alpn: connecto_net::iroh_transport::ALPN,
    }
}

#[test]
fn a_real_ssh_agent_signs_for_a_real_iroh_connection() {
    if !(have("ssh-agent") && have("ssh-keygen") && have("ssh-add")) {
        eprintln!("the OpenSSH tools are not installed: skipped");
        return;
    }
    let agent = throwaway();
    let public = std::fs::read_to_string(agent.dir.join("k.pub")).unwrap();

    // The key the agent holds is the one just generated, and nothing else.
    let held = AgentSigner::list(Some(Path::new(&agent.sock))).unwrap();
    assert_eq!(held.len(), 1);
    let signer = AgentSigner::new(agent.sock.clone(), held[0].clone());

    // The host allows exactly that public key.
    let keys = Arc::new(AuthorizedKeySet::parse(&public).unwrap());
    let log = Lines::default();
    let gate = Arc::new(Gate::new(
        Arc::new(PauseSwitch::default()),
        Audit::with_sink(log.clone()).unwrap(),
        "TEST",
    ));
    let guard = Arc::new(RemoteGate::with_gate(Arc::clone(&gate)));
    guard.set_enabled(true);
    let host = Arc::new(HostService::new(
        Arc::new(FixedTerminals(Mutex::new(vec![]))),
        Arc::new(move || Arc::clone(&keys)),
        guard as Arc<dyn RemoteGuard>,
    ));
    let host_node = Arc::new(IrohNode::start(&cfg(21)).unwrap());
    {
        let (node, host) = (Arc::clone(&host_node), Arc::clone(&host));
        std::thread::spawn(move || {
            while let Ok(next) = node.accept(Duration::from_millis(100)) {
                if let Some(conn) = next {
                    let host = Arc::clone(&host);
                    std::thread::spawn(move || host.serve(&*conn));
                }
            }
        });
    }

    let client_node = IrohNode::start(&cfg(22)).unwrap();
    let conn = client_node
        .connect(&PeerAddr {
            id: host_node.id(),
            relay: None,
            direct: host_node.direct_addrs(),
        })
        .unwrap();
    let client = RemoteClient::connect(Arc::from(conn), "agent-test", &host_node.id(), &signer)
        .expect("the agent's signature was not accepted");
    assert!(client.list().unwrap().is_empty());
    assert!(gate.flush(Duration::from_secs(2)));
    let audit = log.0.lock().unwrap().join("\n");
    assert!(
        audit.contains("\"action\":\"connect\"") && audit.contains("\"decision\":\"allow\""),
        "{audit}"
    );
    assert!(
        audit.contains("SHA256:"),
        "the audit names the key by fingerprint: {audit}"
    );
    client.close();
}
