//! Output sequencing: a bounded ring on the host and a gap check on the client.
//!
//! A sequence number is the offset of a byte in everything the terminal has
//! printed since it started. A client that holds all bytes below `n` asks to
//! resume at `n`. If the ring still holds `n` the host sends the missing bytes;
//! if not, the client fell off the ring and gets a repaint of the screen.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Bytes the host keeps per terminal for resuming.
pub const RING_BYTES: usize = 1 << 20;

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
}

#[derive(Debug)]
struct Inner {
    cap: usize,
    buf: VecDeque<u8>,
    /// Sequence number of `buf[0]`.
    start: u64,
    closed: bool,
}

impl Inner {
    fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
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
        }
        drop(inner);
        self.changed.notify_all();
    }

    /// Call `f` with the sequence number after the last byte, while no output
    /// can be fed. If `f` reads the screen, the screen holds exactly the bytes
    /// below that number.
    pub fn consistent<T>(&self, f: impl FnOnce(u64) -> T) -> T {
        let inner = self.lock();
        f(inner.end())
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
