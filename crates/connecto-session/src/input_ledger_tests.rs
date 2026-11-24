#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use connecto_core::InputRefused;

use super::*;

use InputSource::{Connecto, Protocol, Remote, Window};
use Verdict::{Foreign, Unknown, User};

const FOREIGN: [InputSource; 3] = [Remote, Connecto, Protocol];
/// The attempt the ledgers under test belong to.
const A: u64 = 1;

fn ledger() -> Ledger {
    let mut l = Ledger::default();
    l.set_attempt(A);
    l
}

/// The first quiet point of a fresh ledger: one prompt of the user's (never
/// credited, the attempt starts tainted), its hook, the turn end.
fn settle(l: &mut Ledger) {
    l.write(b"warm up\r", Window);
    assert_ne!(l.prompt(A, "warm up", false), User);
    l.turn_ended(A, true);
    assert!(l.is_clean());
}

fn clean() -> Ledger {
    let mut l = ledger();
    settle(&mut l);
    l
}

fn gate() -> InputGate {
    let g = InputGate::default();
    g.set_attempt(A);
    g
}

fn ok(_: Vec<u8>) -> Result<(), InputRefused> {
    Ok(())
}

fn full(_: Vec<u8>) -> Result<(), InputRefused> {
    Err(InputRefused::QueueFull)
}

fn clean_gate() -> InputGate {
    let g = gate();
    g.write(b"warm up\r".to_vec(), Window, ok).unwrap();
    assert_ne!(g.prompt(A, "warm up", false), User);
    g.turn_ended(A, true);
    assert!(g.is_clean());
    g
}

// ---- The simple case: credits do happen.

#[test]
fn the_user_is_credited_in_a_quiet_terminal() {
    let mut l = ledger();
    // A new attempt starts tainted: its first prompt is never credited.
    l.write(b"first\r", Window);
    assert_eq!(l.prompt(A, "first", false), Unknown);
    l.turn_ended(A, true);
    l.write(b"hello\r", Window);
    assert_eq!(l.prompt(A, "hello", false), User);
    l.turn_ended(A, true);
    l.write(b"xy\x7f\x7fz\r", Window);
    assert_eq!(l.prompt(A, "z", false), User);
    l.turn_ended(A, true);
    // A prompt queued while the turn runs: still one waiting at a time.
    l.write(b"more\r", Window);
    l.turn_ended(A, true);
    assert_eq!(l.prompt(A, "more", false), User);
    assert!(l.is_clean());
}

// ---- Review round 5, one regression test per finding.

/// Finding 1: a refused write changes nothing but the taint. The window's
/// `ship` is refused its Enter; a remote Enter submits it: not the user's.
#[test]
fn finding_1_a_refused_enter_leaves_the_ledger_and_taints() {
    let g = clean_gate();
    g.write(b"ship".to_vec(), Window, ok).unwrap();
    let before = g.ledger().clone();
    assert_eq!(
        g.write(b"\r".to_vec(), Window, full),
        Err(InputRefused::QueueFull)
    );
    {
        let after = g.ledger();
        assert_eq!(after.waiting(), before.waiting(), "no submission counted");
        assert_eq!(after.draft.text, b"ship", "the draft is as the PTY has it");
        assert!(!after.clean, "a refusal taints");
    }
    assert!(g.log().ends_with(b"ship"), "refused bytes are not logged");
    g.write(b"\r".to_vec(), Remote, ok).unwrap();
    assert_ne!(g.prompt(A, "ship", false), User);
    g.turn_ended(A, true);
    // Recoverable: the queue drains, the terminal goes quiet, credits return.
    g.write(b"next\r".to_vec(), Window, ok).unwrap();
    assert_ne!(g.prompt(A, "next", false), User);
    g.turn_ended(A, true);
    g.write(b"mine\r".to_vec(), Window, ok).unwrap();
    assert_eq!(g.prompt(A, "mine", false), User);
}

/// Finding 1, every refusal: a refused write of any source never adds a
/// submission and always taints.
#[test]
fn finding_1_any_refused_write_only_taints() {
    for source in [Window, Remote, Connecto, Protocol] {
        for bytes in [&b"a\r"[..], b"\r", b"\x7f", b"\x1b[200~x"] {
            let g = clean_gate();
            let before = g.ledger().clone();
            let _ = g.write(bytes.to_vec(), source, |_| Err(InputRefused::Closed));
            let after = g.ledger();
            assert!(!after.clean, "{source:?} {bytes:?}");
            assert_eq!(after.waiting(), before.waiting());
            assert_eq!(after.draft.text, before.draft.text);
            assert_eq!(after.in_paste, before.in_paste);
        }
    }
}

/// Finding 2: a hook is judged by the session it came from. A hook of
/// another attempt is never credited and only taints the ledger it reaches.
#[test]
fn finding_2_a_hook_of_another_attempt_only_taints() {
    let mut old = clean();
    old.write(b"yes\r", Remote);
    // The replacement session, quiet, with the user's `yes` waiting.
    let mut new = ledger();
    new.set_attempt(A + 1);
    new.write(b"warm\r", Window);
    let _ = new.prompt(A + 1, "warm", false);
    new.turn_ended(A + 1, true);
    assert!(new.is_clean());
    new.write(b"yes\r", Window);
    // The old attempt's hook reaches the new ledger (a restart raced the
    // dequeue): not the user's, the count untouched, the ledger tainted.
    assert_ne!(new.prompt(A, "yes", false), User);
    assert_eq!(new.waiting(), 1);
    assert!(!new.is_clean());
    // And an old turn end never cleans it.
    new.turn_ended(A, true);
    assert!(!new.is_clean());
    // The old session's own ledger never credits its foreign `yes`.
    assert_ne!(old.prompt(A, "yes", false), User);
}

/// Finding 3: remote `yes`, window `yes`, hooks in reverse order: neither is
/// the user's.
#[test]
fn finding_3_reordered_hooks_of_equal_text() {
    for peer in FOREIGN {
        let mut l = clean();
        l.write(b"yes\r", peer);
        l.write(b"yes\r", Window);
        assert_ne!(l.prompt(A, "yes", false), User, "{peer:?}");
        assert_ne!(l.prompt(A, "yes", false), User, "{peer:?}");
    }
}

/// Finding 4: remote `yes `, window `yes`; the foreign hook comes trimmed.
#[test]
fn finding_4_a_trimmed_foreign_hook() {
    for peer in FOREIGN {
        let mut l = clean();
        l.write(b"yes \r", peer);
        l.write(b"yes\r", Window);
        assert_ne!(l.prompt(A, "yes", false), User, "{peer:?}");
        assert_ne!(l.prompt(A, "yes", false), User, "{peer:?}");
    }
}

/// Finding 5: more waiting submissions than the ledger keeps: the count
/// stays exact, so the first remote hook, in order and unchanged, is not
/// the user's, and no hook is until every one of them has come.
#[test]
fn finding_5_an_overflowing_queue_keeps_its_count() {
    let mut l = clean();
    l.write(b"yes\r", Remote);
    for n in 0..63 {
        l.write(format!("r{n}\r").as_bytes(), Remote);
    }
    l.write(b"yes\r", Window);
    assert_eq!(l.waiting(), 65);
    assert_ne!(l.prompt(A, "yes", false), User);
    for n in 0..63 {
        assert_ne!(l.prompt(A, &format!("r{n}"), false), User);
        l.turn_ended(A, true);
        assert!(!l.is_clean(), "hooks still due");
    }
    assert_ne!(l.prompt(A, "yes", false), User);
    l.turn_ended(A, true);
    // Every hook came, but the last submission was before the last taint.
    assert!(!l.is_clean());
    l.write(b"again\r", Window);
    assert_ne!(l.prompt(A, "again", false), User);
    l.turn_ended(A, true);
    assert!(l.is_clean());
}

/// Finding 6: control bytes inside a window paste make it uncertain.
#[test]
fn finding_6_a_pasted_control_key_is_not_the_users() {
    let mut l = clean();
    l.write(b"\x1b[200~abc\x03def\x1b[201~\r", Window);
    assert_ne!(l.prompt(A, "abc\x03def", false), User);
    assert_ne!(l.prompt(A, "abcdef", false), User);
    // The oracle agrees.
    let mut tool = Tool::default();
    let mut subs = Vec::new();
    tool.feed(b"\x1b[200~abc\x03def\x1b[201~\r", true, &mut subs);
    assert!(subs.iter().all(|s| !s.truly));
}

// ---- The rules one by one.

/// Rule 2 and 4: any foreign byte taints, and a tainted ledger credits
/// nothing, whatever the text.
#[test]
fn a_foreign_byte_anywhere_taints() {
    for peer in FOREIGN {
        for (name, foreign) in [
            ("a key", &b"x"[..]),
            ("a backspace", b"\x7f"),
            ("a lone Enter", b"\r"),
            ("an empty paste", b"\x1b[200~\x1b[201~"),
        ] {
            let mut l = clean();
            l.write(foreign, peer);
            l.write(b"\x7f\x7fmine\r", Window);
            assert_ne!(l.prompt(A, "mine", false), User, "{peer:?} {name}");
        }
    }
}

/// Rule 2: window keys that make the composer uncertain.
#[test]
fn uncertain_window_keys_taint() {
    for (name, bytes, hook) in [
        ("arrow", &b"\x1b[A\r"[..], "recalled"),
        ("escape", b"\x1bx\r", "x"),
        ("tab", b"a\tb\r", "a\tb"),
        ("line feed", b"a\nb\r", "a\nb"),
        ("ctrl-c", b"\x03mine\r", "mine"),
        ("backslash-Enter", b"one\\\rtwo\r", "one\ntwo"),
        ("bare Enter, then text", b"\rmine\r", "mine"),
        ("blank", b"  \rmine\r", "mine"),
    ] {
        let mut l = clean();
        l.write(bytes, Window);
        assert_ne!(l.prompt(A, hook, false), User, "{name}");
        assert!(!l.is_clean(), "{name}");
    }
}

/// Rule 2: an overlong text.
#[test]
fn an_overlong_text_is_uncertain() {
    let mut l = clean();
    let long = vec![b'a'; MAX_TEXT + 1];
    l.write(&long, Window);
    l.write(b"\r", Window);
    let text = String::from_utf8_lossy(&long[..MAX_TEXT]).into_owned();
    assert_ne!(l.prompt(A, &text, false), User);
}

/// Rule 6: exactly one waiting, its exact text, not cut.
#[test]
fn a_clean_hook_needs_one_waiting_submission_of_its_exact_text() {
    let mut l = clean();
    l.write(b"one\rtwo\r", Window);
    assert_ne!(l.prompt(A, "one", false), User, "two waiting");
    let mut l = clean();
    l.write(b"yes\r", Window);
    assert_ne!(l.prompt(A, "yes ", false), User, "other text");
    let mut l = clean();
    l.write(b"long\r", Window);
    assert_ne!(l.prompt(A, "long", true), User, "cut");
    let mut l = clean();
    assert_ne!(l.prompt(A, "from the command line", false), User);
    assert!(!l.is_clean(), "a hook with nothing waiting taints");
}

/// Rule 5: what a quiet point needs.
#[test]
fn a_quiet_point_needs_all_of_its_conditions() {
    // No submission after the taint.
    let mut l = clean();
    l.write(b"yes\r", Remote);
    let _ = l.prompt(A, "yes", false);
    l.turn_ended(A, true);
    assert!(!l.is_clean(), "no fresh submission");
    // A fresh submission without its hook yet.
    l.write(b"a\r", Window);
    l.turn_ended(A, true);
    assert!(!l.is_clean(), "no fresh hook");
    let _ = l.prompt(A, "a", false);
    // Bytes in the composer.
    l.write(b"dra", Window);
    l.turn_ended(A, true);
    assert!(!l.is_clean(), "composer not empty");
    l.write(b"ft\r", Window);
    let _ = l.prompt(A, "draft", false);
    // A turn end that could not be read.
    l.turn_ended(A, false);
    assert!(!l.is_clean(), "unreadable turn end");
    l.write(b"b\r", Window);
    let _ = l.prompt(A, "b", false);
    // A hook still due.
    l.write(b"c\r", Window);
    l.turn_ended(A, true);
    assert!(!l.is_clean(), "a hook still due");
    let _ = l.prompt(A, "c", false);
    l.turn_ended(A, true);
    assert!(l.is_clean());
}

/// Rule 5: a hook that brings the count to zero does not clear the taint;
/// only its turn end does.
#[test]
fn only_a_turn_end_clears_the_taint() {
    let mut l = clean();
    l.write(b"r\r", Remote);
    let _ = l.prompt(A, "r", false);
    l.turn_ended(A, true);
    l.write(b"a\r", Window);
    let _ = l.prompt(A, "a", false);
    assert_eq!(l.waiting(), 0);
    assert!(!l.is_clean(), "no turn end yet");
    l.turn_ended(A, true);
    assert!(l.is_clean());
}

/// Rule 3: a hook delivered twice takes one submission off, and taints.
/// The heavy randomised run's case: a repeated hook must not leave an
/// uncertain submission's late hook to meet a clean ledger.
#[test]
fn a_repeated_hook_is_counted_once() {
    let mut l = clean();
    l.write(b"yes\r", Window);
    l.write(b"\n", Window);
    l.write(b"yes \r", Window);
    assert_ne!(l.prompt(A, "yes", false), User);
    assert_ne!(l.prompt(A, "yes", false), User, "the same hook again");
    // The line feed and the uncertain submission: one hook took one off.
    assert_eq!(l.waiting(), 2, "the uncertain submission still waits");
    l.turn_ended(A, true);
    l.write(b"yes \r", Window);
    assert_ne!(l.prompt(A, "yes ", false), User, "the uncertain one's hook");
    l.turn_ended(A, true);
    assert!(!l.is_clean(), "a submission still waits");
}

/// Rule 2 and 6: a hook before the previous hook's turn ended (an
/// interrupted turn, a prompt taken mid-turn) is never the user's.
#[test]
fn a_hook_inside_an_open_turn_is_not_the_users() {
    let mut l = clean();
    l.write(b"one\r", Window);
    assert_eq!(l.prompt(A, "one", false), User);
    l.write(b"two\r", Window);
    assert_ne!(l.prompt(A, "two", false), User);
    assert_eq!(
        l.waiting(),
        0,
        "a different text is a submission of its own"
    );
}

/// The tool's start, resume or clear, and lost hook events, taint.
#[test]
fn a_start_or_lost_events_taint() {
    let g = clean_gate();
    g.taint();
    g.write(b"yes\r".to_vec(), Window, ok).unwrap();
    assert_ne!(g.prompt(A, "yes", false), User);
}

#[test]
fn verdict_labels() {
    let mut l = clean();
    l.write(b"x\r", Remote);
    assert_eq!(l.prompt(A, "x", false), Foreign);
    let mut l = clean();
    l.write(b"\x1b[A\r", Window);
    assert_eq!(l.prompt(A, "recalled", false), Unknown);
}

// ---- Order: one lock for the ledger and the enqueue.

/// Several threads at once: the ledger's byte order is the PTY's.
#[test]
fn many_threads_ledger_order_is_pty_order() {
    let gate = Arc::new(InputGate::default());
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let threads: Vec<_> = (0u8..4)
        .map(|t| {
            let (gate, tx) = (Arc::clone(&gate), tx.clone());
            std::thread::spawn(move || {
                let source = [Window, Remote, Connecto, Window][usize::from(t)];
                for n in 0..500u32 {
                    let bytes = format!("{t}:{n}{}", if n % 7 == 0 { "\r" } else { " " });
                    let _ = gate.write(bytes.into_bytes(), source, |b| tx.send(b));
                }
            })
        })
        .collect();
    for t in threads {
        assert!(t.join().is_ok());
    }
    drop(tx);
    let pty: Vec<u8> = rx.iter().flatten().collect();
    assert_eq!(gate.log(), pty);
}

// ---- The oracle: an independent model of the tool.

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[usize::try_from(self.below(items.len() as u64)).unwrap_or(0)]
    }
}

/// How a tool's parser recovers when an escape sequence is broken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Resync {
    /// A stray ESC inside a sequence is ignored, a C0 control is executed,
    /// the sequence goes on until a final byte (ECMA-48 style).
    #[default]
    UntilFinal,
    /// Each ESC starts the sequence again; a C0 control aborts it.
    RestartAtEsc,
    /// Any ESC is only a separator: what follows, with or without `[` or
    /// `O`, is read as a sequence body up to a final byte.
    Separator,
}

/// How the tool reads its input: chosen per case, adversarially, and never
/// from the window's view of the terminal's modes. This is the oracle's
/// own model of a tool's parser: it shares no code with the ledger.
#[derive(Clone, Copy, Debug, Default)]
struct Policy {
    /// The tool is out of bracketed-paste mode: paste markers are junk keys
    /// and the line breaks between them are Enters.
    no_paste_mode: bool,
    /// In paste mode, the tool still submits at each line break of a paste.
    split_pastes: bool,
    /// A line feed (and a backslash-Enter) submits.
    lf_submits: bool,
    /// The tool's parser reads `CSI 13 u`, `CSI 27;m;13 ~` and keypad Enter
    /// as Enter.
    csi_enter: bool,
    /// The parser's resynchronization.
    resync: Resync,
    /// U+009B (UTF-8 `C2 9B`) is a CSI introducer.
    c1: bool,
}

/// What the tool's composer really holds, with each character's writer.
#[derive(Clone, Default)]
struct Tool {
    policy: Policy,
    chars: Vec<(u8, bool)>,
    in_paste: bool,
    /// An escape sequence being read.
    escape: Vec<u8>,
    /// The previous byte (for the UTF-8 C1 introducer).
    prev: u8,
    /// An uncertain key went in since the last submission: the content's
    /// origin cannot be vouched for.
    uncertain: bool,
    /// A byte not from the window went in since the last submission.
    foreign: bool,
    /// Text the tool itself put in the empty composer (a suggestion, a
    /// recalled prompt): no byte shows it, an Enter submits it.
    suggestion: Option<String>,
}

/// A real submission: its text and whether it is truly the user's (every
/// byte and the Enter the window's, no uncertain key, no tool text).
#[derive(Clone, Debug)]
struct Sub {
    text: String,
    truly: bool,
}

impl Tool {
    fn with(policy: Policy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// The tool runs what the composer holds, if anything.
    fn enter(&mut self, window: bool, out: &mut Vec<Sub>) {
        if self.chars.is_empty()
            && let Some(text) = self.suggestion.take()
        {
            // The tool's own text, whoever pressed Enter.
            out.push(Sub { text, truly: false });
        } else {
            let has_text = self.chars.iter().any(|(c, _)| !c.is_ascii_whitespace());
            if has_text || self.uncertain {
                let text: Vec<u8> = self.chars.iter().map(|(c, _)| *c).collect();
                out.push(Sub {
                    text: String::from_utf8_lossy(&text).into_owned(),
                    truly: window
                        && !self.uncertain
                        && !self.foreign
                        && self.chars.iter().all(|(_, w)| *w),
                });
            }
        }
        let (policy, in_paste, escape) = (self.policy, self.in_paste, self.escape.clone());
        *self = Self::with(policy);
        // A line break inside a paste or a sequence leaves them open.
        self.in_paste = in_paste;
        self.escape = escape;
    }

    /// Whether the sequence read so far is complete, for this parser.
    fn complete(&self) -> bool {
        let seq = self.escape.as_slice();
        let fin = |b: &u8| (0x40..=0x7e).contains(b);
        match self.policy.resync {
            Resync::Separator => match seq {
                [] | [_] => false,
                [_, b'[' | b'O'] => false,
                [_, b'[' | b'O', .., last] | [_, .., last] => fin(last),
            },
            _ => match seq {
                [] | [_] | [_, b'['] | [_, b'O'] => false,
                [_, b'[', .., last] => fin(last),
                _ => true,
            },
        }
    }

    /// Whether a complete sequence is Enter to this parser.
    fn is_enter(&self, seq: &[u8]) -> bool {
        let (body, last) = match seq {
            [_, b'O', b'M'] => return true,
            [_, b'[', body @ .., last] => (body, *last),
            [_, body @ .., last] if self.policy.resync == Resync::Separator => (body, *last),
            _ => return false,
        };
        if self.policy.resync == Resync::Separator && last == b'M' && body.is_empty() {
            return true;
        }
        matches!(last, b'u' | b'~') && body.split(|b| matches!(b, b';' | b':')).any(|f| f == b"13")
    }

    fn feed(&mut self, bytes: &[u8], window: bool, out: &mut Vec<Sub>) {
        for &b in bytes {
            if !window {
                self.foreign = true;
            }
            if b != b'\r' {
                // Typing, or any other key, replaces the suggestion.
                self.suggestion = None;
            }
            let prev = std::mem::replace(&mut self.prev, b);
            if self.policy.c1 && prev == 0xc2 && b == 0x9b {
                // U+009B: a CSI introducer for this parser.
                self.chars.pop();
                self.escape = vec![0x1b, b'['];
                self.uncertain = true;
                continue;
            }
            if !self.escape.is_empty() {
                if b == 0x1b {
                    match self.policy.resync {
                        Resync::UntilFinal => {}
                        Resync::RestartAtEsc | Resync::Separator => self.escape = vec![b],
                    }
                    continue;
                }
                if b < 0x20 {
                    if self.policy.resync != Resync::UntilFinal {
                        self.escape.clear();
                        self.uncertain = true;
                    }
                    self.key(b, window, out);
                    continue;
                }
                self.escape.push(b);
                if self.complete() {
                    let seq = std::mem::take(&mut self.escape);
                    self.sequence(&seq, window, out);
                }
                continue;
            }
            if b == 0x1b {
                self.escape.push(b);
                continue;
            }
            self.key(b, window, out);
        }
    }

    /// A key outside an escape sequence.
    fn key(&mut self, b: u8, window: bool, out: &mut Vec<Sub>) {
        if self.in_paste {
            if b == b'\r' || b == b'\n' {
                if self.policy.split_pastes {
                    self.enter(window, out);
                } else {
                    self.chars.push((b'\n', window));
                }
                return;
            }
            if (b < 0x20 && b != b'\t') || b == 0x7f {
                self.uncertain = true;
            }
            self.chars.push((b, window));
            return;
        }
        match b {
            b'\r' if self.chars.last().is_some_and(|(c, _)| *c == b'\\') => {
                if self.policy.lf_submits {
                    self.enter(window, out);
                } else {
                    self.chars.pop();
                    self.chars.push((b'\n', window));
                    self.uncertain = true;
                }
            }
            b'\r' => self.enter(window, out),
            b'\n' if self.policy.lf_submits => self.enter(window, out),
            0x7f | 0x08 => {
                self.chars.pop();
            }
            0x00..=0x1f => self.uncertain = true,
            _ => self.chars.push((b, window)),
        }
    }

    /// A complete escape sequence, as this tool reads it.
    fn sequence(&mut self, seq: &[u8], window: bool, out: &mut Vec<Sub>) {
        let marker = seq == b"\x1b[200~" || seq == b"\x1b[201~";
        if marker && !self.policy.no_paste_mode {
            self.in_paste = seq == b"\x1b[200~";
            return;
        }
        if self.policy.csi_enter && self.is_enter(seq) {
            self.enter(window, out);
            return;
        }
        // Junk to this tool: it may do anything to the composer.
        self.uncertain = true;
    }
}

/// A hook on its way from the tool.
#[derive(Clone, Debug)]
struct Hook {
    text: String,
    truly: bool,
    /// The newest submission it stands for.
    idx: u64,
    attempt: u64,
    /// Delivered twice, back to back (a transport retry).
    twice: bool,
}

/// How the tool reports one submission.
#[derive(Clone, Copy, Debug)]
enum Fate {
    Keep,
    Drop,
    Duplicate,
    Merge,
    Trim,
    Alter,
}

/// The tool and its hooks: hooks may be late, reordered, dropped,
/// duplicated, merged, trimmed or altered; a turn end comes only once every
/// hook of every submission up to that turn's has been delivered (the
/// assumption in the module doc).
#[derive(Clone)]
struct World {
    gate_ledger: Ledger,
    attempt: u64,
    tool: Tool,
    outbox: Vec<Hook>,
    next_idx: u64,
    /// The submission of the last hook delivered in this attempt.
    last_delivered: Option<u64>,
    /// The switch the capture tap applies.
    credit: TerminalCredit,
    /// Panic on a wrong credit (else count it in `wrong`).
    strict: bool,
    wrong: u64,
    /// The text of the last pure window submission (an autonomous hook's
    /// adversarial choice).
    last_window: Option<String>,
    credits: u64,
    trace: Vec<String>,
}

/// An autonomous hook's submission number: it stands for no key.
const AUTONOMOUS: u64 = u64::MAX;

const POOL: &[&str] = &["yes", "y", "ok", "ab", "yes ", "mine", "z"];

impl World {
    /// A world whose tool may have been launched with a command-line prompt.
    fn starting(clean: bool, startup: Option<&str>, policy: Policy) -> Self {
        let mut w = Self {
            gate_ledger: ledger(),
            attempt: A,
            tool: Tool::with(policy),
            outbox: Vec::new(),
            next_idx: 0,
            last_delivered: None,
            credit: TerminalCredit::Quarantine,
            strict: true,
            wrong: 0,
            last_window: None,
            credits: 0,
            trace: Vec::new(),
        };
        if let Some(text) = startup {
            w.startup(text);
        }
        if clean {
            w.write(Window, b"warm up\r", false, &mut |_| Fate::Keep);
            w.deliver(0);
            w.turn_end(true);
            assert!(w.gate_ledger.is_clean());
        }
        w
    }

    /// A write through a gate; `refused`: the PTY queue refuses it.
    fn write(
        &mut self,
        source: InputSource,
        bytes: &[u8],
        refused: bool,
        fate: &mut dyn FnMut(&Sub) -> Fate,
    ) {
        let gate = InputGate {
            ledger: Mutex::new(std::mem::take(&mut self.gate_ledger)),
        };
        let sent = gate.write(bytes.to_vec(), source, |_| {
            if refused {
                Err(InputRefused::QueueFull)
            } else {
                Ok(())
            }
        });
        self.gate_ledger = gate
            .ledger
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        self.trace.push(format!(
            "{source:?} {:?}{}",
            String::from_utf8_lossy(bytes),
            if refused { " REFUSED" } else { "" }
        ));
        if sent.is_err() {
            return;
        }
        let mut subs = Vec::new();
        self.tool.feed(bytes, source == Window, &mut subs);
        for sub in subs {
            if sub.truly {
                self.last_window = Some(sub.text.clone());
            }
            self.next_idx += 1;
            let hook = Hook {
                text: sub.text.clone(),
                truly: sub.truly,
                idx: self.next_idx,
                attempt: self.attempt,
                twice: false,
            };
            let fate = fate(&sub);
            self.trace.push(format!("  submits {sub:?} -> {fate:?}"));
            match fate {
                Fate::Keep => self.outbox.push(hook),
                Fate::Drop => {}
                Fate::Duplicate => self.outbox.push(Hook {
                    twice: true,
                    ..hook
                }),
                Fate::Merge => match self
                    .outbox
                    .iter_mut()
                    .rev()
                    .find(|h| h.attempt == self.attempt)
                {
                    Some(last) => {
                        last.text = format!("{}\n{}", last.text, hook.text);
                        last.truly &= hook.truly;
                        last.idx = hook.idx;
                    }
                    None => self.outbox.push(hook),
                },
                Fate::Trim => self.outbox.push(Hook {
                    text: hook.text.trim().to_owned(),
                    ..hook
                }),
                Fate::Alter => self.outbox.push(Hook {
                    text: POOL[usize::try_from(hook.idx).unwrap_or(0) % POOL.len()].to_owned(),
                    ..hook
                }),
            }
        }
    }

    /// Deliver the hook at `i` of the outbox; checks the invariant.
    fn deliver(&mut self, i: usize) {
        let hook = self.outbox.remove(i);
        if hook.twice {
            self.outbox.insert(
                i,
                Hook {
                    twice: false,
                    ..hook.clone()
                },
            );
            self.deliver_one(&hook);
            self.deliver(i);
        } else {
            self.deliver_one(&hook);
        }
    }

    fn deliver_one(&mut self, hook: &Hook) {
        if hook.attempt == self.attempt && hook.idx != AUTONOMOUS {
            self.last_delivered = Some(hook.idx);
        }
        let judged = self.gate_ledger.prompt(hook.attempt, &hook.text, false);
        let verdict = self.credit.apply(judged);
        self.trace.push(format!(
            "HOOK {:?} (attempt {}, truly {}) -> {verdict:?}",
            hook.text, hook.attempt, hook.truly
        ));
        if verdict == User {
            self.credits += 1;
            if !hook.truly {
                assert!(
                    !self.strict,
                    "credited the user wrongly:\n{}",
                    self.trace.join("\n")
                );
                self.wrong += 1;
            }
        }
    }

    /// The tool starts a prompt by itself, with any text: here the last
    /// window submission's, or one of the pool.
    fn autonomous(&mut self, text: Option<&str>) {
        let text = text
            .map(str::to_owned)
            .or_else(|| self.last_window.clone())
            .unwrap_or_else(|| "yes".to_owned());
        self.trace.push(format!("AUTONOMOUS {text:?}"));
        self.outbox.push(Hook {
            text,
            truly: false,
            idx: AUTONOMOUS,
            attempt: self.attempt,
            twice: false,
        });
    }

    /// A turn end may come now (the tool's guarantee).
    fn turn_end_allowed(&self) -> bool {
        self.last_delivered.is_some_and(|last| {
            self.outbox
                .iter()
                .filter(|h| h.attempt == self.attempt)
                .all(|h| h.idx > last)
        })
    }

    fn turn_end(&mut self, known: bool) {
        self.trace.push(format!("TURN END known={known}"));
        self.gate_ledger.turn_ended(self.attempt, known);
    }

    /// The terminal restarts: a new session, a new ledger; the old hooks may
    /// still reach it, with their own attempt. The tool may start with a
    /// command-line prompt.
    fn restart(&mut self, spurious: Option<&str>) {
        self.attempt += 1;
        self.gate_ledger = Ledger::default();
        self.gate_ledger.set_attempt(self.attempt);
        self.gate_ledger.taint(false);
        self.tool = Tool::with(self.tool.policy);
        self.last_delivered = None;
        self.trace.push(format!("RESTART -> {}", self.attempt));
        if let Some(text) = spurious {
            self.startup(text);
        }
    }

    /// The tool was launched with `text` on its command line: Connecto knows
    /// (the ledger is told); its hook may come late, as long as the contract
    /// holds.
    fn startup(&mut self, text: &str) {
        self.gate_ledger.expect_startup_prompt();
        self.next_idx += 1;
        self.outbox.push(Hook {
            text: text.to_owned(),
            truly: false,
            idx: self.next_idx,
            attempt: self.attempt,
            twice: false,
        });
        self.trace.push(format!("STARTUP PROMPT {text:?}"));
    }

    /// The tool puts its own text in an empty composer.
    fn suggest(&mut self, text: &str) {
        let t = &mut self.tool;
        if t.chars.is_empty() && !t.uncertain && !t.in_paste {
            t.suggestion = Some(text.to_owned());
            self.trace.push(format!("SUGGESTION {text:?}"));
        }
    }
}

const CHUNKS: &[&[u8]] = &[
    b"ab",
    b"yes",
    b"yes",
    b"y",
    b" ",
    b"\r",
    b"\r",
    b"\r",
    b"z\r",
    b"yes\r",
    b"yes \r",
    b"\\",
    b"\x7f",
    b"\x1b[A",
    b"\x1b[200~",
    b"\x1b[201~",
    b"\n",
    b"\x03",
    b"ok",
    b"\x1b[200~a\rb\rc\r\x1b[201~",
    b"\x1b[200~yes\x1b[201~",
    b"\x1b[200~",
    b"\x1b[13u",
    b"\x1bOM",
    b"\x1b[27;5;13~",
    b"\x1b",
    b"[13u",
];

/// What a batch of random runs saw.
#[derive(Default)]
struct Totals {
    credits: u64,
    wrong: u64,
    overflowed: u64,
}

/// Seeds for the random runs: `LEDGER_SEEDS` (more by hand when changing
/// the ledger, e.g. 100000 in release).
fn seeds() -> u64 {
    std::env::var("LEDGER_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000)
}

/// Random runs against the oracle. `autonomous`: the tool may start prompts
/// by itself, with any text. `strict`: a wrong credit panics with its trace.
fn random_runs(seeds: u64, credit: TerminalCredit, autonomous: bool, strict: bool) -> Totals {
    let big = vec![b'a'; MAX_TEXT / 2 + 1];
    let mut totals = Totals::default();
    for seed in 1..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let policy = Policy {
            no_paste_mode: rng.below(3) == 0,
            split_pastes: rng.below(3) == 0,
            lf_submits: rng.below(2) == 0,
            csi_enter: rng.below(2) == 0,
            resync: [Resync::UntilFinal, Resync::RestartAtEsc, Resync::Separator]
                [usize::try_from(rng.below(3)).unwrap_or(0)],
            c1: rng.below(2) == 0,
        };
        let mut w = World::starting(rng.below(2) == 0, None, policy);
        w.credit = credit;
        w.strict = strict;
        w.trace.push(format!("{policy:?}"));
        w.trace.push(format!("seed {seed}"));
        // Some runs have the window alone; some a tool that holds its hooks
        // back for long (the ledger's queue limit).
        let writers = rng.below(4);
        let hold = rng.below(5) == 0;
        // Some runs are a user alone at a well-behaved tool, so credits are
        // seen to happen.
        let calm = rng.below(4) == 0;
        let steps = if hold { 500 } else { 60 };
        for _ in 0..steps {
            match rng.below(if autonomous { 106 } else { 100 }) {
                0..=54 => {
                    let source = match if calm { 0 } else { rng.below(writers + 1) } {
                        0 | 1 => Window,
                        2 => Remote,
                        _ => [Connecto, Protocol][usize::from(rng.below(2) == 0)],
                    };
                    let chunk: &[u8] = if calm {
                        rng.pick(CALM)
                    } else if rng.below(150) == 0 {
                        &big
                    } else {
                        rng.pick(CHUNKS)
                    };
                    let refused = !calm && rng.below(20) == 0;
                    let mut fates = |_: &Sub| match rng.below(if calm { 200 } else { 24 }) {
                        0 => Fate::Drop,
                        1 => Fate::Duplicate,
                        2 => Fate::Merge,
                        3 => Fate::Trim,
                        4 => Fate::Alter,
                        _ => Fate::Keep,
                    };
                    w.write(source, chunk, refused, &mut fates);
                }
                55..=79 if !w.outbox.is_empty() && !(hold && rng.below(40) != 0) => {
                    let len = w.outbox.len() as u64;
                    let i = if !calm && rng.below(3) == 0 {
                        rng.below(len)
                    } else {
                        0
                    };
                    w.deliver(usize::try_from(i).unwrap_or(0));
                    // A calm tool ends each turn before the next prompt.
                    if calm && w.turn_end_allowed() {
                        w.turn_end(true);
                    }
                }
                80..=94 if w.turn_end_allowed() => w.turn_end(true),
                95 => w.turn_end(false),
                96 => w.gate_ledger.taint(false),
                98 | 99 => w.suggest(rng.pick(POOL)),
                97 => {
                    let spurious = (rng.below(2) == 0).then(|| rng.pick(POOL));
                    w.restart(spurious);
                }
                100..=105 => {
                    let text = (rng.below(3) == 0).then(|| rng.pick(POOL));
                    w.autonomous(text);
                    if rng.below(2) == 0 {
                        // Delivered at once, with or without a turn end.
                        let last = w.outbox.len() - 1;
                        w.deliver(last);
                        if rng.below(2) == 0 {
                            w.turn_end(true);
                        }
                    }
                }
                _ => {}
            }
            totals.overflowed += u64::from(w.gate_ledger.unkept > 0);
        }
        while !w.outbox.is_empty() {
            w.deliver(0);
            if w.turn_end_allowed() {
                w.turn_end(true);
            }
        }
        totals.credits += w.credits;
        totals.wrong += w.wrong;
    }
    totals
}

/// `Quarantine`, under its documented assumption (no prompt the tool starts
/// by itself): a credit is always truly the user's typed submission.
#[test]
fn randomised_interleavings_never_credit_the_user_wrongly() {
    let seeds = seeds();
    let t = random_runs(seeds, TerminalCredit::Quarantine, false, true);
    // Not vacuous: real window submissions were credited, and the queue
    // limit was reached.
    assert!(t.credits > seeds / 10, "{}", t.credits);
    assert!(t.overflowed > 0);
}

/// `Off`, with no assumption: autonomous hooks included, nothing is ever
/// credited.
#[test]
fn randomised_with_credit_off_nothing_is_ever_credited() {
    let t = random_runs(seeds(), TerminalCredit::Off, true, true);
    assert_eq!(t.credits, 0);
}

/// The recorded limitation of `Quarantine` (review round 8, finding 1): a
/// tool that starts a prompt by itself, with a waiting window submission's
/// text, is credited to the user. This test asserts that it happens.
#[test]
fn autonomous_hooks_defeat_quarantine() {
    // The plainest case: a clean terminal, the user's `yes` waiting, an
    // autonomous `yes` hook.
    let mut l = clean();
    l.write(b"yes\r", Window);
    assert_eq!(l.prompt(A, "yes", false), User, "the limitation");
    assert_eq!(TerminalCredit::Off.apply(User), Unknown, "and Off drops it");
    // And the random runs find it too.
    let t = random_runs(seeds().min(3000), TerminalCredit::Quarantine, true, false);
    assert!(t.wrong > 0, "autonomous hooks were credited");
}

const CALM: &[&[u8]] = &[b"ab", b"yes", b" ", b"z\r", b"yes\r", b"\x7f"];

// ---- Exhaustive: every interleaving of a small model.

const SMALL: &[&[u8]] = &[
    b"y\r",
    b"y \r",
    b"\r",
    b"\x1b[200~y\ry\x1b[201~",
    b"\x1b[13u",
    b"\x1b[\x1b[13u",
];

/// Every interleaving from `w` to `depth` events. `autonomous`: the tool may
/// also start a prompt by itself, with the text `y`.
fn explore(w: &World, depth: u32, autonomous: bool, credits: &mut u64) {
    *credits += w.credits;
    if depth == 0 {
        return;
    }
    for source in [Window, Remote] {
        for chunk in SMALL {
            for refused in [false, true] {
                if refused && source == Remote {
                    continue;
                }
                // Each submission is kept, dropped or trimmed: one branch per
                // choice (a chunk submits at most once).
                for fate in [Fate::Keep, Fate::Drop, Fate::Trim] {
                    let mut next = w.clone();
                    next.credits = 0;
                    let (mut submitted, mut same) = (false, false);
                    next.write(source, chunk, refused, &mut |sub| {
                        submitted = true;
                        same = sub.text.trim() == sub.text;
                        fate
                    });
                    let redundant = match fate {
                        Fate::Keep => false,
                        Fate::Trim => !submitted || same,
                        _ => !submitted,
                    };
                    if redundant {
                        continue;
                    }
                    explore(&next, depth - 1, autonomous, credits);
                }
            }
        }
    }
    for i in 0..w.outbox.len() {
        let mut next = w.clone();
        next.credits = 0;
        next.deliver(i);
        explore(&next, depth - 1, autonomous, credits);
    }
    if w.turn_end_allowed() {
        let mut next = w.clone();
        next.credits = 0;
        next.turn_end(true);
        explore(&next, depth - 1, autonomous, credits);
    }
    if w.tool.chars.is_empty() && w.tool.suggestion.is_none() {
        let mut next = w.clone();
        next.credits = 0;
        next.suggest("y");
        explore(&next, depth - 1, autonomous, credits);
    }
    if autonomous {
        let mut next = w.clone();
        next.credits = 0;
        next.autonomous(Some("y"));
        explore(&next, depth - 1, autonomous, credits);
    }
}

fn depth() -> u32 {
    // Deeper with `LEDGER_DEPTH=6` (in release: run by hand when changing
    // the ledger).
    std::env::var("LEDGER_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
}

/// The tools of the exhaustive runs: one per resynchronization, paste mode
/// on and off.
fn small_policies() -> [Policy; 3] {
    let liberal = Policy {
        no_paste_mode: false,
        split_pastes: true,
        lf_submits: true,
        csi_enter: true,
        resync: Resync::RestartAtEsc,
        c1: true,
    };
    [
        Policy::default(),
        liberal,
        Policy {
            no_paste_mode: true,
            resync: Resync::Separator,
            ..liberal
        },
    ]
}

#[test]
fn every_small_interleaving_never_credits_the_user_wrongly() {
    let depth = depth();
    let [plain, liberal, off] = small_policies();
    for (start_clean, startup, policy) in [
        (true, None, plain),
        (true, None, liberal),
        (true, None, off),
        (false, Some("y"), liberal),
    ] {
        let mut credits = 0;
        explore(
            &World::starting(start_clean, startup, policy),
            depth,
            false,
            &mut credits,
        );
        if start_clean {
            assert!(credits > 0, "not vacuous");
        }
    }
}

/// `Off`: every small interleaving, autonomous hooks included, credits
/// nothing.
#[test]
fn every_small_interleaving_with_credit_off_credits_nothing() {
    let depth = depth();
    for policy in small_policies() {
        let mut w = World::starting(true, None, policy);
        w.credit = TerminalCredit::Off;
        let mut credits = 0;
        explore(&w, depth, true, &mut credits);
        assert_eq!(credits, 0, "{policy:?}");
    }
}

/// Rule 5: only a pure submission can lead to a quiet point. A command-line
/// prompt's hook (the tool's start) takes an uncertain submission's count;
/// its turn end must not clean the ledger while that hook can still come.
#[test]
fn a_quiet_point_needs_a_pure_submission() {
    let mut l = ledger();
    l.write(b"\n", Window);
    l.write(b"yes\r", Window);
    assert_ne!(l.prompt(A, "from the command line", false), User);
    l.turn_ended(A, true);
    assert!(!l.is_clean());
    l.write(b"yes\r", Window);
    assert_ne!(l.prompt(A, "yes", false), User, "the uncertain one's hook");
    // Recovery needs a pure submission even when the count is balanced: an
    // arrow (recalled text) before the Enter makes it impure.
    let mut l = clean();
    l.write(b"\x1b[A", Window);
    l.write(b"yes\r", Window);
    assert_ne!(l.prompt(A, "yes", false), User);
    assert_eq!(l.waiting(), 0);
    l.turn_ended(A, true);
    assert!(!l.is_clean());
}

// ---- Review round 6.

/// Finding 1: a remote Enter on an apparently empty composer submits the
/// tool's suggestion; a delayed startup hook must not take the count of the
/// window's `a`, and the suggestion's late hook is never the user's.
#[test]
fn round_6_finding_1_every_enter_and_the_startup_prompt_are_counted() {
    let mut l = ledger();
    l.expect_startup_prompt();
    l.write(b"\r", Remote);
    l.write(b"a\r", Window);
    assert_eq!(l.waiting(), 3);
    assert_ne!(l.prompt(A, "the command line", false), User);
    l.turn_ended(A, true);
    assert_ne!(l.prompt(A, "a", false), User);
    l.turn_ended(A, true);
    assert!(!l.is_clean(), "the remote Enter's hook may still come");
    l.write(b"yes\r", Window);
    assert_ne!(l.prompt(A, "yes", false), User, "the suggestion's hook");
}

/// Finding 1: a window Enter on the tool's text is not the user's typed
/// statement, and keeps the ledger tainted.
#[test]
fn round_6_finding_1_a_window_enter_on_tool_text_is_not_the_users() {
    let mut l = clean();
    l.write(b"\r", Window);
    assert_eq!(l.waiting(), 1);
    assert_ne!(l.prompt(A, "a suggestion", false), User);
    l.turn_ended(A, true);
    assert!(!l.is_clean(), "no pure submission since");
    // The oracle agrees: the tool's text, whoever pressed Enter.
    let mut tool = Tool {
        suggestion: Some("yes".into()),
        ..Tool::default()
    };
    let mut subs = Vec::new();
    tool.feed(b"\r", true, &mut subs);
    assert!(subs.len() == 1 && !subs[0].truly);
}

/// Finding 1: an Enter that submits nothing (a dialog's answer) keeps the
/// ledger tainted: nothing but a hook clears a possible submission.
#[test]
fn round_6_an_enter_that_submits_nothing_keeps_the_taint() {
    let mut l = clean();
    l.write(b"\r", Window);
    for n in 0..5 {
        let text = format!("p{n}");
        l.write(format!("{text}\r").as_bytes(), Window);
        assert_ne!(l.prompt(A, &text, false), User);
        l.turn_ended(A, true);
        assert!(!l.is_clean());
    }
}

/// Finding 2: a cut hook while tainted ends the recovery.
#[test]
fn round_6_finding_2_a_cut_hook_while_tainted_taints() {
    let mut l = ledger();
    l.write(b"warm up\r", Window);
    assert_ne!(l.prompt(A, "warm up", true), User);
    l.turn_ended(A, true);
    assert!(!l.is_clean());
}

/// Every rule-2 event, in the middle of a recovery, keeps the next turn end
/// from cleaning the ledger.
#[test]
fn round_6_every_taint_event_ends_a_recovery() {
    type Event = fn(&InputGate);
    let events: Vec<(&str, Event)> = vec![
        ("remote byte", |g| {
            g.write(b"x\x7f".to_vec(), Remote, ok).unwrap();
        }),
        ("connecto byte", |g| {
            g.write(b" ".to_vec(), Connecto, ok).unwrap();
        }),
        ("protocol reply", |g| {
            g.write(b"\x1b[1;1R".to_vec(), Protocol, ok).unwrap();
        }),
        ("uncertain window key", |g| {
            g.write(b"\x1b[A".to_vec(), Window, ok).unwrap();
        }),
        ("refused write", |g| {
            let _ = g.write(b"q".to_vec(), Window, full);
        }),
        ("hook of another attempt", |g| {
            let _ = g.prompt(A + 7, "zz", false);
        }),
        ("unreadable turn end", |g| g.turn_ended(A, false)),
        ("start or lost events", InputGate::taint),
    ];
    // A recovery: a pure submission, its hook, its turn end. Each event goes
    // before the hook, or between the hook and the turn end.
    for (name, event) in &events {
        for at in 0..2 {
            let g = gate();
            g.write(b"one\r".to_vec(), Window, ok).unwrap();
            if at == 0 {
                event(&g);
            }
            let _ = g.prompt(A, "one", false);
            if at == 1 {
                event(&g);
            }
            g.turn_ended(A, true);
            assert!(!g.is_clean(), "{name} at {at}");
        }
    }
    // Hook events with no turn end of their own: a cut hook, an extra hook,
    // a hook before the previous turn ended, and the overflow.
    let hooks: Vec<(&str, Event)> = vec![
        ("cut hook", |g| {
            let _ = g.prompt(A, "one", true);
        }),
        ("hook with nothing waiting", |g| {
            let _ = g.prompt(A, "one", false);
            let _ = g.prompt(A, "extra", false);
        }),
        ("overflow", |g| {
            for _ in 0..=MAX_PENDING {
                g.write(b"w\r".to_vec(), Window, ok).unwrap();
            }
        }),
    ];
    for (name, event) in &hooks {
        let g = gate();
        g.write(b"one\r".to_vec(), Window, ok).unwrap();
        event(&g);
        g.turn_ended(A, true);
        assert!(!g.is_clean(), "{name}");
    }
    // And with the recovery done, the same table taints a clean ledger.
    for (name, event) in events.iter().chain(&hooks) {
        let g = clean_gate();
        event(&g);
        assert!(!g.is_clean(), "{name} while clean");
    }
}

// ---- Review round 7.

/// The reviewer's case: a framed window paste with three line breaks and an
/// outer Enter. A tool out of paste mode runs three prompts from it; the
/// ledger counts every line break, so draining them cannot clean it while
/// the remote `yes` is still on its way.
#[test]
fn round_7_line_breaks_in_a_window_paste_are_counted() {
    let mut l = clean();
    l.write(b"\x1b[200~p1\rp2\rp3\r\x1b[201~", Window);
    l.write(b"\r", Window);
    l.write(b"yes\r", Remote);
    l.write(b"a\r", Window);
    assert_eq!(l.waiting(), 6);
    for text in ["p1", "p2", "p3", "a"] {
        assert_ne!(l.prompt(A, text, false), User);
        l.turn_ended(A, true);
    }
    assert!(!l.is_clean());
    l.write(b"yes\r", Window);
    assert_ne!(l.prompt(A, "yes", false), User, "the remote's late hook");
    // The oracle's tool, out of paste mode, does run three prompts.
    let mut tool = Tool::with(Policy {
        no_paste_mode: true,
        ..Policy::default()
    });
    let mut subs = Vec::new();
    tool.feed(b"\x1b[200~p1\rp2\rp3\r\x1b[201~\r", true, &mut subs);
    assert!(subs.len() >= 3, "{subs:?}");
}

/// Every key some tool may read as Enter is counted, whoever sent it, also
/// split across writes.
#[test]
fn round_7_every_possible_enter_is_counted() {
    for (name, writes) in [
        ("line feed", vec![&b"x\n"[..]]),
        ("CR in a paste", vec![b"\x1b[200~a\rb\x1b[201~"]),
        ("LF in a paste", vec![b"\x1b[200~a\nb\x1b[201~"]),
        ("kitty Enter", vec![b"\x1b[13u"]),
        ("modifyOtherKeys Enter", vec![b"\x1b[27;5;13~"]),
        ("keypad Enter", vec![b"\x1bOM"]),
        ("kitty Enter split", vec![b"\x1b", b"[13u"]),
        ("backslash-Enter", vec![b"a\\\r"]),
    ] {
        for source in [Window, Remote] {
            let mut l = clean();
            for w in &writes {
                l.write(w, source);
            }
            assert_eq!(l.waiting(), 1, "{name} {source:?}");
            assert!(!l.is_clean(), "{name} {source:?}");
        }
    }
    // A window paste with no line break is counted once, with its Enter,
    // and is never pure.
    let mut l = clean();
    l.write(b"\x1b[200~yes\x1b[201~\r", Window);
    assert_eq!(l.waiting(), 1);
    assert_ne!(l.prompt(A, "yes", false), User);
}

// ---- Review round 8.

/// Finding 2: an ESC inside an unfinished sequence. `ESC [ ESC [ 13 u` is
/// Enter to a parser that restarts at each ESC: the ledger counts it, so
/// three of them, the remote `yes` and the window `a` cannot all be drained
/// by four hooks, and the remote's late hook is never the user's.
#[test]
fn round_8_an_esc_inside_a_sequence_is_counted() {
    let mut l = clean();
    for _ in 0..3 {
        l.write(b"\x1b[\x1b[13u", Window);
    }
    l.write(b"yes\r", Remote);
    l.write(b"a\r", Window);
    assert_eq!(l.waiting(), 5);
    for text in ["", "", "", "a"] {
        assert_ne!(l.prompt(A, text, false), User);
        l.turn_ended(A, true);
    }
    assert!(!l.is_clean());
    l.write(b"yes\r", Window);
    assert_ne!(l.prompt(A, "yes", false), User, "the remote's late hook");
    // The oracle's restarting parser does read it as Enter; the one that
    // ignores stray ESCs does not.
    for (resync, enters) in [(Resync::RestartAtEsc, 1), (Resync::UntilFinal, 0)] {
        let mut tool = Tool::with(Policy {
            csi_enter: true,
            resync,
            ..Policy::default()
        });
        tool.chars.push((b'x', true));
        let mut subs = Vec::new();
        tool.feed(b"\x1b[\x1b[13u", true, &mut subs);
        assert_eq!(subs.len(), enters, "{resync:?}");
    }
}

/// The over-count: every Enter-like byte inside an escape stretch, however
/// it is broken or split, and C1 introducers, count; an exact paste marker
/// does not.
#[test]
fn round_8_escape_stretches_are_over_counted() {
    for (name, writes, want) in [
        ("restart at ESC", vec![&b"\x1b[\x1b[13u"[..]], 1),
        ("control inside", vec![b"\x1b[1\x003u"], 1),
        ("CR inside", vec![b"\x1b[1\r3u"], 2),
        ("split", vec![b"\x1b[", b"\x1b", b"[13", b"u"], 1),
        ("ESC as separator", vec![b"\x1b13u"], 1),
        ("keypad", vec![b"\x1bOM"], 1),
        ("modifyOtherKeys", vec![b"\x1b[27;5;13~"], 1),
        ("C1 CSI (UTF-8)", vec![b"\xc2\x9b13u"], 1),
        ("stray C1 byte", vec![b"\x9b13u"], 1),
        ("a closed sequence, then text", vec![b"\x1b[Auuu"], 0),
        ("paste markers", vec![b"\x1b[200~x\x1b[201~"], 0),
        ("non-ASCII text", vec!["é€😀u".as_bytes()], 0),
    ] {
        let mut l = clean();
        for w in &writes {
            l.write(w, Window);
        }
        assert_eq!(l.waiting(), want, "{name}");
    }
    // Non-ASCII text alone does not even taint.
    let mut l = clean();
    l.write("è€😀\r".as_bytes(), Window);
    assert_eq!(l.prompt(A, "è€😀", false), User);
}

/// `Off`, the simple case: the plainest user prompt in a quiet terminal is
/// still not recorded as the user's.
#[test]
fn round_8_credit_off_never_credits_the_simple_case() {
    let mut l = clean();
    l.write(b"hello\r", Window);
    let judged = l.prompt(A, "hello", false);
    assert_eq!(judged, User, "the ledger still runs");
    assert_ne!(TerminalCredit::Off.apply(judged), User);
    assert_eq!(TerminalCredit::Quarantine.apply(judged), User);
}
