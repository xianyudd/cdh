/* Theme cycler. The button is built here rather than shipped in the markup: it
   only works with script, and a dead control is worse than none. Without script
   the page keeps following the OS, which is the right fallback. */
(function () {
  var mount = document.querySelector(".theme-mount");
  if (!mount) { return; }
  var root = document.documentElement;
  var order = ["system", "light", "dark"];
  var labels = {
    system: mount.dataset.labelSystem,
    light: mount.dataset.labelLight,
    dark: mount.dataset.labelDark
  };
  var btn = document.createElement("button");
  btn.type = "button";
  btn.className = "theme-btn";
  if (mount.dataset.aria) { btn.setAttribute("aria-label", mount.dataset.aria); }

  function current() {
    var t = root.dataset.theme;
    return t === "light" || t === "dark" ? t : "system";
  }

  function show(mode) { btn.textContent = labels[mode] || mode; }

  function apply(mode) {
    if (mode === "system") {
      delete root.dataset.theme;
    } else {
      root.dataset.theme = mode;
    }
    try {
      if (mode === "system") {
        localStorage.removeItem("cdh-theme");
      } else {
        localStorage.setItem("cdh-theme", mode);
      }
    } catch (e) { /* private mode or a blocked origin: the choice just will not stick */ }
    show(mode);
  }

  btn.addEventListener("click", function () {
    apply(order[(order.indexOf(current()) + 1) % order.length]);
  });
  show(current());
  mount.appendChild(btn);
})();

(function () {
  var btn = document.getElementById("copy-btn");
  var note = document.getElementById("copy-note");
  var cmd = document.getElementById("install-cmd");
  if (!btn || !cmd) { return; }
  /* Ships as `hidden` so a scriptless page never offers a control that cannot
     copy; revealing it here is the only thing that makes it clickable. */
  btn.hidden = false;
  /* Every user-visible string is read out of the markup, so a translated page can
     load this file unchanged. */
  var idle = btn.textContent;
  function feedback(msg, ok) {
    note.textContent = msg;
    btn.textContent = ok ? btn.dataset.labelDone : idle;
    if (ok) {
      setTimeout(function () { btn.textContent = idle; }, 2000);
    }
  }
  btn.addEventListener("click", function () {
    var text = cmd.textContent.replace(/^\$ /, "").trim();
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(
        function () { feedback(btn.dataset.noteDone, true); },
        function () { feedback(btn.dataset.noteFail, false); }
      );
    } else {
      var ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand("copy");
        feedback(btn.dataset.noteDone, true);
      } catch (e) {
        feedback(btn.dataset.noteFail, false);
      }
      document.body.removeChild(ta);
    }
  });
})();

/* Pointer spotlight on the feature cards. Independent of GSAP: it only writes
   --mx / --my, which a radial-gradient in .card::before reads. */
(function () {
  var grid = document.querySelector(".bento");
  if (!grid || !window.matchMedia) { return; }
  if (!window.matchMedia("(hover: hover)").matches) { return; }
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) { return; }
  grid.addEventListener("pointermove", function (ev) {
    var card = ev.target && ev.target.closest ? ev.target.closest(".card") : null;
    if (!card) { return; }
    var box = card.getBoundingClientRect();
    if (!box.width || !box.height) { return; }
    card.style.setProperty("--mx", ((ev.clientX - box.left) / box.width * 100).toFixed(2) + "%");
    card.style.setProperty("--my", ((ev.clientY - box.top) / box.height * 100).toFixed(2) + "%");
  });
})();

(function () {
  if (!window.gsap || !window.ScrollTrigger) { return; }
  gsap.registerPlugin(ScrollTrigger);

  var nav = document.getElementById("site-nav");
  var bar = document.querySelector(".progress i");
  var heroWords = null;

  var CJK = "぀-ヿ㐀-䶿一-鿿豈-﫿";
  /* Closing punctuation glues to the character before it so it can never begin a line. */
  var GLUE = "、。…》」』】〕！），．：；？｝";
  var TOKEN = new RegExp("[" + CJK + "][" + GLUE + "]*|[^\\s" + CJK + "]+|\\s+", "g");

  /* Wrap each word of the headline in a span so it can be staggered. Built from
     textContent, never markup, and the accent fragment keeps its own element. */
  function wordsOf(root) {
    var out = [];
    (function walk(node) {
      Array.prototype.slice.call(node.childNodes).forEach(function (kid) {
        if (kid.nodeType === 3) {
          var frag = document.createDocumentFragment();
          (kid.nodeValue.match(TOKEN) || []).forEach(function (piece) {
            if (/^\s+$/.test(piece)) { frag.appendChild(document.createTextNode(" ")); return; }
            var span = document.createElement("span");
            span.className = "w";
            span.textContent = piece;
            frag.appendChild(span);
            out.push(span);
          });
          node.replaceChild(frag, kid);
        } else if (kid.nodeType === 1) {
          walk(kid);
        }
      });
    })(root);
    return out;
  }

  gsap.matchMedia().add("(prefers-reduced-motion: no-preference)", function () {
    var title = document.querySelector(".hero-title");
    if (title && !heroWords) { heroWords = wordsOf(title); }

    gsap.timeline({ defaults: { ease: "power3.out" } })
      .from(".hero .eyebrow", { y: 16, opacity: 0, duration: 0.45, clearProps: "transform,opacity" }, 0)
      .from(heroWords || [], { y: 16, opacity: 0, duration: 0.45, stagger: 0.035, clearProps: "transform,opacity" }, 0.1)
      .from([".hero-rail", ".hero-sub", ".install-box", ".copy-note", ".ghost-btn"],
        { y: 16, opacity: 0, duration: 0.5, stagger: 0.045, clearProps: "transform,opacity" }, 0.42);

    gsap.from(".term-lift", {
      y: 60, scale: 0.94, rotationX: 8, opacity: 0,
      transformPerspective: 1200, transformOrigin: "50% 100%",
      duration: 0.85, ease: "power3.out",
      /* The recording is pixel-exact at 1x; a leftover fractional transform would
         make the compositor resample it, so drop it the moment we land. */
      clearProps: "transform,transformOrigin,opacity",
      scrollTrigger: { trigger: ".term-stage", start: "top 85%", once: true }
    });

    gsap.utils.toArray(".sec-head").forEach(function (head) {
      gsap.from(head.children, {
        y: 20, opacity: 0, duration: 0.55, stagger: 0.08, ease: "power3.out",
        clearProps: "transform,opacity",
        scrollTrigger: { trigger: head, start: "top 88%", once: true }
      });
    });

    gsap.from(".beats li", {
      x: -16, opacity: 0, duration: 0.5, stagger: 0.06, ease: "power3.out",
      clearProps: "transform,opacity",
      scrollTrigger: { trigger: ".beats", start: "top 88%", once: true }
    });

    gsap.from(".bento > .card", {
      y: 24, opacity: 0, duration: 0.55, stagger: 0.05, ease: "power3.out",
      clearProps: "transform,opacity",
      scrollTrigger: { trigger: ".bento", start: "top 82%", once: true }
    });

    gsap.from(".install-grid > .way", {
      y: 24, opacity: 0, duration: 0.55, stagger: 0.05, ease: "power3.out",
      clearProps: "transform,opacity",
      scrollTrigger: { trigger: ".install-grid", start: "top 82%", once: true }
    });

    gsap.to(".bg-glow", {
      yPercent: 10, ease: "none",
      scrollTrigger: { start: 0, end: "max", scrub: true }
    });

    if (bar) {
      gsap.fromTo(bar, { scaleX: 0 }, {
        scaleX: 1, ease: "none",
        scrollTrigger: { start: 0, end: "max", scrub: true }
      });
    }

    if (nav) {
      ScrollTrigger.create({
        start: 40, end: 99999,
        toggleClass: { targets: nav, className: "is-stuck" }
      });
    }

    return function () {
      if (nav) { nav.classList.remove("is-stuck"); }
    };
  });
})();
