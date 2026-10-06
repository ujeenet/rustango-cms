/* Page-builder conditional rules runtime (#559/#564).
 *
 * Reads `#ptb-body[data-rules]` = [{field, rule}] where rule mirrors
 * forms::schema::ConditionalRule ({match, action, conditions:[{field,op,
 * value}]}). Shows/hides the governed field live as its dependency inputs
 * change. Server-side save re-checks visibility so hidden required fields
 * don't block (values.rs). Top-level fields only in v1.
 */
(function () {
  "use strict";
  var body = document.getElementById("ptb-body");
  if (!body) return;
  var rules = [];
  try { rules = JSON.parse(body.dataset.rules || "[]"); } catch (e) { return; }
  if (!rules.length) return;

  // Find a field's input(s) by builder key (pb__<key>), and its wrapper.
  function inputFor(key) {
    return body.querySelector('[name="pb__' + CSS.escape(key) + '"]');
  }
  function wrapperOf(input) {
    return input ? input.closest(".rcms-field, .rcms-field-row > div, [data-widget]") || input.parentElement : null;
  }
  function valueOf(key) {
    var inp = inputFor(key);
    if (!inp) return "";
    if (inp.type === "checkbox") return inp.checked ? "on" : "";
    return inp.value || "";
  }
  function test(cond) {
    var v = valueOf(cond.field);
    var t = cond.value || "";
    switch (cond.op) {
      case "eq": return v === t;
      case "ne": return v !== t;
      case "contains": return v.indexOf(t) >= 0;
      case "gt": return parseFloat(v) > parseFloat(t);
      case "lt": return parseFloat(v) < parseFloat(t);
      case "empty": return !v;
      case "not_empty": return !!v;
      default: return true;
    }
  }
  function passes(rule) {
    var conds = rule.conditions || [];
    if (!conds.length) return true;
    var mode = rule.match === "any" ? "any" : "all";
    return mode === "any" ? conds.some(test) : conds.every(test);
  }

  function evaluate() {
    rules.forEach(function (rb) {
      var inp = inputFor(rb.field);
      var wrap = wrapperOf(inp);
      if (!wrap) return;
      var show = passes(rb.rule);
      // action "show" (default) → visible when conditions pass; "hide"
      // inverts.
      if ((rb.rule.action || "show") === "hide") show = !show;
      wrap.style.display = show ? "" : "none";
      wrap.setAttribute("data-rule-hidden", show ? "" : "1");
    });
  }

  // Re-evaluate on any input in the builder body (cheap; the rule set is
  // small and top-level).
  body.addEventListener("input", evaluate);
  body.addEventListener("change", evaluate);
  evaluate();
})();
