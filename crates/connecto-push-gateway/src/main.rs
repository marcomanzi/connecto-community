//! `connecto-push-gateway <config.json>`: serves `POST /v1/push` on the
//! config's `listen` address. Put it behind a TLS proxy: hosts only send to
//! an `https://` gateway.

use std::path::Path;
use std::sync::Arc;

fn main() -> std::process::ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: connecto-push-gateway <config.json>");
        return std::process::ExitCode::from(2);
    };
    match run(Path::new(&path)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("connecto-push-gateway: {why}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(path: &Path) -> Result<(), String> {
    use connecto_push_gateway::server::{Config, Gateway};
    let text = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let v = serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let config = Config::from_json(&v, path.parent().unwrap_or(Path::new(".")))?;
    let listen = config.listen;
    let hosts = config.hosts.len();
    let gateway = Arc::new(Gateway::new(config)?);
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| format!("{listen}: {e}"))?;
        eprintln!("connecto-push-gateway: {hosts} hosts, listening on {listen}");
        gateway.serve(listener).await;
        Ok(())
    })
}
