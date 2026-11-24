import Foundation

#if canImport(FoundationModels)
  import FoundationModels
#endif

/// What Siri says, in Italian or English as the device speaks.
struct SiriWords {
  let italian: Bool

  static var current: SiriWords {
    let code = Locale.preferredLanguages.first.map { Locale(identifier: $0).language.languageCode?.identifier ?? "en" } ?? "en"
    return SiriWords(italian: code == "it")
  }

  func pick(_ it: String, _ en: String) -> String { italian ? it : en }

  // speakable()
  var codeInChat: String { pick("C'è del codice nella chat.", "There is code in the chat.") }
  var someCode: String { pick("del codice", "some code") }
  var aLink: String { pick("un link", "a link") }
  var restInChat: String { pick("Il resto è nella chat.", "The rest is in the chat.") }

  // Errors (the call's CallWords).
  var hostUnreachable: String { pick("Il Mac non risponde, non ho inviato nulla.", "The Mac isn't answering. Nothing was sent.") }
  var hostUnreachableRead: String { pick("Il Mac non risponde.", "The Mac isn't answering.") }
  var noProjects: String {
    pick(
      "Non conosco ancora nessun progetto. Apri Connecto e collegati al Mac.",
      "I don't know any project yet. Open Connecto and connect to your Mac.")
  }
  func noSession(_ p: String) -> String {
    pick(
      "Il progetto \(p) non ha una sessione per l'app.",
      "The project \(p) has no app session.")
  }
  func sessionStopped(_ p: String) -> String {
    pick("La sessione di \(p) è ferma.", "The \(p) session is stopped.")
  }
  var choiceOnScreen: String { pick("C'è una scelta da fare sullo schermo.", "There's a choice to make on screen.") }
  var paused: String { pick("Connecto è in pausa: riprendilo dal Mac.", "Connecto is paused: resume it on the Mac.") }
  var notSent: String { pick("Il messaggio non è stato inviato.", "The message wasn't sent.") }
  var notReady: String {
    pick("L'agente non è pronto a ricevere un messaggio.", "The agent isn't ready for a message.")
  }

  // Status.
  func working(_ p: String, minutes: Int?) -> String {
    if let minutes, minutes >= 1 {
      return pick(
        "L'agente di \(p) sta lavorando da \(minutes) minut\(minutes == 1 ? "o" : "i").",
        "The \(p) agent has been working for \(minutes) minute\(minutes == 1 ? "" : "s").")
    }
    return pick("L'agente di \(p) sta lavorando.", "The \(p) agent is working.")
  }
  func ready(_ p: String) -> String { pick("L'agente di \(p) è libero.", "The \(p) agent is ready.") }
  func waiting(_ p: String, _ q: String) -> String {
    pick("L'agente di \(p) aspetta una risposta: \(q)", "The \(p) agent is waiting for an answer: \(q)")
  }
  func status(_ p: String, _ s: String) -> String {
    pick("La sessione di \(p): \(s).", "The \(p) session: \(s).")
  }
  func lastSaid(_ p: String) -> String { pick("L'ultima risposta di \(p): ", "The last \(p) reply: ") }
  func noReply(_ p: String) -> String {
    pick("L'agente di \(p) non ha ancora risposto.", "The \(p) agent hasn't replied yet.")
  }

  // Ask.
  var askWhat: String { pick("Cosa vuoi chiedere a Connecto?", "What do you want to ask Connecto?") }
  var answerWhat: String { pick("Cosa vuoi rispondere?", "What's your answer?") }
  var accepted: String { pick("Ok, controllo. Ti avviso quando ha finito.", "Okay, checking. I'll let you know when it's done.") }
  var sentWithReminder: String {
    pick(
      "Ho inviato la domanda. Tra qualche minuto ti ricordo di guardare la risposta, oppure chiedimi \"cosa ha risposto Connecto\".",
      "Sent. I'll remind you to check the reply in a few minutes, or ask me \"what did Connecto say\".")
  }
  var sentNotificationsOff: String {
    pick(
      "Ho inviato la domanda. Le notifiche di Connecto sono spente: per la risposta chiedimi \"cosa ha risposto Connecto\" o apri l'app.",
      "Sent. Connecto's notifications are off: ask me \"what did Connecto say\" or open the app for the reply.")
  }
  var unconfirmed: String {
    pick(
      "Non ho potuto confermare l'invio. Controllo io appena il Mac risponde.",
      "I couldn't confirm it was sent. I'll check as soon as the Mac answers.")
  }
  var sentNeutral: String {
    pick(
      "Ho inviato la domanda. Per la risposta chiedimi \"cosa ha risposto Connecto\" o apri l'app.",
      "Sent. For the reply, ask me \"what did Connecto say\" or open the app.")
  }
  var sentSessionEnded: String {
    pick(
      "La domanda è partita, ma la sessione si è fermata prima di rispondere. Guarda la chat in Connecto.",
      "The question went, but the session stopped before replying. Check the chat in Connecto.")
  }
  var sentNoReply: String {
    pick(
      "La domanda è partita, ma la risposta non è arrivata in tempo. Guarda la chat in Connecto.",
      "The question went, but no reply came in time. Check the chat in Connecto.")
  }
  var sentUnmatched: String {
    pick(
      "La domanda è partita, ma non trovo la risposta. Guarda la chat in Connecto.",
      "The question went, but I can't find its reply. Check the chat in Connecto.")
  }
  var busy: String {
    pick(
      "C'è già una domanda di Siri in attesa per questo progetto. Riprova tra poco.",
      "There's already a Siri question waiting in this project. Try again shortly.")
  }
  func reminderBody(_ p: String) -> String {
    pick("Guarda la risposta di Connecto su \(p).", "Check Connecto's reply in \(p).")
  }
  var noQuestion: String { pick("L'agente non sta facendo domande.", "The agent isn't asking anything.") }
  var noAgentQuestionByVoice: String { choiceOnScreen }

  // Stop.
  var notRunning: String { pick("L'agente non sta lavorando.", "The agent isn't working.") }
  var stopped: String { pick("Fermato.", "Stopped.") }
  var stopAsked: String { pick("Ho chiesto all'agente di fermarsi.", "I asked the agent to stop.") }
  func confirmStop(_ p: String) -> String { pick("Fermo l'agente di \(p)?", "Stop the \(p) agent?") }

  // Call / session.
  func callNeeds26(_ p: String) -> String {
    pick("Le chiamate richiedono iOS 26. Apro la chat di \(p).", "Calls need iOS 26. Opening the \(p) chat.")
  }
  func started(_ agent: String) -> String { pick("Sessione avviata con \(agent).", "Session started with \(agent).") }
  func resumed(_ agent: String) -> String {
    pick("Ho ripreso la sessione esistente con \(agent).", "Resumed the existing session with \(agent).")
  }
  func alreadyRunning(_ agent: String) -> String {
    pick("La sessione è già attiva con \(agent).", "The session is already running with \(agent).")
  }
  func otherAgent(_ running: String, _ wanted: String) -> String {
    pick(
      "La sessione usa \(running). Vuoi chiuderla e ricominciare con \(wanted)? La cronologia andrà persa.",
      "The session uses \(running). Close it and start over with \(wanted)? Its history goes.")
  }
}

/// A reply as it should sound: no markdown, code or addresses read out
/// (`speakable` of `lib/call.dart`).
func speakable(_ reply: String, _ w: SiriWords = .current) -> String {
  var codeSaid = false
  var text = replacing(reply, #"```[\s\S]*?(```|$)"#) { _ in
    if codeSaid { return "\n" }
    codeSaid = true
    return "\n\(w.codeInChat)\n"
  }
  text = replacing(text, "`([^`\\n]*)`") { g in g[1].count <= 40 ? g[1] : w.someCode }
  text = replacing(text, #"!?\[([^\]]*)\]\([^)]*\)"#) { g in g[1] }
  text = replacing(text, #"https?://\S+"#) { _ in w.aLink }
  text = replacing(text, #"^\s*\|.*$"#, lines: true) { _ in "" }
  text = replacing(text, #"^\s*([-*_=]\s*){3,}$"#, lines: true) { _ in "" }
  text = replacing(text, #"^\s{0,3}#{1,6}\s+"#, lines: true) { _ in "" }
  text = replacing(text, #"^\s*>\s?"#, lines: true) { _ in "" }
  text = replacing(text, #"^\s*[-*+•]\s+"#, lines: true) { _ in "" }
  text = replacing(text, #"(\*\*|__|~~)"#) { _ in "" }
  text = replacing(text, #"(^|\W)[*_]([^*_\n]+)[*_](?=\W|$)"#) { g in g[1] + g[2] }
  let sentences = text.split(separator: "\n", omittingEmptySubsequences: false)
    .map { $0.trimmingCharacters(in: .whitespaces) }
    .filter { !$0.isEmpty }
    .map { s -> String in
      guard let last = s.last else { return s }
      return ".!?:;,".contains(last) ? s : s + "."
    }
  return replacing(sentences.joined(separator: " "), #"\s+"#) { _ in " " }
    .trimmingCharacters(in: .whitespacesAndNewlines)
}

/// The first `count` sentences of `text`.
func firstSentences(_ text: String, _ count: Int) -> String {
  var parts: [String] = []
  var start = text.startIndex
  let regex = try! NSRegularExpression(pattern: #"(?<=[.!?])\s+"#)
  let ns = text as NSString
  for match in regex.matches(in: text, range: NSRange(location: 0, length: ns.length)) {
    guard let range = Range(match.range, in: text) else { continue }
    parts.append(String(text[start..<range.lowerBound]))
    start = range.upperBound
    if parts.count == count { return parts.joined(separator: " ") }
  }
  parts.append(String(text[start...]))
  return parts.prefix(count).joined(separator: " ")
}

/// What Siri says for `reply` within what is left of its budget (§5.7):
/// short replies as they are; long ones condensed on the device only when
/// at least 2 s are left, else their first two sentences. Never the Mac's or
/// the phone's own model.
func spokenWithin(
  _ reply: String, deadline: ContinuousClock.Instant, words: SiriWords = .current,
  condense: ((String, Duration) async -> String?)? = nil
) async -> String {
  let text = speakable(reply, words)
  if text.count <= 320 { return text }
  let left = deadline - ContinuousClock.now
  if left >= .seconds(2) {
    let summarize = condense ?? SiriCondenser.condense
    let within = left - .milliseconds(500)
    // Whatever the model does, Siri gets an answer in time.
    let short: String? = await withDeadline(within, fallback: nil) { await summarize(text, within) }
    if let short, !short.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
      return speakable(short, words)
    }
  }
  return "\(firstSentences(text, 2)) \(words.restInChat)"
}

/// Foundation Models on the device: nil when unavailable.
enum SiriCondenser {
  static func condense(_ text: String, within _: Duration) async -> String? {
    #if canImport(FoundationModels)
      guard #available(iOS 26.0, *) else { return nil }
      guard SystemLanguageModel.default.isAvailable else { return nil }
      let italian = SiriWords.current.italian
      let session = LanguageModelSession(
        instructions: """
          You turn an AI coding agent's written reply into what a phone assistant \
          would say out loud. Use at most three short sentences in plain speech: \
          no markdown, no lists, no code, no file paths or URLs read out. Keep the \
          outcome, any question the agent asks, and anything the person must do. \
          Always answer in \(italian ? "Italian" : "English").
          """)
      let reply = text.count > 6000 ? String(text.prefix(6000)) + "…" : text
      let response = try? await session.respond(
        to: "Reply to read aloud:\n\n\(reply)",
        options: GenerationOptions(maximumResponseTokens: 120))
      return response?.content.trimmingCharacters(in: .whitespacesAndNewlines)
    #else
      return nil
    #endif
  }
}

/// `text` with every match of `pattern` replaced by what `with` makes of its
/// groups (missing groups are "").
func replacing(
  _ text: String, _ pattern: String, lines: Bool = false, with: ([String]) -> String
) -> String {
  guard
    let regex = try? NSRegularExpression(
      pattern: pattern, options: lines ? [.anchorsMatchLines] : [])
  else { return text }
  let ns = text as NSString
  var out = ""
  var last = 0
  for m in regex.matches(in: text, range: NSRange(location: 0, length: ns.length)) {
    out += ns.substring(with: NSRange(location: last, length: m.range.location - last))
    let groups = (0..<m.numberOfRanges).map { i -> String in
      let r = m.range(at: i)
      return r.location == NSNotFound ? "" : ns.substring(with: r)
    }
    out += with(groups)
    last = m.range.location + m.range.length
  }
  out += ns.substring(from: last)
  return out
}

// MARK: - Chat messages

/// A chat message: `(fromUser, text)`.
typealias ChatMessage = (user: Bool, text: String)

func chatMessages(_ raw: Any?) -> [ChatMessage] {
  (raw as? [[Any]] ?? []).compactMap { m in
    guard m.count == 2, let user = m[0] as? Bool else { return nil }
    return (user, "\(m[1])")
  }
}
