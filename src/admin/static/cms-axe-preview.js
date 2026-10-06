// #208 — axe-core preview glue.
//
// Runs INSIDE the preview iframe (loaded only after the editor's
// preview pane requested it via `inject_axe_into_preview` on the
// server side). Waits for the page to settle, kicks off
// `axe.run()`, and `postMessage`s the violations back to the editor
// at the parent window.
//
// Also listens for `cms-axe-scroll-to` messages so the editor's
// click-to-scroll affordance can reveal the offending element.
//
// Origin-checked: messages are only accepted from / posted to the
// same origin the iframe lives on (the cms-admin origin == the
// public preview origin in every tang-cms-shaped deploy). Use
// `event.origin === location.origin` rather than a hardcoded
// allow-list so per-tenant subdomains work.
(function () {
  if (typeof window === "undefined") return;
  // The iframe MAY be loaded in a context that doesn't have axe
  // (e.g. someone hits the preview URL directly). Guard so the
  // page still renders without a JS error blob.
  function runAxe() {
    if (typeof window.axe === "undefined") {
      // Last-ditch: try once more after the script load event in
      // case the order races on slow connections.
      setTimeout(runAxe, 200);
      return;
    }
    // Defaults are fine — issue #208 explicitly excludes a per-rule
    // disable / configure UI from V1. Run against the document root.
    window.axe
      .run(document, {
        // Surface only failed rules; passes/incomplete are not
        // useful in the editor side panel.
        resultTypes: ["violations"],
      })
      .then(function (results) {
        // Strip down the violation shape to JSON the editor can
        // serialize. axe nodes carry DOM references that won't
        // structured-clone through postMessage cleanly.
        var violations = (results.violations || []).map(function (v) {
          return {
            id: v.id,
            impact: v.impact, // 'minor' | 'moderate' | 'serious' | 'critical'
            help: v.help,
            helpUrl: v.helpUrl,
            description: v.description,
            nodes: (v.nodes || []).map(function (n) {
              return {
                // The first selector is the most-specific path to the
                // offending element — used by the editor's
                // click-to-scroll affordance.
                target: Array.isArray(n.target) ? n.target.join(" ") : String(n.target || ""),
                html: typeof n.html === "string" ? n.html.slice(0, 240) : "",
                failureSummary: n.failureSummary || "",
              };
            }),
          };
        });
        try {
          window.parent.postMessage(
            {
              type: "cms-axe-results",
              violations: violations,
              // Pass back the URL the iframe is on so the editor
              // can correlate stale results with the next reload.
              previewUrl: location.href,
              ranAt: Date.now(),
            },
            // In a sandboxed preview (no allow-same-origin) or about:blank
            // the origin serializes to the string "null", which postMessage
            // rejects as a target — throwing on every run and spamming the
            // console. "*" is safe here: the payload carries no secrets and
            // the parent validates the message type.
            location.origin === "null" ? "*" : location.origin
          );
        } catch (e) {
          // postMessage shouldn't throw for same-origin, but guard
          // anyway so a malformed parent doesn't break the iframe.
          console.warn("[cms-axe-preview] postMessage failed:", e);
        }
      })
      .catch(function (err) {
        // axe.run rejects on internal errors (rare). Report a clean
        // payload so the editor knows the run happened but yielded
        // no usable results.
        try {
          window.parent.postMessage(
            {
              type: "cms-axe-results",
              violations: [],
              error: String(err && err.message ? err.message : err),
              previewUrl: location.href,
              ranAt: Date.now(),
            },
            location.origin
          );
        } catch (_) {
          /* swallow */
        }
      });
  }

  // First run after the page paints. requestAnimationFrame +
  // setTimeout(0) gives the renderer a chance to layout images /
  // fonts before axe walks the DOM — running too early on a
  // half-painted page reports false alarms for off-screen
  // contrast checks.
  if (document.readyState === "complete") {
    setTimeout(runAxe, 0);
  } else {
    window.addEventListener("load", function () {
      setTimeout(runAxe, 0);
    });
  }

  // Scroll-to listener for the editor's click-on-violation
  // affordance. Editor posts {type: "cms-axe-scroll-to", selector}.
  window.addEventListener("message", function (event) {
    if (event.origin !== location.origin) return;
    var data = event.data;
    if (!data || data.type !== "cms-axe-scroll-to" || typeof data.selector !== "string") {
      return;
    }
    var el = null;
    try {
      el = document.querySelector(data.selector);
    } catch (_) {
      // Bad selector — silently no-op so the editor doesn't crash.
    }
    if (el) {
      el.scrollIntoView({ behavior: "smooth", block: "center" });
      // Brief outline so the editor sees what they targeted. Removed
      // after the scroll settles. The outline color matches the
      // a11y card's error tag for visual continuity.
      var prev = el.style.outline;
      var prevOffset = el.style.outlineOffset;
      el.style.outline = "3px solid #d92d20";
      el.style.outlineOffset = "2px";
      setTimeout(function () {
        el.style.outline = prev;
        el.style.outlineOffset = prevOffset;
      }, 1800);
    }
  });
})();
