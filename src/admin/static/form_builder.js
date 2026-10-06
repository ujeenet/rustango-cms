/* Visual Form Builder editor (epic #533, FB-04..FB-08).
 *
 * Vanilla JS, no framework. Unlike the block stream editor (which scrapes
 * the DOM), this is render-from-state: the form schema is the single source
 * of truth, the DOM is rendered from it, and every mutation updates the
 * state, re-renders, and serializes back into the hidden `#form-schema`
 * input that the build POST persists into the snippet's `rcms-data` column.
 *
 * Preview is NOT here: the sidebar pane is the shared one
 * (`_preview_pane.html` + `cms-preview.js`), which POSTs `#form-schema` to
 * the form preview endpoint and renders through the same Rust renderer the
 * public site uses. This file used to draw its own preview in JS, which
 * meant the layout existed twice in two languages and could only agree by
 * coincidence.
 *
 * Schema shape (mirrors src/forms/schema.rs):
 *   { settings:{...}, pages:[ { id,label, sections:[ { id,title,
 *     rows:[ { id, columns:[ { id,width, fields:[ Field ] } ] } ] } ] } ] }
 */
(function () {
  "use strict";

  var root = document.getElementById("form-builder-root");
  if (!root) return;

  var hidden = document.getElementById("form-schema");
  var elTabs = document.getElementById("fb-pagetabs");
  var elCanvas = document.getElementById("fb-canvas");
  var elInspector = document.getElementById("fb-inspector");

  // ---- field-type catalog (keep in sync with schema::FieldType) ----
  var FIELD_TYPES = [
    { type: "text", label: "Text", icon: "short_text", group: "Basic" },
    { type: "textarea", label: "Paragraph text", icon: "notes", group: "Basic" },
    { type: "email", label: "Email", icon: "alternate_email", group: "Basic" },
    { type: "tel", label: "Phone", icon: "call", group: "Basic" },
    { type: "url", label: "URL", icon: "link", group: "Basic" },
    { type: "number", label: "Number", icon: "tag", group: "Basic" },
    { type: "date", label: "Date", icon: "calendar_today", group: "Basic" },
    { type: "datetime", label: "Date & time", icon: "schedule", group: "Basic" },
    { type: "time", label: "Time", icon: "schedule", group: "Basic" },
    { type: "select", label: "Dropdown", icon: "arrow_drop_down_circle", group: "Choice" },
    { type: "radio", label: "Radio", icon: "radio_button_checked", group: "Choice" },
    { type: "checkbox", label: "Checkbox", icon: "check_box", group: "Choice" },
    { type: "checkboxes", label: "Checkbox group", icon: "checklist", group: "Choice" },
    { type: "multiselect", label: "Multi-select", icon: "list", group: "Choice" },
    { type: "rating", label: "Rating", icon: "star", group: "Choice" },
    { type: "file", label: "File upload", icon: "attach_file", group: "Advanced" },
    { type: "hidden", label: "Hidden", icon: "visibility_off", group: "Advanced" },
    { type: "static", label: "Heading / text", icon: "title", group: "Layout" },
    { type: "richtext", label: "Rich text", icon: "article", group: "Layout" },
  ];
  function typeMeta(t) {
    for (var i = 0; i < FIELD_TYPES.length; i++) if (FIELD_TYPES[i].type === t) return FIELD_TYPES[i];
    return { type: t, label: t, icon: "help" };
  }
  var CHOICE_TYPES = { select: 1, radio: 1, checkboxes: 1, multiselect: 1 };
  var PRESENTATION_TYPES = { static: 1, richtext: 1 };

  // ---- state ----
  var state = parseInitial();
  var current = 0; // active page index
  var selectedId = null; // selected field id

  function parseInitial() {
    var raw = (hidden && hidden.value) || "";
    var data = {};
    try {
      data = raw.trim() ? JSON.parse(raw) : {};
    } catch (e) {
      data = {};
    }
    if (!data || typeof data !== "object") data = {};
    if (!data.settings || typeof data.settings !== "object") data.settings = {};
    if (!Array.isArray(data.pages)) data.pages = [];
    if (data.pages.length === 0) data.pages.push(newPage("Page 1"));
    // backfill ids so the editor can address every node
    data.pages.forEach(function (p) {
      p.id = p.id || uid("p");
      if (!Array.isArray(p.sections)) p.sections = [];
      p.sections.forEach(function (s) {
        s.id = s.id || uid("s");
        if (!Array.isArray(s.rows)) s.rows = [];
        s.rows.forEach(function (r) {
          r.id = r.id || uid("r");
          if (!Array.isArray(r.columns)) r.columns = [];
          r.columns.forEach(function (c) {
            c.id = c.id || uid("c");
            if (!c.width) c.width = 12;
            if (!Array.isArray(c.fields)) c.fields = [];
            c.fields.forEach(function (f) {
              f.id = f.id || uid("f");
            });
          });
        });
      });
    });
    return data;
  }

  function uid(p) {
    var rnd =
      window.crypto && window.crypto.randomUUID
        ? window.crypto.randomUUID().slice(0, 8)
        : Math.floor((1 + Math.random()) * 0x100000000).toString(16);
    return (p || "id") + "_" + rnd;
  }

  function newPage(label) {
    return { id: uid("p"), label: label || "Page", sections: [newSection()] };
  }
  function newSection() {
    return { id: uid("s"), title: "", rows: [newRow()] };
  }
  function newRow() {
    return { id: uid("r"), columns: [newColumn(12)] };
  }
  function newColumn(w) {
    return { id: uid("c"), width: w || 12, fields: [] };
  }
  function newField(type) {
    var f = { id: uid("f"), type: type, label: defaultLabel(type), key: "", required: false };
    if (CHOICE_TYPES[type]) {
      f.options = [
        { value: "1", label: "Option one" },
        { value: "2", label: "Option two" },
      ];
    }
    if (PRESENTATION_TYPES[type]) {
      f.label = "";
      f.content = type === "static" ? "Section heading" : "<p>Rich text…</p>";
    } else {
      f.key = autoKey(f.label, type);
    }
    if (type === "rating") f.scale = 5;
    return f;
  }
  function defaultLabel(type) {
    var m = typeMeta(type);
    return PRESENTATION_TYPES[type] ? "" : m.label;
  }

  function autoKey(label, type) {
    var base = (label || type || "field")
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "_")
      .replace(/^_+|_+$/g, "");
    if (!base) base = "field";
    if (!/^[a-z]/.test(base)) base = "f_" + base;
    // ensure uniqueness across the whole form
    var used = {};
    eachField(function (f) {
      if (f.key) used[f.key] = 1;
    });
    if (!used[base]) return base;
    var i = 2;
    while (used[base + "_" + i]) i++;
    return base + "_" + i;
  }

  // ---- traversal helpers ----
  function eachField(fn) {
    state.pages.forEach(function (p) {
      p.sections.forEach(function (s) {
        s.rows.forEach(function (r) {
          r.columns.forEach(function (c) {
            c.fields.forEach(fn);
          });
        });
      });
    });
  }
  function findColumn(colId) {
    var found = null;
    state.pages.forEach(function (p) {
      p.sections.forEach(function (s) {
        s.rows.forEach(function (r) {
          r.columns.forEach(function (c) {
            if (c.id === colId) found = c;
          });
        });
      });
    });
    return found;
  }
  function findField(fieldId) {
    var out = null;
    eachField(function (f) {
      if (f.id === fieldId) out = f;
    });
    return out;
  }

  // ---- persistence ----
  function save() {
    if (hidden) hidden.value = JSON.stringify(state);
  }

  function rerender() {
    save();
    renderTabs();
    renderCanvas();
    renderInspector();
  }

  // ---- DOM helpers ----
  function el(tag, attrs, kids) {
    var n = document.createElement(tag);
    if (attrs)
      Object.keys(attrs).forEach(function (k) {
        if (k === "class") n.className = attrs[k];
        else if (k === "html") n.innerHTML = attrs[k];
        else if (k === "text") n.textContent = attrs[k];
        else if (k.slice(0, 2) === "on") n.addEventListener(k.slice(2), attrs[k]);
        else if (attrs[k] != null) n.setAttribute(k, attrs[k]);
      });
    (kids || []).forEach(function (c) {
      if (c == null) return;
      n.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    });
    return n;
  }
  function icon(name) {
    return el("span", { class: "material-symbols-rounded sm", text: name });
  }
  function iconBtn(name, title, onclick, cls) {
    return el("button", {
      type: "button",
      class: "rcms-fb-icon-btn" + (cls ? " " + cls : ""),
      title: title,
      "aria-label": title,
      onclick: onclick,
    }, [icon(name)]);
  }

  // ---- page tabs ----
  function renderTabs() {
    elTabs.innerHTML = "";
    state.pages.forEach(function (p, i) {
      var tab = el("button", {
        type: "button",
        class: "rcms-fb-tab" + (i === current ? " active" : ""),
        onclick: function () {
          current = i;
          selectedId = null;
          rerender();
        },
      }, [p.label || "Page " + (i + 1)]);
      elTabs.appendChild(tab);
    });
    elTabs.appendChild(
      iconBtn("add", "Add page", function () {
        state.pages.push(newPage("Page " + (state.pages.length + 1)));
        current = state.pages.length - 1;
        rerender();
      }, "rcms-fb-tab-add")
    );
  }

  // ---- canvas ----
  function renderCanvas() {
    elCanvas.innerHTML = "";
    var page = state.pages[current];
    if (!page) return;

    // page header: rename + move + delete
    var head = el("div", { class: "rcms-fb-page-head" }, [
      el("input", {
        class: "rcms-fb-page-name",
        type: "text",
        value: page.label || "",
        placeholder: "Page name",
        oninput: function (e) {
          page.label = e.target.value;
          save();
          renderTabs();
        },
      }),
      el("div", { class: "rcms-fb-page-tools" }, [
        iconBtn("chevron_left", "Move page left", function () {
          if (current > 0) {
            swap(state.pages, current, current - 1);
            current--;
            rerender();
          }
        }),
        iconBtn("chevron_right", "Move page right", function () {
          if (current < state.pages.length - 1) {
            swap(state.pages, current, current + 1);
            current++;
            rerender();
          }
        }),
        iconBtn("delete", "Delete page", function () {
          if (state.pages.length <= 1) return alertMsg("A form needs at least one page.");
          if (!confirm("Delete this page and its fields?")) return;
          state.pages.splice(current, 1);
          current = Math.max(0, current - 1);
          rerender();
        }, "rcms-fb-danger"),
      ]),
    ]);
    elCanvas.appendChild(head);

    page.sections.forEach(function (section, si) {
      elCanvas.appendChild(renderSection(page, section, si));
    });

    elCanvas.appendChild(
      el("button", {
        type: "button",
        class: "rcms-fb-add-section",
        onclick: function () {
          page.sections.push(newSection());
          rerender();
        },
      }, [icon("add"), "Add section"])
    );
  }

  function renderSection(page, section, si) {
    var box = el("section", { class: "rcms-fb-section" });
    box.appendChild(
      el("div", { class: "rcms-fb-section-head" }, [
        el("input", {
          class: "rcms-fb-section-title",
          type: "text",
          value: section.title || "",
          placeholder: "Section title (optional)",
          oninput: function (e) {
            section.title = e.target.value;
            save();
          },
        }),
        el("div", { class: "rcms-fb-section-tools" }, [
          iconBtn("keyboard_arrow_up", "Move up", function () {
            if (si > 0) {
              swap(page.sections, si, si - 1);
              rerender();
            }
          }),
          iconBtn("keyboard_arrow_down", "Move down", function () {
            if (si < page.sections.length - 1) {
              swap(page.sections, si, si + 1);
              rerender();
            }
          }),
          iconBtn("delete", "Delete section", function () {
            if (!confirm("Delete this section?")) return;
            page.sections.splice(si, 1);
            rerender();
          }, "rcms-fb-danger"),
        ]),
      ])
    );

    section.rows.forEach(function (row, ri) {
      box.appendChild(renderRow(section, row, ri));
    });

    box.appendChild(
      el("button", {
        type: "button",
        class: "rcms-fb-add-row",
        onclick: function () {
          section.rows.push(newRow());
          rerender();
        },
      }, [icon("add"), "Add row"])
    );
    return box;
  }

  function renderRow(section, row, ri) {
    var box = el("div", { class: "rcms-fb-row" });
    var grid = el("div", { class: "rcms-fb-row-grid" });
    row.columns.forEach(function (col, ci) {
      grid.appendChild(renderColumn(row, col, ci));
    });
    box.appendChild(grid);
    box.appendChild(
      el("div", { class: "rcms-fb-row-tools" }, [
        iconBtn("view_column", "Add column", function () {
          row.columns.push(newColumn(rebalance(row)));
          rerender();
        }),
        iconBtn("keyboard_arrow_up", "Move row up", function () {
          if (ri > 0) {
            swap(section.rows, ri, ri - 1);
            rerender();
          }
        }),
        iconBtn("keyboard_arrow_down", "Move row down", function () {
          if (ri < section.rows.length - 1) {
            swap(section.rows, ri, ri + 1);
            rerender();
          }
        }),
        iconBtn("delete", "Delete row", function () {
          if (!confirm("Delete this row?")) return;
          section.rows.splice(ri, 1);
          rerender();
        }, "rcms-fb-danger"),
      ])
    );
    return box;
  }

  function rebalance(row) {
    // suggest a width for a newly-added column so the row stays ~12 wide
    var n = row.columns.length + 1;
    return Math.max(1, Math.floor(12 / n));
  }

  function renderColumn(row, col, ci) {
    var box = el("div", { class: "rcms-fb-col" });
    box.style.flex = col.width + " 0 0%";
    box.setAttribute("data-col", col.id);

    box.appendChild(
      el("div", { class: "rcms-fb-col-head" }, [
        widthControl(col),
        el("div", { class: "rcms-fb-col-tools" }, [
          iconBtn("delete", "Delete column", function () {
            if (row.columns.length <= 1) return alertMsg("A row needs at least one column.");
            if (col.fields.length && !confirm("Delete this column and its fields?")) return;
            row.columns.splice(ci, 1);
            rerender();
          }, "rcms-fb-danger"),
        ]),
      ])
    );

    col.fields.forEach(function (field, fi) {
      box.appendChild(renderFieldCard(col, field, fi));
    });

    box.appendChild(addFieldControl(col));
    return box;
  }

  function widthControl(col) {
    var sel = el("select", {
      class: "rcms-fb-width",
      title: "Column width (out of 12)",
      onchange: function (e) {
        col.width = parseInt(e.target.value, 10) || 12;
        rerender();
      },
    });
    for (var w = 1; w <= 12; w++) {
      var o = el("option", { value: w, text: w + "/12" });
      if (w === col.width) o.selected = true;
      sel.appendChild(o);
    }
    return sel;
  }

  function renderFieldCard(col, field, fi) {
    var m = typeMeta(field.type);
    var card = el("div", {
      class: "rcms-fb-field" + (field.id === selectedId ? " selected" : ""),
      "data-field": field.id,
      onclick: function (e) {
        if (e.target.closest(".rcms-fb-field-tools")) return;
        selectedId = field.id;
        renderCanvas();
        renderInspector();
      },
    }, [
      el("div", { class: "rcms-fb-field-main" }, [
        icon(m.icon),
        el("div", { class: "fb-field-text" }, [
          el("div", { class: "rcms-fb-field-label" }, [
            PRESENTATION_TYPES[field.type]
              ? (field.content ? stripHtml(field.content).slice(0, 40) || m.label : m.label)
              : (field.label || "(untitled)"),
          ]),
          el("div", { class: "rcms-fb-field-meta" }, [
            m.label + (field.key ? " · " + field.key : "") + (field.required ? " · required" : ""),
          ]),
        ]),
      ]),
      el("div", { class: "rcms-fb-field-tools" }, [
        iconBtn("keyboard_arrow_up", "Move up", function () {
          if (fi > 0) {
            swap(col.fields, fi, fi - 1);
            rerender();
          }
        }),
        iconBtn("keyboard_arrow_down", "Move down", function () {
          if (fi < col.fields.length - 1) {
            swap(col.fields, fi, fi + 1);
            rerender();
          }
        }),
        iconBtn("delete", "Delete field", function () {
          if (!confirm("Delete this field?")) return;
          if (selectedId === field.id) selectedId = null;
          col.fields.splice(fi, 1);
          rerender();
        }, "rcms-fb-danger"),
      ]),
    ]);
    return card;
  }

  function addFieldControl(col) {
    var wrap = el("div", { class: "rcms-fb-add-field-wrap" });
    var btn = el("button", {
      type: "button",
      class: "rcms-fb-add-field",
      onclick: function () {
        menu.hidden = !menu.hidden;
      },
    }, [icon("add"), "Add field"]);
    var menu = el("div", { class: "rcms-fb-palette", hidden: "" });
    var groups = {};
    FIELD_TYPES.forEach(function (t) {
      (groups[t.group] = groups[t.group] || []).push(t);
    });
    Object.keys(groups).forEach(function (g) {
      menu.appendChild(el("div", { class: "rcms-fb-palette-group" }, [g]));
      groups[g].forEach(function (t) {
        menu.appendChild(
          el("button", {
            type: "button",
            class: "rcms-fb-palette-item",
            onclick: function () {
              var f = newField(t.type);
              col.fields.push(f);
              selectedId = f.id;
              rerender();
            },
          }, [icon(t.icon), t.label])
        );
      });
    });
    wrap.appendChild(btn);
    wrap.appendChild(menu);
    return wrap;
  }

  // ---- inspector (property panel) ----
  function renderInspector() {
    elInspector.innerHTML = "";
    var field = selectedId ? findField(selectedId) : null;
    if (!field) {
      elInspector.appendChild(
        el("div", { class: "rcms-fb-inspector-empty" }, [
          icon("touch_app"),
          el("p", {}, ["Select a field to edit its properties."]),
        ])
      );
      return;
    }
    var m = typeMeta(field.type);
    elInspector.appendChild(el("div", { class: "rcms-fb-inspector-head" }, [icon(m.icon), m.label]));

    function row(labelText, control) {
      return el("label", { class: "rcms-fb-prop" }, [el("span", { class: "rcms-fb-prop-label" }, [labelText]), control]);
    }
    function textInput(val, on, ph) {
      return el("input", { type: "text", value: val || "", placeholder: ph || "", oninput: on });
    }

    if (PRESENTATION_TYPES[field.type]) {
      elInspector.appendChild(
        row("Content", el("textarea", {
          rows: "4",
          value: field.content || "",
          oninput: function (e) {
            field.content = e.target.value;
            save();
            renderCanvas();
          },
        }))
      );
      // textarea value attr doesn't set content; set it explicitly
      var ta = elInspector.querySelector("textarea");
      if (ta) ta.value = field.content || "";
      return;
    }

    elInspector.appendChild(
      row("Label", textInput(field.label, function (e) {
        var wasAuto = field.key === autoKeyFrom(field.label, field);
        field.label = e.target.value;
        if (wasAuto || !field.key) field.key = autoKey(field.label, field.type);
        save();
        renderCanvas();
        syncKeyInput();
      }))
    );

    var keyInput = textInput(field.key, function (e) {
      field.key = e.target.value;
      save();
      renderCanvas();
    });
    keyInput.id = "fb-key-input";
    elInspector.appendChild(row("Key", keyInput));
    function syncKeyInput() {
      var k = document.getElementById("fb-key-input");
      if (k) k.value = field.key;
    }

    elInspector.appendChild(
      row("Help text", textInput(field.help, function (e) {
        field.help = e.target.value;
        save();
      }))
    );

    if (field.type !== "checkbox") {
      elInspector.appendChild(
        row("Placeholder", textInput(field.placeholder, function (e) {
          field.placeholder = e.target.value;
          save();
        }))
      );
    }

    if (
      ["text", "textarea", "email", "url", "tel", "number", "date", "datetime", "time", "hidden", "select", "radio"].indexOf(
        field.type
      ) >= 0
    ) {
      elInspector.appendChild(
        row("Default value", textInput(field.default, function (e) {
          field.default = e.target.value;
          save();
        }))
      );
    }

    // required
    var reqWrap = el("label", { class: "rcms-fb-prop rcms-fb-prop-check" }, [
      el("input", {
        type: "checkbox",
        onchange: function (e) {
          field.required = e.target.checked;
          save();
          renderCanvas();
        },
      }),
      el("span", {}, ["Required"]),
    ]);
    reqWrap.querySelector("input").checked = !!field.required;
    elInspector.appendChild(reqWrap);

    // number config
    if (field.type === "number") {
      elInspector.appendChild(row("Min", numInput(field.min, function (v) { field.min = v; })));
      elInspector.appendChild(row("Max", numInput(field.max, function (v) { field.max = v; })));
      elInspector.appendChild(row("Step", numInput(field.step, function (v) { field.step = v; })));
    }
    if (field.type === "rating") {
      elInspector.appendChild(
        row("Scale", numInput(field.scale, function (v) { field.scale = v || 5; renderCanvas(); }))
      );
    }
    if (field.type === "file") {
      elInspector.appendChild(
        row("Accept", textInput(field.accept, function (e) { field.accept = e.target.value; save(); }, ".pdf,.png"))
      );
    }

    // FB-13 — text validation rules.
    if (["text", "textarea", "tel"].indexOf(field.type) >= 0) {
      elInspector.appendChild(
        row("Min length", numInput(field.min_length, function (v) {
          field.min_length = v == null ? null : Math.max(0, Math.round(v));
        }))
      );
      elInspector.appendChild(
        row("Max length", numInput(field.max_length, function (v) {
          field.max_length = v == null ? null : Math.max(0, Math.round(v));
        }))
      );
      elInspector.appendChild(
        row("Pattern (regex)", textInput(field.pattern, function (e) {
          field.pattern = e.target.value;
          save();
        }))
      );
    }

    if (CHOICE_TYPES[field.type]) {
      elInspector.appendChild(renderOptionsEditor(field));
    }

    elInspector.appendChild(renderRulesEditor(field));
  }

  // ---- conditional logic editor (FB-14) ----
  // opts: plain values, or [value, label] pairs.
  function selectEl(opts, cur, on) {
    var s = el("select", { onchange: function (e) { on(e.target.value); } });
    opts.forEach(function (o) {
      var v = Array.isArray(o) ? o[0] : o;
      var op = el("option", { value: v, text: Array.isArray(o) ? o[1] : o });
      if (v === cur) op.selected = true;
      s.appendChild(op);
    });
    return s;
  }
  var RULE_ACTIONS = [["show", "Show"], ["hide", "Hide"]];
  var RULE_MATCH = [["all", "all"], ["any", "any"]];
  var RULE_OPS = [["eq", "is"], ["ne", "is not"], ["contains", "contains"], ["not_empty", "is answered"], ["empty", "is not answered"]];
  function fieldByKey(key) {
    var found = null;
    eachField(function (ff) { if (ff.key === key) found = ff; });
    return found;
  }
  // The value box of a condition: the field's own options when it has
  // them (the stored value, shown by its label), else free text.
  function conditionValueInput(c) {
    var src = fieldByKey(c.field);
    var opts = null;
    if (src && Array.isArray(src.options) && src.options.length && ["select", "radio", "checkboxes", "multiselect"].indexOf(src.type) >= 0) {
      opts = src.options.map(function (o) { return [o.value, o.label || o.value]; });
    } else if (src && src.type === "checkbox") {
      opts = [["yes", "ticked"]];
    }
    if (!opts) {
      return el("input", { type: "text", value: c.value || "", placeholder: "value", oninput: function (e) { c.value = e.target.value; save(); } });
    }
    if (c.value && !opts.some(function (o) { return o[0] === c.value; })) opts.unshift([c.value, c.value]);
    if (!c.value) { c.value = opts[0][0]; save(); }
    return selectEl(opts, c.value, function (v) { c.value = v; save(); });
  }
  function fieldKeySelect(cur, on) {
    var s = el("select", { onchange: function (e) { on(e.target.value); } });
    s.appendChild(el("option", { value: "", text: "— field —" }));
    eachField(function (ff) {
      if (ff.key && ff.id !== selectedId) {
        var op = el("option", { value: ff.key, text: ff.label ? ff.label + " (" + ff.key + ")" : ff.key });
        if (ff.key === cur) op.selected = true;
        s.appendChild(op);
      }
    });
    return s;
  }
  function renderRulesEditor(field) {
    if (!Array.isArray(field.rules)) field.rules = [];
    var wrap = el("div", { class: "rcms-fb-rules" }, [el("div", { class: "rcms-fb-prop-label" }, ["Conditional logic"])]);
    field.rules.forEach(function (rule, ri) {
      if (!Array.isArray(rule.conditions)) rule.conditions = [];
      var rd = el("div", { class: "rcms-fb-rule" });
      rd.appendChild(el("div", { class: "rcms-fb-rule-head" }, [
        selectEl(RULE_ACTIONS, rule.action || "show", function (v) { rule.action = v; save(); }),
        el("span", {}, [" this field when "]),
        selectEl(RULE_MATCH, rule.match || "all", function (v) { rule.match = v; save(); }),
        el("span", {}, [" of these are true:"]),
        iconBtn("close", "Remove rule", function () { field.rules.splice(ri, 1); renderInspector(); save(); }, "rcms-fb-danger"),
      ]));
      rule.conditions.forEach(function (c, ci) {
        var needsValue = c.op !== "empty" && c.op !== "not_empty";
        rd.appendChild(el("div", { class: "rcms-fb-cond" }, [
          // A new field or operator can change the value box: redraw.
          fieldKeySelect(c.field, function (v) { c.field = v; c.value = ""; save(); renderInspector(); }),
          selectEl(RULE_OPS, c.op || "eq", function (v) { c.op = v; save(); renderInspector(); }),
          needsValue ? conditionValueInput(c) : el("span", {}, []),
          iconBtn("close", "Remove condition", function () { rule.conditions.splice(ci, 1); renderInspector(); save(); }, "rcms-fb-danger"),
        ]));
      });
      rd.appendChild(el("button", { type: "button", class: "rcms-fb-add-option", onclick: function () { rule.conditions.push({ field: "", op: "eq", value: "" }); renderInspector(); } }, [icon("add"), "Add condition"]));
      wrap.appendChild(rd);
    });
    wrap.appendChild(el("button", { type: "button", class: "rcms-fb-add-option", onclick: function () { field.rules.push({ action: "show", match: "all", conditions: [{ field: "", op: "eq", value: "" }] }); renderInspector(); } }, [icon("add"), "Add rule"]));
    return wrap;
  }

  function numInput(val, set) {
    return el("input", {
      type: "number",
      value: val == null ? "" : val,
      oninput: function (e) {
        var v = e.target.value === "" ? null : parseFloat(e.target.value);
        set(v);
        save();
      },
    });
  }

  // recompute what the auto-key WOULD be for a label (to detect manual edits)
  function autoKeyFrom(label, field) {
    var base = (label || "").toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "");
    if (!base) return field.key;
    if (!/^[a-z]/.test(base)) base = "f_" + base;
    return base;
  }

  function renderOptionsEditor(field) {
    if (!Array.isArray(field.options)) field.options = [];
    var wrap = el("div", { class: "rcms-fb-options" }, [el("div", { class: "rcms-fb-prop-label" }, ["Options"])]);
    field.options.forEach(function (opt, i) {
      wrap.appendChild(
        el("div", { class: "rcms-fb-option" }, [
          el("input", {
            type: "text", value: opt.label || "", placeholder: "Label",
            oninput: function (e) {
              opt.label = e.target.value;
              if (!opt._valueLocked) opt.value = e.target.value;
              save();
            },
          }),
          el("input", {
            type: "text", value: opt.value || "", placeholder: "Value", class: "rcms-fb-option-value",
            oninput: function (e) {
              opt.value = e.target.value;
              opt._valueLocked = true;
              save();
            },
          }),
          iconBtn("close", "Remove option", function () {
            field.options.splice(i, 1);
            renderInspector();
            save();
          }, "rcms-fb-danger"),
        ])
      );
    });
    wrap.appendChild(
      el("button", {
        type: "button", class: "rcms-fb-add-option",
        onclick: function () {
          field.options.push({ value: "", label: "" });
          renderInspector();
        },
      }, [icon("add"), "Add option"])
    );
    return wrap;
  }

  // ---- preview ----


  // ---- misc ----
  function swap(arr, a, b) {
    var t = arr[a];
    arr[a] = arr[b];
    arr[b] = t;
  }
  function stripHtml(s) {
    var d = document.createElement("div");
    d.innerHTML = s || "";
    return d.textContent || "";
  }
  function escapeHtml(s) {
    return (s || "").replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }
  function alertMsg(m) {
    if (window.rcmsToast) window.rcmsToast({ level: "warning", body: m });
    else alert(m);
  }

  // ---- settings panel (FB-16) ----
  var elSettings = document.getElementById("fb-settings");
  function renderSettings() {
    if (!elSettings) return;
    if (!state.settings || typeof state.settings !== "object") state.settings = {};
    var s = state.settings;
    elSettings.innerHTML = "";
    elSettings.appendChild(el("h3", {}, ["Form settings"]));
    function field(labelText, key, ph, kind) {
      var ctrl;
      if (kind === "textarea") {
        ctrl = el("textarea", { rows: "3", placeholder: ph || "" });
        ctrl.value = s[key] || "";
      } else {
        ctrl = el("input", { type: "text", value: s[key] || "", placeholder: ph || "" });
      }
      ctrl.addEventListener("input", function (e) {
        s[key] = e.target.value;
        save();
      });
      return el("label", { class: "rcms-fb-prop" }, [el("span", { class: "rcms-fb-prop-label" }, [labelText]), ctrl]);
    }
    elSettings.appendChild(field("Submit button label", "submit_label", "Submit"));
    elSettings.appendChild(field("Success message (default)", "success_message", "Thanks! Your submission was received.", "textarea"));
    elSettings.appendChild(field("Success redirect URL (default)", "redirect_url", "/thank-you"));
    elSettings.appendChild(field("Notification emails", "notify_emails", "alice@example.com, bob@example.com"));
    elSettings.appendChild(field("Error message", "error_message", "Your answers could not be sent. Please check the highlighted question and try again.", "textarea"));
    // Multi-step forms only; empty keeps the default shown in grey.
    elSettings.appendChild(field("Next button label", "next_label", "Next"));
    elSettings.appendChild(field("Back button label", "back_label", "Back"));
    elSettings.appendChild(field("Progress line ({n} = step, {total} = steps)", "progress_label", "Step {n} of {total}"));

    // Built-in styles toggle (default on). Off → the public form omits the
    // bundled /forms/forms.css so the host fully owns the styling.
    var cssWrap = el("label", { class: "rcms-fb-prop rcms-fb-prop-check" }, [
      el("input", {
        type: "checkbox",
        onchange: function (e) {
          s.builtin_css = e.target.checked;
          save();
        },
      }),
      el("span", {}, ["Use built-in form styles"]),
    ]);
    cssWrap.querySelector("input").checked = s.builtin_css !== false;
    elSettings.appendChild(cssWrap);

    elSettings.appendChild(el("p", { class: "rcms-fb-prop-label" }, ["A form-embed block can override the success message/redirect per page."]));
  }

  var settingsBtn = document.getElementById("fb-settings-toggle");
  if (settingsBtn) {
    settingsBtn.addEventListener("click", function () {
      var show = elSettings.hidden;
      if (show) renderSettings();
      elSettings.hidden = !show;
      document.querySelector(".rcms-fb-workspace").hidden = show;
      settingsBtn.classList.toggle("active", show);
    });
  }


  // serialize one last time before the save POST
  var formEl = document.getElementById("form-builder-form");
  if (formEl) formEl.addEventListener("submit", save);

  // close palettes on outside click
  document.addEventListener("click", function (e) {
    if (!e.target.closest(".rcms-fb-add-field-wrap")) {
      var open = elCanvas.querySelectorAll(".rcms-fb-palette:not([hidden])");
      open.forEach(function (m) { m.hidden = true; });
    }
  });

  // boot
  rerender();
})();
