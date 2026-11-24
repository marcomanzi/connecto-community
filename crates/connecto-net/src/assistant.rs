//! The host's conversation model: a local OpenAI-compatible server (LM Studio,
//! llama.cpp, Ollama…) that answers a peer's [`Completion`] requests, such as
//! a phone's voice calls that have no model of their own.
//!
//! The host alone talks to it, and only on this machine: the address must be
//! a loopback one, so nothing is opened to the network and a setting cannot
//! turn the host into a proxy. It is off until a model is set in
//! `assistant.json` (`{"model": "…", "url": "…"}`, the URL defaulting to
//! [`DEFAULT_URL`]).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::wire::Completion;

/// LM Studio's address on this machine.
pub const DEFAULT_URL: &str = "http://127.0.0.1:1234/v1/chat/completions";

/// The answer when no model is set: the peer stops asking for a while.
pub const NOT_SET_UP: &str = "no conversation model is set up on this machine";

/// The longest a request may take, whatever the peer waits.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// The most bytes read back from the server.
const MAX_RESPONSE: u64 = 4 * 1024 * 1024;

/// Which model answers, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantConfig {
    pub url: String,
    pub model: String,
}

impl AssistantConfig {
    /// The settings in `path`; `None` when the file is missing, unreadable or
    /// names no model.
    #[must_use]
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Self::parse(&text)
    }

    /// The settings in `text`, as [`Self::load`] reads them.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        let model = v["model"].as_str()?.trim();
        if model.is_empty() {
            return None;
        }
        let url = v["url"]
            .as_str()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .unwrap_or(DEFAULT_URL);
        Some(Self {
            url: url.to_owned(),
            model: model.to_owned(),
        })
    }
}

/// An `http://` URL on this machine: its socket address, host header and
/// path. Anything else (https, another host, a name that resolves elsewhere)
/// is refused.
fn local_endpoint(url: &str) -> Result<(SocketAddr, String, String), String> {
    const BAD: &str = "the conversation model's address must be http:// on this machine";
    let rest = url.strip_prefix("http://").ok_or(BAD)?;
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    if authority.is_empty() || authority.contains('@') {
        return Err(BAD.to_owned());
    }
    let with_port = if authority.ends_with(']') || !authority.contains(':') {
        format!("{authority}:80")
    } else {
        authority.to_owned()
    };
    let host = with_port.rsplit_once(':').map_or("", |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if !(host == "localhost" || host.parse::<std::net::IpAddr>().is_ok()) {
        return Err(BAD.to_owned());
    }
    let addr = with_port
        .to_socket_addrs()
        .map_err(|_| BAD.to_owned())?
        .find(|a| a.ip().is_loopback())
        .ok_or(BAD)?;
    Ok((addr, authority.to_owned(), path.to_owned()))
}

/// Asks the model in `config` for `request`, waiting at most `wait` (and
/// never more than [`MAX_WAIT`]): the answer's text, with any reasoning
/// ("thinking") left out.
///
/// # Errors
/// Why there is no answer, in words a person can read: a bad address, the
/// server not running, too slow, or its own error.
pub fn complete(
    config: &AssistantConfig,
    request: &Completion,
    wait: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + wait.min(MAX_WAIT);
    let (addr, authority, path) = local_endpoint(&config.url)?;
    let body = json!({
        "model": config.model,
        "messages": request
            .messages
            .iter()
            .map(|(role, text)| json!({"role": role.name(), "content": text}))
            .collect::<Vec<_>>(),
        "max_tokens": request.max_tokens,
        "temperature": f64::from(request.temperature) / 100.0,
        "stream": false,
        // Qwen3 and others: answer at once, without a reasoning pass.
        "chat_template_kwargs": {"enable_thinking": false},
        "reasoning_effort": "none",
    })
    .to_string();
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| "the conversation model took too long".to_owned())
    };
    let mut stream = TcpStream::connect_timeout(&addr, left()?.min(Duration::from_secs(2)))
        .map_err(|_| "the conversation model is not running".to_owned())?;
    stream.set_write_timeout(Some(left()?)).ok();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Accept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body.as_bytes()))
        .map_err(|_| "the conversation model is not running".to_owned())?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        stream.set_read_timeout(Some(left()?)).ok();
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() as u64 > MAX_RESPONSE {
                    return Err("the conversation model answered too much".to_owned());
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("the conversation model took too long".to_owned());
            }
            Err(_) => break,
        }
    }
    let (status, body) = parse_response(&raw)?;
    let v: Value = serde_json::from_slice(&body)
        .map_err(|_| "the conversation model sent something unexpected".to_owned())?;
    if status != 200 {
        let why = v["error"]["message"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or("an error");
        return Err(format!(
            "the conversation model answered {status}: {}",
            why.chars().take(160).collect::<String>()
        ));
    }
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("the conversation model sent no answer")?;
    let text = without_reasoning(content);
    if text.is_empty() {
        return Err("the conversation model sent no answer".to_owned());
    }
    Ok(text)
}

/// The status and body of an HTTP/1.1 response, chunked or not.
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    const BAD: &str = "the conversation model sent something unexpected";
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or(BAD)?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| BAD)?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(BAD)?;
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if !chunked {
        return Ok((status, body.to_vec()));
    }
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let end = rest.windows(2).position(|w| w == b"\r\n").ok_or(BAD)?;
        let size_text = std::str::from_utf8(&rest[..end]).map_err(|_| BAD)?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| BAD)?;
        rest = rest.get(end + 2..).ok_or(BAD)?;
        if size == 0 {
            return Ok((status, out));
        }
        let chunk = rest.get(..size).ok_or(BAD)?;
        if rest.get(size..size.checked_add(2).ok_or(BAD)?) != Some(b"\r\n".as_slice()) {
            return Err(BAD.to_owned());
        }
        out.extend_from_slice(chunk);
        rest = &rest[size + 2..];
    }
}

/// `text` without a model's reasoning: `<think>…</think>` blocks, and a
/// reasoning block never closed (cut off by the token limit) with all after it.
#[must_use]
pub fn without_reasoning(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    // A template that opens the block itself leaves only its end.
    if let Some(end) = out.rfind("</think>") {
        out = out[end + "</think>".len()..].to_owned();
    }
    out.trim().to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;
    use crate::wire::Role;

    fn request() -> Completion {
        Completion {
            messages: vec![
                (Role::System, "Be brief.".into()),
                (Role::User, "Ciao".into()),
            ],
            max_tokens: 40,
            temperature: 30,
            wait_ms: 5000,
        }
    }

    /// A one-shot server on this machine answering `reply`; it hands back
    /// the request it read.
    fn serve(reply: String) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let handle = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = conn.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some(at) = text.find("\r\n\r\n") {
                    let len: usize = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if got.len() >= at + 4 + len {
                        break;
                    }
                }
            }
            conn.write_all(reply.as_bytes()).unwrap();
            String::from_utf8(got).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn settings_need_a_model_and_default_to_lm_studio() {
        assert_eq!(AssistantConfig::parse("{}"), None);
        assert_eq!(AssistantConfig::parse(r#"{"model":"  "}"#), None);
        assert_eq!(
            AssistantConfig::parse(r#"{"model":"qwen"}"#),
            Some(AssistantConfig {
                url: DEFAULT_URL.into(),
                model: "qwen".into()
            })
        );
    }

    #[test]
    fn only_addresses_on_this_machine_are_reached() {
        for good in [
            "http://127.0.0.1:1234/v1/chat/completions",
            "http://localhost:1235/v1/chat/completions",
            "http://[::1]:8080/x",
        ] {
            assert!(local_endpoint(good).is_ok(), "{good}");
        }
        for bad in [
            "https://127.0.0.1:1234/v1",
            "http://192.168.1.5:1234/v1",
            "http://example.com/v1",
            "http://user@127.0.0.1:1234/v1",
            "ftp://127.0.0.1/",
        ] {
            assert!(local_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn answers_without_thinking_from_a_local_server() {
        let json = r#"{"choices":[{"message":{"content":"<think>hm</think> Ciao!"}}]}"#;
        let (head, tail) = json.split_at(20);
        let (url, server) = serve(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{head}\r\n{:x}\r\n{tail}\r\n0\r\n\r\n",
            head.len(),
            tail.len()
        ));
        let config = AssistantConfig {
            url,
            model: "qwen".into(),
        };
        let text = complete(&config, &request(), Duration::from_secs(5)).unwrap();
        assert_eq!(text, "Ciao!");
        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        let body: Value =
            serde_json::from_str(&sent[sent.find("\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(body["model"], "qwen");
        assert_eq!(body["max_tokens"], 40);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "Ciao");
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn a_server_error_or_none_at_all_is_said() {
        let json = r#"{"error":{"message":"no model loaded"}}"#;
        let (url, server) = serve(format!(
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{json}",
            json.len()
        ));
        let config = AssistantConfig {
            url,
            model: "qwen".into(),
        };
        let err = complete(&config, &request(), Duration::from_secs(5)).unwrap_err();
        assert_eq!(err, "the conversation model answered 404: no model loaded");
        server.join().unwrap();

        // Nothing listening.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = AssistantConfig {
            url: format!("http://127.0.0.1:{port}/v1/chat/completions"),
            model: "qwen".into(),
        };
        let err = complete(&config, &request(), Duration::from_secs(5)).unwrap_err();
        assert_eq!(err, "the conversation model is not running");
    }

    #[test]
    fn a_slow_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let hold = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(800));
            drop(conn);
        });
        let config = AssistantConfig {
            url,
            model: "qwen".into(),
        };
        let started = Instant::now();
        let err = complete(&config, &request(), Duration::from_millis(300)).unwrap_err();
        assert_eq!(err, "the conversation model took too long");
        assert!(started.elapsed() < Duration::from_millis(700));
        hold.join().unwrap();
    }

    #[test]
    fn a_broken_chunked_body_is_an_error_not_a_panic() {
        for raw in [
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nab",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabXY0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
        ] {
            assert!(parse_response(raw.as_bytes()).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn reasoning_is_left_out() {
        assert_eq!(without_reasoning("<think>a\nb</think>\n\nCiao."), "Ciao.");
        assert_eq!(without_reasoning("Ciao.<think>never closed"), "Ciao.");
        assert_eq!(without_reasoning("reasoning only</think>Ciao."), "Ciao.");
        assert_eq!(without_reasoning("Ciao."), "Ciao.");
    }
}
