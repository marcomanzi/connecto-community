//! The web proxy: terminals send requests to Connecto, which sends them on to
//! the secret's bound address with the secret in a header.
//!
//! It listens on 127.0.0.1 only. A request's path is
//! `/<terminal token>/<secret name>/<rest>`: the token (from `connecto secret
//! url`, which the bridge gives only to the terminal it identified) says whose
//! request it is, so the secret is looked up for that terminal's project. The
//! request goes to `<bound base URL>/<rest>` with the bound header set to the
//! prefix and the value; a header of that name from the terminal is dropped.
//! Redirects are not followed (a redirect to another host would take the
//! header with it), the upstream is asked not to compress, and the value in
//! any form is removed from the response before the terminal gets it. Every
//! request is written to the vault's log of uses.
//!
//! One request per connection, plain HTTP/1.1. Bodies up to 32 MB in, 64 MB
//! out.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

use connecto_core::{Binding, SecretCaller};

use crate::Vault;

const MAX_HEAD: usize = 64 * 1024;
const MAX_HEADERS: usize = 100;
const MAX_BODY_IN: usize = 32 * 1024 * 1024;
const MAX_BODY_OUT: u64 = 64 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 32;
const CLIENT_WAIT: Duration = Duration::from_secs(30);
const UPSTREAM_WAIT: Duration = Duration::from_secs(300);

/// Headers that belong to one hop, not the request.
const HOP: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "proxy-authenticate",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "accept-encoding",
];

/// Start listening; returns the port.
///
/// # Errors
/// When no port on 127.0.0.1 can be had.
pub(crate) fn start(vault: Weak<Vault>) -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| format!("the secret proxy could not listen: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("the secret proxy has no address: {err}"))?
        .port();
    let live = Arc::new(AtomicUsize::new(0));
    thread::Builder::new()
        .name("vault-proxy".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if vault.strong_count() == 0 {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                if live.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    live.fetch_sub(1, Ordering::SeqCst);
                    let _ = respond_text(&mut stream, 503, "Too many requests at once.");
                    continue;
                }
                let vault = Weak::clone(&vault);
                let live = Arc::clone(&live);
                let spawned = thread::Builder::new()
                    .name("vault-proxy-conn".into())
                    .spawn(move || {
                        let _ = handle(stream, &vault);
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    // The closure (and its count) never ran.
                }
            }
        })
        .map_err(|err| format!("the secret proxy could not start: {err}"))?;
    Ok(port)
}

/// A request as the terminal sent it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Incoming {
    pub(crate) method: String,
    pub(crate) token: String,
    pub(crate) name: String,
    /// What follows the name, with its query, without the leading `/`.
    pub(crate) rest: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

/// Why a request is refused, as a status and a sentence.
type Refusal = (u16, String);

fn refuse(status: u16, why: &str) -> Refusal {
    (status, why.to_owned())
}

/// Read one request.
pub(crate) fn read_request(reader: &mut impl BufRead) -> Result<Incoming, Refusal> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        reader
            .read_until(b'\n', &mut head)
            .map_err(|_| refuse(400, "The request could not be read."))?;
        if head.len() == before {
            return Err(refuse(400, "The request ended early."));
        }
        if head.len() > MAX_HEAD {
            return Err(refuse(431, "The request's head is too long."));
        }
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
    }
    let head =
        String::from_utf8(head).map_err(|_| refuse(400, "The request's head is not text."))?;
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(refuse(400, "The request line is not understood."));
    };
    if !version.starts_with("HTTP/1.") || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(refuse(400, "Only HTTP/1.1 requests are understood."));
    }
    let path = target.strip_prefix('/').ok_or_else(|| {
        refuse(
            400,
            "Send requests to the address `connecto secret url` gives.",
        )
    })?;
    let mut pieces = path.splitn(3, '/');
    let token = pieces.next().unwrap_or_default().to_owned();
    let name = pieces.next().unwrap_or_default();
    let (name, rest) = match name.split_once('?') {
        // `/<token>/<name>?q`: no path after the name.
        Some((name, query)) => (name.to_owned(), format!("?{query}")),
        None => (
            name.to_owned(),
            pieces.next().unwrap_or_default().to_owned(),
        ),
    };
    if token.is_empty() || name.is_empty() {
        return Err(refuse(
            404,
            "Send requests to the address `connecto secret url` gives.",
        ));
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| refuse(400, "A header is not understood."))?;
        headers.push((key.trim().to_owned(), value.trim().to_owned()));
        if headers.len() > MAX_HEADERS {
            return Err(refuse(431, "Too many headers."));
        }
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let body = if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        read_chunked(reader)?
    } else if let Some(len) = header("content-length") {
        let len: usize = len
            .parse()
            .map_err(|_| refuse(400, "Content-Length is not a number."))?;
        if len > MAX_BODY_IN {
            return Err(refuse(413, "The body is over 32 MB."));
        }
        let mut body = vec![0_u8; len];
        reader
            .read_exact(&mut body)
            .map_err(|_| refuse(400, "The body ended early."))?;
        body
    } else {
        Vec::new()
    };
    Ok(Incoming {
        method: method.to_owned(),
        token,
        name,
        rest,
        headers,
        body,
    })
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, Refusal> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|_| refuse(400, "A chunk could not be read."))?;
        let size = line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| refuse(400, "A chunk size is not understood."))?;
        if size == 0 {
            // Trailers, then the empty line.
            loop {
                let mut trailer = String::new();
                let read = reader
                    .read_line(&mut trailer)
                    .map_err(|_| refuse(400, "The body ended early."))?;
                if read == 0 || trailer.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > MAX_BODY_IN {
            return Err(refuse(413, "The body is over 32 MB."));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader
            .read_exact(&mut body[start..])
            .map_err(|_| refuse(400, "The body ended early."))?;
        let mut end = [0_u8; 2];
        reader
            .read_exact(&mut end)
            .map_err(|_| refuse(400, "The body ended early."))?;
    }
}

fn http_binding(binding: &Binding) -> Option<(String, String, String)> {
    match binding {
        Binding::Http {
            base_url,
            header,
            prefix,
        } => Some((base_url.clone(), header.clone(), prefix.clone())),
        _ => None,
    }
}

/// The address a request goes to: the bound base and what followed the name.
pub(crate) fn upstream_url(base: &str, rest: &str) -> String {
    let base = base.trim_end_matches('/');
    if rest.starts_with('?') {
        format!("{base}{rest}")
    } else {
        format!("{base}/{}", rest.trim_start_matches('/'))
    }
}

/// `bytes` with every form of `value` replaced.
pub(crate) fn hide_value(bytes: Vec<u8>, value: &str) -> Vec<u8> {
    let mut out = bytes;
    for form in connecto_redact::known_forms(value) {
        out = replace_bytes(&out, form.as_bytes(), b"[REDACTED]");
    }
    out
}

fn replace_bytes(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn handle(stream: TcpStream, vault: &Weak<Vault>) -> io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_WAIT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let request = match read_request(&mut reader) {
        Ok(request) => request,
        Err((status, why)) => return respond_text(&mut writer, status, &why),
    };
    let Some(vault) = vault.upgrade() else {
        return respond_text(&mut writer, 503, "Connecto is closing.");
    };
    let Some(caller) = vault.caller_of(&request.token) else {
        return respond_text(
            &mut writer,
            403,
            "This address is not valid; ask again with `connecto secret url`.",
        );
    };
    match forward(&vault, &caller, request) {
        Ok(response) => writer.write_all(&response),
        Err((status, why)) => respond_text(&mut writer, status, &why),
    }
}

/// Send the request on, and the bytes of the response to give back.
fn forward(vault: &Vault, caller: &SecretCaller, request: Incoming) -> Result<Vec<u8>, Refusal> {
    let (entry, (base, header, prefix), value) = vault
        .use_secret(caller, &request.name, "web requests", http_binding)
        .map_err(|why| (403, why))?;
    let url = upstream_url(&base, &request.rest);
    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(&url);
    for (key, val) in &request.headers {
        let lower = key.to_ascii_lowercase();
        if HOP.contains(&lower.as_str()) || lower.eq_ignore_ascii_case(&header) {
            continue;
        }
        builder = builder.header(key.as_str(), val.as_str());
    }
    builder = builder
        .header(header.as_str(), format!("{prefix}{value}"))
        .header("accept-encoding", "identity");
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
        .to_owned();
    vault.log_use(
        caller,
        &entry,
        "http",
        &format!("{} {host}", request.method),
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(UPSTREAM_WAIT))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let sent = if request.body.is_empty() {
        builder
            .body(())
            .map_err(|_| refuse(400, "The request could not be built."))
            .and_then(|req| {
                agent
                    .run(req)
                    .map_err(|err| upstream_error(&err.to_string(), &value))
            })
    } else {
        builder
            .body(request.body)
            .map_err(|_| refuse(400, "The request could not be built."))
            .and_then(|req| {
                agent
                    .run(req)
                    .map_err(|err| upstream_error(&err.to_string(), &value))
            })
    };
    let response = sent?;
    let status = response.status();
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    for (key, val) in response.headers() {
        let lower = key.as_str().to_ascii_lowercase();
        if HOP.contains(&lower.as_str()) || lower == "content-encoding" {
            continue;
        }
        let val = String::from_utf8_lossy(val.as_bytes());
        let val =
            String::from_utf8_lossy(&hide_value(val.as_bytes().to_vec(), &value)).into_owned();
        head.push_str(&format!("{}: {val}\r\n", key.as_str()));
    }
    let body = response
        .into_body()
        .with_config()
        .limit(MAX_BODY_OUT)
        .read_to_vec()
        .map_err(|err| upstream_error(&err.to_string(), &value))?;
    let body = hide_value(body, &value);
    head.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    ));
    let mut out = head.into_bytes();
    out.extend_from_slice(&body);
    Ok(out)
}

fn upstream_error(why: &str, value: &str) -> Refusal {
    let why = String::from_utf8_lossy(&hide_value(why.as_bytes().to_vec(), value)).into_owned();
    (502, format!("The request could not be sent on: {why}"))
}

fn respond_text(stream: &mut impl Write, status: u16, text: &str) -> io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let body = format!("connecto: {text}\n");
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Incoming, Refusal> {
        read_request(&mut BufReader::new(raw.as_bytes()))
    }

    #[test]
    fn a_request_names_its_token_secret_and_rest() {
        let got = parse(
            "GET /abc/GITHUB/repos/x?per_page=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: */*\r\n\r\n",
        )
        .unwrap();
        assert_eq!(got.token, "abc");
        assert_eq!(got.name, "GITHUB");
        assert_eq!(got.rest, "repos/x?per_page=1");
        assert_eq!(got.headers.len(), 2);
        assert!(got.body.is_empty());
        let bare = parse("GET /abc/GITHUB?q=1 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(bare.rest, "?q=1");
    }

    #[test]
    fn bodies_are_read_by_length_and_by_chunks() {
        let got = parse("POST /t/A/x HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!(got.body, b"hello");
        let got = parse(
            "POST /t/A/x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(got.body, b"hello world");
    }

    #[test]
    fn odd_requests_are_refused() {
        assert_eq!(parse("GET / HTTP/1.1\r\n\r\n").unwrap_err().0, 404);
        assert_eq!(parse("GET http://x/ HTTP/1.1\r\n\r\n").unwrap_err().0, 400);
        assert_eq!(parse("get /t/A HTTP/1.1\r\n\r\n").unwrap_err().0, 400);
        assert_eq!(parse("GET /t/A HTTP/2\r\n\r\n").unwrap_err().0, 400);
        assert_eq!(
            parse("POST /t/A HTTP/1.1\r\nContent-Length: 999999999\r\n\r\n")
                .unwrap_err()
                .0,
            413
        );
    }

    #[test]
    fn the_upstream_keeps_the_bound_host() {
        assert_eq!(
            upstream_url("https://api.github.com", "repos/x?y=1"),
            "https://api.github.com/repos/x?y=1"
        );
        assert_eq!(
            upstream_url("https://api.github.com/", "@evil.com/x"),
            "https://api.github.com/@evil.com/x"
        );
        assert_eq!(upstream_url("https://h/v1", "?a=b"), "https://h/v1?a=b");
    }

    #[test]
    fn the_value_is_removed_from_what_comes_back_in_any_form() {
        let value = "secret-value-123";
        let b64 = "c2VjcmV0LXZhbHVlLTEyMw";
        let body = format!("echo: {value} and {b64}").into_bytes();
        let out = String::from_utf8(hide_value(body, value)).unwrap();
        assert!(!out.contains(value));
        assert!(!out.contains(b64), "{out}");
    }
}
