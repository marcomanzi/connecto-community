//! Jev judges a phone's voice call turn (ADR 0028): is what the person has
//! said so far finished, so the call may stop listening; and, given the call
//! so far, is it meant for the call at all, so the call may ask before
//! sending it. It only answers that; the words go to the agent as before.
//! The key is the one in Settings > Automations; without it the phone is
//! told so and goes on as without Jev.

use std::sync::Arc;
use std::time::Duration;

use connecto_automation::KeyStore;
use connecto_automation::jev::{Answer, Exchange, Response};
use connecto_net::hosting::TurnJudge;
use connecto_net::wire::{NO_TURN_JUDGE, TurnScores, TurnVerdict};
use serde_json::{Value, json};

/// The Jev model asked.
pub const MODEL: &str = "jev-1.14.0";
/// The questions' names in the request.
const TURN: &str = "turn";
const FITS: &str = "fits";
/// What comes before the person's words in a state with a context.
const SAID: &str = "The person just said:";
/// The longest any one request may take, whatever the phone allows.
const LIMIT: Duration = connecto_net::host::MAX_TURN_WAIT;

/// Without a context: the state is the person's words alone.
const TURN_ALONE: &str = "The text is what a person has said so far, by voice, to an \
assistant, transcribed live; they have just paused. Decide whether they have finished what \
they wanted to say, so the assistant may answer now. Answer no if the sentence or thought is \
cut off half-way (it ends on a connective, article, preposition or an unfinished clause), or \
if they are clearly about to go on.";

const TURN_IN_CONTEXT: &str = "The text is a voice call between a person and an assistant \
that works on a software project for them: first the call's context, then, after 'The person \
just said:', what the person has said so far, transcribed live; they have just paused. Decide \
whether they have finished what they wanted to say, so the assistant may answer now. Answer \
no if the sentence or thought is cut off half-way (it ends on a connective, article, \
preposition or an unfinished clause), or if they are clearly about to go on.";

const FITS_IN_CONTEXT: &str = "The text is a voice call between a person and an assistant \
that works on a software project for them: first the call's context, then, after 'The person \
just said:', what the person has said. Decide whether those words are meant for the \
assistant and make sense in this call: a request, an answer, a question, a change to the \
task, a command for the call (wait, stop, go on, hang up), or a short reply such as yes or \
no. Answer no if they are said to someone else in the room (calling someone by name or by a \
pet name such as honey or amore, or about home and family matters with no link to the \
project), are chatter unrelated to the call, or make no sense.";

/// One request to Jev that is over by `within`, sent once.
pub trait TimedAsker: Send + Sync {
    fn ask_within(
        &self,
        key: &str,
        model: &str,
        state: &str,
        questions: &[(String, Value)],
        within: Duration,
    ) -> Exchange;
}

impl TimedAsker for connecto_automation::jev::Client {
    fn ask_within(
        &self,
        key: &str,
        model: &str,
        state: &str,
        questions: &[(String, Value)],
        within: Duration,
    ) -> Exchange {
        self.exchange_within(key, model, state, questions, within)
    }
}

pub struct JevTurns {
    key: Arc<dyn KeyStore>,
    asker: Arc<dyn TimedAsker>,
}

impl JevTurns {
    /// Asks Jev with `key`'s key: each request is sent once (never again on
    /// 429 or 529) and is over by the phone's wait, at most [`LIMIT`]. It
    /// runs on the peer's stream, so the host's turn-check slot stays taken
    /// for as long as the request lives.
    #[must_use]
    pub fn new(key: Arc<dyn KeyStore>) -> Self {
        Self::with(
            key,
            Arc::new(connecto_automation::jev::Client::single_attempt(
                connecto_automation::jev::DEFAULT_URL,
            )),
        )
    }

    #[must_use]
    pub fn with(key: Arc<dyn KeyStore>, asker: Arc<dyn TimedAsker>) -> Self {
        Self { key, asker }
    }
}

/// The state and the questions: whether the turn is over, and with a
/// context, whether it fits the call (both in one request).
fn request(text: &str, context: &str) -> (String, Vec<(String, Value)>) {
    let noul = |instructions| json!({"type": "noul", "instructions": instructions});
    if context.trim().is_empty() {
        return (text.to_owned(), vec![(TURN.to_owned(), noul(TURN_ALONE))]);
    }
    (
        format!("{}\n\n{SAID} {text}", context.trim()),
        vec![
            (TURN.to_owned(), noul(TURN_IN_CONTEXT)),
            (FITS.to_owned(), noul(FITS_IN_CONTEXT)),
        ],
    )
}

/// A `noul` answer, only as a probability.
fn probability(response: &Response, name: &str) -> Option<f64> {
    match response.answer(name) {
        Some(Answer::Noul(p)) if (0.0..=1.0).contains(p) => Some(*p),
        _ => None,
    }
}

impl TurnJudge for JevTurns {
    fn judge_turn(
        &self,
        text: &str,
        context: &str,
        wait: Duration,
    ) -> Result<TurnScores, TurnVerdict> {
        let Some(key) = self
            .key
            .get()
            .ok()
            .flatten()
            .filter(|k| !k.trim().is_empty())
        else {
            return Err(TurnVerdict::Unavailable(NO_TURN_JUDGE.to_owned()));
        };
        let (state, questions) = request(text, context);
        // Never send a secret the person read out (or the key itself),
        // in their words or in the call's context.
        let state = connecto_redact::redact(&state, std::slice::from_ref(&key))
            .map_err(|why| TurnVerdict::Failed(format!("not sent: {why}")))?
            .get()
            .clone();
        let within = wait.min(LIMIT);
        if within.is_zero() {
            return Err(TurnVerdict::Failed("no time left".to_owned()));
        }
        let started = std::time::Instant::now();
        let exchange = self
            .asker
            .ask_within(&key, MODEL, &state, &questions, within);
        // An answer after the deadline is of no use to the phone.
        if started.elapsed() > within {
            return Err(TurnVerdict::Failed("Jev took too long".to_owned()));
        }
        let response = exchange
            .result
            .map_err(|why| TurnVerdict::Failed(why.to_string()))?;
        // Both questions are `noul`: anything but a probability, for either
        // one asked, is not an answer.
        let unusable = || TurnVerdict::Failed("Jev gave no usable answer".to_owned());
        let complete = probability(&response, TURN).ok_or_else(unusable)?;
        let fits = if questions.len() > 1 {
            Some(probability(&response, FITS).ok_or_else(unusable)?)
        } else {
            None
        };
        Ok(TurnScores { complete, fits })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Mutex;

    use connecto_automation::MemoryKeys;
    use connecto_automation::jev::{Exchange, JevError, Response, Usage};

    use super::*;

    /// Key, model, state, and the questions' names and instructions.
    type Sent = (String, String, String, Vec<(String, String)>);

    /// Answers each question asked (`turn` with `noul`, `fits` with `fits`),
    /// or fails, or sleeps; keeps what it was sent and the deadline given.
    struct Fake {
        noul: Option<Answer>,
        fits: Answer,
        sleep: Duration,
        sent: Mutex<Vec<Sent>>,
        within: Mutex<Vec<Duration>>,
    }

    impl Fake {
        fn new(noul: Option<f64>, sleep: Duration) -> Arc<Self> {
            Self::answering(noul.map(Answer::Noul), Answer::Noul(0.5), sleep)
        }

        fn answering(noul: Option<Answer>, fits: Answer, sleep: Duration) -> Arc<Self> {
            Arc::new(Self {
                noul,
                fits,
                sleep,
                sent: Mutex::new(Vec::new()),
                within: Mutex::new(Vec::new()),
            })
        }
    }

    impl TimedAsker for Fake {
        fn ask_within(
            &self,
            key: &str,
            model: &str,
            state: &str,
            q: &[(String, Value)],
            within: Duration,
        ) -> Exchange {
            self.within.lock().unwrap().push(within);
            // A real request is over by its deadline.
            std::thread::sleep(self.sleep.min(within + Duration::from_millis(5)));
            self.sent.lock().unwrap().push((
                key.to_owned(),
                model.to_owned(),
                state.to_owned(),
                q.iter().map(|(n, v)| (n.clone(), v.to_string())).collect(),
            ));
            Exchange {
                result: match &self.noul {
                    Some(a) => Ok(Response {
                        answers: q
                            .iter()
                            .map(|(n, _)| {
                                let answer = if n == FITS { &self.fits } else { a };
                                (n.clone(), answer.clone())
                            })
                            .collect(),
                        usage: Usage::default(),
                    }),
                    None => Err(JevError::Network("off".into())),
                },
                request: String::new(),
                response: String::new(),
            }
        }
    }

    const WAIT: Duration = Duration::from_millis(500);

    fn complete(p: f64) -> Result<TurnScores, TurnVerdict> {
        Ok(TurnScores {
            complete: p,
            fits: None,
        })
    }

    #[test]
    fn without_a_key_it_cannot_judge_and_asks_nobody() {
        let fake = Fake::new(Some(0.9), Duration::ZERO);
        let judge = JevTurns::with(Arc::new(MemoryKeys::default()), fake.clone());
        assert_eq!(
            judge.judge_turn("ciao", "Project: x", WAIT),
            Err(TurnVerdict::Unavailable(NO_TURN_JUDGE.to_owned()))
        );
        let judge = JevTurns::with(Arc::new(MemoryKeys::with("  ")), fake.clone());
        assert!(matches!(
            judge.judge_turn("ciao", "", WAIT),
            Err(TurnVerdict::Unavailable(_))
        ));
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn without_a_context_only_the_turn_is_asked_of_the_redacted_text() {
        let fake = Fake::new(Some(0.07), Duration::ZERO);
        let judge = JevTurns::with(Arc::new(MemoryKeys::with("sekret-key")), fake.clone());
        let said = "my token is sekret-key and then I want you to";
        assert_eq!(judge.judge_turn(said, " ", WAIT), complete(0.07));
        let sent = fake.sent.lock().unwrap();
        let (key, model, state, questions) = &sent[0];
        assert_eq!((key.as_str(), model.as_str()), ("sekret-key", MODEL));
        assert!(!state.contains("sekret-key"), "{state}");
        assert!(state.starts_with("my token is") && state.ends_with("I want you to"));
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].0, TURN);
        assert!(questions[0].1.contains("\"noul\""));
    }

    #[test]
    fn with_a_context_both_questions_go_in_one_request_redacted() {
        let fake = Fake::answering(Some(Answer::Noul(0.9)), Answer::Noul(0.05), Duration::ZERO);
        let judge = JevTurns::with(Arc::new(MemoryKeys::with("sekret-key")), fake.clone());
        let context = "Project: connecto\nPerson: the key is sekret-key";
        assert_eq!(
            judge.judge_turn("honey, the pasta", context, WAIT),
            Ok(TurnScores {
                complete: 0.9,
                fits: Some(0.05)
            })
        );
        let sent = fake.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "one request");
        let (_, _, state, questions) = &sent[0];
        assert!(!state.contains("sekret-key"), "{state}");
        assert!(state.starts_with("Project: connecto"));
        assert!(
            state.ends_with(&format!("{SAID} honey, the pasta")),
            "{state}"
        );
        let names: Vec<&str> = questions.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, [TURN, FITS]);
    }

    #[test]
    fn an_answer_that_is_not_a_probability_is_a_failed_check() {
        let bad = [
            Answer::Noul(-0.2),
            Answer::Noul(1.5),
            Answer::Noul(f64::NAN),
            Answer::Choice {
                choice: "maybe".into(),
                confidence: 0.9,
            },
            Answer::Other,
        ];
        for answer in &bad {
            // As the turn's answer, and as the fit's.
            for (turn, fits) in [
                (answer.clone(), Answer::Noul(0.5)),
                (Answer::Noul(0.5), answer.clone()),
            ] {
                let judge = JevTurns::with(
                    Arc::new(MemoryKeys::with("k")),
                    Fake::answering(Some(turn), fits, Duration::ZERO),
                );
                assert!(
                    matches!(
                        judge.judge_turn("x", "Project: p", WAIT),
                        Err(TurnVerdict::Failed(_))
                    ),
                    "{answer:?}"
                );
            }
        }
    }

    #[test]
    fn a_failure_or_a_late_answer_is_a_failed_check() {
        let judge = JevTurns::with(
            Arc::new(MemoryKeys::with("k")),
            Fake::new(None, Duration::ZERO),
        );
        assert!(matches!(
            judge.judge_turn("x", "", WAIT),
            Err(TurnVerdict::Failed(_))
        ));
        let slow = Fake::new(Some(0.9), Duration::from_millis(400));
        let judge = JevTurns::with(Arc::new(MemoryKeys::with("k")), slow.clone());
        let started = std::time::Instant::now();
        assert_eq!(
            judge.judge_turn("x", "", Duration::from_millis(100)),
            Err(TurnVerdict::Failed("Jev took too long".to_owned()))
        );
        // The request itself was given the phone's wait, and returned within
        // it: the caller (holding the host's slot) waited for it to end.
        assert_eq!(*slow.within.lock().unwrap(), [Duration::from_millis(100)]);
        assert!(started.elapsed() < Duration::from_millis(350));
        // Never longer than the host's limit, whatever the phone asks.
        judge.judge_turn("x", "", Duration::from_secs(60)).ok();
        assert_eq!(slow.within.lock().unwrap()[1], LIMIT);
    }
}
