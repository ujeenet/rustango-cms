// #294 — TipTap WYSIWYG for RichText widget fields.
//
// Progressive enhancement: every `<textarea data-widget-mode="richtext">`
// (emitted by _widget.html for the `RichText` widget kind) is replaced by a
// TipTap editor. The textarea stays in the DOM as the hidden form value —
// the editor's HTML is synced into it on every change, so the existing POST
// + server-side sanitization path is unchanged.
//
// The editor engine (headless TipTap, MIT) is the vendored
// `vendor/tiptap.bundle.js`, which exposes `window.RcmsRichtext.create(...)`.
// The toolbar UI + wiring below are hand-authored (no inline styles — see the
// `.rcms-richtext-*` classes in cms.css).
//
// Slice-1 scope (#294): bold / italic / strike / inline-code, h2–h4,
// bullet + ordered lists, blockquote, plain links, undo/redo. Move-safe
// `linktype` internal-link nodes, stream-block lifecycle, and the extended
// toolbar (tables/images/embeds) are tracked follow-ups.
(function () {
  "use strict";

  // The TipTap bundle is served immutable, so it carries this script's own
  // `?v=` build token: a new release is a new URL (#696). Read now —
  // `currentScript` is only set while the script first runs.
  const ASSET_VERSION = document.currentScript
    ? new URL(document.currentScript.src, location.href).search
    : "";

  const TOOLBAR = [
    { cmd: "toggleBold", icon: "format_bold", title: "Bold", active: "bold" },
    { cmd: "toggleItalic", icon: "format_italic", title: "Italic", active: "italic" },
    { cmd: "toggleStrike", icon: "strikethrough_s", title: "Strikethrough", active: "strike" },
    { cmd: "toggleCode", icon: "code", title: "Inline code", active: "code" },
    { sep: true },
    { cmd: "toggleHeading", arg: { level: 2 }, icon: "title", title: "Heading 2", active: "heading", activeArg: { level: 2 } },
    { cmd: "toggleHeading", arg: { level: 3 }, label: "H3", title: "Heading 3", active: "heading", activeArg: { level: 3 } },
    { cmd: "toggleHeading", arg: { level: 4 }, label: "H4", title: "Heading 4", active: "heading", activeArg: { level: 4 } },
    { sep: true },
    { cmd: "toggleBulletList", icon: "format_list_bulleted", title: "Bullet list", active: "bulletList" },
    { cmd: "toggleOrderedList", icon: "format_list_numbered", title: "Numbered list", active: "orderedList" },
    { cmd: "toggleBlockquote", icon: "format_quote", title: "Quote", active: "blockquote" },
    { cmd: "toggleCodeBlock", icon: "code_blocks", title: "Code block", active: "codeBlock" },
    { cmd: "setHorizontalRule", icon: "horizontal_rule", title: "Divider" },
    { sep: true },
    { fn: "setLink", icon: "link", title: "Insert / edit external link" },
    { fn: "linkInternal", kind: "page", linktype: "page", icon: "article", title: "Link to a page" },
    { fn: "linkInternal", kind: "document", linktype: "media", icon: "attachment", title: "Link to a document" },
    { fn: "linkEmail", icon: "mail", title: "Insert email link (mailto:)" },
    { fn: "linkAnchor", icon: "tag", title: "Link to an on-page anchor (#id)" },
    { fn: "unsetLink", icon: "link_off", title: "Remove link" },
    { sep: true },
    { cmd: "insertTable", arg: { rows: 3, cols: 3, withHeaderRow: true }, icon: "grid_on", title: "Insert table" },
    { cmd: "addRowAfter", icon: "add_row_below", title: "Add row" },
    { cmd: "addColumnAfter", icon: "add_column_right", title: "Add column" },
    { cmd: "deleteRow", label: "−R", title: "Delete row" },
    { cmd: "deleteColumn", label: "−C", title: "Delete column" },
    { cmd: "deleteTable", icon: "grid_off", title: "Delete table" },
    { sep: true },
    { cmd: "undo", icon: "undo", title: "Undo" },
    { cmd: "redo", icon: "redo", title: "Redo" },
  ];

  function makeButton(spec, editor) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "rcms-richtext-tool";
    b.title = spec.title;
    b.setAttribute("aria-label", spec.title);
    if (spec.icon) {
      const i = document.createElement("span");
      i.className = "material-symbols-rounded";
      i.textContent = spec.icon;
      b.appendChild(i);
    } else {
      b.textContent = spec.label;
    }
    b.addEventListener("click", function (e) {
      e.preventDefault();
      const chain = editor.chain().focus();
      if (spec.fn === "setLink") {
        const prev = editor.getAttributes("link").href || "";
        const url = window.prompt("Link URL", prev);
        if (url === null) return;
        if (url === "") { chain.unsetLink().run(); return; }
        chain.extendMarkRange("link").setLink({ href: url }).run();
        return;
      }
      if (spec.fn === "linkInternal") {
        // #294 — move-safe internal links: pick a page/document via the
        // shared chooser, then store it as <a linktype id> (no href). The
        // server `| richtext` filter resolves it to a URL at render time,
        // so the link survives the target being moved/renamed.
        if (typeof window.rcmsOpenChooser !== "function") {
          window.alert("The chooser isn't available on this page.");
          return;
        }
        window.rcmsOpenChooser(spec.kind).then(function (item) {
          if (!item || !item.id) return;
          editor
            .chain()
            .focus()
            .extendMarkRange("link")
            .setLink({ href: null, linktype: spec.linktype, id: String(item.id) })
            .run();
        });
        return;
      }
      if (spec.fn === "linkEmail") {
        // #399 — email link. Stored as a plain <a href="mailto:…"> (the
        // on-save sanitizer keeps mailto:, the render filter passes it
        // through unchanged).
        const prev = (editor.getAttributes("link").href || "").replace(/^mailto:/, "");
        const email = window.prompt("Email address", prev);
        if (email === null) return;
        const trimmed = email.trim();
        if (trimmed === "") { chain.unsetLink().run(); return; }
        chain.extendMarkRange("link").setLink({ href: "mailto:" + trimmed }).run();
        return;
      }
      if (spec.fn === "linkAnchor") {
        // #399 — in-page anchor link. Stored as <a href="#id">; `#frag`
        // survives sanitization (relative URL) and renders verbatim.
        const prev = (editor.getAttributes("link").href || "").replace(/^#/, "");
        const name = window.prompt("Anchor name (target element id, without #)", prev);
        if (name === null) return;
        const trimmed = name.trim().replace(/^#/, "");
        if (trimmed === "") { chain.unsetLink().run(); return; }
        chain.extendMarkRange("link").setLink({ href: "#" + trimmed }).run();
        return;
      }
      if (spec.fn === "unsetLink") { chain.unsetLink().run(); return; }
      if (spec.arg) chain[spec.cmd](spec.arg).run();
      else chain[spec.cmd]().run();
    });
    return b;
  }

  function syncActive(editor, buttons) {
    buttons.forEach(function (entry) {
      if (!entry.spec.active) return;
      const on = entry.spec.activeArg
        ? editor.isActive(entry.spec.active, entry.spec.activeArg)
        : editor.isActive(entry.spec.active);
      entry.el.classList.toggle("is-active", on);
      entry.el.setAttribute("aria-pressed", String(on));
    });
  }

  function enhance(textarea) {
    if (textarea.dataset.rcmsTiptap === "1") return;
    if (!window.RcmsRichtext || typeof window.RcmsRichtext.create !== "function") return;
    textarea.dataset.rcmsTiptap = "1";

    const wrap = document.createElement("div");
    wrap.className = "rcms-richtext";
    const toolbar = document.createElement("div");
    toolbar.className = "rcms-richtext-toolbar";
    toolbar.setAttribute("role", "toolbar");
    toolbar.setAttribute("aria-label", "Rich text formatting");
    const surface = document.createElement("div");
    surface.className = "rcms-richtext-surface";
    wrap.appendChild(toolbar);
    wrap.appendChild(surface);

    // Hide the textarea but keep it submitting; insert the editor after it.
    textarea.hidden = true;
    textarea.parentNode.insertBefore(wrap, textarea.nextSibling);

    const editor = window.RcmsRichtext.create({
      element: surface,
      content: textarea.value || "",
      onUpdate: function (html) {
        // An empty TipTap doc serializes to "<p></p>"; store "" instead so
        // required-field + emptiness checks behave.
        textarea.value = html === "<p></p>" ? "" : html;
        textarea.dispatchEvent(new Event("input", { bubbles: true }));
      },
    });

    // #294 — keep a handle on the instance so the stream-block lifecycle
    // (below) can destroy it when its block is removed. The wrap is tagged
    // so a cloned (duplicated) block can find + reset the dead editor DOM.
    textarea.__rcmsEditor = editor;
    wrap.dataset.rcmsRichtextWrap = "1";

    const buttons = [];
    TOOLBAR.forEach(function (spec) {
      if (spec.sep) {
        const s = document.createElement("span");
        s.className = "rcms-richtext-sep";
        s.setAttribute("aria-hidden", "true");
        toolbar.appendChild(s);
        return;
      }
      const el = makeButton(spec, editor);
      toolbar.appendChild(el);
      buttons.push({ spec: spec, el: el });
    });

    editor.on("selectionUpdate", function () { syncActive(editor, buttons); });
    editor.on("transaction", function () { syncActive(editor, buttons); });
    syncActive(editor, buttons);
  }

  // #294 — lazy-load the ~312 KB engine bundle ONLY when a RichText field
  // is actually present, so it isn't fetched/parsed on admin pages that
  // have none. It's a same-origin `/cms-admin/static/...` script, so it
  // loads under a strict `script-src 'self'` CSP without a nonce.
  var bundlePromise = null;
  function ensureBundle() {
    if (window.RcmsRichtext && typeof window.RcmsRichtext.create === "function") {
      return Promise.resolve();
    }
    if (bundlePromise) return bundlePromise;
    bundlePromise = new Promise(function (resolve, reject) {
      var s = document.createElement("script");
      s.src = "/cms-admin/static/vendor/tiptap.bundle.js" + ASSET_VERSION;
      s.onload = resolve;
      s.onerror = reject;
      document.head.appendChild(s);
    });
    return bundlePromise;
  }

  function enhanceAll(root) {
    var pending = (root || document).querySelectorAll(
      'textarea[data-widget-mode="richtext"]:not([data-rcms-tiptap])'
    );
    if (!pending.length) return;
    ensureBundle()
      .then(function () { pending.forEach(enhance); })
      .catch(function () {
        // Bundle failed to load — leave the plain textarea editable as a
        // graceful fallback rather than breaking the form.
      });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () { enhanceAll(document); });
  } else {
    enhanceAll(document);
  }

  // Exposed so the stream-block editor can enhance dynamically-added
  // richtext fields; it loads the engine on demand too.
  window.rcmsEnhanceRichtext = enhanceAll;

  // #294 — stream-block lifecycle: tear down every TipTap instance inside
  // `root` before its block is removed from the DOM, so editors don't leak
  // (ProseMirror keeps document-level listeners until `destroy()`).
  window.rcmsDestroyRichtext = function (root) {
    (root || document)
      .querySelectorAll('textarea[data-rcms-tiptap="1"]')
      .forEach(function (ta) {
        if (ta.__rcmsEditor) {
          try { ta.__rcmsEditor.destroy(); } catch (e) { /* already gone */ }
          ta.__rcmsEditor = null;
        }
      });
  };

  // #294 — re-enhance a freshly *duplicated* block. `cloneNode(true)` copies
  // the live editor's DOM as a dead snapshot (no TipTap instance bound) and
  // a textarea still flagged `data-rcms-tiptap`. Strip that snapshot, lift
  // its current HTML back into the textarea (so the duplicate keeps its
  // content — cloneNode doesn't copy a textarea's live `value`), then run a
  // normal enhance to build a real editor.
  window.rcmsReenhanceRichtext = function (root) {
    (root || document)
      .querySelectorAll('[data-rcms-richtext-wrap="1"]')
      .forEach(function (wrap) {
        var ta = wrap.previousElementSibling;
        if (ta && ta.matches && ta.matches('textarea[data-widget-mode="richtext"]')) {
          var ce = wrap.querySelector("[contenteditable]");
          if (ce) {
            var html = ce.innerHTML;
            ta.value = html === "<p></p>" || html === "<p><br></p>" ? "" : html;
          }
          ta.hidden = false;
          delete ta.dataset.rcmsTiptap;
          ta.__rcmsEditor = null;
        }
        wrap.remove();
      });
    enhanceAll(root);
  };
})();
