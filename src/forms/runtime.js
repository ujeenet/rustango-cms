/* Public form runtime (#543 FB-10 + #547 FB-14).
 *
 * One static, cacheable script that enhances every `form[data-rcms-form]`
 * on the page:
 *  - fills the `_csrf` field from the JS-readable double-submit cookie
 *    (the rendered HTML is cache-safe — no per-user token baked in);
 *  - checks every field before sending, including "at least one" on a
 *    required checkbox group (`data-rcms-required`);
 *  - drives multi-page steps: Next / Back / Enter, a progress line, one
 *    browser-history entry per step (the Back button returns to the
 *    previous step, a reload stays on the current one);
 *  - keeps the answers for the visit (sessionStorage), so a refused
 *    submission returns to the refused question with everything filled
 *    in (?form_error=<form id>:<field key>);
 *  - shows the thanks message after a successful submit
 *    (?form_submitted=<form id>);
 *  - evaluates conditional show/hide rules (mirrors forms::schema::is_visible).
 *
 * With JS off all pages stay visible, no fields are hidden, and the form
 * still submits (the server re-validates + re-evaluates rules).
 */
(function () {
  "use strict";
  if (window.__rcmsFormRuntime) return;
  window.__rcmsFormRuntime = 1;

  var CSRF_COOKIE = "rustango_csrf"; // forms::csrf::CSRF_COOKIE
  var CSRF_FIELD = "_csrf"; // forms::csrf::CSRF_FORM_FIELD
  var NOT_SAVED = { _csrf: 1, _hp: 1, _form: 1, _embed: 1 };

  function param(name) {
    var m = location.search.match(new RegExp("[?&]" + name + "=([^&]*)"));
    return m ? decodeURIComponent(m[1].replace(/\+/g, " ")) : null;
  }

  function csrfToken() {
    var m = document.cookie.match(new RegExp("(?:^|;\\s*)" + CSRF_COOKIE + "=([^;]*)"));
    return m ? m[1] : "";
  }

  // sessionStorage can be missing or throw (private windows, blocked storage).
  function load(key) {
    try { return JSON.parse(sessionStorage.getItem(key) || "null"); } catch (e) { return null; }
  }
  function store(key, value) {
    try {
      if (value === null) sessionStorage.removeItem(key);
      else sessionStorage.setItem(key, JSON.stringify(value));
    } catch (e) { /* the form still works without it */ }
  }

  function init(f) {
    var formKey = f.getAttribute("data-rcms-form-key") || f.getAttribute("action") || "form";
    var formId = formKey.split(":")[0];
    var draftKey = "rcms-form:" + formKey;
    var stepKey = "rcms-step:" + formKey;

    var i = f.querySelector('input[name="' + CSRF_FIELD + '"]');
    if (i) i.value = csrfToken();

    // A result flag belongs to this form when it names it (older links
    // carry `1`, which every form on the page accepts).
    function mine(flag) { return flag !== null && (flag === "1" || flag.split(":")[0] === formId); }

    // ---- thanks (post-submit) ----
    if (mine(param("form_submitted"))) {
      store(draftKey, null);
      var t = f.parentElement && f.parentElement.querySelector(".rcms-form-thanks");
      if (t) {
        t.hidden = false;
        f.hidden = true;
        return;
      }
    }

    // ---- answers kept for the visit ----
    function fields() {
      return [].slice.call(f.querySelectorAll("input[name],select[name],textarea[name]")).filter(function (el) {
        return !NOT_SAVED[el.name] && el.type !== "file" && el.type !== "submit" && el.type !== "button";
      });
    }
    function save() {
      var data = {};
      fields().forEach(function (el) {
        if (el.type === "checkbox" || el.type === "radio") {
          if (!data[el.name]) data[el.name] = [];
          if (el.checked) data[el.name].push(el.value);
        } else if (el.multiple) {
          data[el.name] = [].slice.call(el.selectedOptions).map(function (o) { return o.value; });
        } else {
          data[el.name] = [el.value];
        }
      });
      store(draftKey, data);
    }
    function restore(data) {
      fields().forEach(function (el) {
        var v = data[el.name];
        if (!v) return;
        if (el.type === "checkbox" || el.type === "radio") el.checked = v.indexOf(el.value) >= 0;
        else if (el.multiple) [].forEach.call(el.options, function (o) { o.selected = v.indexOf(o.value) >= 0; });
        else el.value = v[0];
      });
    }
    var draft = load(draftKey);
    if (draft) restore(draft);

    // ---- "at least one" on a required checkbox group ----
    var probe = document.createElement("input");
    probe.type = "checkbox";
    probe.required = true;
    var groups = [].slice.call(f.querySelectorAll("[data-rcms-required]"));
    function syncGroups() {
      groups.forEach(function (g) {
        var boxes = [].slice.call(g.querySelectorAll('input[type="checkbox"]'));
        if (!boxes.length) return;
        var ok = boxes[0].disabled || boxes.some(function (b) { return b.checked; });
        // The browser's own wording for "tick this", in the visitor's language.
        boxes[0].setCustomValidity(ok ? "" : probe.validationMessage || "Please tick at least one box.");
      });
    }

    // ---- conditional logic (FB-14) ----
    var ruled = [].slice.call(f.querySelectorAll("[data-rcms-rules]"));
    function valuesOf(key) {
      var els = f.querySelectorAll('[name="' + cssEscape(key) + '"]');
      var out = [];
      [].forEach.call(els, function (el) {
        if (el.type === "checkbox" || el.type === "radio") { if (el.checked) out.push(el.value); }
        else if (el.multiple) [].forEach.call(el.selectedOptions, function (o) { out.push(o.value); });
        else if (el.value) out.push(el.value);
      });
      return out;
    }
    // A checkbox group answers several values: eq / contains match when any
    // does, ne when none equals (mirrors schema::eval_condition).
    function evalCond(c) {
      var a = valuesOf(c.field);
      switch (c.op) {
        case "eq": return a.indexOf(c.value) >= 0;
        case "ne": return a.indexOf(c.value) < 0;
        case "contains": return a.some(function (v) { return v.indexOf(c.value) >= 0; });
        case "empty": return a.length === 0;
        case "not_empty": return a.length > 0;
        default: return true;
      }
    }
    function visible(rules) {
      var v = true;
      rules.forEach(function (r) {
        var cs = r.conditions || [];
        var matched = cs.length === 0 ? true : (r.match === "any" ? cs.some(evalCond) : cs.every(evalCond));
        if (r.action === "show") v = matched;
        else if (r.action === "hide") { if (matched) v = false; }
      });
      return v;
    }
    function applyRules() {
      ruled.forEach(function (el) {
        var rules;
        try { rules = JSON.parse(el.getAttribute("data-rcms-rules")); } catch (e) { return; }
        var show = visible(rules);
        el.hidden = !show;
        // disable hidden inputs so they neither submit nor block validation
        el.querySelectorAll("input,select,textarea").forEach(function (inp) { inp.disabled = !show; });
      });
    }
    function refresh() {
      applyRules();
      syncGroups();
    }
    f.addEventListener("input", function (e) { refresh(); unmark(e.target); save(); });
    f.addEventListener("change", function (e) { refresh(); unmark(e.target); save(); });
    refresh();

    // ---- steps (FB-10) ----
    var pg = [].slice.call(f.querySelectorAll("[data-rcms-page]"));
    var paged = pg.length > 1;
    var prev = f.querySelector("[data-rcms-prev]");
    var next = f.querySelector("[data-rcms-next]");
    var submit = f.querySelector("[data-rcms-submit]");
    var prog = f.querySelector("[data-rcms-progress]");
    var progText = (prog && prog.getAttribute("data-text")) || "Step {n} of {total}";
    var cur = 0;

    function historyState(n, pushed) {
      var s = {};
      var old = history.state;
      if (old && typeof old === "object") for (var k in old) s[k] = old[k];
      s[stepKey] = { n: n, pushed: !!pushed };
      return s;
    }
    // how: "push" (Next), "replace" (a jump), or "none" (history moved).
    function show(n, how, fromUser) {
      if (!paged) return;
      n = Math.max(0, Math.min(n, pg.length - 1));
      cur = n;
      pg.forEach(function (p, k) { p.hidden = k !== n; });
      if (prev) prev.hidden = n === 0;
      if (next) next.hidden = n >= pg.length - 1;
      if (submit) submit.hidden = n < pg.length - 1;
      if (prog) {
        prog.hidden = false;
        prog.textContent = progText.replace("{n}", n + 1).replace("{total}", pg.length);
      }
      try {
        if (how === "push") history.pushState(historyState(n, true), "");
        else if (how === "replace") history.replaceState(historyState(n, false), "");
      } catch (e) { /* history is optional */ }
      if (fromUser) {
        // Bring the step into view and put the cursor in its first answer.
        if (f.getBoundingClientRect().top < 0) f.scrollIntoView({ block: "start" });
        var first = pg[n].querySelector("input:not([type=hidden]):not(:disabled),select:not(:disabled),textarea:not(:disabled)");
        if (first) first.focus({ preventScroll: true });
      }
    }
    function pageValid(n) {
      var q = pg[n].querySelectorAll("input,select,textarea");
      for (var j = 0; j < q.length; j++) {
        if (!q[j].disabled && !q[j].checkValidity()) { q[j].reportValidity(); return false; }
      }
      return true;
    }
    function goNext() { if (pageValid(cur)) show(cur + 1, "push", true); }
    function goBack() {
      var s = history.state && history.state[stepKey];
      // Walk back through the entry Next made; otherwise just step back.
      if (s && s.pushed && s.n === cur) history.back();
      else show(cur - 1, "replace", true);
    }
    function pageOf(el) {
      for (var k = 0; k < pg.length; k++) if (pg[k].contains(el)) return k;
      return cur;
    }
    if (paged) {
      if (next) next.addEventListener("click", goNext);
      if (prev) prev.addEventListener("click", goBack);
      window.addEventListener("popstate", function (e) {
        var s = e.state && e.state[stepKey];
        show(s ? s.n : 0, "none", true);
      });
      var saved = history.state && history.state[stepKey];
      show(saved ? saved.n : 0, "replace", false);
    }

    // ---- send ----
    // The form has `novalidate` so steps can be checked one at a time.
    // Enter in a field submits a form; before the last step it means Next.
    f.addEventListener("submit", function (e) {
      if (paged && cur < pg.length - 1) {
        e.preventDefault();
        e.stopImmediatePropagation();
        goNext();
        return;
      }
      syncGroups();
      var q = f.querySelectorAll("input,select,textarea");
      for (var j = 0; j < q.length; j++) {
        if (!q[j].disabled && !q[j].checkValidity()) {
          e.preventDefault();
          e.stopImmediatePropagation();
          if (paged) show(pageOf(q[j]), "replace", false);
          q[j].reportValidity();
          return;
        }
      }
      save();
    });

    // ---- multipart (file-upload) forms: submit via fetch ----
    // The CSRF layer can't read `_csrf` from a multipart body, so send the
    // token in the X-CSRF-Token header instead and follow the redirect.
    if (f.hasAttribute("data-rcms-multipart")) {
      f.addEventListener("submit", function (e) {
        e.preventDefault();
        fetch(f.getAttribute("action"), {
          method: "POST",
          body: new FormData(f),
          headers: { "X-CSRF-Token": csrfToken() },
          credentials: "same-origin",
          redirect: "follow",
        })
          .then(function (r) {
            // A refusal without a redirect (upload too large, …) stays here
            // with the answers in place instead of opening the POST URL.
            if (!r.redirected || r.url.indexOf("/forms/submit/") >= 0) throw new Error("refused");
            window.location.href = r.url;
          })
          .catch(function () { showError(null); });
      });
    }

    // ---- the server refused the submission ----
    function unmark(el) {
      var w = el && el.closest && el.closest(".rcms-field");
      if (w && w.classList.contains("rcms-field-invalid")) {
        w.classList.remove("rcms-field-invalid");
        [].forEach.call(w.querySelectorAll("[aria-invalid]"), function (x) { x.removeAttribute("aria-invalid"); });
      }
    }
    function showError(fieldKey) {
      var err = f.querySelector(".rcms-form-error");
      if (err) err.hidden = false;
      var target = fieldKey ? f.querySelector('[name="' + cssEscape(fieldKey) + '"]') : null;
      if (!target) { if (err) err.scrollIntoView({ block: "center" }); return; }
      if (paged) show(pageOf(target), "replace", false);
      var w = target.closest(".rcms-field");
      if (w) {
        w.classList.add("rcms-field-invalid");
        [].forEach.call(w.querySelectorAll("input,select,textarea"), function (x) { x.setAttribute("aria-invalid", "true"); });
      }
      target.focus({ preventScroll: true });
      (w || target).scrollIntoView({ block: "center" });
    }
    var refused = param("form_error");
    if (mine(refused)) {
      var parts = refused.split(":");
      showError(parts.length > 1 ? parts.slice(1).join(":") : null);
    }
  }

  function cssEscape(s) {
    return String(s).replace(/["\\]/g, "\\$&");
  }

  function boot() {
    document.querySelectorAll("form[data-rcms-form]").forEach(init);
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot);
  else boot();
})();
