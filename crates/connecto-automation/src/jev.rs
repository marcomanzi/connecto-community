//! A blocking client for TypeSafe's Jev model: `POST /v1/systemone`.
//! Blocking by design (ADR 0002): callers run on a worker thread.

use std::time::Duration;

use serde_json::{Map, Value, json};

pub const DEFAULT_URL: &str = "https://api.typesafe.ai/v1/systemone";
const TIMEOUT: Duration = Duration::from_secs(30);
/// Wait before the one retry of a 429 or 529.
const RETRY_WAIT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JevError {
    /// 401 or 403: the key is wrong or lacks access.
    Auth(String),
    /// 400 or 422: the request was refused. Carries Jev's detail.
    Invalid(String),
    /// 429 or 529 after the retry.
    Busy(u16),
    Server(u16),
    Network(String),
    Malformed(String),
}

impl std::fmt::Display for JevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(why) => write!(f, "Jev refused the API key: {why}"),
            Self::Invalid(why) => write!(f, "Jev rejected the request: {why}"),
            Self::Busy(code) => write!(f, "Jev is busy (HTTP {code})"),
            Self::Server(code) => write!(f, "Jev failed (HTTP {code})"),
            Self::Network(why) => write!(f, "cannot reach Jev: {why}"),
            Self::Malformed(why) => write!(f, "Jev sent an unexpected answer: {why}"),
        }
    }
}

/// One question's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Choice {
        choice: String,
        confidence: f64,
    },
    /// The probability of yes.
    Noul(f64),
    Other,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub answers: Vec<(String, Answer)>,
    pub usage: Usage,
}

impl Response {
    #[must_use]
    pub fn answer(&self, name: &str) -> Option<&Answer> {
        self.answers.iter().find(|(n, _)| n == name).map(|(_, a)| a)
    }
}

/// One call to Jev as it happened: what came of it, the exact JSON body sent
/// (model, state, questions: never the key or any header), and the raw answer
/// (or the status and body of a refusal, or the network error).
#[derive(Debug, Clone, PartialEq)]
pub struct Exchange {
    pub result: Result<Response, JevError>,
    pub request: String,
    pub response: String,
}

pub struct Client {
    url: String,
    agent: ureq::Agent,
    retry_wait: Duration,
    /// Whether a 429 or 529 is tried once more.
    retries: bool,
}

impl Client {
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self::with_retry_wait(url, RETRY_WAIT)
    }

    #[must_use]
    pub fn with_retry_wait(url: &str, retry_wait: Duration) -> Self {
        Self::with_limits(url, TIMEOUT, retry_wait)
    }

    /// A client whose every request gives up after `timeout`, for a caller
    /// that cannot wait long (the prompt hook).
    #[must_use]
    pub fn with_limits(url: &str, timeout: Duration, retry_wait: Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            url: url.to_owned(),
            agent,
            retry_wait,
            retries: true,
        }
    }

    /// A client that sends each request once, never again on 429 or 529,
    /// for a caller with a deadline of its own ([`Client::exchange_within`]).
    #[must_use]
    pub fn single_attempt(url: &str) -> Self {
        Self {
            retries: false,
            ..Self::with_limits(url, TIMEOUT, Duration::ZERO)
        }
    }

    /// One request with one question per entry. `api_key` is used for the
    /// header only and appears in no error.
    pub fn ask(
        &self,
        api_key: &str,
        model: &str,
        state: &str,
        questions: &[(String, Value)],
    ) -> Result<Response, JevError> {
        self.exchange(api_key, model, state, questions).result
    }

    /// As [`Client::ask`], with the request and the raw response kept.
    pub fn exchange(
        &self,
        api_key: &str,
        model: &str,
        state: &str,
        questions: &[(String, Value)],
    ) -> Exchange {
        self.exchange_timed(api_key, model, state, questions, None)
    }

    /// As [`Client::exchange`], with the whole request (connecting, sending,
    /// the answer) given up after `within`, whatever the client's own limit.
    pub fn exchange_within(
        &self,
        api_key: &str,
        model: &str,
        state: &str,
        questions: &[(String, Value)],
        within: Duration,
    ) -> Exchange {
        self.exchange_timed(api_key, model, state, questions, Some(within))
    }

    fn exchange_timed(
        &self,
        api_key: &str,
        model: &str,
        state: &str,
        questions: &[(String, Value)],
        within: Option<Duration>,
    ) -> Exchange {
        let map: Map<String, Value> = questions.iter().cloned().collect();
        let body = json!({"model": model, "state": state, "questions": map}).to_string();
        let mut retried = false;
        loop {
            let (status, text) = match self.post(api_key, &body, within) {
                Ok(done) => done,
                Err(err) => {
                    return Exchange {
                        response: err.to_string(),
                        result: Err(err),
                        request: body,
                    };
                }
            };
            // Before anything is built from it: a refusal may echo the key, and
            // its detail goes to the view and the history.
            let text = if api_key.is_empty() {
                text
            } else {
                text.replace(api_key, "[key]")
            };
            let result = match status {
                200..=299 => parse(&text),
                429 | 529 if self.retries && !retried => {
                    retried = true;
                    std::thread::sleep(self.retry_wait);
                    continue;
                }
                429 | 529 => Err(JevError::Busy(status)),
                401 | 403 => Err(JevError::Auth(detail(&text))),
                400 | 422 => Err(JevError::Invalid(detail(&text))),
                other => Err(JevError::Server(other)),
            };
            let response = if result.is_ok() {
                text
            } else {
                format!("HTTP {status}: {text}")
            };
            return Exchange {
                result,
                request: body,
                response,
            };
        }
    }

    fn post(
        &self,
        api_key: &str,
        body: &str,
        within: Option<Duration>,
    ) -> Result<(u16, String), JevError> {
        let mut request = self.agent.post(&self.url);
        if let Some(within) = within {
            request = request.config().timeout_global(Some(within)).build();
        }
        let mut response = request
            .header("Authorization", &format!("Bearer {api_key}"))
            .content_type("application/json")
            .send(body)
            .map_err(|err| JevError::Network(err.to_string().replace(api_key, "[key]")))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(1 << 20)
            .read_to_string()
            .unwrap_or_default();
        Ok((status, text))
    }
}

/// Jev's `detail` field, shortened, or the start of the body.
fn detail(text: &str) -> String {
    let value: Option<Value> = serde_json::from_str(text).ok();
    let shown = value
        .and_then(|v| v.get("detail").map(Value::to_string))
        .unwrap_or_else(|| text.to_owned());
    shown.chars().take(300).collect()
}

fn parse(text: &str) -> Result<Response, JevError> {
    let value: Value =
        serde_json::from_str(text).map_err(|err| JevError::Malformed(err.to_string()))?;
    let answers = value
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| JevError::Malformed("no answers".to_owned()))?;
    let answers = answers
        .iter()
        .map(|(name, a)| {
            let parsed = match a.get("type").and_then(Value::as_str) {
                Some("choice") => match (
                    a.get("choice").and_then(Value::as_str),
                    a.get("confidence").and_then(Value::as_f64),
                ) {
                    (Some(choice), Some(confidence)) => Answer::Choice {
                        choice: choice.to_owned(),
                        confidence,
                    },
                    _ => Answer::Other,
                },
                Some("noul") => a
                    .get("noul")
                    .and_then(Value::as_f64)
                    .map_or(Answer::Other, Answer::Noul),
                _ => Answer::Other,
            };
            (name.clone(), parsed)
        })
        .collect();
    let usage = value.get("usage");
    let count = |field: &str| {
        usage
            .and_then(|u| u.get(field))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Ok(Response {
        answers,
        usage: Usage {
            input_tokens: count("input_tokens"),
            output_tokens: count("output_tokens"),
        },
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod fake {
    //! A local HTTP server that answers a script of (status, body) pairs and
    //! records each request (headers and body).
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex, PoisonError};

    pub struct Server {
        pub url: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    pub fn serve(script: Vec<(u16, String)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        std::thread::spawn(move || {
            let mut script = script.into_iter();
            let last = Mutex::new(None);
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut data = Vec::new();
                let mut buf = [0_u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&data).into_owned();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split]
                            .to_ascii_lowercase()
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if data.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                log.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(String::from_utf8_lossy(&data).into_owned());
                let (status, body) = script.next().unwrap_or_else(|| {
                    last.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone()
                        .unwrap_or((500, String::new()))
                });
                *last.lock().unwrap_or_else(PoisonError::into_inner) = Some((status, body.clone()));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Server { url, requests }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ok_body() -> String {
        json!({"model": "jev-1.14.0", "answers": {
            "a": {"type": "choice", "choice": "yes", "confidence": 0.93, "probabilities": {"yes": 0.93, "no": 0.07}},
            "b": {"type": "noul", "noul": 0.2}},
            "usage": {"input_tokens": 120, "output_tokens": 0}})
        .to_string()
    }

    fn client(url: &str) -> Client {
        Client::with_retry_wait(url, Duration::from_millis(10))
    }

    fn q() -> Vec<(String, Value)> {
        vec![("a".to_owned(), json!({"type": "noul", "instructions": "x"}))]
    }

    #[test]
    fn a_good_answer_is_parsed_and_the_request_is_well_formed() {
        let server = fake::serve(vec![(200, ok_body())]);
        let response = client(&server.url)
            .ask("sekret", "jev-1.14.0", "screen", &q())
            .unwrap();
        assert_eq!(
            response.answer("a"),
            Some(&Answer::Choice {
                choice: "yes".into(),
                confidence: 0.93
            })
        );
        assert_eq!(response.answer("b"), Some(&Answer::Noul(0.2)));
        assert_eq!(response.usage.input_tokens, 120);
        let sent = server.requests.lock().unwrap()[0].clone();
        assert!(sent.starts_with("POST /v1/systemone"));
        assert!(
            sent.to_ascii_lowercase()
                .contains("authorization: bearer sekret")
        );
        assert!(sent.contains(r#""model":"jev-1.14.0""#) && sent.contains(r#""state":"screen""#));
    }

    #[test]
    fn busy_is_retried_once() {
        let server = fake::serve(vec![(429, "{}".into()), (200, ok_body())]);
        assert!(client(&server.url).ask("k", "m", "s", &q()).is_ok());
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        let server = fake::serve(vec![(529, "{}".into())]);
        assert_eq!(
            client(&server.url).ask("k", "m", "s", &q()),
            Err(JevError::Busy(529))
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn errors_are_classified_and_never_carry_the_key() {
        let cases = [
            (401, "auth"),
            (403, "auth"),
            (400, "invalid"),
            (422, "invalid"),
            (503, "server"),
        ];
        for (code, kind) in cases {
            let server = fake::serve(vec![(code, r#"{"detail": "no way"}"#.into())]);
            let err = client(&server.url)
                .ask("sekret", "m", "s", &q())
                .unwrap_err();
            let ok = match kind {
                "auth" => matches!(err, JevError::Auth(_)),
                "invalid" => matches!(err, JevError::Invalid(_)),
                _ => matches!(err, JevError::Server(_)),
            };
            assert!(ok, "{code}: {err:?}");
            assert!(!err.to_string().contains("sekret"));
        }
        let err = client("http://127.0.0.1:1/x")
            .ask("sekret", "m", "s", &q())
            .unwrap_err();
        assert!(matches!(err, JevError::Network(_)) && !err.to_string().contains("sekret"));
    }

    #[test]
    fn an_exchange_keeps_the_exact_request_and_the_raw_response_without_the_key() {
        let server = fake::serve(vec![(200, ok_body())]);
        let ex = client(&server.url).exchange("sekret", "jev-1.14.0", "the screen", &q());
        assert!(ex.result.is_ok());
        let body: Value = serde_json::from_str(&ex.request).unwrap();
        assert_eq!(body["model"], "jev-1.14.0");
        assert_eq!(body["state"], "the screen");
        assert!(body["questions"].get("a").is_some());
        assert!(!ex.request.contains("sekret") && !ex.response.contains("sekret"));
        assert!(ex.response.contains("\"usage\""));
        // A refusal keeps its status and body.
        let server = fake::serve(vec![(401, r#"{"detail": "bad key sekret"}"#.into())]);
        let ex = client(&server.url).exchange("sekret", "m", "s", &q());
        assert!(matches!(ex.result, Err(JevError::Auth(_))));
        let shown = ex.result.as_ref().unwrap_err().to_string();
        assert!(!shown.contains("sekret"), "{shown}");
        assert!(ex.response.starts_with("HTTP 401: "), "{}", ex.response);
        assert!(!ex.response.contains("sekret"), "{}", ex.response);
        // A network failure keeps the error.
        let ex = client("http://127.0.0.1:1/x").exchange("sekret", "m", "s", &q());
        assert!(matches!(ex.result, Err(JevError::Network(_))));
        assert!(!ex.response.is_empty() && !ex.response.contains("sekret"));
    }

    #[test]
    fn a_single_attempt_client_never_retries_and_keeps_its_deadline() {
        let server = fake::serve(vec![(429, "{}".into()), (200, ok_body())]);
        let once = Client::single_attempt(&server.url);
        assert_eq!(
            once.exchange_within("k", "m", "s", &q(), Duration::from_secs(5))
                .result,
            Err(JevError::Busy(429))
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        // A server that never answers: given up at the deadline, not the
        // client's 30 s.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/systemone", silent.local_addr().unwrap());
        let started = std::time::Instant::now();
        let ex = Client::single_attempt(&url).exchange_within(
            "sekret",
            "m",
            "s",
            &q(),
            Duration::from_millis(300),
        );
        assert!(
            matches!(ex.result, Err(JevError::Network(_))),
            "{:?}",
            ex.result
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        drop(silent);
    }

    #[test]
    fn a_garbled_answer_is_malformed() {
        let server = fake::serve(vec![(200, "{\"nope\":1}".into())]);
        assert!(matches!(
            client(&server.url).ask("k", "m", "s", &q()),
            Err(JevError::Malformed(_))
        ));
    }
}
