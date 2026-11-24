//! A running job's questions to the user (`connecto works ask`, ADR 0040).
//!
//! A job's agent asks one question at a time and ends its turn. The question
//! waits here, shown to the user, until they answer it, say no answer will
//! come, or its time is over (`answer_by`, a few minutes before the job's own
//! deadline, so the agent still has time to end with its JSON). The reply
//! (the answer, or the words saying none came) then waits for the job's agent
//! to be idle, and the job's own loop hands it over ([`Questions::take_reply`]):
//! a [`READY`] notice is typed as the agent's next prompt, and the agent reads
//! the reply with `connecto works answer` (a notice holds 120 bytes at most).
//!
//! The job's final JSON, once it has asked, is bound to the agent's reading
//! of the reply: [`Questions::read`] records, from the terminal's own
//! counters (not the list's cached view), how many replies it had kept and
//! how many turns it had ended when the agent first ran `works answer`. Only
//! a reply kept after that is the job's answer ([`Phase::Read`]): the reply
//! of the turn that asked (a guess, perhaps valid JSON) never is, however
//! early the user answered.
//!
//! A question and its reply belong to the job's terminal as the bridge found
//! the asker in it (its attempt): a restart of that terminal reads nothing.
//! When the job's run ends, [`Questions::close`] takes its question and reply
//! and admits no other, at once: an `ask` checks and registers under the same
//! lock, so none slips in while the job's terminal is being closed.

use connecto_core::{AskReply, agentic_works::JobQuestion};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// The longest question taken, in characters.
pub(super) const MAX_QUESTION: usize = 4000;
/// The longest answer taken, in characters.
pub(super) const MAX_ANSWER: usize = 16_000;

/// The notice that wakes the job's agent once its question has its reply.
pub(super) const READY: &str =
    "Agentic Works: your question has its reply. Run: connecto works answer";

/// What the job's agent reads when no answer will come.
pub(super) fn no_answer(why: &str) -> String {
    format!(
        "No answer: {why} No answer will come to your question. Do not ask again: end this job now with your final JSON as the work type's schema describes, saying what was missing if the schema allows it."
    )
}

/// What the job's agent reads with the user's answer.
pub(super) fn answered(answer: &str) -> String {
    format!(
        "The user answered your question. Their answer, between the lines:\n---\n{answer}\n---\nContinue the job with it, and end with your final JSON as the work type's schema describes."
    )
}

struct Entry {
    question: JobQuestion,
    /// The job's token.
    token: String,
    /// The attempt of the job's terminal that asked.
    attempt: u64,
    answer_by: Instant,
    /// What tells the job's terminal (the bridge's notice to it).
    notify: AskReply,
    /// The words for the agent, once the user answered or the time is over.
    reply: Option<String>,
}

/// A reply handed to a job's terminal.
struct Handed {
    /// The attempt that asked.
    attempt: u64,
    text: String,
    /// When the agent first read it: the replies the terminal had kept and
    /// the turns it had ended then.
    read: Option<Mark>,
}

/// A terminal's own counters at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mark {
    pub replies: u64,
    pub turns: u64,
}

/// Where a job stands with its questions, for its loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// It asked nothing (or its last question was dropped): its turns end
    /// it as before.
    None,
    /// A question waits for its reply, or the reply was not yet read: no
    /// turn ends the job.
    Waiting,
    /// Its agent read the reply then: only a reply kept after it ends the
    /// job.
    Read(Mark),
}

#[derive(Default)]
pub(super) struct Questions {
    next: u64,
    entries: Vec<Entry>,
    /// The last reply handed to each job, by its token.
    replies: HashMap<String, Handed>,
    /// Jobs whose run ended: they ask nothing more.
    closed: HashSet<String>,
}

impl Questions {
    /// Keep `question` (its id given here) of the job `token`, asked by its
    /// terminal's `attempt`, until `answer_by`. Its id. The job's last reply
    /// goes: `answer` never gives an earlier question's reply for this one.
    ///
    /// # Errors
    /// When the job's run ended, it already has a question waiting, or its
    /// time to ask is over.
    pub(super) fn ask(
        &mut self,
        mut question: JobQuestion,
        token: &str,
        attempt: u64,
        answer_by: Instant,
        now: Instant,
        notify: AskReply,
    ) -> Result<u64, String> {
        if self.closed.contains(token) {
            return Err(super::NOT_A_JOB.into());
        }
        if self.entries.iter().any(|e| e.token == token) {
            return Err("This job already has a question waiting. End your turn now: a [connecto notice] tells you when its reply is in.".into());
        }
        if now >= answer_by {
            return Err("Too little of this job's 30 minutes is left to wait for an answer. Nothing was asked: end the job now with your final JSON, saying what was missing if the schema allows it.".into());
        }
        self.next += 1;
        question.id = self.next;
        self.entries.push(Entry {
            question,
            token: token.to_owned(),
            attempt,
            answer_by,
            notify,
            reply: None,
        });
        self.replies.remove(token);
        Ok(self.next)
    }

    /// The user's answer to `id`, or (None) no answer will come.
    ///
    /// # Errors
    /// When `id` waits for no answer any more.
    pub(super) fn answer(
        &mut self,
        id: u64,
        answer: Option<String>,
        now: Instant,
    ) -> Result<(), String> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.question.id == id && e.reply.is_none() && now < e.answer_by)
            .ok_or(
                "That question no longer waits for an answer: its job ended or its time is over.",
            )?;
        entry.reply = Some(match answer.map(|a| a.trim().to_owned()) {
            Some(a) if !a.is_empty() => {
                if a.chars().count() > MAX_ANSWER {
                    return Err(format!(
                        "The answer is over {MAX_ANSWER} characters: shorten it."
                    ));
                }
                answered(&a)
            }
            _ => no_answer("the user chose not to answer."),
        });
        Ok(())
    }

    /// The questions waiting for an answer (of `project`, or all), oldest
    /// first. One whose time is over is not among them.
    pub(super) fn waiting(&self, project: Option<&str>, now: Instant) -> Vec<JobQuestion> {
        self.entries
            .iter()
            .filter(|e| e.reply.is_none() && now < e.answer_by)
            .filter(|e| project.is_none_or(|p| e.question.project == p))
            .map(|e| e.question.clone())
            .collect()
    }

    /// Whether the job `token` has a question whose reply it was not yet
    /// handed.
    pub(super) fn pending(&self, token: &str) -> bool {
        self.entries.iter().any(|e| e.token == token)
    }

    /// For the job `token`, whose agent is idle now: once its question has
    /// a reply (one whose time is over gets the words saying no answer
    /// came), what tells its terminal. The reply is then kept for
    /// [`Questions::read`], and the question is done.
    pub(super) fn take_reply(&mut self, token: &str, now: Instant) -> Option<AskReply> {
        let at = self.entries.iter().position(|e| e.token == token)?;
        let entry = &mut self.entries[at];
        if entry.reply.is_none() && now >= entry.answer_by {
            entry.reply = Some(no_answer("no answer came in time."));
        }
        entry.reply.as_ref()?;
        let entry = self.entries.remove(at);
        self.replies.insert(
            entry.token,
            Handed {
                attempt: entry.attempt,
                text: entry.reply.unwrap_or_default(),
                read: None,
            },
        );
        Some(entry.notify)
    }

    /// The agent of the job `token` (its terminal's `attempt`) reads the
    /// reply handed to it, its terminal's counters at `now`: the first read
    /// is kept as the point after which its JSON counts.
    pub(super) fn read(&mut self, token: &str, attempt: u64, now: Mark) -> Option<&str> {
        let handed = self
            .replies
            .get_mut(token)
            .filter(|h| h.attempt == attempt)?;
        handed.read.get_or_insert(now);
        Some(handed.text.as_str())
    }

    /// Where the job `token` stands with its questions.
    pub(super) fn phase(&self, token: &str) -> Phase {
        if self.pending(token) {
            return Phase::Waiting;
        }
        match self.replies.get(token) {
            None => Phase::None,
            Some(Handed { read: None, .. }) => Phase::Waiting,
            Some(Handed {
                read: Some(mark), ..
            }) => Phase::Read(*mark),
        }
    }

    /// The job `token`'s run ended: its question and reply go, and it asks
    /// nothing more until [`Questions::release`].
    pub(super) fn close(&mut self, token: &str) {
        self.entries.retain(|e| e.token != token);
        self.replies.remove(token);
        self.closed.insert(token.to_owned());
    }

    /// The job `token` is no longer running anywhere (an `ask` finds no job
    /// for it): its closed mark can go.
    pub(super) fn release(&mut self, token: &str) {
        self.closed.remove(token);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn question(project: &str, text: &str) -> JobQuestion {
        JobQuestion {
            id: 0,
            project: project.into(),
            project_name: "harbor".into(),
            database: "dev".into(),
            job: 7,
            kind: "review".into(),
            terminal: "claude-3".into(),
            text: text.into(),
            asked_at: String::new(),
            answer_by: String::new(),
        }
    }

    fn told() -> (Arc<Mutex<Vec<String>>>, AskReply) {
        let said = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&said);
        (said, Box::new(move |t| into.lock().unwrap().push(t)))
    }

    #[test]
    fn an_answer_goes_back_to_its_job_once_and_only_when_asked_for() {
        let now = Instant::now();
        let by = now + Duration::from_secs(60);
        let mut q = Questions::default();
        let (said, notify) = told();
        let a = q
            .ask(question("p1", "Which season?"), "tok-a", 1, by, now, notify)
            .unwrap();
        let (_, notify) = told();
        // One question at a time per job.
        assert!(
            q.ask(question("p1", "again"), "tok-a", 1, by, now, notify)
                .is_err()
        );
        let (_, notify) = told();
        let b = q
            .ask(question("p2", "Which team?"), "tok-b", 1, by, now, notify)
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(q.waiting(None, now).len(), 2);
        assert_eq!(q.waiting(Some("p2"), now)[0].text, "Which team?");
        // Nothing to hand over before an answer.
        assert!(q.take_reply("tok-a", now).is_none());
        assert!(q.pending("tok-a") && q.read("tok-a", 1, AT).is_none());
        assert_eq!(q.phase("tok-a"), Phase::Waiting);
        assert_eq!(q.phase("tok-none"), Phase::None);
        q.answer(a, Some(" 2024/25 ".into()), now).unwrap();
        // Answered: no longer shown, not answered twice.
        assert_eq!(q.waiting(None, now).len(), 1);
        assert!(q.answer(a, Some("x".into()), now).is_err());
        let notify = q.take_reply("tok-a", now).unwrap();
        notify(READY.to_owned());
        assert_eq!(*said.lock().unwrap(), [READY]);
        assert!(READY.len() <= 120, "a notice holds 120 bytes");
        // Handed over but not yet read: still no turn ends the job.
        assert_eq!(q.phase("tok-a"), Phase::Waiting);
        let first = Mark {
            replies: 4,
            turns: 2,
        };
        assert!(
            q.read("tok-a", 1, first)
                .unwrap()
                .contains("---\n2024/25\n---")
        );
        // The first read is the point after which a reply counts.
        q.read(
            "tok-a",
            1,
            Mark {
                replies: 9,
                turns: 9,
            },
        )
        .unwrap();
        assert_eq!(q.phase("tok-a"), Phase::Read(first));
        assert!(!q.pending("tok-a"));
        assert!(q.take_reply("tok-a", now).is_none());
        // Declined.
        q.answer(b, None, now).unwrap();
        assert!(q.take_reply("tok-b", now).is_some());
        assert!(
            q.read("tok-b", 1, AT)
                .unwrap()
                .contains("chose not to answer")
        );
    }

    const AT: Mark = Mark {
        replies: 0,
        turns: 0,
    };

    #[test]
    fn a_reply_is_only_for_the_attempt_and_the_question_that_asked() {
        let now = Instant::now();
        let by = now + Duration::from_secs(60);
        let mut q = Questions::default();
        let (_, notify) = told();
        let first = q
            .ask(question("p", "Which season?"), "tok", 3, by, now, notify)
            .unwrap();
        q.answer(first, Some("2024/25".into()), now).unwrap();
        assert!(q.take_reply("tok", now).is_some());
        // The terminal restarted in place: same id, another attempt.
        assert!(q.read("tok", 4, AT).is_none());
        assert_eq!(
            q.phase("tok"),
            Phase::Waiting,
            "a restart's read counts not"
        );
        assert!(q.read("tok", 3, AT).is_some());
        // A second question: the first one's reply is gone at once.
        let (_, notify) = told();
        q.ask(question("p", "Which team?"), "tok", 3, by, now, notify)
            .unwrap();
        assert!(q.read("tok", 3, AT).is_none());
        assert!(q.pending("tok"));
        assert_eq!(q.phase("tok"), Phase::Waiting, "the old read counts not");
    }

    #[test]
    fn a_closed_job_keeps_nothing_and_asks_nothing() {
        let now = Instant::now();
        let by = now + Duration::from_secs(60);
        let mut q = Questions::default();
        let (_, notify) = told();
        q.ask(question("p", "?"), "tok", 1, by, now, notify)
            .unwrap();
        q.close("tok");
        assert!(!q.pending("tok") && q.waiting(None, now).is_empty());
        // An ask in flight when the run ended is refused.
        let (_, notify) = told();
        assert!(
            q.ask(question("p", "?"), "tok", 1, by, now, notify)
                .is_err()
        );
        assert!(q.waiting(None, now).is_empty());
        q.release("tok");
        let (_, notify) = told();
        assert!(q.ask(question("p", "?"), "tok", 1, by, now, notify).is_ok());
    }

    #[test]
    fn a_question_whose_time_is_over_tells_the_job_no_answer_came() {
        let now = Instant::now();
        let by = now + Duration::from_secs(60);
        let later = by + Duration::from_secs(1);
        let mut q = Questions::default();
        let (_, notify) = told();
        // Too late to ask.
        assert!(
            q.ask(question("p", "?"), "tok", 1, now, now, notify)
                .is_err()
        );
        let (_, notify) = told();
        let id = q
            .ask(question("p", "?"), "tok", 1, by, now, notify)
            .unwrap();
        assert!(q.waiting(None, later).is_empty());
        assert!(q.answer(id, Some("late".into()), later).is_err());
        assert!(q.take_reply("tok", later).is_some());
        assert!(
            q.read("tok", 1, AT)
                .unwrap()
                .contains("no answer came in time")
        );
    }
}
