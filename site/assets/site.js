// Connecto landing page: the network canvas, the terminal demo, small effects.
(function () {
  "use strict";
  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var $ = function (s) { return document.querySelector(s); };

  /* ---- top bar border once scrolled ---- */
  var top = $("#top");
  function onScroll() {
    if (top) top.classList.toggle("scrolled", window.scrollY > 8);
    var mock = $("#mock");
    if (mock && !reduce) {
      var t = Math.max(0, Math.min(1, window.scrollY / 420));
      mock.style.setProperty("--tilt", (6 - 6 * t).toFixed(2) + "deg");
    }
  }
  window.addEventListener("scroll", onScroll, { passive: true });
  onScroll();

  /* ---- reveal on scroll ---- */
  var rv = document.querySelectorAll(".rv");
  if ("IntersectionObserver" in window && !reduce) {
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) { if (e.isIntersecting) { e.target.classList.add("in"); io.unobserve(e.target); } });
    }, { threshold: 0.12 });
    rv.forEach(function (el) { io.observe(el); });
  } else {
    rv.forEach(function (el) { el.classList.add("in"); });
  }

  /* ---- card spotlight follows the pointer ---- */
  document.querySelectorAll(".card").forEach(function (c) {
    c.addEventListener("pointermove", function (e) {
      var r = c.getBoundingClientRect();
      c.style.setProperty("--mx", e.clientX - r.left + "px");
      c.style.setProperty("--my", e.clientY - r.top + "px");
    });
  });

  /* ---- hero canvas: blocks joined by lines, with messages travelling ---- */
  var cv = $("#net");
  if (cv && cv.getContext) {
    var ctx = cv.getContext("2d"), W = 0, H = 0, dpr = 1, nodes = [], pulses = [], mouse = null, raf = 0;
    var LINK = 170;

    function size() {
      var r = cv.getBoundingClientRect();
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      W = r.width; H = r.height;
      cv.width = W * dpr; cv.height = H * dpr;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      var n = Math.round(Math.min(70, (W * H) / 21000));
      nodes = [];
      for (var i = 0; i < n; i++) {
        nodes.push({
          x: Math.random() * W, y: Math.random() * H,
          vx: (Math.random() - 0.5) * 0.22, vy: (Math.random() - 0.5) * 0.22,
          s: 2 + Math.random() * 2.5, red: Math.random() < 0.12
        });
      }
      pulses = [];
    }

    function frame() {
      ctx.clearRect(0, 0, W, H);
      var i, j, a, b, d, dx, dy;
      for (i = 0; i < nodes.length; i++) {
        a = nodes[i];
        a.x += a.vx; a.y += a.vy;
        if (a.x < -20) a.x = W + 20; else if (a.x > W + 20) a.x = -20;
        if (a.y < -20) a.y = H + 20; else if (a.y > H + 20) a.y = -20;
        if (mouse) {
          dx = a.x - mouse.x; dy = a.y - mouse.y; d = Math.sqrt(dx * dx + dy * dy);
          if (d < 130 && d > 0.1) { a.x += (dx / d) * 0.6; a.y += (dy / d) * 0.6; }
        }
      }
      ctx.lineWidth = 1;
      for (i = 0; i < nodes.length; i++) {
        a = nodes[i];
        for (j = i + 1; j < nodes.length; j++) {
          b = nodes[j]; dx = a.x - b.x; dy = a.y - b.y; d = dx * dx + dy * dy;
          if (d < LINK * LINK) {
            d = Math.sqrt(d);
            ctx.strokeStyle = "rgba(115,248,252," + (0.16 * (1 - d / LINK)).toFixed(3) + ")";
            ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke();
            if (pulses.length < 14 && Math.random() < 0.0009) pulses.push({ a: a, b: b, t: 0, red: a.red || b.red });
          }
        }
      }
      for (i = pulses.length - 1; i >= 0; i--) {
        var p = pulses[i]; p.t += 0.012;
        dx = p.a.x - p.b.x; dy = p.a.y - p.b.y;
        if (p.t >= 1 || dx * dx + dy * dy > LINK * LINK * 1.5) { pulses.splice(i, 1); continue; }
        var x = p.a.x + (p.b.x - p.a.x) * p.t, y = p.a.y + (p.b.y - p.a.y) * p.t;
        ctx.fillStyle = p.red ? "rgba(237,130,127,0.95)" : "rgba(115,248,252,0.95)";
        ctx.shadowColor = ctx.fillStyle; ctx.shadowBlur = 12;
        ctx.beginPath(); ctx.arc(x, y, 2.1, 0, 6.2832); ctx.fill();
        ctx.shadowBlur = 0;
      }
      for (i = 0; i < nodes.length; i++) {
        a = nodes[i];
        ctx.fillStyle = a.red ? "rgba(229,75,75,0.75)" : "rgba(115,248,252,0.55)";
        ctx.fillRect(a.x - a.s / 2, a.y - a.s / 2, a.s, a.s);
      }
      raf = requestAnimationFrame(frame);
    }

    size();
    window.addEventListener("resize", size);
    if (reduce) {
      frame(); cancelAnimationFrame(raf);
    } else {
      cv.parentElement.addEventListener("pointermove", function (e) {
        var r = cv.getBoundingClientRect(); mouse = { x: e.clientX - r.left, y: e.clientY - r.top };
      });
      cv.parentElement.addEventListener("pointerleave", function () { mouse = null; });
      new IntersectionObserver(function (es) {
        cancelAnimationFrame(raf);
        if (es[0].isIntersecting) raf = requestAnimationFrame(frame);
      }).observe(cv);
    }
  }

  /* ---- terminal demo ---- */
  var term = $("#term");
  if (term) {
    // [text, class, kind]: kind "type" is typed a character at a time.
    var script = [
      ["> ", "m"], ["Add the orders migration, then get it reviewed.\n\n", "", "type"],
      ["● ", "c"], ["I'll write the migration and ask Codex to check it.\n\n", ""],
      ["● Bash", "c"], ["(connecto add codex --name \"review the diff\")\n", ""],
      ["  ⎿ ", "m"], ["codex-4 started\n\n", "g"],
      ["● Bash", "c"], ["(connecto send codex-4 --ask)\n", ""],
      ["  ⎿ ", "m"], ["request r12 · the reply will be delivered here\n\n", "m"],
      ["[connecto from codex-4, request r12]\n", "a"],
      ["The index on orders(customer_id) is created without\nCONCURRENTLY and will lock writes. Everything else is safe.\n\n", ""],
      ["● ", "c"], ["Good catch. Switching to CREATE INDEX CONCURRENTLY.\n", ""]
    ];
    var full = function () {
      term.innerHTML = "";
      script.forEach(function (s) { var sp = document.createElement("span"); if (s[1]) sp.className = s[1]; sp.textContent = s[0]; term.appendChild(sp); });
    };
    if (reduce) { full(); } else {
      var caret = document.createElement("span"); caret.className = "caret";
      var run = function () {
        term.innerHTML = ""; term.appendChild(caret);
        var i = 0;
        var next = function () {
          if (i >= script.length) { setTimeout(run, 6500); return; }
          var s = script[i++], sp = document.createElement("span");
          if (s[1]) sp.className = s[1];
          term.insertBefore(sp, caret);
          if (s[2] === "type") {
            var k = 0;
            var tick = function () {
              sp.textContent = s[0].slice(0, ++k);
              if (k < s[0].length) setTimeout(tick, 24 + Math.random() * 30); else setTimeout(next, 500);
            };
            tick();
          } else {
            sp.textContent = s[0];
            setTimeout(next, /\n\n$/.test(s[0]) ? 800 : 180);
          }
        };
        setTimeout(next, 700);
      };
      var started = false;
      new IntersectionObserver(function (es, o) {
        if (es[0].isIntersecting && !started) { started = true; o.disconnect(); run(); }
      }, { threshold: 0.3 }).observe(term);
    }
  }

  /* ---- automation flow: light the steps in turn ---- */
  var flow = $("#flow");
  if (flow && !reduce) {
    var steps = flow.querySelectorAll(".step"), cur = -1;
    setInterval(function () {
      cur = (cur + 1) % (steps.length + 1);
      steps.forEach(function (s, i) { s.classList.toggle("lit", i <= cur && cur < steps.length); });
    }, 1300);
  }

  /* ---- pairing code changes each cycle ---- */
  var pc = $("#paircode");
  if (pc && !reduce) {
    setInterval(function () {
      var n = String(Math.floor(100000 + Math.random() * 900000));
      pc.textContent = n.slice(0, 3) + " " + n.slice(3);
    }, 12000);
  }

  /* ---- downloads ---- */
  /* ---- the download counter ----
     downloads/stats.json is written on the server, hourly, from the web server's
     log of the download files (scripts/site/download-stats); downloads by the
     in-app updater are left out. No API is called from here, and nothing about
     the visitor is sent anywhere. The odometer always shows: dashes until a
     usable number has loaded (never a made-up one), 000000 and an invitation at
     zero, and the digits roll up whenever the number grows. Read again every
     few minutes while the page is open. */
  var odo = $("#odo");
  if (odo) {
    var digits = $("#odo-digits"), text = $("#odo-text"), week = $("#odo-week"), ghost = $("#odo-ghost");
    var pill = $("#dl-float");
    // The pill shows the same count once the odometer is out of view.
    if (pill && "IntersectionObserver" in window) {
      new IntersectionObserver(function (es) {
        es.forEach(function (e) { pill.classList.toggle("away", e.isIntersecting); });
      }, { threshold: 0.15 }).observe(odo);
    }
    var MIN_CELLS = 6, shown = null, everLoaded = false;
    var whole = function (n) { return typeof n === "number" && isFinite(n) && n >= 0 && Math.floor(n) === n && n < 1e12; };
    var fmt = function (n) { return n.toLocaleString("en-US"); };
    var cellsFor = function (n) { return Math.max(MIN_CELLS, String(n).length); };
    // `count` cells, each a strip of 0-9 (or a dash while nothing is known),
    // with a small gap every three digits from the right.
    var build = function (count, dashes) {
      digits.innerHTML = "";
      for (var i = 0; i < count; i++) {
        if (i > 0 && (count - i) % 3 === 0) digits.appendChild(el("span", "odo-sep"));
        var cell = el("span", "odo-cell");
        if (dashes) {
          cell.appendChild(el("span", "odo-dash", "–"));
        } else {
          var strip = el("span", "odo-strip");
          for (var d = 0; d <= 9; d++) strip.appendChild(el("span", "", String(d)));
          strip.style.setProperty("--delay", ((count - 1 - i) * 0.07).toFixed(2) + "s");
          cell.appendChild(strip);
        }
        digits.appendChild(cell);
      }
    };
    var el = function (tag, cls, txt) {
      var e = document.createElement(tag);
      if (cls) e.className = cls;
      if (txt != null) e.textContent = txt;
      return e;
    };
    var setDigits = function (n) {
      var str = String(n), cells = digits.querySelectorAll(".odo-strip");
      while (str.length < cells.length) str = "0" + str;
      for (var i = 0; i < cells.length; i++) cells[i].style.setProperty("--n", str.charAt(i));
    };
    var show = function (s) {
      var total = s.total;
      var grew = shown !== null && total > shown;
      if (shown === null || cellsFor(total) !== digits.querySelectorAll(".odo-strip").length) {
        var from = shown === null ? 0 : shown;
        build(cellsFor(total), false);
        setDigits(from);
        // A frame at the old value, then roll to the new one.
        void digits.offsetWidth;
      }
      setTimeout(function () { setDigits(total); }, 40);
      shown = total;
      if (ghost) {
        // The odometer's digits, giant and faint, behind the whole hero.
        var padded = String(total);
        while (padded.length < MIN_CELLS) padded = "0" + padded;
        ghost.textContent = padded.replace(/\B(?=(\d{3})+(?!\d))/g, "\u2009");
        ghost.classList.add("in");
      }
      if (total === 0) {
        text.innerHTML = "<b>Be one of the first.</b> The count starts with your download.";
      } else {
        text.innerHTML = "<b>" + fmt(total) + "</b> " + (total === 1 ? "download" : "downloads") + " so far, and counting.";
      }
      if (whole(s.last7days) && s.last7days > 0) {
        week.textContent = "+" + fmt(s.last7days) + " this week";
        week.hidden = false;
      } else {
        week.hidden = true;
      }
      if (pill) {
        $("#dl-float-n").textContent = fmt(total);
        $("#dl-float-word").textContent = total === 1 ? "download" : "downloads";
        var pw = $("#dl-float-week");
        pw.hidden = week.hidden;
        pw.textContent = week.textContent;
        if (pill.hidden) {
          pill.hidden = false;
          // A moment after it is displayed, so the fade-in runs.
          setTimeout(function () { pill.classList.add("in"); }, 40);
        }
      }
      if (grew && !reduce) {
        odo.classList.remove("bump");
        void odo.offsetWidth;
        odo.classList.add("bump");
      }
    };
    var load = function () {
      if (!window.fetch) return;
      fetch("downloads/stats.json", { cache: "no-cache", credentials: "omit" })
        .then(function (r) { return r.ok ? r.json() : null; })
        .then(function (s) {
          if (s && whole(s.total)) {
            everLoaded = true;
            show(s);
          } else if (!everLoaded) {
            text.textContent = "The live count is resting. Check back soon.";
          }
        })
        .catch(function () {
          if (!everLoaded) text.textContent = "The live count is resting. Check back soon.";
        });
    };
    build(MIN_CELLS, true);
    load();
    setInterval(function () { if (!document.hidden) load(); }, 3 * 60 * 1000);
    document.addEventListener("visibilitychange", function () { if (!document.hidden) load(); });
  }

  // The cards and the platform texts come from assets/downloads.js, which reads
  // assets/releases.js; the Windows texts appear once a Windows build is listed.
  var dl = $("#dl");
  var D = window.ConnectoDownloads;
  if (dl && D) {
    var rel = window.CONNECTO_RELEASES || [];
    var visitor = D.visitorPlatform(navigator);
    var hasWin = D.applyPlatformTexts(document, rel);
    var hero = $("#hero-dl");
    if (hero && hasWin && visitor === "windows") hero.textContent = "Download for Windows";
    if (rel.length) {
      dl.innerHTML = D.cardsHtml(rel, visitor);
    } else {
      dl.innerHTML = ["Apple silicon", "Intel"].map(function (t) {
        return '<div class="dl-card"><h3>macOS · ' + t + '</h3><p>The first public build is being prepared.</p>' +
          '<a class="btn btn-ghost" aria-disabled="true">Coming soon</a></div>';
      }).join("");
    }
  }
})();
