/* Page-type field builder (#559/#562).
 *
 * Render-from-state, framework-free: the schema document is the single
 * source of truth, the canvas is rendered from it, and every mutation
 * updates state → re-renders → serializes `{nodes: state}` into the
 * hidden #ptb-schema input the Save/Publish forms submit.
 *
 * Node shapes mirror src/page_builder/schema.rs (serde `kind` tag):
 *   field | row{children} | group{key,children} | component{ref}
 *   repeater{key,item{children}} | flex{key,allowed[],groups[]}
 * Widget kinds here mirror schema::widget_allowed (kept in sync by hand).
 */
(function () {
  "use strict";
  var root = document.getElementById("ptb-root");
  if (!root) return;
  var hidden = document.getElementById("ptb-schema");
  var canvas = document.getElementById("ptb-canvas");
  var inspector = document.getElementById("ptb-inspector");

  // ---- state -----------------------------------------------------------
  var state = [];
  try {
    var doc = JSON.parse(root.dataset.schema || '{"nodes":[]}');
    // The server omits empty arrays on round-trip — normalize every node so
    // the rest of the builder can rely on the shapes existing.
    (function normalize(nodes) {
      (nodes || []).forEach(function (n) {
        if (n.kind === "flex") { n.allowed = n.allowed || []; n.groups = n.groups || []; n.groups.forEach(function (g) { g.children = g.children || []; normalize(g.children); }); }
        if (n.kind === "group" || n.kind === "row") { n.children = n.children || []; normalize(n.children); }
        if (n.kind === "repeater") { n.item = n.item || { key: "item", label: "Item", children: [] }; n.item.children = n.item.children || []; normalize(n.item.children); }
        if (n.kind === "field") { n.options = n.options || []; n.rules = n.rules || []; }
      });
    })(doc.nodes);
    state = Array.isArray(doc.nodes) ? doc.nodes : [];
  } catch (e) { state = []; }
  var components = [];
  try { components = JSON.parse(root.dataset.components || "[]"); } catch (e) {}
  // #563 — component mode: fields + rows only (no groups/repeaters/zones/
  // nested components). Same engine, restricted top-level palette.
  var componentMode = root.hasAttribute("data-component-mode");
  var selId = null; // id of the node shown in the inspector

  // Widget catalog — keep in sync with schema::widget_allowed.
  var WIDGETS = [
    { g: "Basic", items: ["text", "textarea", "markdown", "richtext", "email", "url", "tel", "number", "integer", "float", "boolean", "color"] },
    { g: "Date", items: ["date", "time", "datetime", "datetimetz", "range"] },
    { g: "Choice", items: ["select", "radio", "checkboxes", "multiselect"] },
    { g: "Media / references", items: ["mediapicker", "file", "pagechooser", "snippetchooser", "documentchooser"] },
  ];
  var CHOICE_KINDS = ["select", "radio", "checkboxes", "multiselect"];
  // Material Symbols icon per widget kind — makes the field-type picker read
  // as labelled tiles instead of a wall of text links.
  var WIDGET_ICON = {
    text: "text_fields", textarea: "notes", markdown: "code", richtext: "edit_note",
    email: "mail", url: "link", tel: "call", number: "numbers", integer: "tag",
    float: "functions", boolean: "toggle_on", color: "palette",
    date: "calendar_today", time: "schedule", datetime: "event", datetimetz: "public", range: "tune",
    select: "expand_circle_down", radio: "radio_button_checked", checkboxes: "check_box", multiselect: "checklist",
    mediapicker: "image", file: "attach_file", pagechooser: "article", snippetchooser: "widgets", documentchooser: "description",
  };

  function uid() {
    return "n" + Math.random().toString(36).slice(2, 10);
  }
  function slugify(s) {
    return (s || "").toLowerCase().replace(/[^a-z0-9_]+/g, "_").replace(/^_+|_+$/g, "") || "field";
  }
  // Human label from a key: "hero_image" → "Hero Image", "field_2" → "Field 2".
  function titleize(s) {
    return (s || "").split("_").filter(Boolean)
      .map(function (w) { return w.charAt(0).toUpperCase() + w.slice(1); })
      .join(" ") || "Field";
  }
  // Insert a freshly-created node into `list`, giving its key (+ matching
  // label) a suffix so it's unique among siblings. Without this, adding
  // several nodes of the same kind yields duplicate names/keys — visually
  // confusing and rejected on publish (dup-key validation).
  function insertInto(list, node) {
    if (node.key) {
      var taken = list.map(function (n) { return n.key; }).filter(Boolean);
      var base = node.key, k = base, i = 2;
      while (taken.indexOf(k) >= 0) { k = base + "_" + i; i++; }
      node.key = k;
      node.label = titleize(k);
    }
    list.push(node);
    select(node);
    rerender();
  }
  function el(tag, attrs, kids) {
    var e = document.createElement(tag);
    if (attrs) Object.keys(attrs).forEach(function (k) {
      if (k === "class") e.className = attrs[k];
      else if (k === "text") e.textContent = attrs[k];
      else if (k === "html") e.innerHTML = attrs[k];
      else if (k.slice(0, 2) === "on") e.addEventListener(k.slice(2), attrs[k]);
      else e.setAttribute(k, attrs[k]);
    });
    (kids || []).forEach(function (c) { if (c) e.appendChild(c); });
    return e;
  }
  function icon(name) { return el("span", { class: "material-symbols-rounded sm", text: name }); }

  function save() {
    if (hidden) hidden.value = JSON.stringify({ nodes: state });
  }
  function select(node) { selId = node ? node.id : null; }
  function rerender() { save(); renderCanvas(); renderInspector(); }
  // Canvas-only refresh for edits made FROM the inspector: re-rendering the
  // inspector mid-keystroke destroys the very input being typed in (one
  // character per focus). The canvas mirrors the new label; the inspector
  // keeps its live inputs.
  function rerenderCanvasOnly() { save(); renderCanvas(); }

  // ---- node constructors ----------------------------------------------
  function newField() {
    return { kind: "field", id: uid(), key: "field", label: "Field", widget: "text",
      help: "", placeholder: "", default: "", required: false, options: [], rules: [] };
  }
  function newRow() { return { kind: "row", id: uid(), children: [newField()] }; }
  function newGroup() { return { kind: "group", id: uid(), key: "group", label: "Group", children: [] }; }
  function newRepeater() {
    return { kind: "repeater", id: uid(), key: "items", label: "Items",
      item: { key: "item", label: "Item", children: [] } };
  }
  function newFlex() { return { kind: "flex", id: uid(), key: "sections", label: "Sections", allowed: [], groups: [] }; }
  function newComponent(ref) { return { kind: "component", id: uid(), ref: ref }; }

  // ---- canvas ----------------------------------------------------------
  function palette(list, opts) {
    // opts.fieldsOnly → rows disallow non-field; groups/items allow field+row.
    var bar = el("div", { class: "ptb-palette rcms-flex rcms-gap-1 rcms-flex-wrap rcms-mb-2" });
    function addBtn(label, mk, ico) {
      bar.appendChild(el("button", {
        class: "rcms-btn rcms-btn-outlined rcms-btn-small", type: "button", "data-t": label,
        onclick: function () { insertInto(list, mk()); },
      }, [icon(ico), document.createTextNode(" " + label)]));
    }
    // Field picker (opens a widget-kind menu).
    var fieldWrap = el("details", { class: "ptb-fieldpick" });
    var sum = el("summary", { class: "rcms-btn rcms-btn-small", html: '<span class="material-symbols-rounded sm">add</span> Field' });
    fieldWrap.appendChild(sum);
    var menu = el("div", { class: "ptb-fieldpick-menu rcms-card", style: "position:absolute;z-index:20;padding:12px;width:380px;max-height:60vh;overflow:auto;" });
    WIDGETS.forEach(function (grp, gi) {
      menu.appendChild(el("div", {
        text: grp.g,
        style: "font-size:11px;font-weight:600;text-transform:uppercase;letter-spacing:.05em;"
          + "color:var(--md-sys-color-on-surface-variant);margin:" + (gi ? "12px" : "0") + " 0 6px;",
      }));
      var grid = el("div", { style: "display:grid;grid-template-columns:repeat(auto-fill,minmax(168px,1fr));gap:6px;" });
      grp.items.forEach(function (kind) {
        grid.appendChild(el("button", {
          class: "rcms-btn rcms-btn-outlined rcms-btn-small", type: "button", title: kind,
          style: "justify-content:flex-start;gap:8px;overflow:hidden;",
          onclick: function () {
            var f = newField(); f.widget = kind;
            fieldWrap.removeAttribute("open");
            insertInto(list, f);
          },
        }, [
          icon(WIDGET_ICON[kind] || "label"),
          el("span", { text: kind, style: "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;" }),
        ]));
      });
      menu.appendChild(grid);
    });
    fieldWrap.appendChild(menu);
    bar.appendChild(fieldWrap);

    if (!opts || !opts.fieldsOnly) {
      addBtn("Row", newRow, "table_rows");
    }
    if (opts && opts.top) {
      addBtn("Group", newGroup, "widgets");
      addBtn("Repeater", newRepeater, "repeat");
      addBtn("Flexible zone", newFlex, "dashboard_customize");
      // Component insert (from the library).
      if (components.length) {
        var cwrap = el("details", { class: "ptb-fieldpick" });
        cwrap.appendChild(el("summary", { class: "rcms-btn rcms-btn-outlined rcms-btn-small", html: '<span class="material-symbols-rounded sm">extension</span> Component' }));
        var cmenu = el("div", { class: "rcms-card", style: "position:absolute;z-index:20;padding:8px;" });
        components.forEach(function (c) {
          cmenu.appendChild(el("button", {
            class: "rcms-btn rcms-btn-text rcms-btn-small", type: "button", text: c.label + " (" + c.slug + ")",
            onclick: function () { var n = newComponent(c.slug); list.push(n); select(n); cwrap.removeAttribute("open"); rerender(); },
          }));
        });
        cwrap.appendChild(cmenu);
        bar.appendChild(cwrap);
      }
    }
    return bar;
  }

  function nodeHeader(node, list, idx, label, ico, meta) {
    var head = el("div", { class: "ptb-node-head rcms-flex rcms-items-center rcms-gap-2" });
    var isSel = node.id === selId;
    var name = el("button", {
      class: "rcms-btn rcms-btn-text rcms-btn-small" + (isSel ? " is-active" : ""), type: "button",
      onclick: function () { select(node); renderInspector(); highlightSelected(); },
    }, [icon(ico), el("strong", { text: " " + label })]);
    head.appendChild(name);
    // Show the machine key only when it's been customized away from the
    // auto-slug of the label — otherwise it's the same word again (e.g.
    // "Subtitle" + `subtitle`), which reads as a duplicated name.
    if (node.key && node.key !== slugify(node.label || "")) {
      head.appendChild(el("code", { class: "rcms-text-2xs rcms-text-muted", text: node.key }));
    }
    // Inline meta (e.g. a field's widget kind) — keeps the node a single
    // compact row instead of spending a second line on it.
    if (meta) head.appendChild(el("span", { class: "rcms-text-2xs rcms-text-muted", text: meta }));
    var sp = el("span", { style: "flex:1" }); head.appendChild(sp);
    head.appendChild(iconBtn("arrow_upward", function () { moveNode(list, idx, -1); }));
    head.appendChild(iconBtn("arrow_downward", function () { moveNode(list, idx, 1); }));
    head.appendChild(iconBtn("delete", function () { list.splice(idx, 1); if (node.id === selId) selId = null; rerender(); }, "rcms-text-danger"));
    return head;
  }
  function iconBtn(ico, fn, cls) {
    return el("button", { class: "rcms-btn rcms-btn-text rcms-btn-small " + (cls || ""), type: "button", onclick: fn }, [icon(ico)]);
  }
  function moveNode(list, idx, dir) {
    var j = idx + dir;
    if (j < 0 || j >= list.length) return;
    var t = list[idx]; list[idx] = list[j]; list[j] = t; rerender();
  }

  function renderNode(node, list, idx) {
    var wrap = el("div", { class: "ptb-node rcms-card", "data-node-id": node.id,
      style: "padding:4px 10px;margin-bottom:6px;" + (node.id === selId ? "outline:2px solid var(--md-sys-color-primary);" : "") });
    if (node.kind === "field") {
      wrap.appendChild(nodeHeader(node, list, idx, node.label || node.key, "input",
        node.widget + (node.required ? " · required" : "")));
    } else if (node.kind === "row") {
      wrap.appendChild(nodeHeader(node, list, idx, node.label || "Row", "table_rows"));
      var inner = el("div", { class: "rcms-flex rcms-gap-2 rcms-flex-wrap rcms-mt-2" });
      node.children.forEach(function (c, i) {
        var cell = el("div", { style: "flex:1 1 160px;min-width:140px;" });
        cell.appendChild(renderNode(c, node.children, i));
        inner.appendChild(cell);
      });
      wrap.appendChild(inner);
      wrap.appendChild(palette(node.children, { fieldsOnly: true }));
    } else if (node.kind === "group") {
      wrap.appendChild(nodeHeader(node, list, idx, node.label || node.key, "widgets"));
      var body = el("div", { class: "rcms-ml-3 rcms-mt-2" });
      renderChildList(body, node.children);
      wrap.appendChild(body);
    } else if (node.kind === "repeater") {
      wrap.appendChild(nodeHeader(node, list, idx, (node.label || node.key) + " (repeater)", "repeat"));
      var rb = el("div", { class: "rcms-ml-3 rcms-mt-2" });
      rb.appendChild(el("div", { class: "rcms-text-2xs rcms-text-muted", text: "Each item:" }));
      renderChildList(rb, node.item.children);
      wrap.appendChild(rb);
    } else if (node.kind === "flex") {
      wrap.appendChild(nodeHeader(node, list, idx, (node.label || node.key) + " (flexible zone)", "dashboard_customize"));
      wrap.appendChild(el("div", { class: "rcms-text-2xs rcms-text-muted rcms-mt-1",
        text: "Allowed: " + (node.allowed.length ? node.allowed.join(", ") : "(none yet — edit in the inspector)") }));
      (node.groups || []).forEach(function (g, gi) {
        var gb = el("div", { class: "rcms-card rcms-ml-3 rcms-mt-2", style: "padding:8px;" });
        gb.appendChild(el("div", { class: "rcms-flex rcms-items-center rcms-gap-2" }, [
          icon("widgets"), el("strong", { text: g.label || g.key }),
          el("span", { style: "flex:1" }),
          iconBtn("delete", function () {
            node.groups.splice(gi, 1);
            var ai = node.allowed.indexOf(g.key); if (ai >= 0) node.allowed.splice(ai, 1);
            rerender();
          }, "rcms-text-danger"),
        ]));
        var gcb = el("div", { class: "rcms-ml-2 rcms-mt-1" });
        renderChildList(gcb, g.children);
        gb.appendChild(gcb);
        wrap.appendChild(gb);
      });
      wrap.appendChild(el("button", {
        class: "rcms-btn rcms-btn-outlined rcms-btn-small rcms-mt-2", type: "button",
        html: '<span class="material-symbols-rounded sm">add</span> Zone group',
        onclick: function () {
          var k = "group" + (node.groups.length + 1);
          node.groups.push({ key: k, label: "Group " + (node.groups.length + 1), children: [] });
          node.allowed.push(k);
          rerender();
        },
      }));
    } else if (node.kind === "component") {
      wrap.appendChild(nodeHeader(node, list, idx, "Component: " + node.ref, "extension"));
    }
    return wrap;
  }

  function renderChildList(container, childList) {
    childList.forEach(function (c, i) { container.appendChild(renderNode(c, childList, i)); });
    container.appendChild(palette(childList, {}));
  }

  function renderCanvas() {
    canvas.innerHTML = "";
    canvas.appendChild(el("div", { class: "rcms-text-sm rcms-text-muted-2 rcms-mb-2", text: "Body structure" }));
    canvas.appendChild(palette(state, componentMode ? {} : { top: true }));
    if (!state.length) {
      canvas.appendChild(el("div", { class: "rcms-empty rcms-mt-2", html: "<p>No fields yet — add one above.</p>" }));
      return;
    }
    state.forEach(function (n, i) { canvas.appendChild(renderNode(n, state, i)); });
  }
  function highlightSelected() {
    canvas.querySelectorAll(".ptb-node").forEach(function (n) {
      n.style.outline = n.getAttribute("data-node-id") === selId ? "2px solid var(--md-sys-color-primary)" : "";
    });
  }

  // ---- inspector -------------------------------------------------------
  function findNode(id, list) {
    for (var i = 0; i < list.length; i++) {
      var n = list[i];
      if (n.id === id) return n;
      var kids = n.children || (n.item && n.item.children) || null;
      if (kids) { var r = findNode(id, kids); if (r) return r; }
      if (n.groups) { for (var g = 0; g < n.groups.length; g++) { var rg = findNode(id, n.groups[g].children); if (rg) return rg; } }
    }
    return null;
  }
  function labeled(labelText, control) {
    return el("div", { class: "rcms-field" }, [el("label", { text: labelText }), control]);
  }
  function textField(labelText, val, oninput) {
    var inp = el("input", { type: "text", value: val || "" });
    inp.addEventListener("input", function () { oninput(inp.value); });
    return labeled(labelText, inp);
  }
  function checkField(labelText, val, onchange) {
    var w = el("div", { class: "rcms-field rcms-checkbox-field" });
    var inp = el("input", { type: "checkbox" }); inp.checked = !!val;
    inp.addEventListener("change", function () { onchange(inp.checked); });
    w.appendChild(inp); w.appendChild(el("label", { text: labelText }));
    return w;
  }

  function renderInspector() {
    inspector.innerHTML = "";
    var node = selId ? findNode(selId, state) : null;
    if (!node) {
      inspector.appendChild(el("p", { class: "rcms-text-muted rcms-text-sm", text: "Select a field or container to edit it." }));
      return;
    }
    inspector.appendChild(el("h3", { class: "rcms-text-sm rcms-m-0 rcms-mb-2", text: "Edit " + node.kind }));

    if (node.kind === "field") {
      inspector.appendChild(textField("Label", node.label, function (v) {
        node.label = v; if (!node._keyTouched) node.key = slugify(v); rerenderCanvasOnly();
      }));
      inspector.appendChild(textField("Key", node.key, function (v) { node.key = slugify(v); node._keyTouched = true; save(); }));
      var wsel = el("select");
      WIDGETS.forEach(function (grp) {
        var og = el("optgroup", { label: grp.g });
        grp.items.forEach(function (k) {
          var o = el("option", { value: k, text: k }); if (k === node.widget) o.selected = true; og.appendChild(o);
        });
        wsel.appendChild(og);
      });
      wsel.addEventListener("change", function () { node.widget = wsel.value; rerender(); });
      inspector.appendChild(labeled("Widget", wsel));
      inspector.appendChild(checkField("Required", node.required, function (v) { node.required = v; rerender(); }));
      inspector.appendChild(textField("Help text", node.help, function (v) { node.help = v; save(); }));
      inspector.appendChild(textField("Placeholder", node.placeholder, function (v) { node.placeholder = v; save(); }));
      if (CHOICE_KINDS.indexOf(node.widget) >= 0) inspector.appendChild(optionsEditor(node));
      inspector.appendChild(rulesEditor(node));
    } else if (node.kind === "row") {
      inspector.appendChild(el("p", { class: "rcms-text-sm rcms-text-muted", text: "A row lays its fields side by side. Set each field's width (1–12) below." }));
      node.children.forEach(function (f) {
        var inp = el("input", { type: "number", min: "1", max: "12", value: String(f.width || "") });
        inp.addEventListener("input", function () { f.width = parseInt(inp.value, 10) || undefined; save(); });
        inspector.appendChild(labeled((f.label || f.key) + " width", inp));
      });
    } else if (node.kind === "group" || node.kind === "repeater") {
      inspector.appendChild(textField("Label", node.label, function (v) { node.label = v; if (!node._kt) node.key = slugify(v); rerenderCanvasOnly(); }));
      inspector.appendChild(textField("Key", node.key, function (v) { node.key = slugify(v); node._kt = true; save(); }));
      if (node.kind === "repeater") {
        var mn = el("input", { type: "number", min: "0", value: node.min != null ? String(node.min) : "" });
        mn.addEventListener("input", function () { node.min = mn.value === "" ? undefined : parseInt(mn.value, 10); save(); });
        inspector.appendChild(labeled("Min items", mn));
        var mx = el("input", { type: "number", min: "0", value: node.max != null ? String(node.max) : "" });
        mx.addEventListener("input", function () { node.max = mx.value === "" ? undefined : parseInt(mx.value, 10); save(); });
        inspector.appendChild(labeled("Max items", mx));
      }
    } else if (node.kind === "flex") {
      inspector.appendChild(textField("Label", node.label, function (v) { node.label = v; if (!node._kt) node.key = slugify(v); rerenderCanvasOnly(); }));
      inspector.appendChild(textField("Key", node.key, function (v) { node.key = slugify(v); node._kt = true; save(); }));
      inspector.appendChild(el("label", { text: "Allowed blocks" }));
      // Zone-local groups.
      node.groups.forEach(function (g) {
        inspector.appendChild(allowedToggle(node, g.key, g.label + " (zone group)"));
      });
      // Library components.
      components.forEach(function (c) {
        inspector.appendChild(allowedToggle(node, "c_" + c.slug, c.label + " (component)"));
      });
      // Allowed code-block types — removable, like groups/components. Without
      // these toggles a mistyped entry was permanent (no removal UI).
      node.allowed.forEach(function (k) {
        if (k.indexOf("c_") === 0) return; // component toggle rendered above
        var isGroup = node.groups.some(function (g) { return g.key === k; });
        if (isGroup) return;
        inspector.appendChild(allowedToggle(node, k, k + " (code block)"));
      });
      // Free-form (code blocks) entry — commits on Enter or blur. Committing
      // per input event pushed a one-letter junk type and re-rendered the
      // inspector away mid-typing (focus lost on the first keystroke).
      var allowWrap = el("div", { class: "rcms-field" });
      allowWrap.appendChild(el("label", { text: "+ allow a code block type (press Enter)" }));
      var allowInp = el("input", { type: "text" });
      function commitAllow() {
        var t = slugify(allowInp.value);
        allowInp.value = "";
        if (t && t !== "field" && node.allowed.indexOf(t) < 0) {
          node.allowed.push(t);
          rerender();
        }
      }
      allowInp.addEventListener("keydown", function (e) {
        if (e.key === "Enter") { e.preventDefault(); commitAllow(); }
      });
      allowInp.addEventListener("change", commitAllow);
      allowWrap.appendChild(allowInp);
      inspector.appendChild(allowWrap);
    } else if (node.kind === "component") {
      var sel = el("select");
      components.forEach(function (c) { var o = el("option", { value: c.slug, text: c.label }); if (c.slug === node.ref) o.selected = true; sel.appendChild(o); });
      sel.addEventListener("change", function () { node.ref = sel.value; rerender(); });
      inspector.appendChild(labeled("Component", sel));
      inspector.appendChild(textField("Store under key (optional)", node.key || "", function (v) { node.key = slugify(v) || undefined; save(); }));
    }
  }

  function allowedToggle(node, key, label) {
    var w = el("div", { class: "rcms-field rcms-checkbox-field" });
    var inp = el("input", { type: "checkbox" }); inp.checked = node.allowed.indexOf(key) >= 0;
    inp.addEventListener("change", function () {
      var i = node.allowed.indexOf(key);
      if (inp.checked && i < 0) node.allowed.push(key);
      else if (!inp.checked && i >= 0) node.allowed.splice(i, 1);
      save();
    });
    w.appendChild(inp); w.appendChild(el("label", { text: label }));
    return w;
  }

  function optionsEditor(node) {
    var box = el("div", { class: "rcms-field" }, [el("label", { text: "Options (value = label)" })]);
    (node.options || []).forEach(function (opt, i) {
      var row = el("div", { class: "rcms-flex rcms-gap-1 rcms-mb-1" });
      var v = el("input", { type: "text", value: opt.value, placeholder: "value", style: "flex:1" });
      var l = el("input", { type: "text", value: opt.label, placeholder: "label", style: "flex:1" });
      v.addEventListener("input", function () { opt.value = v.value; save(); });
      l.addEventListener("input", function () { opt.label = l.value; save(); });
      row.appendChild(v); row.appendChild(l);
      row.appendChild(iconBtn("delete", function () { node.options.splice(i, 1); renderInspector(); save(); }, "rcms-text-danger"));
      box.appendChild(row);
    });
    box.appendChild(el("button", { class: "rcms-btn rcms-btn-text rcms-btn-small", type: "button",
      html: '<span class="material-symbols-rounded sm">add</span> Option',
      onclick: function () { node.options.push({ value: "", label: "" }); renderInspector(); save(); } }));
    return box;
  }

  function rulesEditor(node) {
    var box = el("div", { class: "rcms-field" }, [el("label", { text: "Show only when (conditional)" })]);
    node.rules = node.rules || [];
    node.rules.forEach(function (rule, ri) {
      rule.action = rule.action || "show";
      rule.conditions = rule.conditions || [{ field: "", op: "eq", value: "" }];
      var c = rule.conditions[0];
      var row = el("div", { class: "rcms-flex rcms-gap-1 rcms-items-center rcms-mb-1 rcms-flex-wrap" });
      var f = el("input", { type: "text", value: c.field, placeholder: "field key", style: "flex:1 1 90px" });
      var op = el("select");
      ["eq", "ne", "contains", "gt", "lt", "empty", "not_empty"].forEach(function (o) {
        var opt = el("option", { value: o, text: o }); if (o === c.op) opt.selected = true; op.appendChild(opt);
      });
      var val = el("input", { type: "text", value: c.value, placeholder: "value", style: "flex:1 1 80px" });
      f.addEventListener("input", function () { c.field = f.value; save(); });
      op.addEventListener("change", function () { c.op = op.value; save(); });
      val.addEventListener("input", function () { c.value = val.value; save(); });
      row.appendChild(f); row.appendChild(op); row.appendChild(val);
      row.appendChild(iconBtn("delete", function () { node.rules.splice(ri, 1); renderInspector(); save(); }, "rcms-text-danger"));
      box.appendChild(row);
    });
    box.appendChild(el("button", { class: "rcms-btn rcms-btn-text rcms-btn-small", type: "button",
      html: '<span class="material-symbols-rounded sm">rule</span> Add rule',
      onclick: function () { node.rules.push({ match: "all", action: "show", conditions: [{ field: "", op: "eq", value: "" }] }); renderInspector(); save(); } }));
    return box;
  }

  // Backfill ids on any node missing one (documents authored before ids).
  (function backfillIds(list) {
    list.forEach(function (n) {
      if (!n.id) n.id = uid();
      if (n.children) backfillIds(n.children);
      if (n.item && n.item.children) backfillIds(n.item.children);
      if (n.groups) n.groups.forEach(function (g) { backfillIds(g.children || []); });
    });
  })(state);

  rerender();
})();
