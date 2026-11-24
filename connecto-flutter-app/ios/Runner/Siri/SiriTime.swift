import Foundation

/// `operation`'s result, or `fallback` once `within` has passed, whichever
/// comes first. An operation that ignores cancellation (a blocking FFI call,
/// a stuck model) is left behind: the caller returns in time regardless.
func withDeadline<T: Sendable>(
  _ within: Duration, fallback: T, _ operation: @escaping @Sendable () async -> T
) async -> T {
  guard within > .zero else { return fallback }
  return await withCheckedContinuation { done in
    let once = Once()
    let work = Task {
      let r = await operation()
      if once.claim() { done.resume(returning: r) }
    }
    Task {
      try? await Task.sleep(for: within)
      if once.claim() {
        work.cancel()
        done.resume(returning: fallback)
      }
    }
  }
}

/// True for the first caller only.
final class Once: @unchecked Sendable {
  private let lock = NSLock()
  private var taken = false
  func claim() -> Bool {
    lock.withLock {
      defer { taken = true }
      return !taken
    }
  }
}
