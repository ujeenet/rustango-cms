// Template editor — enhances the source textarea with CodeMirror.
//
// The bundle is lazy-loaded exactly like `tiptap.bundle.js`: injected on
// demand from `/cms-admin/static/vendor/`, so no other admin page pays
// its ~86 KB gzipped, and it loads under a strict `script-src 'self'`
// CSP with no nonce.
//
// If the bundle is missing or fails, the textarea is left alone — styled
// monospace, tab-indents, and still perfectly usable. An editor that
// degrades to "nothing" would be worse than no editor.
(function () {
  "use strict";

  // The bundle is served immutable, so it carries this script's own
  // `?v=` build token: a new release is a new URL (#696).
  const ASSET_VERSION = document.currentScript
    ? new URL(document.currentScript.src, location.href).search
    : "";
  var VENDOR = "/cms-admin/static/vendor/codemirror.bundle.js" + ASSET_VERSION;
  var bundlePromise = null;

  function ensureBundle() {
    if (window.CodeMirror) return Promise.resolve();
    if (bundlePromise) return bundlePromise;
    bundlePromise = new Promise(function (resolve, reject) {
      var s = document.createElement("script");
      s.src = VENDOR;
      s.onload = resolve;
      s.onerror = reject;
      document.head.appendChild(s);
    });
    return bundlePromise;
  }

  /** Tab indents instead of leaving the field — the plain-textarea path. */
  function plainFallback(ta) {
    ta.classList.add("rcms-code-plain");
    ta.addEventListener("keydown", function (e) {
      if (e.key !== "Tab") return;
      e.preventDefault();
      var s = ta.selectionStart, t = ta.selectionEnd;
      ta.value = ta.value.slice(0, s) + "  " + ta.value.slice(t);
      ta.selectionStart = ta.selectionEnd = s + 2;
    });
  }

  function enhance(ta) {
    if (ta.dataset.rcmsCm) return;
    ta.dataset.rcmsCm = "1";

    ensureBundle()
      .then(function () {
        var isDark = document.documentElement.getAttribute("data-theme") === "dark"
          || (!document.documentElement.hasAttribute("data-theme")
              && window.matchMedia("(prefers-color-scheme: dark)").matches);

        var cm = window.CodeMirror.fromTextArea(ta, {
          mode: "tera",
          lineNumbers: true,
          lineWrapping: true,
          matchBrackets: true,
          autoCloseTags: true,
          styleActiveLine: true,
          indentUnit: 2,
          tabSize: 2,
          viewportMargin: Infinity,
          extraKeys: {
            "Ctrl-/": "toggleComment",
            "Cmd-/": "toggleComment",
            // Save from the keyboard, like any editor.
            "Ctrl-S": submit,
            "Cmd-S": submit,
            Tab: function (editor) {
              if (editor.somethingSelected()) editor.indentSelection("add");
              else editor.replaceSelection("  ", "end");
            },
          },
        });
        cm.getWrapperElement().classList.toggle("rcms-cm-dark", isDark);

        function submit() {
          cm.save();
          var form = ta.form;
          if (form) form.requestSubmit ? form.requestSubmit() : form.submit();
        }
        // `fromTextArea` syncs on form submit only via `cm.save()`; wire
        // it explicitly so the button path is covered too.
        if (ta.form) {
          ta.form.addEventListener("submit", function () { cm.save(); });
        }
      })
      .catch(function () {
        plainFallback(ta);
      });
  }

  /**
   * Check the current body without saving.
   *
   * Saving already refuses a body that will not parse, so this is the
   * feedback loop rather than the safety net — it answers in place so an
   * author does not lose their cursor to a round trip.
   */
  function wireValidate() {
    var btn = document.querySelector("[data-validate]");
    var out = document.querySelector("[data-validate-result]");
    if (!btn || !out) return;
    var ta = document.querySelector("textarea[data-template-editor]");
    if (!ta) return;

    btn.addEventListener("click", function () {
      // Pull from CodeMirror when it has taken over the textarea.
      var wrapper = ta.nextElementSibling;
      var cm = wrapper && wrapper.CodeMirror;
      if (cm) cm.save();

      var form = ta.form;
      var body = new URLSearchParams({
        name: form.querySelector('input[name="name"]').value,
        body: ta.value,
      });
      var csrf = form.querySelector('input[name="_csrf"]');
      if (csrf) body.append("_csrf", csrf.value);

      btn.disabled = true;
      out.hidden = false;
      out.className = "rcms-validate-result";
      out.textContent = "…";

      fetch(btn.dataset.validateUrl, {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: body,
        credentials: "same-origin",
      })
        .then(function (r) { return r.json(); })
        .then(function (j) {
          out.className = "rcms-validate-result " + (j.ok ? "is-ok" : "is-bad");
          out.textContent = j.ok ? btn.dataset.okText || "Template parses." : j.error;
        })
        .catch(function (e) {
          out.className = "rcms-validate-result is-bad";
          out.textContent = String(e);
        })
        .finally(function () { btn.disabled = false; });
    });
  }

  function init() {
    var nodes = document.querySelectorAll("textarea[data-template-editor]");
    for (var i = 0; i < nodes.length; i++) enhance(nodes[i]);
    wireValidate();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
