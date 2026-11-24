// What the download section, its file dialog and the platform texts show, read
// from window.CONNECTO_RELEASES and window.CONNECTO_CHAT_RELEASES
// (assets/releases.js). Used by site.js (the landing page) and docs.js (the
// install page). No DOM work here except applyPlatformTexts and the Copy
// buttons, so the rest runs under Node too.
//
// Platform texts: an element with class "if-win" (written with `hidden`) is
// shown, and one with class "if-no-win" hidden, once releases.js lists a
// Windows build. An element with data-release-file="windows" gets that
// build's file name. So publishing the Windows zip is one entry in releases.js.
(function (w) {
  "use strict";

  function esc(s) {
    return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }

  // "macos", "windows" or "linux": the entry's `platform`, else read off its file name.
  function platformOf(r) {
    if (r && r.platform) return String(r.platform);
    var f = String((r && r.file) || "");
    if (/-windows-/.test(f)) return "windows";
    if (/-linux-/.test(f)) return "linux";
    return "macos";
  }

  function first(rel, platform) {
    return (rel || []).filter(function (r) { return platformOf(r) === platform; })[0] || null;
  }

  // A phone or tablet (navigator-like object): iOS, iPadOS (which reports
  // itself as a Mac, but has touch points), Android, or any browser that says
  // it is mobile. None of the desktop builds is for it.
  function isMobile(nav) {
    if (!nav) return false;
    var ua = nav.userAgent || "";
    if (nav.userAgentData && nav.userAgentData.mobile) return true;
    if (/iphone|ipad|ipod|android|mobile/i.test(ua)) return true;
    var p = (nav.userAgentData && nav.userAgentData.platform) || nav.platform || "";
    if (/android|iphone|ipad|ipod/i.test(p)) return true;
    return /^mac/i.test(p) && nav.maxTouchPoints > 1;
  }

  // The visitor's desktop platform, as far as the browser says (navigator-like
  // object); "" on a phone or tablet, so no desktop file is recommended there.
  function visitorPlatform(nav) {
    if (!nav || isMobile(nav)) return "";
    var p = (nav.userAgentData && nav.userAgentData.platform) || nav.platform || "";
    var ua = nav.userAgent || "";
    if (/^win|windows/i.test(p) || /windows/i.test(ua)) return "windows";
    if (/^mac/i.test(p) || /macintosh|mac os x/i.test(ua)) return "macos";
    if (/linux/i.test(p) || /linux/i.test(ua)) return "linux";
    return "";
  }

  // The install note for each platform, shown under its heading in the
  // Connecto download dialog. Static HTML, written here, not from releases.js.
  var MAC_NOTE = 'Signed and notarized by Apple. Open the disk image, drag Connecto to Applications, and open it. <a href="docs/install.html#unsigned">Step by step →</a>';
  var WIN_NOTE = 'Not signed yet, so Windows SmartScreen warns the first time you start it: click <b>More info</b>, then <b>Run anyway</b>. <a href="docs/install.html#windows">Windows steps →</a>';
  var LINUX_NOTE = 'Experimental. Extract the archive and run <code>./connecto-app</code> inside the extracted folder; agent provisioning is incomplete.';
  var INSTALL_NOTES = { macos: MAC_NOTE, windows: WIN_NOTE, linux: LINUX_NOTE };
  var NAMES = { macos: "macOS", windows: "Windows", linux: "Linux" };
  var ORDER = ["macos", "windows", "linux"];

  // The platforms that have a file, the visitor's first.
  function platformsOf(rel, visitor) {
    var have = ORDER.filter(function (p) { return !!first(rel, p); });
    if (visitor && have.indexOf(visitor) > 0) have = [visitor].concat(have.filter(function (p) { return p !== visitor; }));
    return have;
  }

  // The line under a product's title: version(s) and the systems it runs on.
  function summaryText(rel) {
    var versions = [];
    (rel || []).forEach(function (r) { if (r.version && versions.indexOf(r.version) < 0) versions.push(r.version); });
    var systems = platformsOf(rel, "").map(function (p) { return NAMES[p]; });
    return (versions.length === 1 ? "Version " + versions[0] + " · " : "") + systems.join(", ");
  }

  // One file in the dialog: its architecture, size and requirement, the
  // Download link, and its checksum (inline with a Copy button, or a link to the
  // published .sha256 file).
  function rowHtml(r) {
    var parts = String(r.title || "").split(" · ");
    var name = parts.length > 1 ? parts.slice(1).join(" · ") : parts[0];
    var meta = [r.version ? "v" + r.version : "", r.size || "", r.note || ""].filter(Boolean).join(" · ");
    var file = String(r.file).split("/").pop();
    var sha = "";
    if (r.sha256) {
      sha = '<details class="pick-sha"><summary>SHA-256 checksum</summary><p><code>' + esc(file) + "</code></p>" +
        '<p class="dl-sha"><code>' + esc(r.sha256) + '</code> <button type="button" class="copy-btn" data-copy="' + esc(r.sha256) + '" aria-label="Copy the SHA-256 of ' + esc(file) + '">Copy</button><span class="copy-status" role="status" aria-live="polite"></span></p></details>';
    } else if (r.sha256File) {
      sha = '<a class="pick-sha-link" href="' + esc(r.sha256File) + '">SHA-256 checksum</a>';
    }
    return '<li class="pick-row"><div class="pick-main"><b>' + esc(name) + "</b>" +
      '<span class="pick-meta">' + esc(meta) + "</span>" + sha + "</div>" +
      '<a class="btn btn-primary btn-sm" href="' + esc(r.file) + '" download aria-label="Download ' + esc(r.title) + '">Download</a></li>';
  }

  // The dialog's list: one group per system (the visitor's first, marked), each
  // with its install note (for Connecto) and its files.
  function pickHtml(rel, visitor, opts) {
    opts = opts || {};
    return platformsOf(rel, visitor).map(function (p) {
      var rows = (rel || []).filter(function (r) { return platformOf(r) === p; });
      var note = opts.notes ? (INSTALL_NOTES[p] || "") : "";
      return '<div class="pick-group" role="group" aria-label="' + NAMES[p] + '"><h3>' + NAMES[p] +
        (p === visitor ? ' <span class="pick-you">This computer</span>' : "") + "</h3>" +
        (note ? '<p class="pick-note">' + note + "</p>" : "") +
        '<ul class="pick-list">' + rows.map(rowHtml).join("") + "</ul></div>";
    }).join("");
  }

  // Show or hide the platform texts in `root` (a document or element).
  function applyPlatformTexts(root, rel) {
    var win = first(rel, "windows");
    var each = function (sel, f) { Array.prototype.forEach.call(root.querySelectorAll(sel), f); };
    each(".if-win", function (e) { e.hidden = !win; });
    each(".if-no-win", function (e) { e.hidden = !!win; });
    var linux = first(rel, "linux");
    each(".if-linux", function (e) { e.hidden = !linux; });
    each(".if-no-linux", function (e) { e.hidden = !!linux; });
    if (win) each('[data-release-file="windows"]', function (e) { e.textContent = String(win.file).split("/").pop(); });
    return !!win;
  }

  function copyText(text, button) {
    var status = button.parentNode.querySelector(".copy-status");
    var say = function (msg) {
      if (status) status.textContent = msg;
      setTimeout(function () { if (status) status.textContent = ""; }, 1600);
    };
    var fallback = function () {
      var area = document.createElement("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.opacity = "0";
      button.parentNode.appendChild(area);
      area.select();
      var ok = false;
      try { ok = document.execCommand("copy"); } catch (err) { ok = false; }
      button.parentNode.removeChild(area);
      button.focus();
      say(ok ? "Copied" : "Could not copy");
    };
    if (w.navigator && navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(function () { say("Copied"); }, fallback);
    } else {
      fallback();
    }
  }
  if (w.document) {
    document.addEventListener("click", function (e) {
      var copy = e.target.closest && e.target.closest(".copy-btn");
      if (copy) copyText(copy.getAttribute("data-copy"), copy);
    });
  }

  w.ConnectoDownloads = {
    esc: esc,
    platformOf: platformOf,
    first: first,
    isMobile: isMobile,
    visitorPlatform: visitorPlatform,
    platformsOf: platformsOf,
    summaryText: summaryText,
    pickHtml: pickHtml,
    applyPlatformTexts: applyPlatformTexts
  };
})(window);
