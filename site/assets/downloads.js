// What the download cards and the platform texts show, read from
// window.CONNECTO_RELEASES (assets/releases.js). Used by site.js (the landing
// page) and docs.js (the install page). No DOM work here except
// applyPlatformTexts, so the rest runs under Node too.
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

  // The visitor's platform, as far as the browser says (navigator-like object).
  function visitorPlatform(nav) {
    if (!nav) return "";
    var p = (nav.userAgentData && nav.userAgentData.platform) || nav.platform || "";
    var ua = nav.userAgent || "";
    if (/^win|windows/i.test(p) || /windows/i.test(ua)) return "windows";
    if (/^mac/i.test(p) || /macintosh|mac os x/i.test(ua)) return "macos";
    if (/linux/i.test(p) || /linux/i.test(ua)) return "linux";
    return "";
  }

  function card(r) {
    var win = platformOf(r) === "windows";
    return '<div class="dl-card"><h3>' + esc(r.title) + "</h3><p>Version " + esc(r.version) +
      (r.size ? " · " + esc(r.size) : "") + (r.note ? " · " + esc(r.note) : "") + "</p>" +
      '<a class="btn btn-primary" href="' + esc(r.file) + '" download>Download</a>' +
      (win ? '<p>Not signed yet: SmartScreen warns the first time. <a href="docs/install.html#windows">How to open it →</a></p>' : "") +
      (r.sha256 ? '<p class="meta">' + esc(String(r.file).split("/").pop()) + "<br>sha256 " + esc(r.sha256) + "</p>" : "") + "</div>";
  }

  // The cards' HTML, the visitor's platform first (otherwise in list order).
  function cardsHtml(rel, visitor) {
    var list = (rel || []).slice();
    if (visitor) {
      list = list.filter(function (r) { return platformOf(r) === visitor; })
        .concat(list.filter(function (r) { return platformOf(r) !== visitor; }));
    }
    return list.map(card).join("");
  }

  // Show or hide the platform texts in `root` (a document or element).
  function applyPlatformTexts(root, rel) {
    var win = first(rel, "windows");
    var each = function (sel, f) { Array.prototype.forEach.call(root.querySelectorAll(sel), f); };
    each(".if-win", function (e) { e.hidden = !win; });
    each(".if-no-win", function (e) { e.hidden = !!win; });
    if (win) each('[data-release-file="windows"]', function (e) { e.textContent = String(win.file).split("/").pop(); });
    return !!win;
  }

  w.ConnectoDownloads = {
    esc: esc,
    platformOf: platformOf,
    first: first,
    visitorPlatform: visitorPlatform,
    cardsHtml: cardsHtml,
    applyPlatformTexts: applyPlatformTexts
  };
})(window);
