// rustango-cms block tree — a live outline of every StreamField block
// (including nested Stream/Repeat sub-blocks) that takes over the main
// sidebar while active. Vanilla JS, no framework.
//
// Features:
//   • tree of all blocks + nested sub-blocks (page-builder zones included —
//     same [data-stream-block] markup)
//   • click a node → smooth-scroll to the block (expanding it if collapsed)
//   • per-node fold icon toggles the CANVAS block's collapse (kept in sync
//     with the block header's own toggle) + Collapse all / Expand all
//   • live client-side validation: a block with invalid controls (HTML5
//     validity) shows red with an error count; the canvas header gets a dot
//   • submit assist: a `required` field inside a collapsed block used to
//     abort submit silently (non-focusable control) — the tree expands the
//     offending block and lets the browser show its validation bubble.
//
// Self-activating: only wires up when the page has a [data-stream-root].
(function () {
    "use strict";

    let treeEl = null;      // .sidebar-blocktree container
    let asideEl = null;     // aside.sidebar
    let refreshTimer = 0;
    let treeDirty = true;   // canvas changed while the tree panel was hidden

    const esc = (s) => String(s ?? "")
        .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

    // ---- model ------------------------------------------------------------

    // Recursive walk mirroring stream_editor.js's serializeList enumeration.
    // `errMap` is the single-pass validation result from computeErrors().
    function walkList(listEl, errMap) {
        const nodes = [];
        for (const block of listEl.querySelectorAll(":scope > [data-stream-block]")) {
            const titleEl = block.querySelector(":scope > .rcms-stream-block-header [data-stream-block-title]");
            const iconEl = block.querySelector(":scope > .rcms-stream-block-header .rcms-stream-block-icon");
            const groups = [];
            const body = block.querySelector(":scope > .rcms-stream-block-body");
            if (body) {
                // Nested Stream/Repeat containers: the fieldset itself carries
                // data-stream-field (it IS the field wrapper, not a child of
                // one) — see block/admin.rs's stream-nested markup.
                for (const nested of body.querySelectorAll("fieldset.rcms-stream-nested[data-stream-field]")) {
                    // Only fieldsets belonging to THIS block, not grandchildren.
                    if (nested.closest("[data-stream-block]") !== block) continue;
                    const list = nested.querySelector(":scope > .rcms-stream-list[data-stream-list]");
                    if (!list) continue;
                    groups.push({
                        label: nested.querySelector(":scope > legend")?.textContent.trim() || "Items",
                        children: walkList(list, errMap),
                    });
                }
            }
            nodes.push({
                el: block,
                type: block.dataset.type || "",
                icon: iconEl ? iconEl.textContent.trim() : "widgets",
                label: titleEl ? titleEl.textContent.trim() : (block.dataset.type || "block"),
                collapsed: block.hasAttribute("data-collapsed"),
                errors: errMap.get(block) || 0,
                groups,
            });
        }
        // Repeated identical labels (a repeater full of "Card"s) are
        // indistinguishable — suffix an ordinal on duplicates.
        const seen = new Map();
        for (const n of nodes) seen.set(n.label, (seen.get(n.label) || 0) + 1);
        const idx = new Map();
        for (const n of nodes) {
            if ((seen.get(n.label) || 0) < 2) continue;
            const i = (idx.get(n.label) || 0) + 1;
            idx.set(n.label, i);
            n.label = `${n.label} ${i}`;
        }
        return nodes;
    }

    // checkValidity can THROW (an invalid `pattern` regex under Chrome's
    // v-flag compilation) — treat an unverifiable control as valid rather
    // than letting one bad pattern take down the whole tree/assist.
    function safeValid(ctrl) {
        if (typeof ctrl.checkValidity !== "function") return true;
        try { return ctrl.checkValidity(); } catch (e) { return true; }
    }

    // ONE validation sweep for the whole canvas: every invalid control is
    // attributed to its NEAREST block (so a nested child's errors don't
    // double-count on its parent). O(controls) — the previous per-block
    // counting re-walked each block's whole subtree and went quadratic on
    // deeply nested pages.
    function computeErrors() {
        const map = new Map();
        for (const root of collectRoots()) {
            for (const ctrl of root.querySelectorAll("input, select, textarea")) {
                if (ctrl.disabled) continue;
                if (ctrl.closest("template")) continue;
                if (safeValid(ctrl)) continue;
                const block = ctrl.closest("[data-stream-block]");
                if (!block) continue;
                map.set(block, (map.get(block) || 0) + 1);
            }
        }
        return map;
    }

    // Human name for a stream root: the widget's own <label> when present,
    // else a prettified data-stream-name (pb__sections -> "Sections").
    function rootLabel(root) {
        const lbl = root.closest(".rcms-field, .rcms-stream-field, fieldset")?.querySelector("label, legend");
        if (lbl && lbl.textContent.trim()) return lbl.textContent.trim().replace(/\s*\*$/, "");
        const raw = (root.dataset.streamName || "blocks").split("__").pop();
        return raw.replace(/[_-]+/g, " ").replace(/^./, (c) => c.toUpperCase());
    }

    function collectRoots() {
        // Only stream roots inside the page-edit form (skip pickers/dialogs).
        return Array.from(document.querySelectorAll("[data-stream-root]"))
            .filter((r) => r.querySelector(":scope > .rcms-stream-list[data-stream-list]"));
    }

    // ---- rendering ----------------------------------------------------------

    function renderNode(node, depth) {
        const err = node.errors > 0;
        const kids = node.groups.flatMap((g) => g.children);
        const rows = [`
            <div class="rcms-blocktree-node${err ? " is-invalid" : ""}${node.collapsed ? " is-collapsed" : ""}"
                 data-bt-node data-bt-id="${esc(node.el.dataset.id || "")}" style="--bt-depth: ${depth}">
                <button type="button" class="rcms-blocktree-jump" data-bt-jump title="${esc(node.type)}">
                    <span class="material-symbols-rounded sm">${esc(node.icon)}</span>
                    <span class="rcms-blocktree-label">${esc(node.label)}</span>
                    ${err ? `<span class="rcms-blocktree-errors" title="${node.errors} invalid field${node.errors > 1 ? "s" : ""}">${node.errors}</span>` : ""}
                </button>
                <button type="button" class="rcms-blocktree-fold" data-bt-fold
                        title="${node.collapsed ? "Expand block" : "Collapse block"}">
                    <span class="material-symbols-rounded sm">${node.collapsed ? "unfold_more" : "unfold_less"}</span>
                </button>
            </div>`];
        for (const g of node.groups) {
            if (g.children.length === 0) continue;
            rows.push(`<div class="rcms-blocktree-group" style="--bt-depth: ${depth + 1}">${esc(g.label)}</div>`);
            for (const child of g.children) rows.push(renderNode(child, depth + 2));
        }
        // groups already rendered children; nothing else to append
        void kids;
        return rows.join("");
    }

    function renderTree(errMap) {
        if (!treeEl) return;
        const body = treeEl.querySelector("[data-bt-body]");
        if (!body) return;
        const roots = collectRoots();
        const parts = [];
        for (const root of roots) {
            const list = root.querySelector(":scope > .rcms-stream-list[data-stream-list]");
            const nodes = walkList(list, errMap);
            // With several stream fields on one page (page-builder zones,
            // repeaters), a per-root header says WHICH field a node lives in.
            if (roots.length > 1) {
                parts.push(`<div class="rcms-blocktree-root">${esc(rootLabel(root))}</div>`);
            }
            for (const n of nodes) parts.push(renderNode(n, 0));
        }
        body.innerHTML = parts.length
            ? parts.join("")
            : `<div class="rcms-blocktree-empty">No blocks yet — add one in the editor.</div>`;
    }

    // Mirror invalid state onto the canvas headers (red dot). Toggling
    // every block both sets fresh dots and clears stale ones.
    function markInvalidCanvas(errMap) {
        for (const root of collectRoots()) {
            for (const block of root.querySelectorAll("[data-stream-block]")) {
                block.classList.toggle("rcms-stream-block--invalid", (errMap.get(block) || 0) > 0);
            }
        }
    }

    // One validation pass feeds both surfaces; the tree's DOM is only
    // rebuilt while its panel is actually visible — while it's hidden we
    // just remember that it's stale and rebuild once on activation.
    function refresh() {
        const errMap = computeErrors();
        markInvalidCanvas(errMap);
        if (treeEl && !treeEl.hidden) {
            renderTree(errMap);
            treeDirty = false;
        } else {
            treeDirty = true;
        }
    }

    function scheduleRefresh() {
        clearTimeout(refreshTimer);
        refreshTimer = setTimeout(refresh, 300);
    }

    // ---- canvas interactions ------------------------------------------------

    function setBlockCollapsed(block, collapsed) {
        if (collapsed) block.setAttribute("data-collapsed", "");
        else block.removeAttribute("data-collapsed");
        // Keep the block header's own toggle icon in sync.
        const icon = block.querySelector(
            ':scope > .rcms-stream-block-header [data-action="toggle-collapse"] .material-symbols-rounded'
        );
        if (icon) icon.textContent = collapsed ? "unfold_more" : "unfold_less";
    }

    function findBlockById(id) {
        return document.querySelector(`[data-stream-block][data-id="${CSS.escape(id)}"]`);
    }

    function jumpToBlock(block) {
        // Make sure it's visible: expand collapsed ancestors (and itself) and
        // switch to the tab panel that hosts it.
        let anc = block;
        while (anc) {
            if (anc.matches?.("[data-stream-block][data-collapsed]")) setBlockCollapsed(anc, false);
            anc = anc.parentElement?.closest("[data-stream-block]");
        }
        const panel = block.closest(".rcms-tab-panel[data-tab-panel]");
        if (panel && panel.hidden) {
            document
                .querySelector(`[role="tab"][aria-controls="${panel.id}"], [data-tab-target="${panel.dataset.tabPanel}"]`)
                ?.click();
        }
        block.scrollIntoView({ behavior: "smooth", block: "center" });
        block.classList.add("rcms-side-panel-flash");
        setTimeout(() => block.classList.remove("rcms-side-panel-flash"), 1200);
        // On phones the sidebar is a fixed overlay covering most of the
        // viewport — a jump would scroll the canvas BEHIND it. Slide the
        // overlay away so the flash is actually visible; the tree stays
        // active for the next hamburger open.
        if (window.matchMedia("(max-width: 768px)").matches) {
            const navToggle = document.getElementById("__nav_toggle");
            if (navToggle) navToggle.checked = false;
        }
    }

    function setAllCollapsed(collapsed) {
        for (const root of collectRoots()) {
            for (const block of root.querySelectorAll("[data-stream-block]")) {
                if (block.closest("template")) continue;
                setBlockCollapsed(block, collapsed);
            }
        }
        refresh();
    }

    // ---- submit assist --------------------------------------------------------

    function wireSubmitAssist() {
        const form = document.getElementById("page-edit-form");
        if (!form) return;
        // A `submit` listener never fires here: native constraint validation
        // runs FIRST and — when the invalid control sits inside a collapsed
        // (display:none) block — aborts silently on a non-focusable control.
        // So hook the submit BUTTON's click in the capture phase and expand
        // every collapsed block that hides an invalid control before the
        // browser validates; the native bubble then lands on a visible field.
        document.addEventListener(
            "click",
            (e) => {
                const btn = e.target.closest('button[type="submit"], input[type="submit"]');
                if (!btn) return;
                if (btn.form !== form && btn.getAttribute("form") !== form.id) return;
                let formValid = true;
                try { formValid = form.checkValidity(); } catch (e) { /* bad pattern */ }
                if (formValid) return;
                let firstInvalid = null;
                let firstBlock = null;
                for (const el of form.elements) {
                    if (safeValid(el)) continue;
                    if (!firstInvalid) firstInvalid = el;
                    const block = el.closest("[data-stream-block]");
                    if (!block) continue;
                    if (!firstBlock) firstBlock = block;
                    let anc = block;
                    while (anc) {
                        if (anc.hasAttribute("data-collapsed")) setBlockCollapsed(anc, false);
                        anc = anc.parentElement?.closest("[data-stream-block]");
                    }
                }
                // Jump only when the browser's own bubble will land inside a
                // block — otherwise our scroll fights the native focus of an
                // earlier non-block field (visible scroll flicker).
                if (firstBlock && firstInvalid && firstInvalid.closest("[data-stream-block]") === firstBlock) {
                    jumpToBlock(firstBlock);
                    refresh();
                }
                // No preventDefault — with the controls now visible, native
                // validation shows its bubble on the first invalid field.
            },
            true
        );
    }

    // ---- sidebar takeover -----------------------------------------------------

    function buildSidebarUi() {
        asideEl = document.querySelector("aside.rcms-sidebar");
        const nav = asideEl?.querySelector(".rcms-sidebar-nav");
        if (!asideEl || !nav) return false;

        // Toggle entry at the top of the normal nav.
        const toggle = document.createElement("a");
        toggle.href = "#";
        toggle.className = "rcms-sidebar-link rcms-blocktree-toggle";
        toggle.innerHTML = `<span class="material-symbols-rounded">account_tree</span> Page blocks`;
        toggle.addEventListener("click", (e) => {
            e.preventDefault();
            activate(true);
        });
        nav.prepend(toggle);

        // The tree panel itself (sibling of the nav).
        treeEl = document.createElement("div");
        treeEl.className = "rcms-sidebar-blocktree";
        treeEl.hidden = true;
        treeEl.innerHTML = `
            <button type="button" class="rcms-blocktree-back" data-bt-back>
                <span class="material-symbols-rounded sm">arrow_back</span> Menu
            </button>
            <div class="rcms-blocktree-toolbar">
                <span class="rcms-blocktree-title">Page blocks</span>
                <button type="button" class="rcms-btn rcms-btn-text rcms-btn-small" data-bt-collapse-all title="Collapse all blocks">
                    <span class="material-symbols-rounded sm">unfold_less</span>
                </button>
                <button type="button" class="rcms-btn rcms-btn-text rcms-btn-small" data-bt-expand-all title="Expand all blocks">
                    <span class="material-symbols-rounded sm">unfold_more</span>
                </button>
            </div>
            <div class="rcms-blocktree-body" data-bt-body></div>`;
        nav.after(treeEl);

        treeEl.querySelector("[data-bt-back]").addEventListener("click", () => activate(false));
        treeEl.querySelector("[data-bt-collapse-all]").addEventListener("click", () => setAllCollapsed(true));
        treeEl.querySelector("[data-bt-expand-all]").addEventListener("click", () => setAllCollapsed(false));
        treeEl.addEventListener("click", (e) => {
            const node = e.target.closest("[data-bt-node]");
            if (!node) return;
            const block = findBlockById(node.dataset.btId || "");
            if (!block) return;
            if (e.target.closest("[data-bt-fold]")) {
                setBlockCollapsed(block, !block.hasAttribute("data-collapsed"));
                refresh();
                return;
            }
            if (e.target.closest("[data-bt-jump]")) {
                jumpToBlock(block);
                refresh();
            }
        });
        return true;
    }

    let railWasCollapsed = false;
    function activate(on) {
        if (!asideEl || !treeEl) return;
        asideEl.classList.toggle("rcms-sidebar--blocktree", on);
        treeEl.hidden = !on;
        if (on) {
            // The icon-rail collapsed sidebar can't host a tree — expand it
            // while the tree is open, and restore the rail on the way out
            // (the stored preference itself is untouched).
            railWasCollapsed = document.documentElement.hasAttribute("data-sidebar-collapsed");
            document.documentElement.removeAttribute("data-sidebar-collapsed");
            // Lazy build: the tree DOM is only produced on first activation
            // (and after edits made while the panel was hidden).
            if (treeDirty) refresh();
        } else if (railWasCollapsed) {
            document.documentElement.setAttribute("data-sidebar-collapsed", "");
            railWasCollapsed = false;
        }
    }

    // ---- init -------------------------------------------------------------------

    document.addEventListener("DOMContentLoaded", () => {
        if (collectRoots().length === 0) return; // not a stream editor page
        if (!buildSidebarUi()) return;
        wireSubmitAssist();
        refresh();
        // Structural changes (add/remove/move/duplicate/collapse) fire
        // rcms:stream-changed from stream_editor.js; field edits bubble
        // input/change.
        document.addEventListener("rcms:stream-changed", scheduleRefresh);
        document.addEventListener("input", (e) => {
            if (e.target.closest?.("[data-stream-root]")) scheduleRefresh();
        });
        document.addEventListener("change", (e) => {
            if (e.target.closest?.("[data-stream-root]")) scheduleRefresh();
        });
    });
})();
