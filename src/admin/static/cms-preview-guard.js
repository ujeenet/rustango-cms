// Preview navigation guard.
//
// Runs INSIDE the page-editor preview iframe (injected by the server's
// `inject_axe_into_body_str`, the same seam as cms-axe-preview.js, so it
// loads on the initial `src` AND every `srcdoc` reload). The preview is a
// CONTAINED render, not a browser tab: clicking a link or submitting a
// form must never navigate the iframe away. Same-document hash anchors
// scroll within the preview ("same page"); every other link/form is a
// no-op, with a small cue so the inaction is explained.
(function () {
  if (typeof document === "undefined" || window.__rcmsPreviewGuard) return;
  window.__rcmsPreviewGuard = true;

  // A link that points within the current document (so it should scroll,
  // not navigate). Use the raw attribute first: in a `srcdoc` reload the
  // document URL is `about:srcdoc`, so resolved `.pathname` is unreliable
  // for relative `#id` links — but the raw `#…` href is not.
  function sameDocHash(a) {
    var raw = a.getAttribute("href") || "";
    if (raw.charAt(0) === "#") return a.hash.length > 1;
    return !!a.hash && a.pathname === location.pathname && a.search === location.search;
  }

  // Minimal, dependency-free cue. Never let it throw into the preview.
  function flashDisabled() {
    try {
      var id = "__rcms-preview-toast";
      if (document.getElementById(id) || !document.body) return;
      var t = document.createElement("div");
      t.id = id;
      t.textContent = "Links are disabled in preview";
      t.setAttribute("role", "status");
      t.style.cssText =
        "position:fixed;left:50%;bottom:16px;transform:translateX(-50%);" +
        "z-index:2147483647;pointer-events:none;background:rgba(17,24,39,.92);" +
        "color:#fff;font:500 13px/1.4 system-ui,-apple-system,sans-serif;" +
        "padding:8px 14px;border-radius:8px;box-shadow:0 4px 16px rgba(0,0,0,.25);" +
        "opacity:0;transition:opacity .15s ease";
      document.body.appendChild(t);
      requestAnimationFrame(function () { t.style.opacity = "1"; });
      setTimeout(function () {
        t.style.opacity = "0";
        setTimeout(function () { t.remove(); }, 200);
      }, 1500);
    } catch (_) { /* swallow — a cosmetic cue must never break the preview */ }
  }

  function onActivate(e) {
    var a = e.target && e.target.closest ? e.target.closest("a[href]") : null;
    if (!a) return;
    // Block navigation in all cases (covers cross-page, external, and
    // target="_blank" — the preview never spawns tabs).
    e.preventDefault();
    e.stopPropagation();
    if (sameDocHash(a)) {
      var key = decodeURIComponent(a.hash.slice(1));
      var target = null;
      try {
        target =
          (key && document.getElementById(key)) ||
          (key && document.querySelector(
            'a[name="' + (window.CSS && CSS.escape ? CSS.escape(key) : key) + '"]'
          ));
      } catch (_) { /* bad selector — fall through to no scroll */ }
      if (target) target.scrollIntoView({ behavior: "smooth", block: "start" });
      return;
    }
    flashDisabled();
  }

  // Capture phase so we win before the page's own handlers. `auxclick`
  // covers middle-click / modified-click that would otherwise open a tab.
  document.addEventListener("click", onActivate, true);
  document.addEventListener("auxclick", onActivate, true);
  // Forms must not navigate the preview either.
  document.addEventListener(
    "submit",
    function (e) { e.preventDefault(); e.stopPropagation(); },
    true
  );
})();
