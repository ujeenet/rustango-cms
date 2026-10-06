// rustango-cms stream-editor — vanilla JS, no framework.
//
// One generic module that operates entirely on `data-*` attributes so
// it doesn't know about widget *types*. The Rust side pre-renders the
// editor HTML (each block's per-field widget markup) — this script
// only handles add / remove / move / serialize-on-input.
//
// Contract:
//   <div data-stream-root data-stream-name="…" data-allowed="…">
//     <input type="hidden" name="…" data-stream-input>
//     <div data-stream-list [data-allowed="…"] [data-repeat="1"]>
//       <div data-stream-block data-type="…" data-id="UUID" data-version="…">
//         <div data-stream-field="…">
//           <input | select | textarea | nested data-stream-list>
//         </div>
//       </div>
//     </div>
//     <template data-block-template="…">{cloneable empty block}</template>
//     <dialog data-block-picker> … </dialog>
//   </div>
//
// Submission flow: on every `input` event on the form root we
// re-serialize the entire tree and rewrite the hidden input's `value`.
// Per-block UUIDs persist across edits; new blocks get fresh UUIDs
// from `crypto.randomUUID()`.

(function () {
    "use strict";

    function uuid() {
        if (window.crypto && typeof window.crypto.randomUUID === "function") {
            return window.crypto.randomUUID();
        }
        // Fallback for non-secure contexts — RFC 4122 v4 shape via Math.random.
        return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, function (c) {
            const r = (Math.random() * 16) | 0;
            const v = c === "x" ? r : (r & 0x3) | 0x8;
            return v.toString(16);
        });
    }

    // Walk one `[data-stream-list]` (immediate children only) and
    // produce `[{type, id, value}, …]` matching the Wagtail wire shape.
    function serializeList(listEl) {
        const out = [];
        const blocks = listEl.querySelectorAll(":scope > [data-stream-block]");
        for (const block of blocks) {
            const type = block.dataset.type;
            const id = block.dataset.id || uuid();
            block.dataset.id = id;
            const version = parseInt(block.dataset.version || "1", 10);
            const value = serializeBlockValue(block);
            out.push({ type: type, id: id, value: value, version: version });
        }
        return out;
    }

    // Walk the `.rcms-stream-block-body > [data-stream-field]` wrappers
    // inside one block and produce a flat `{name: value}` dict. A
    // wrapper containing a nested `[data-stream-list]` recurses; one
    // containing a leaf widget reads its `<input>/<select>/<textarea>`.
    function serializeBlockValue(blockEl) {
        const body = blockEl.querySelector(":scope > .rcms-stream-block-body");
        if (!body) return {};
        const out = {};
        const fields = body.querySelectorAll(":scope > [data-stream-field]");
        for (const field of fields) {
            const name = field.dataset.streamField;
            if (!name) continue;
            // Nested stream / repeat — recurse on the inner list.
            const nestedList = field.querySelector(":scope > .rcms-stream-list[data-stream-list], :scope fieldset > .rcms-stream-list[data-stream-list]");
            if (nestedList) {
                out[name] = serializeList(nestedList);
                continue;
            }
            // Leaf widget — find the first form control inside.
            const ctrl = field.querySelector("input, select, textarea");
            if (!ctrl) continue;
            if (ctrl.type === "checkbox") {
                out[name] = ctrl.checked;
            } else {
                out[name] = ctrl.value;
            }
        }
        return out;
    }

    // Walk every stream root in the document, serialize, write the
    // hidden input. Idempotent — safe to call on every input event.
    function serializeAll() {
        const roots = document.querySelectorAll("[data-stream-root]");
        for (const root of roots) {
            const topList = root.querySelector(":scope > .rcms-stream-list[data-stream-list]");
            if (!topList) continue;
            const tree = serializeList(topList);
            const hidden = root.querySelector(":scope > [data-stream-input]");
            if (hidden) hidden.value = JSON.stringify(tree);
        }
        // After serializing, refresh every collapsed-header label that
        // opted into `Block::label_format` (server-stamped as
        // `data-label-format` on the block). Cheap O(N-blocks) walk
        // over the doc — runs on every input event the editor already
        // serializes for. Mirrors the server-side `format_label`.
        refreshLabelFormats();
    }

    /// Substitute `{field}` tokens in `data-label-format` against the
    /// block's serialized value, rewrite `[data-stream-block-title]`.
    /// Mirrors the Rust-side `format_label` in `block::admin`.
    function refreshLabelFormats() {
        const blocks = document.querySelectorAll("[data-stream-block][data-label-format]");
        for (const block of blocks) {
            const fmt = block.getAttribute("data-label-format");
            if (!fmt) continue;
            const titleEl = block.querySelector(":scope > .rcms-stream-block-header [data-stream-block-title]");
            if (!titleEl) continue;
            const value = serializeBlockValue(block);
            const rendered = fmt.replace(/\{([a-zA-Z0-9_]+)\}/g, function (_match, name) {
                const v = value && value[name];
                if (v == null) return "";
                if (typeof v === "string") return v;
                return String(v);
            });
            // Fall back to the icon-paired verbose_name baked into
            // the title element on first render when the substituted
            // string is blank (e.g. brand-new empty block).
            if (rendered.trim() === "") {
                if (!titleEl.dataset.fallback) {
                    titleEl.dataset.fallback = titleEl.textContent;
                }
                titleEl.textContent = titleEl.dataset.fallback;
            } else {
                if (!titleEl.dataset.fallback) {
                    titleEl.dataset.fallback = titleEl.textContent;
                }
                titleEl.textContent = rendered;
            }
        }
    }

    // -- click delegation ----------------------------------------------
    // Structural-change signal for interested chrome (the block tree,
    // page_form's preview refresh): fired after any add / remove / move /
    // duplicate / collapse-toggle, once the DOM + serialized JSON settle.
    function notifyStreamChanged(root, verb) {
        document.dispatchEvent(
            new CustomEvent("rcms:stream-changed", { detail: { root, verb } })
        );
    }

    document.addEventListener("click", function (e) {
        const action = e.target.closest("[data-action]");
        if (!action) return;
        const root = action.closest("[data-stream-root]");
        if (!root) return;
        const verb = action.dataset.action;

        if (verb === "open-picker") {
            // Resolve the target list — for the top-level button it's
            // the root's first `.rcms-stream-list`; for nested Stream / Repeat
            // it's the `[data-stream-list]` sibling immediately before
            // the clicked `.rcms-stream-picker` wrapper. This is what makes
            // Stream-in-Stream / Repeat-in-Stream nesting editable.
            const picker = action.closest(".rcms-stream-picker");
            const nestedList = picker && picker.previousElementSibling &&
                picker.previousElementSibling.matches("[data-stream-list]")
                    ? picker.previousElementSibling
                    : null;
            const targetList = nestedList || root.querySelector(":scope > .rcms-stream-list[data-stream-list]");
            // Repeat fields (one allowed type) bypass the picker and
            // mint a row of that type immediately.
            if (targetList && targetList.dataset.repeat === "1") {
                insertBlock(root, targetList.dataset.allowed, targetList);
                return;
            }
            // A single-type list (e.g. a page-builder repeater, whose root
            // allows exactly one item type) makes the picker a pointless
            // one-option dialog — insert that type directly, matching the
            // Repeat fast path above. Scoped to the TARGET list's allowed
            // set (falling back to the root's), never the shared dialog.
            const allowedCsv = (targetList && targetList.dataset.allowed) || root.dataset.allowed || "";
            const allowedTypes = allowedCsv.split(",").map((s) => s.trim()).filter(Boolean);
            if (allowedTypes.length === 1) {
                insertBlock(root, allowedTypes[0], targetList);
                return;
            }
            openPicker(root, targetList);
            return;
        }
        if (verb === "pick-block") {
            const type = action.dataset.type;
            // Honour the dialog's recorded target list — set by
            // `openPicker` when a nested list opened it.
            const dialog = root.querySelector("[data-block-picker]");
            const targetList = dialog && dialog.__rcmsTargetList
                ? dialog.__rcmsTargetList
                : null;
            insertBlock(root, type, targetList);
            if (dialog) dialog.close();
            return;
        }
        if (verb === "close-picker") {
            // Picker close button — `type="button"` so the outer
            // page-edit form never sees a synthetic submit.
            const dialog = root.querySelector("[data-block-picker]");
            if (dialog) dialog.close();
            return;
        }
        if (verb === "remove") {
            const block = action.closest("[data-stream-block]");
            if (block) {
                // #294 — tear down any RichText editors before unmounting.
                if (window.rcmsDestroyRichtext) window.rcmsDestroyRichtext(block);
                block.remove();
                serializeAll();
                notifyStreamChanged(root, "remove");
            }
            return;
        }
        if (verb === "move-up") {
            const block = action.closest("[data-stream-block]");
            const prev = block && block.previousElementSibling;
            if (prev && prev.matches("[data-stream-block]")) {
                block.parentNode.insertBefore(block, prev);
                serializeAll();
                notifyStreamChanged(root, "move-up");
            }
            return;
        }
        if (verb === "move-down") {
            const block = action.closest("[data-stream-block]");
            const next = block && block.nextElementSibling;
            if (next && next.matches("[data-stream-block]")) {
                block.parentNode.insertBefore(next, block);
                serializeAll();
                notifyStreamChanged(root, "move-down");
            }
            return;
        }
        if (verb === "duplicate") {
            const block = action.closest("[data-stream-block]");
            if (!block) return;
            // Deep clone preserves every nested field value AND any
            // nested stream sub-trees. Re-mint a fresh UUID on the
            // clone + every descendant `data-stream-block` so the
            // serialized JSON treats them as new entries.
            const clone = block.cloneNode(true);
            const nested = clone.querySelectorAll("[data-stream-block]");
            for (const b of nested) b.dataset.id = uuid();
            clone.dataset.id = uuid();
            // Insert immediately after the source block (Wagtail-like).
            block.parentNode.insertBefore(clone, block.nextSibling);
            // #294 — the clone carries a dead snapshot of the source's
            // RichText editor DOM; reset it + build a fresh editor
            // (preserving the duplicated content).
            if (window.rcmsReenhanceRichtext) window.rcmsReenhanceRichtext(clone);
            serializeAll();
            notifyStreamChanged(root, "duplicate");
            return;
        }
        if (verb === "toggle-collapse") {
            // #205 — collapse / expand the block body. CSS
            // hides .stream-block-body when the host has
            // data-collapsed; the action icon flips between
            // unfold_less / unfold_more for cue.
            const block = action.closest("[data-stream-block]");
            if (!block) return;
            const icon = action.querySelector(".material-symbols-rounded");
            if (block.hasAttribute("data-collapsed")) {
                block.removeAttribute("data-collapsed");
                if (icon) icon.textContent = "unfold_less";
            } else {
                block.setAttribute("data-collapsed", "");
                if (icon) icon.textContent = "unfold_more";
            }
            notifyStreamChanged(root, "toggle-collapse");
            return;
        }
    });

    function openPicker(root, targetList) {
        const dialog = root.querySelector("[data-block-picker]");
        if (!dialog) return;
        // Remember which list this open-picker should target so the
        // subsequent pick-block click inserts there. Falls back to the
        // root's first stream-list (top-level Stream behaviour).
        dialog.__rcmsTargetList = targetList || root.querySelector(":scope > .rcms-stream-list[data-stream-list]");
        // Filter visible options by the target list's data-allowed
        // (nested lists carry their OWN allowed set distinct from the
        // root's). Falls back to the root's set when no target list.
        const allowedSrc = (targetList && targetList.dataset.allowed) || root.dataset.allowed || "";
        dialog.__rcmsAllowed = new Set(allowedSrc.split(",").filter(Boolean));
        // #411 — reset the search on every open so a stale query from a
        // previous open doesn't hide the block they want this time.
        const search = dialog.querySelector("[data-block-picker-search]");
        if (search) search.value = "";
        dialog.__rcmsQuery = "";
        refilterPicker(dialog);
        if (typeof dialog.showModal === "function") dialog.showModal();
        else dialog.setAttribute("open", "");
        if (search) search.focus();
    }

    // #411 — apply the current allowed-set + search query to the picker:
    // hide non-matching tiles, then hide any group header left with no
    // visible tiles. A single mechanism (the `hidden` attribute) so the
    // allowed filter and the search filter compose cleanly.
    function refilterPicker(dialog) {
        const allow = dialog.__rcmsAllowed;
        const q = (dialog.__rcmsQuery || "").toLowerCase();
        for (const opt of dialog.querySelectorAll("[data-block-option]")) {
            const type = opt.dataset.type || "";
            const allowed = allow ? allow.has(type) : true;
            let matches = true;
            if (q) {
                const label = (opt.dataset.label || "").toLowerCase();
                matches = label.includes(q) || type.toLowerCase().includes(q);
            }
            opt.hidden = !(allowed && matches);
        }
        // A group header is visible iff some tile beneath it (up to the
        // next header) is still visible.
        for (const header of dialog.querySelectorAll("[data-group-header]")) {
            let any = false;
            let sib = header.nextElementSibling;
            while (sib && !sib.hasAttribute("data-group-header")) {
                if (sib.hasAttribute("data-block-option") && !sib.hidden) {
                    any = true;
                    break;
                }
                sib = sib.nextElementSibling;
            }
            header.hidden = !any;
        }
    }

    // #411 — one-time: reorder the flat tile list into per-group runs,
    // each preceded by a presentational header <li>. Skipped when no
    // block declares a group (or there's only one bucket) so the common
    // small library stays a clean flat list. The ungrouped bucket sorts
    // last under an "Other" header.
    function groupPickerTiles(dialog) {
        const list = dialog.querySelector(".rcms-block-picker-list");
        if (!list || list.dataset.grouped === "1") return;
        list.dataset.grouped = "1";
        const OTHER = "\u0000other";
        const order = [];
        const buckets = new Map();
        let hasNamedGroup = false;
        for (const opt of list.querySelectorAll("[data-block-option]")) {
            const g = opt.dataset.group || "";
            if (g) hasNamedGroup = true;
            const key = g || OTHER;
            if (!buckets.has(key)) {
                buckets.set(key, []);
                order.push(key);
            }
            buckets.get(key).push(opt);
        }
        if (!hasNamedGroup || buckets.size < 2) return; // leave it flat
        const otherIdx = order.indexOf(OTHER);
        if (otherIdx !== -1) {
            order.splice(otherIdx, 1);
            order.push(OTHER);
        }
        for (const key of order) {
            const header = document.createElement("li");
            header.className = "rcms-block-picker-group";
            header.setAttribute("role", "presentation");
            header.dataset.groupHeader = key;
            header.textContent = key === OTHER ? "Other" : key;
            list.appendChild(header);
            // appendChild relocates the existing tiles into grouped order.
            for (const opt of buckets.get(key)) list.appendChild(opt);
        }
    }

    // #411 — group tiles + wire the search box on every picker present
    // at load. Idempotent (guarded by `data-grouped` / a wired flag).
    function initPickers() {
        for (const dialog of document.querySelectorAll("[data-block-picker]")) {
            groupPickerTiles(dialog);
            const search = dialog.querySelector("[data-block-picker-search]");
            if (search && search.dataset.wired !== "1") {
                search.dataset.wired = "1";
                search.addEventListener("input", function () {
                    dialog.__rcmsQuery = search.value || "";
                    refilterPicker(dialog);
                });
            }
        }
    }

    function insertBlock(root, type, targetList) {
        const tpl = root.querySelector(`template[data-block-template="${CSS.escape(type)}"]`);
        if (!tpl) {
            console.warn("rcms stream editor: no template for block type", type);
            return;
        }
        const clone = tpl.content.firstElementChild.cloneNode(true);
        // Mint a fresh UUID — the empty template carries no `data-id`.
        clone.dataset.id = uuid();
        // Nested lists insert into themselves, not the root.
        const list = targetList || root.querySelector(":scope > .rcms-stream-list[data-stream-list]");

        // #205 — honour Block::default_value() + Block::collapsed()
        // declared via the picker option's data attributes. Default
        // values pre-fill the new block's fields so the editor doesn't
        // start from blank when the block author specified one.
        const picker = root.querySelector("dialog[data-block-picker]");
        const option = picker?.querySelector(`[data-block-option][data-type="${CSS.escape(type)}"]`);
        if (option) {
            const defaultJson = option.getAttribute("data-default-value");
            if (defaultJson) {
                try {
                    const defaults = JSON.parse(defaultJson);
                    applyDefaults(clone, defaults);
                } catch (e) {
                    console.warn("rcms stream editor: bad default JSON for", type, e);
                }
            }
            if (option.hasAttribute("data-collapsed")) {
                clone.dataset.collapsed = "1";
                // Stream-block header reads this on mount; CSS hides
                // .stream-block-body when the host element has
                // data-collapsed.
            }
        }

        // Use the resolved target list (nested for Stream-in-Stream /
        // Repeat-in-Stream; root's top-level list otherwise).
        if (!list) return;
        list.appendChild(clone);
        serializeAll();
        notifyStreamChanged(root, "insert");
        // #294 — give a freshly-inserted RichText block its WYSIWYG editor
        // (no-op for blocks without a richtext field).
        if (window.rcmsEnhanceRichtext) window.rcmsEnhanceRichtext(clone);
    }

    // Apply a dict of `{field_name: value}` defaults to a fresh
    // block clone. Each key targets an input/select/textarea whose
    // `name=""` ends with the field name (the stream editor names
    // form controls by their field, even though they get suppressed
    // from POST — see `block::admin::render_block_inline`).
    function applyDefaults(blockEl, defaults) {
        if (!defaults || typeof defaults !== "object") return;
        for (const [name, value] of Object.entries(defaults)) {
            const field = blockEl.querySelector(
                `.rcms-stream-field[data-stream-field="${CSS.escape(name)}"] input, ` +
                `.rcms-stream-field[data-stream-field="${CSS.escape(name)}"] textarea, ` +
                `.rcms-stream-field[data-stream-field="${CSS.escape(name)}"] select`
            );
            if (!field) continue;
            if (field.type === "checkbox") {
                field.checked = Boolean(value);
            } else if (field.tagName === "SELECT" && field.multiple && Array.isArray(value)) {
                Array.from(field.options).forEach((o) => {
                    o.selected = value.includes(o.value);
                });
            } else {
                field.value = value;
            }
        }
    }

    // -- live serialization --------------------------------------------
    // Bind once to the document so dynamically-inserted blocks just
    // work. `input` bubbles from every form control.
    document.addEventListener("input", function (e) {
        if (e.target.closest("[data-stream-root]")) {
            serializeAll();
        }
    });
    document.addEventListener("change", function (e) {
        if (e.target.closest("[data-stream-root]")) {
            serializeAll();
        }
    });

    // Final pre-submit serialize — guard against any input event that
    // might not have fired (e.g. programmatic .value changes).
    document.addEventListener("submit", function () { serializeAll(); }, true);

    // Initial pass after DOM load so hidden inputs match the rendered
    // tree (covers the case where the server set stale values). #411 —
    // also group the picker tiles + wire their search box.
    function init() {
        serializeAll();
        initPickers();
    }
    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", init);
    } else {
        init();
    }
})();
