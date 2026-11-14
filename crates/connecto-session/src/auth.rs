//! Running a tool's status command.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use connecto_adapter::{LoginCtx, ToolAdapter};
use connecto_core::AuthState;

const TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);
const MAX_OUTPUT: u64 = 64 * 1024;

fn collect(stream: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stream) = stream {
            let _ = stream.take(MAX_OUTPUT).read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn wait_within(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => thread::sleep(POLL),
            Err(_) => return false,
        }
    }
    false
}

/// Ask the tool whether it is logged in. Only the state and method are kept. A check that fails or hangs is [`AuthState::Unknown`]. Blocks, up to a
/// limit.
pub(crate) fn auth_status(adapter: &dyn ToolAdapter, ctx: &LoginCtx) -> AuthState {
    let spec = adapter.status(ctx);
    let spawned = crate::platform::no_console(&mut Command::new(&spec.program))
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = spawned else {
        return AuthState::Unknown;
    };
    let stdout = collect(child.stdout.take());
    let stderr = collect(child.stderr.take());
    let finished = wait_within(&mut child, TIMEOUT);
    if !finished {
        let _ = child.kill();
        let _ = child.wait();
        return AuthState::Unknown;
    }
    let stdout = stdout.join().unwrap_or_default();
    match adapter.parse_status(&stdout) {
        AuthState::Unknown => adapter.parse_status(&stderr.join().unwrap_or_default()),
        known => known,
    }
}
