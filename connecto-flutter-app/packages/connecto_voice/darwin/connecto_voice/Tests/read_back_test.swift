// Tests for ReadBack.swift, without Flutter or the audio engine. Run:
//   swiftc -parse-as-library Sources/connecto_voice/ReadBack.swift \
//     Tests/read_back_test.swift -o "$TMPDIR/read_back_test" && "$TMPDIR/read_back_test"
// (from darwin/connecto_voice). It exits non-zero on a failure.

import Foundation

var failures = 0

func check(_ ok: Bool, _ what: String, line: Int = #line) {
  if !ok {
    failures += 1
    print("FAIL (line \(line)): \(what)")
  }
}

@main
struct ReadBackTests {
  static func main() async {
    // A buffer that failed to convert in the middle: nothing is played.
    check(ReadBackTracker.allConverted([1, nil, 3]) == nil, "a failed middle buffer drops the read-back")
    check(ReadBackTracker.allConverted([Int]()) == nil, "no buffers is not a read-back")
    check(ReadBackTracker.allConverted([1, 2, 3]) == [1, 2, 3], "all converted, in order")

    // A flush before the synthesis finished: the read-back no longer counts.
    let tracker = ReadBackTracker()
    let first = tracker.begin()
    check(tracker.isValid(first), "a new read-back counts")
    tracker.invalidate()
    check(!tracker.isValid(first), "a flush during synthesis invalidates it")

    // A newer read-back replaces the one before.
    let a = tracker.begin()
    let b = tracker.begin()
    check(!tracker.isValid(a) && tracker.isValid(b), "a newer read-back replaces the earlier one")

    // A synthesis cancelled before its last buffer: the wait ends, it is
    // marked cancelled, and later buffers are dropped.
    let spoken = SpokenBuffers<Int>()
    await spoken.wait {
      spoken.add(1)
      spoken.cancel()
      spoken.add(2)
      spoken.finish()
    }
    check(spoken.cancelled, "cancelled before the last buffer")
    check(spoken.all == [1], "nothing kept after the cancel")
    check(!spoken.cancel(), "it ends once")

    // A synthesis that finishes: every buffer, not cancelled.
    let whole = SpokenBuffers<Int>()
    await whole.wait {
      whole.add(1)
      whole.add(2)
      whole.finish()
    }
    check(!whole.cancelled && whole.all == [1, 2], "a finished synthesis keeps every buffer")

    // Cancelled from elsewhere while it waits (a flush, a timeout).
    let stalled = SpokenBuffers<Int>()
    Task {
      try? await Task.sleep(for: .milliseconds(20))
      stalled.cancel()
    }
    await stalled.wait {}
    check(stalled.cancelled, "a stalled synthesis ends when cancelled")

    print(failures == 0 ? "All read-back tests passed." : "\(failures) failed.")
    exit(failures == 0 ? 0 : 1)
  }
}
