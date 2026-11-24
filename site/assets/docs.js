// Connecto docs: builds the header, sidebar, on-page contents, search and
// previous/next links around each page's <main class="doc">.
//
// To add a page: create docs/<file>.html from an existing one, set
// <body data-page="<file>">, and add it to NAV below.
(function () {
  "use strict";

  var NAV = [
    ["Start here", [
      ["index", "Introduction", "what is connecto overview"],
      ["install", "Install and first launch", "download provision login requirements setup"],
      ["updates", "Updates", "update upgrade new version restart rollback check automatically"],
      ["terminals", "Projects and terminals", "sidebar tree states working idle nested children file drop 16"]
    ]],
    ["Working with agents", [
      ["agents", "Agents using other terminals", "connecto command send ask list read keys watch add bridge"],
      ["automations", "Automations", "trigger jev classifier keys threshold approve prompt"],
      ["agentic-works", "Agentic Works", "jobs queue postgresql postgres connecto_jobs connecto_works work type schema json prompt producer n8n cron"],
      ["packages", "Skills & Plugins packages", "skills plugins subagents commands hooks connecto home default package extends"],
      ["game-dev", "Game Dev package", "game games godot blender ffmpeg gdscript playtest design 3d art glb requirements install"],
      ["memory", "Project memory", "memory palace rooms decisions conventions pitfalls notes dream development cycle initial build ask brief search recall connecto memory privacy redaction"],
      ["isolation", "Isolated and system terminals", "profile config login launch mode bypass auto permissions"],
      ["cli-improvements", "CLI improvements", "claude insights /insights report apply suggestions settings hooks claude.md skills opus"],
      ["remote", "Remote sessions", "host pairing code relay ssh key device revoke"],
      ["relay", "Run your own relay", "relay self-host iroh-relay server token tls caddy nginx upgrade"],
      ["history", "History and utility sessions", "transcript continue resume title robot"]
    ]],
    ["Staying in control", [
      ["safety", "Pause, audit and redaction", "stop brake audit log verify secrets redacted security"],
      ["secrets", "Secrets", "vault secret api key token password ssh key keychain request url run proxy ssh-agent connecto secret"]
    ]],
    ["Reference", [
      ["agents-supported", "Supported agents", "claude code codex gemini cli cursor cli opencode roadmap"],
      ["shortcuts", "Keyboard shortcuts", "keys menu cmd"],
      ["reference", "Commands and files", "cli exit codes folders settings.json workspace.json limits"]
    ]]
  ];

  var page = document.body.getAttribute("data-page") || "index";
  var href = function (id) { return id === "index" ? "./" : id + ".html"; };
  var flat = [];
  NAV.forEach(function (g) { g[1].forEach(function (p) { flat.push({ id: p[0], title: p[1], keys: p[2], group: g[0] }); }); });
  var el = function (tag, cls, html) { var e = document.createElement(tag); if (cls) e.className = cls; if (html != null) e.innerHTML = html; return e; };
  var doc = document.querySelector("main.doc");
  if (!doc) return;

  /* platform texts (pages that load releases.js and downloads.js): the
     Windows parts show once a Windows build is listed */
  if (window.ConnectoDownloads) window.ConnectoDownloads.applyPlatformTexts(document, window.CONNECTO_RELEASES || []);

  /* header */
  var top = el("header", "dtop",
    '<button class="menu-btn" aria-label="Menu">☰</button>' +
    '<a class="brand" href="../"><img class="mark" src="../assets/img/icon-256.png" alt="">Connecto <span>docs</span></a>' +
    '<div class="search"><input type="search" placeholder="Search the docs" aria-label="Search the docs"><div class="results"></div></div>' +
    '<div class="links"><a href="../">Home</a><a href="./">Docs</a><a class="keep gh-link" href="https://github.com/marcomanzi/connecto-community" target="_blank" rel="noopener" aria-label="Connecto on GitHub"><svg class="gh" viewBox="0 0 16 16" width="16" height="16" aria-hidden="true"><path fill="currentColor" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z"/></svg><span>GitHub</span></a><a class="btn btn-primary btn-sm" href="../#donate">Support</a></div>');
  document.body.insertBefore(top, document.body.firstChild);
  top.querySelector(".menu-btn").addEventListener("click", function () { document.body.classList.toggle("nav-open"); });

  /* layout */
  var book = el("div", "book");
  var side = el("nav", "dside");
  side.setAttribute("aria-label", "Docs");
  NAV.forEach(function (g) {
    side.appendChild(el("h6", "", g[0]));
    g[1].forEach(function (p) {
      var a = el("a", p[0] === page ? "on" : "", p[1]);
      a.href = href(p[0]);
      side.appendChild(a);
    });
  });
  var toc = el("aside", "dtoc");
  doc.parentNode.insertBefore(book, doc);
  book.appendChild(side); book.appendChild(doc); book.appendChild(toc);

  /* breadcrumb */
  var me = flat.filter(function (p) { return p.id === page; })[0];
  var h1 = doc.querySelector("h1");
  if (me && h1) h1.parentNode.insertBefore(el("div", "crumb", me.group), h1);

  /* headings: ids, anchors, on-page contents */
  var heads = Array.prototype.filter.call(doc.querySelectorAll("h2, h3"), function (h) { return !h.closest("[hidden]"); });
  if (heads.length) toc.appendChild(el("h6", "", "On this page"));
  var links = [];
  heads.forEach(function (h) {
    if (!h.id) h.id = h.textContent.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
    var a = el("a", h.tagName === "H3" ? "h3" : "", h.textContent);
    a.href = "#" + h.id; toc.appendChild(a); links.push(a);
    var anc = el("a", "anchor", "#"); anc.href = "#" + h.id; anc.setAttribute("aria-label", "Link to this section");
    h.appendChild(anc);
  });
  if ("IntersectionObserver" in window && heads.length) {
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) {
        if (!e.isIntersecting) return;
        links.forEach(function (l) { l.classList.toggle("on", l.getAttribute("href") === "#" + e.target.id); });
      });
    }, { rootMargin: "-70px 0px -70% 0px" });
    heads.forEach(function (h) { io.observe(h); });
  }

  /* wrap tables so they scroll on small screens */
  doc.querySelectorAll("table").forEach(function (t) {
    var w = el("div", "tbl"); t.parentNode.insertBefore(w, t); w.appendChild(t);
  });

  /* copy buttons */
  doc.querySelectorAll("pre").forEach(function (pre) {
    var b = el("button", "copy", "Copy");
    b.addEventListener("click", function () {
      var text = pre.textContent.replace(/Copy$/, "");
      var done = function () { b.textContent = "Copied"; setTimeout(function () { b.textContent = "Copy"; }, 1400); };
      if (navigator.clipboard) navigator.clipboard.writeText(text).then(done, function () {});
    });
    pre.appendChild(b);
  });

  /* previous / next */
  var i = flat.indexOf(me);
  if (i >= 0) {
    var pager = el("div", "pager");
    if (i > 0) { var p = el("a", "", "<small>Previous</small>" + flat[i - 1].title); p.href = href(flat[i - 1].id); pager.appendChild(p); }
    if (i < flat.length - 1) { var n = el("a", "next", "<small>Next</small>" + flat[i + 1].title); n.href = href(flat[i + 1].id); pager.appendChild(n); }
    doc.appendChild(pager);
  }

  /* search over page titles and keywords */
  var input = top.querySelector(".search input"), box = top.querySelector(".results"), sel = 0;
  function search() {
    var q = input.value.trim().toLowerCase();
    if (!q) { box.classList.remove("open"); return; }
    var words = q.split(/\s+/);
    var hits = flat.filter(function (p) {
      var hay = (p.title + " " + p.keys + " " + p.group).toLowerCase();
      return words.every(function (w) { return hay.indexOf(w) >= 0; });
    });
    sel = 0; box.innerHTML = "";
    if (!hits.length) box.appendChild(el("p", "", "Nothing found."));
    hits.forEach(function (p, k) {
      var a = el("a", k === 0 ? "sel" : "", p.title + "<small>" + p.group + "</small>");
      a.href = href(p.id); box.appendChild(a);
    });
    box.classList.add("open");
  }
  input.addEventListener("input", search);
  input.addEventListener("focus", search);
  input.addEventListener("keydown", function (e) {
    var as = box.querySelectorAll("a");
    if (e.key === "Escape") { box.classList.remove("open"); input.blur(); }
    if (!as.length) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      sel = (sel + (e.key === "ArrowDown" ? 1 : as.length - 1)) % as.length;
      as.forEach(function (a, k) { a.classList.toggle("sel", k === sel); });
    }
    if (e.key === "Enter") { e.preventDefault(); location.href = as[sel].href; }
  });
  document.addEventListener("click", function (e) { if (!top.querySelector(".search").contains(e.target)) box.classList.remove("open"); });
  document.addEventListener("keydown", function (e) {
    if (e.key === "/" && document.activeElement !== input) { e.preventDefault(); input.focus(); }
  });
})();
