//! Output sequencing: a bounded ring on the host and a gap check on the client.
//!
//! A sequence number is the offset of a byte in everything the terminal has
//! printed since it started. A client that holds all bytes below `n` asks to
//! resume at `n`. If the ring still holds `n` the host sends the missing bytes;
//! if not, the client fell off the ring and gets a repaint of the screen.
//!
//! The ring also keeps where the terminal's screen changed size: a size mark
//! at the byte position where the screen took it, made under the same lock
//! that feeds the screen. A reader passes each mark in order, and never reads
//! bytes across one it has not passed, so a peer applies every size exactly
//! between the bytes the host's screen took before and after it.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use connecto_core::GridSize;

/// Bytes the host keeps per terminal for resuming.
pub const RING_BYTES: usize = 1 << 20;

/// Size marks kept, at most. A burst of resizes with no output between them
/// past this drops the oldest; a reader that had not passed it is repainted.
pub const MAX_MARKS: usize = 1024;

/// Where a reader is: the next byte, and the next size mark, it wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct At {
    pub seq: u64,
    pub mark: u64,
}

/// What asking the ring for a position found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RingRead {
    /// Bytes from the asked position, at most the asked count.
    Data { seq: u64, bytes: Vec<u8> },
    /// The client is caught up.
    Empty,
    /// The position was dropped; the oldest kept byte is `oldest`.
    Evicted { oldest: u64 },
    /// The position is beyond anything printed. A client bug, or a restarted
    /// terminal; either way it needs a repaint.
    Future { end: u64 },
    /// The screen took this size here, before the bytes that follow. Only
    /// [`Ring::read_at`] and [`Ring::read_wait_at`] give it; the reader's next
    /// mark is `mark + 1`.
    Size { mark: u64, size: GridSize },
}

#[derive(Debug)]
struct Inner {
    cap: usize,
    buf: VecDeque<u8>,
    /// Sequence number of `buf[0]`.
    start: u64,
    closed: bool,
    /// Size marks, oldest first, as `(seq, size)`: the size the screen took
    /// before byte `seq`.
    marks: VecDeque<(u64, GridSize)>,
    /// The number of `marks[0]`; marks are numbered from 0 in the order made.
    marks_start: u64,
    /// The size in effect before `marks[0]`, once any mark was made. None:
    /// this ring's sizes are not followed, and readers ask the session.
    base: Option<GridSize>,
    /// Where the newest dropped mark was. A resume at or before it cannot be
    /// told the sizes it needs, and is repainted.
    dropped_at: Option<u64>,
}

impl Inner {
    fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    fn marks_end(&self) -> u64 {
        self.marks_start + self.marks.len() as u64
    }

    /// Drop the oldest mark; what it set is in effect from the start.
    fn drop_mark(&mut self) {
        if let Some((seq, size)) = self.marks.pop_front() {
            self.base = Some(size);
            self.dropped_at = Some(seq);
            self.marks_start += 1;
        }
    }

    /// Marks before the oldest kept byte serve no reader: one that has not
    /// passed them is behind that byte too, and is repainted.
    fn drop_old_marks(&mut self) {
        while self.marks.front().is_some_and(|(seq, _)| *seq < self.start) {
            self.drop_mark();
        }
    }

    /// The first mark not at or before `seq`, and the size in effect after
    /// every mark at or before it.
    fn after(&self, seq: u64) -> (u64, Option<GridSize>) {
        let passed = self.marks.iter().take_while(|(at, _)| *at <= seq).count();
        let size = match passed {
            0 => self.base,
            n => self.marks.get(n - 1).map(|(_, size)| *size),
        };
        (self.marks_start + passed as u64, size)
    }
}

/// The host's record of one terminal's output.
#[derive(Debug)]
pub struct Ring {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Ring {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                cap: cap.max(1),
                buf: VecDeque::new(),
                start: 0,
                closed: false,
                marks: VecDeque::new(),
                marks_start: 0,
                base: None,
                dropped_at: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Append output and wake readers.
    pub fn append(&self, bytes: &[u8]) {
        let mut inner = self.lock();
        Self::append_locked(&mut inner, bytes);
        drop(inner);
        self.changed.notify_all();
    }

    fn append_locked(inner: &mut Inner, bytes: &[u8]) {
        inner.buf.extend(bytes);
        let over = inner.buf.len().saturating_sub(inner.cap);
        if over > 0 {
            inner.buf.drain(..over);
            inner.start += over as u64;
            inner.drop_old_marks();
        }
    }

    /// Run `feed` and append `bytes` as one step that `consistent` cannot split.
    /// The PTY reader uses this so the screen and the sequence number agree.
    ///
    /// With `retain` off the bytes are only counted: the sequence number moves
    /// on and nothing is kept, so a reader that asks for them is told they are
    /// gone and gets a repaint. Used while nobody can be looking.
    pub fn append_with(&self, bytes: &[u8], feed: &mut dyn FnMut(&[u8]), retain: bool) {
        let mut inner = self.lock();
        feed(bytes);
        if retain {
            Self::append_locked(&mut inner, bytes);
        } else {
            let end = inner.end() + bytes.len() as u64;
            inner.buf.clear();
            inner.start = end;
            inner.drop_old_marks();
        }
        drop(inner);
        self.changed.notify_all();
    }

    /// Run `resize` while no output can be fed, and when it returns the size
    /// the screen took, mark it at the current position. The session resizes
    /// its screen this way, so the mark sits exactly between the bytes the
    /// screen took before the resize and after it.
    ///
    /// Never waits: when output is being fed, `resize` is not run and false
    /// is returned, so the caller tries again as it does for a busy screen.
    pub fn resize_with(&self, resize: &mut dyn FnMut() -> Option<GridSize>) -> bool {
        let mut inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        let Some(size) = resize() else {
            return true;
        };
        if inner.marks.len() >= MAX_MARKS {
            inner.drop_mark();
        }
        let end = inner.end();
        inner.marks.push_back((end, size));
        drop(inner);
        self.changed.notify_all();
        true
    }

    /// Whether this ring follows the screen's size: a session marked one. A
    /// ring that does not leaves readers to ask the session for the size.
    #[must_use]
    pub fn follows_size(&self) -> bool {
        let inner = self.lock();
        inner.base.is_some() || !inner.marks.is_empty()
    }

    /// Call `f` with the sequence number after the last byte, while no output
    /// can be fed. If `f` reads the screen, the screen holds exactly the bytes
    /// below that number.
    pub fn consistent<T>(&self, f: impl FnOnce(u64) -> T) -> T {
        let inner = self.lock();
        f(inner.end())
    }

    /// As [`Ring::consistent`], with the position after the last byte and
    /// the last size mark: a screen `f` reads took every mark before it.
    pub fn consistent_at<T>(&self, f: impl FnOnce(At) -> T) -> T {
        let inner = self.lock();
        f(At {
            seq: inner.end(),
            mark: inner.marks_end(),
        })
    }

    /// Where a reader that holds every byte below `seq` resumes: the first
    /// mark after `seq`, and the size in effect at `seq` (None when the ring
    /// does not follow sizes). None when `seq` is not held, or when the ring
    /// cannot say what size the reader's screen must have there: a size was
    /// marked exactly at `seq` (the reader may or may not have taken it, and
    /// a shrink and grow back there loses cells a resume would keep), or the
    /// marks up to `seq` were dropped. Such a reader is repainted.
    pub fn resume_at(&self, seq: u64) -> Option<(At, Option<GridSize>)> {
        let inner = self.lock();
        if seq < inner.start || seq > inner.end() {
            return None;
        }
        if inner.dropped_at.is_some_and(|at| at >= seq)
            || inner.marks.iter().any(|(at, _)| *at == seq)
        {
            return None;
        }
        let (mark, size) = inner.after(seq);
        Some((At { seq, mark }, size))
    }

    /// Mark the terminal's output as finished.
    pub fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// The sequence number after the last byte.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.lock().end()
    }

    /// Read up to `max` bytes at `seq`.
    #[must_use]
    pub fn read(&self, seq: u64, max: usize) -> RingRead {
        Self::read_locked(&self.lock(), seq, max)
    }

    fn read_locked(inner: &Inner, seq: u64, max: usize) -> RingRead {
        if seq < inner.start {
            return RingRead::Evicted {
                oldest: inner.start,
            };
        }
        let end = inner.end();
        if seq > end {
            return RingRead::Future { end };
        }
        if seq == end {
            return RingRead::Empty;
        }
        let skip = usize::try_from(seq - inner.start).unwrap_or(usize::MAX);
        let bytes: Vec<u8> = inner.buf.iter().skip(skip).take(max).copied().collect();
        RingRead::Data { seq, bytes }
    }

    /// Like [`Ring::read`], but waits up to `timeout` for something to read
    /// (or for the ring to close) when the client is caught up.
    #[must_use]
    pub fn read_wait(&self, seq: u64, max: usize, timeout: Duration) -> RingRead {
        let mut inner = self.lock();
        if seq == inner.end() && !inner.closed {
            inner = self
                .changed
                .wait_timeout(inner, timeout)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Self::read_locked(&inner, seq, max)
    }

    /// Read at `at`, passing size marks in order: the next mark when it is
    /// due (at or before `at.seq`), else up to `max` bytes, never past the
    /// next mark. A mark the ring no longer holds reads as evicted.
    #[must_use]
    pub fn read_at(&self, at: At, max: usize) -> RingRead {
        Self::read_at_locked(&self.lock(), at, max)
    }

    /// Like [`Ring::read_at`], but waits up to `timeout` for a byte or a mark
    /// (or for the ring to close) when the reader is caught up.
    #[must_use]
    pub fn read_wait_at(&self, at: At, max: usize, timeout: Duration) -> RingRead {
        let mut inner = self.lock();
        if at.seq == inner.end() && at.mark == inner.marks_end() && !inner.closed {
            inner = self
                .changed
                .wait_timeout(inner, timeout)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Self::read_at_locked(&inner, at, max)
    }

    fn read_at_locked(inner: &Inner, at: At, max: usize) -> RingRead {
        if at.mark < inner.marks_start {
            return RingRead::Evicted {
                oldest: inner.start,
            };
        }
        if at.mark > inner.marks_end() {
            return RingRead::Future { end: inner.end() };
        }
        let next = usize::try_from(at.mark - inner.marks_start)
            .ok()
            .and_then(|i| inner.marks.get(i));
        match next {
            Some(&(seq, size)) if seq <= at.seq && at.seq >= inner.start => RingRead::Size {
                mark: at.mark,
                size,
            },
            Some(&(seq, _)) => {
                let room = usize::try_from(seq.saturating_sub(at.seq)).unwrap_or(usize::MAX);
                Self::read_locked(inner, at.seq, max.min(room))
            }
            None => Self::read_locked(inner, at.seq, max),
        }
    }
}

/// What the client does with an `Output` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accept {
    /// Apply the bytes from this offset into the message.
    Apply { skip: usize },
    /// All of it was applied already (a repeat after a resume).
    Duplicate,
    /// Bytes are missing before this message. The stream is broken.
    Gap { expected: u64, got: u64 },
}

/// The client's position in a terminal's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tracker {
    next: u64,
}

impl Tracker {
    #[must_use]
    pub const fn at(next: u64) -> Self {
        Self { next }
    }

    /// The next byte wanted; also the value to resume with.
    #[must_use]
    pub const fn next(&self) -> u64 {
        self.next
    }

    /// Jump to a new position, after a repaint.
    pub fn reset(&mut self, next: u64) {
        self.next = next;
    }

    /// Judge a message of `len` bytes at `seq`, and advance if it applies.
    pub fn accept(&mut self, seq: u64, len: usize) -> Accept {
        let end = seq + len as u64;
        if seq > self.next {
            return Accept::Gap {
                expected: self.next,
                got: seq,
            };
        }
        if end <= self.next {
            return Accept::Duplicate;
        }
        let skip = usize::try_from(self.next - seq).unwrap_or(usize::MAX);
        self.next = end;
        Accept::Apply { skip }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_serves_what_it_holds_and_drops_the_oldest() {
        let ring = Ring::new(8);
        ring.append(b"abcd");
        assert_eq!(
            ring.read(0, 100),
            RingRead::Data {
                seq: 0,
                bytes: b"abcd".to_vec()
            }
        );
        ring.append(b"efghij");
        // 10 bytes written, 8 kept: bytes 0 and 1 are gone.
        assert_eq!(ring.read(0, 100), RingRead::Evicted { oldest: 2 });
        assert_eq!(
            ring.read(2, 3),
            RingRead::Data {
                seq: 2,
                bytes: b"cde".to_vec()
            }
        );
        assert_eq!(ring.read(10, 5), RingRead::Empty);
        assert_eq!(ring.read(11, 5), RingRead::Future { end: 10 });
        assert_eq!(ring.end(), 10);
    }

    #[test]
    fn a_waiting_reader_wakes_on_append_and_on_close() {
        let ring = std::sync::Arc::new(Ring::new(64));
        let waiter = {
            let ring = std::sync::Arc::clone(&ring);
            std::thread::spawn(move || ring.read_wait(0, 10, Duration::from_secs(5)))
        };
        std::thread::sleep(Duration::from_millis(30));
        ring.append(b"hi");
        assert_eq!(
            waiter.join().unwrap(),
            RingRead::Data {
                seq: 0,
                bytes: b"hi".to_vec()
            }
        );
        let started = std::time::Instant::now();
        let ring2 = std::sync::Arc::clone(&ring);
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            ring2.close();
        });
        assert_eq!(
            ring.read_wait(2, 10, Duration::from_secs(5)),
            RingRead::Empty
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        closer.join().unwrap();
        assert!(ring.is_closed());
    }

    #[test]
    fn append_with_feeds_and_appends_under_one_lock() {
        let ring = Ring::new(64);
        let mut fed = Vec::new();
        ring.append_with(
            b"xyz",
            &mut |b| {
                fed.extend_from_slice(b);
            },
            true,
        );
        assert_eq!(fed, b"xyz");
        assert_eq!(ring.consistent(|end| end), 3);
    }

    #[test]
    fn without_retaining_the_ring_counts_bytes_and_keeps_none() {
        let ring = Ring::new(64);
        let mut fed = 0;
        ring.append_with(b"abc", &mut |b| fed += b.len(), true);
        ring.append_with(b"defg", &mut |b| fed += b.len(), false);
        assert_eq!(fed, 7, "the screen still takes every byte");
        assert_eq!(ring.end(), 7);
        // What was counted but not kept is gone: a reader is told to repaint.
        assert_eq!(ring.read(0, 10), RingRead::Evicted { oldest: 7 });
        assert_eq!(ring.read(7, 10), RingRead::Empty);
        ring.append_with(b"hi", &mut |_| {}, true);
        assert_eq!(
            ring.read(7, 10),
            RingRead::Data {
                seq: 7,
                bytes: b"hi".to_vec()
            }
        );
    }

    const fn size(cols: u16, rows: u16) -> GridSize {
        GridSize { cols, rows }
    }

    /// Everything a reader at `at` gets until it is caught up: sizes and
    /// bytes, in order.
    fn drain(ring: &Ring, mut at: At) -> (Vec<String>, At) {
        let mut got = Vec::new();
        loop {
            match ring.read_at(at, 100) {
                RingRead::Size { mark, size } => {
                    got.push(format!("{}x{}", size.cols, size.rows));
                    at.mark = mark + 1;
                }
                RingRead::Data { seq, bytes } => {
                    got.push(String::from_utf8(bytes.clone()).unwrap());
                    at.seq = seq + bytes.len() as u64;
                }
                RingRead::Empty => return (got, at),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn sizes_are_read_at_the_byte_they_were_taken_and_never_read_across() {
        let ring = Ring::new(1024);
        assert!(!ring.follows_size());
        ring.resize_with(&mut || Some(size(80, 24)));
        assert!(ring.follows_size());
        ring.append(b"old ");
        ring.append(b"width");
        ring.resize_with(&mut || Some(size(40, 10)));
        ring.resize_with(&mut || Some(size(50, 12)));
        // A resize the screen did not take marks nothing.
        ring.resize_with(&mut || None);
        ring.append(b"new");
        let (got, at) = drain(&ring, At::default());
        assert_eq!(got, ["80x24", "old width", "40x10", "50x12", "new"]);
        assert_eq!(at, At { seq: 12, mark: 3 });
        // A size with no output after it is read too.
        ring.resize_with(&mut || Some(size(60, 12)));
        assert_eq!(drain(&ring, at).0, ["60x12"]);
    }

    #[test]
    fn a_resume_or_a_capture_takes_the_sizes_before_it_as_passed() {
        let ring = Ring::new(1024);
        ring.resize_with(&mut || Some(size(80, 24)));
        ring.append(b"abc");
        ring.resize_with(&mut || Some(size(40, 10)));
        ring.append(b"def");
        assert_eq!(
            ring.resume_at(4),
            Some((At { seq: 4, mark: 2 }, Some(size(40, 10))))
        );
        assert_eq!(
            ring.resume_at(2),
            Some((At { seq: 2, mark: 1 }, Some(size(80, 24))))
        );
        // A size marked exactly at the resume byte: the reader may or may not
        // have taken it, so it is repainted.
        assert_eq!(ring.resume_at(3), None);
        assert_eq!(ring.resume_at(7), None);
        assert_eq!(ring.consistent_at(|at| at), At { seq: 6, mark: 2 });
        // A ring that does not follow sizes resumes with no size.
        let plain = Ring::new(16);
        plain.append(b"x");
        assert_eq!(plain.resume_at(1), Some((At { seq: 1, mark: 0 }, None)));
    }

    #[test]
    fn a_reader_behind_a_dropped_size_is_told_to_repaint() {
        let ring = Ring::new(8);
        ring.resize_with(&mut || Some(size(80, 24)));
        ring.append(b"abcd");
        ring.resize_with(&mut || Some(size(40, 10)));
        // The bytes before the second size go, and the first size with them.
        ring.append(b"efghijkl");
        assert!(matches!(
            ring.read_at(At::default(), 10),
            RingRead::Evicted { .. }
        ));
        assert_eq!(
            ring.resume_at(5),
            Some((At { seq: 5, mark: 2 }, Some(size(40, 10))))
        );
        // Bytes kept, but more sizes after them than the ring keeps: a resume
        // there cannot be told its sizes, and is repainted.
        let many = Ring::new(1 << 16);
        many.resize_with(&mut || Some(size(80, 24)));
        many.append(b"ab");
        for n in 0..=MAX_MARKS as u16 {
            many.append(b"xy");
            many.resize_with(&mut || Some(size(10 + n, 5)));
        }
        assert_eq!(many.resume_at(1), None);
        assert_eq!(many.resume_at(2), None);
        // Between an x and a y: no mark there, and every mark after it kept.
        let late = many.end() - 1;
        assert!(many.resume_at(late).is_some());
        // A burst of sizes with no output between them keeps the newest.
        let burst = Ring::new(8);
        for n in 0..=MAX_MARKS as u16 {
            burst.resize_with(&mut || Some(size(10 + n, 5)));
        }
        assert!(matches!(
            burst.read_at(At::default(), 10),
            RingRead::Evicted { .. }
        ));
        let (got, _) = drain(&burst, At { seq: 0, mark: 1 });
        assert_eq!(got.len(), MAX_MARKS);
        assert_eq!(got.last().unwrap(), &format!("{}x5", 10 + MAX_MARKS));
    }

    #[test]
    fn a_resize_never_waits_for_output_being_fed() {
        let ring = std::sync::Arc::new(Ring::new(64));
        let (inside, go) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
        let feeder = {
            let ring = std::sync::Arc::clone(&ring);
            let (inside, go) = (inside.0, go.1);
            std::thread::spawn(move || {
                ring.append_with(
                    b"x",
                    &mut |_| {
                        inside.send(()).unwrap();
                        go.recv().unwrap();
                    },
                    true,
                );
            })
        };
        inside.1.recv().unwrap();
        let mut ran = false;
        assert!(!ring.resize_with(&mut || {
            ran = true;
            Some(size(1, 1))
        }));
        assert!(!ran, "a busy ring runs no resize");
        go.0.send(()).unwrap();
        feeder.join().unwrap();
        assert!(ring.resize_with(&mut || Some(size(1, 1))));
    }

    #[test]
    fn the_tracker_applies_skips_overlap_and_flags_gaps() {
        let mut t = Tracker::default();
        assert_eq!(t.accept(0, 5), Accept::Apply { skip: 0 });
        assert_eq!(t.next(), 5);
        // A resume resends from an earlier point: only the new tail applies.
        assert_eq!(t.accept(3, 5), Accept::Apply { skip: 2 });
        assert_eq!(t.next(), 8);
        assert_eq!(t.accept(0, 8), Accept::Duplicate);
        assert_eq!(
            t.accept(10, 1),
            Accept::Gap {
                expected: 8,
                got: 10
            }
        );
        assert_eq!(t.next(), 8);
        t.reset(100);
        assert_eq!(t.accept(100, 1), Accept::Apply { skip: 0 });
    }

    #[test]
    fn resume_is_exact_after_a_disconnect() {
        let ring = Ring::new(1024);
        let mut tracker = Tracker::default();
        let mut screen = Vec::new();
        ring.append(b"hello ");
        if let RingRead::Data { seq, bytes } = ring.read(tracker.next(), 100)
            && let Accept::Apply { skip } = tracker.accept(seq, bytes.len())
        {
            screen.extend_from_slice(&bytes[skip..]);
        }
        // The link drops; output continues.
        ring.append(b"world");
        let RingRead::Data { seq, bytes } = ring.read(tracker.next(), 100) else {
            panic!("expected data");
        };
        if let Accept::Apply { skip } = tracker.accept(seq, bytes.len()) {
            screen.extend_from_slice(&bytes[skip..]);
        }
        assert_eq!(screen, b"hello world");
    }
}
