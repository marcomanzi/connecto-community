// What decides whether the phone's own read-back (ADR 0034, "answers by
// voice") counts as said: kept apart from the audio engine so it can be
// tested on its own (Tests/read_back_test.swift).

import Foundation

/// The read-backs of a call, one at a time: the current one counts only
/// while nothing invalidated it, from the start of its synthesis to its last
/// buffer played. A newer one, a flush (talked over), a timeout or the call
/// stopping invalidates it.
final class ReadBackTracker {
  private(set) var current = 0
  private var valid = false

  /// A new read-back: the one before it no longer counts.
  func begin() -> Int {
    current += 1
    valid = true
    return current
  }

  /// Whatever is being read back no longer counts.
  func invalidate() {
    valid = false
  }

  func isValid(_ id: Int) -> Bool {
    valid && id == current
  }

  /// Every buffer converted, in order, or none: a read-back missing a
  /// piece is not said.
  static func allConverted<T>(_ items: [T?]) -> [T]? {
    guard !items.isEmpty, !items.contains(where: { $0 == nil }) else { return nil }
    return items.compactMap { $0 }
  }
}

/// What the synthesizer writes for one utterance, from its own queue, and
/// the wait for it to end: resolved exactly once, by its last (empty)
/// buffer, or earlier by [cancel] (a flush, a timeout, the call stopping).
final class SpokenBuffers<Buffer>: @unchecked Sendable {
  private let lock = NSLock()
  private var buffers: [Buffer] = []
  private var waiting: CheckedContinuation<Void, Never>?
  private var over = false
  private(set) var cancelled = false

  /// Waits until it ends; [begin] starts the synthesis.
  func wait(_ begin: () -> Void) async {
    await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
      let resumeNow = lock.withLock { () -> Bool in
        if over { return true }
        waiting = done
        return false
      }
      if resumeNow {
        done.resume()
      } else {
        begin()
      }
    }
  }

  func add(_ buffer: Buffer) {
    lock.withLock {
      if !over { buffers.append(buffer) }
    }
  }

  /// The last buffer came.
  func finish() {
    resolve(cancelling: false)
  }

  /// It no longer counts: the wait ends now, and what comes later is
  /// dropped. False when it had already ended.
  @discardableResult
  func cancel() -> Bool {
    resolve(cancelling: true)
  }

  var all: [Buffer] { lock.withLock { buffers } }

  @discardableResult
  private func resolve(cancelling: Bool) -> Bool {
    let (done, first) = lock.withLock { () -> (CheckedContinuation<Void, Never>?, Bool) in
      if over { return (nil, false) }
      over = true
      cancelled = cancelling
      defer { waiting = nil }
      return (waiting, true)
    }
    done?.resume()
    return first
  }
}
