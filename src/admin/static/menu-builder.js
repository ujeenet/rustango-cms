// WP-style drag-and-drop menu builder (#44).
//
// The tree is rendered as a flat <ol> with each <li> carrying a
// `data-depth` attribute (visual indent driven by CSS). Drag-to-
// reorder updates DOM order + indent in real time. Save POSTs a
// JSON tree to /cms-admin/navigation/{id}/save-tree.
//
// Two intentional simplifications vs. a full tree-widget library:
//
// 1. The tree is FLAT in the DOM but logically nested via depth.
//    This makes drop-zone hit-testing trivial (between consecutive
//    <li>s) at the cost of a little post-processing on save to map
//    depth → parent.
// 2. No external dependencies — pure HTML5 drag-and-drop. The
//    keyboard fallback (per-card up / down / indent / outdent
//    buttons) is the a11y contract; the drag is the convenience.

(() => {
    const MAX_DEPTH = 3;

    const root = document.querySelector(".rcms-menu-builder");
    if (!root) return;
    const menuId = root.dataset.menuId;
    const tree = root.querySelector(".rcms-menu-builder-tree");
    const saveBtn = document.getElementById("menu-builder-save");
    const dirtyFlag = root.querySelector(".rcms-menu-builder-dirty");
    const empty = root.querySelector(".rcms-menu-builder-empty");

    let dirty = false;
    const markDirty = () => {
        dirty = true;
        if (dirtyFlag) dirtyFlag.hidden = false;
    };
    const clearDirty = () => {
        dirty = false;
        if (dirtyFlag) dirtyFlag.hidden = true;
    };

    // Tag every card with its current depth (read on save).
    const computeDepths = () => {
        const cards = Array.from(tree.querySelectorAll(".rcms-menu-builder-card"));
        cards.forEach((card) => {
            const d = parseInt(card.dataset.depth || "0", 10);
            card.style.setProperty("--menu-builder-depth", d);
        });
    };

    const setDepth = (card, depth) => {
        const clamped = Math.max(0, Math.min(MAX_DEPTH, depth));
        card.dataset.depth = clamped;
        card.style.setProperty("--menu-builder-depth", clamped);
    };

    const initialiseDepths = () => {
        // The server stuffs `parent_id` on each card. To translate
        // that into depth we walk in DOM order: a card with a parent
        // already in the same tree sits one deeper than its parent.
        const cards = Array.from(tree.querySelectorAll(".rcms-menu-builder-card"));
        const idToCard = new Map();
        cards.forEach((c) => idToCard.set(c.dataset.itemId, c));
        cards.forEach((c) => {
            let depth = 0;
            let parentId = c.dataset.parentId;
            while (parentId && idToCard.has(parentId)) {
                depth += 1;
                parentId = idToCard.get(parentId).dataset.parentId;
                if (depth > MAX_DEPTH) break;
            }
            setDepth(c, depth);
        });
    };

    // ------------------------------------------------------------------
    // Drag-and-drop wiring.

    let dragging = null;
    let dragSource = null; // "tree" or "picker"
    // #49b — drag-to-indent: track the pointer X at drag start so we
    // can convert horizontal drift on drop into a depth delta
    // (1 indent step = INDENT_PX_PER_STEP px).
    const INDENT_PX_PER_STEP = 32;
    let dragStartX = 0;
    let dragStartDepth = 0;

    tree.addEventListener("dragstart", (event) => {
        const card = event.target.closest(".rcms-menu-builder-card");
        if (!card) return;
        dragging = card;
        dragSource = "tree";
        dragStartX = event.clientX;
        dragStartDepth = parseInt(card.dataset.depth || "0", 10);
        card.classList.add("rcms-menu-builder-card--dragging");
        event.dataTransfer.effectAllowed = "move";
    });
    tree.addEventListener("dragend", () => {
        if (dragging) dragging.classList.remove("rcms-menu-builder-card--dragging");
        dragging = null;
        dragSource = null;
        dragStartX = 0;
        dragStartDepth = 0;
        tree.querySelectorAll(".rcms-menu-builder-drop-target").forEach((el) =>
            el.classList.remove("rcms-menu-builder-drop-target"),
        );
        tree
            .querySelectorAll(".rcms-menu-builder-card--indent-preview")
            .forEach((el) => el.classList.remove("rcms-menu-builder-card--indent-preview"));
    });

    // Picker → tree: every picker item is draggable.
    document.querySelectorAll(".rcms-menu-builder-picker-item").forEach((item) => {
        item.addEventListener("dragstart", (event) => {
            dragSource = "picker";
            dragging = item;
            event.dataTransfer.effectAllowed = "copy";
        });
        item.addEventListener("dragend", () => {
            dragging = null;
            dragSource = null;
        });
        item.addEventListener("click", () => insertFromPicker(item));
    });

    tree.addEventListener("dragover", (event) => {
        event.preventDefault();
        event.dataTransfer.dropEffect = dragSource === "picker" ? "copy" : "move";
        // Hit-test: find the card under the pointer and decide above /
        // below based on the pointer's vertical position relative to
        // its center.
        const card = event.target.closest(".rcms-menu-builder-card");
        tree.querySelectorAll(".rcms-menu-builder-drop-target").forEach((el) =>
            el.classList.remove("rcms-menu-builder-drop-target"),
        );
        if (card && card !== dragging) {
            card.classList.add("rcms-menu-builder-drop-target");
        }
        // #49b — depth preview during drag. Compute the would-be
        // depth and visually preview it on the dragged card so the
        // editor sees the indent decision before they drop.
        if (dragSource === "tree" && dragging) {
            const deltaSteps = Math.round((event.clientX - dragStartX) / INDENT_PX_PER_STEP);
            const previewDepth = Math.max(
                0,
                Math.min(MAX_DEPTH, dragStartDepth + deltaSteps),
            );
            dragging.style.setProperty("--menu-builder-depth", previewDepth);
            dragging.classList.add("rcms-menu-builder-card--indent-preview");
        }
    });

    tree.addEventListener("drop", (event) => {
        event.preventDefault();
        const targetCard = event.target.closest(".rcms-menu-builder-card");
        const rect = targetCard ? targetCard.getBoundingClientRect() : null;
        const insertBefore = rect ? event.clientY < rect.top + rect.height / 2 : false;
        if (dragSource === "tree" && dragging) {
            // #49b — apply pointer-horizontal-drift → depth delta.
            // Computed against the dragging card's start depth so the
            // editor's intent ("drag right two steps") survives the
            // reorder.
            const deltaSteps = Math.round((event.clientX - dragStartX) / INDENT_PX_PER_STEP);
            const targetDepth = Math.max(
                0,
                Math.min(MAX_DEPTH, dragStartDepth + deltaSteps),
            );
            if (targetCard && targetCard !== dragging) {
                if (insertBefore) tree.insertBefore(dragging, targetCard);
                else tree.insertBefore(dragging, targetCard.nextSibling);
            }
            setDepth(dragging, targetDepth);
            normaliseDepths();
            dragging.classList.remove("rcms-menu-builder-card--indent-preview");
            markDirty();
        } else if (dragSource === "picker" && dragging) {
            const card = createCardFromPicker(dragging);
            if (targetCard) {
                if (insertBefore) tree.insertBefore(card, targetCard);
                else tree.insertBefore(card, targetCard.nextSibling);
            } else {
                tree.appendChild(card);
            }
            normaliseDepths();
            markDirty();
            removeEmptyState();
        }
        tree.querySelectorAll(".rcms-menu-builder-drop-target").forEach((el) =>
            el.classList.remove("rcms-menu-builder-drop-target"),
        );
    });

    // Tree as a fallback drop zone for the empty state.
    if (empty) {
        empty.addEventListener("dragover", (e) => e.preventDefault());
        empty.addEventListener("drop", (event) => {
            event.preventDefault();
            if (dragSource === "picker" && dragging) {
                const card = createCardFromPicker(dragging);
                tree.appendChild(card);
                normaliseDepths();
                markDirty();
                removeEmptyState();
            }
        });
    }

    function removeEmptyState() {
        if (empty && tree.children.length > 0) empty.remove();
    }

    function createCardFromPicker(pickerItem) {
        const li = document.createElement("li");
        li.className = "rcms-menu-builder-card";
        li.dataset.itemId = ""; // new item — no DB id yet
        li.dataset.pageId = pickerItem.dataset.pageId || "";
        // A page item keeps no label of its own: it shows the page's
        // title, which then follows renames and translations. The title
        // is kept only to display the card.
        li.dataset.label = li.dataset.pageId ? "" : (pickerItem.dataset.label || "");
        li.dataset.pageTitle = li.dataset.pageId ? (pickerItem.dataset.label || "") : "";
        li.dataset.externalUrl = pickerItem.dataset.externalUrl || "";
        li.dataset.openNewTab = "false";
        li.draggable = true;
        li.innerHTML = renderCardInner(li, pickerItem.dataset.urlPath || "");
        setDepth(li, 0);
        return li;
    }

    // Single source of truth for a card's inner HTML — used by
    // createCardFromPicker (for new items) and by ensureDisclosure
    // (when an existing card is re-rendered after a label edit).
    function renderCardInner(card, urlPath) {
        const isPage = !!card.dataset.pageId;
        const explicit = card.dataset.label || "";
        const displayLabel = explicit || card.dataset.pageTitle || urlPath || "(no label)";
        return `
            <span class="rcms-menu-builder-handle material-symbols-rounded">drag_indicator</span>
            <div class="rcms-menu-builder-card-body" data-toggle-disclosure>
                <div class="rcms-menu-builder-card-label">${escapeHtml(displayLabel)}</div>
                <div class="rcms-menu-builder-card-target">
                    ${isPage
                        ? `<span class="rcms-tag" style="font-size: 11px;">${escapeHtml(root.dataset.tagPage || "page")}</span><code>${escapeHtml(urlPath)}</code>`
                        : `<span class="rcms-tag scheduled" style="font-size: 11px;">${escapeHtml(root.dataset.tagLink || "link")}</span><code>${escapeHtml(card.dataset.externalUrl)}</code>`}
                </div>
            </div>
            <div class="rcms-menu-builder-card-actions">
                <button type="button" class="rcms-btn-icon rcms-btn rcms-btn-small" data-action="up" title="Move up" aria-label="Move up"><span class="material-symbols-rounded sm">keyboard_arrow_up</span></button>
                <button type="button" class="rcms-btn-icon rcms-btn rcms-btn-small" data-action="down" title="Move down" aria-label="Move down"><span class="material-symbols-rounded sm">keyboard_arrow_down</span></button>
                <button type="button" class="rcms-btn-icon rcms-btn rcms-btn-small" data-action="outdent" title="Outdent" aria-label="Outdent"><span class="material-symbols-rounded sm">format_indent_decrease</span></button>
                <button type="button" class="rcms-btn-icon rcms-btn rcms-btn-small" data-action="indent" title="Indent" aria-label="Indent"><span class="material-symbols-rounded sm">format_indent_increase</span></button>
                <button type="button" class="rcms-btn-icon rcms-btn-danger rcms-btn-small" data-action="remove" title="Remove" aria-label="Remove"><span class="material-symbols-rounded sm">close</span></button>
            </div>
            <div class="rcms-menu-builder-card-disclosure" hidden>
                <div class="rcms-field" style="margin-bottom: 8px;">
                    <label>Label override</label>
                    <input type="text" data-disclosure-label value="${escapeHtml(explicit)}" placeholder="Falls back to ${escapeHtml(isPage ? (urlPath || "page title") : "URL")}">
                </div>
                <label class="rcms-checkbox-field">
                    <input type="checkbox" data-disclosure-new-tab ${card.dataset.openNewTab === "true" ? "checked" : ""}>
                    Open in new tab
                </label>
            </div>
        `;
    }

    function insertFromPicker(pickerItem) {
        const card = createCardFromPicker(pickerItem);
        tree.appendChild(card);
        markDirty();
        removeEmptyState();
    }

    function escapeHtml(s) {
        return (s || "")
            .replaceAll("&", "&amp;")
            .replaceAll("<", "&lt;")
            .replaceAll(">", "&gt;")
            .replaceAll('"', "&quot;");
    }

    // ------------------------------------------------------------------
    // Inline disclosure (#49) — clicking a card body opens an
    // editable panel for label override + open-in-new-tab. Edits
    // sync back to the card's data-* attrs immediately so the next
    // save POST picks them up.

    // Retrofit server-rendered cards. They were emitted by Tera
    // without the disclosure panel, so we hot-swap their innerHTML
    // via renderCardInner using the original target URL pulled from
    // the existing <code> child.
    Array.from(tree.querySelectorAll(".rcms-menu-builder-card")).forEach((card) => {
        if (card.querySelector("[data-disclosure-label]")) return;
        // Pull URL from any existing target chip's <code> before we
        // overwrite innerHTML — for page-link items it's the page's
        // url_path.
        const urlEl = card.querySelector(".rcms-menu-builder-card-target code");
        const url = urlEl ? urlEl.textContent : "";
        card.innerHTML = renderCardInner(card, url);
    });

    tree.addEventListener("click", (event) => {
        // Toggle disclosure when clicking the card body (not the
        // handle, drag indicator, or action buttons).
        const bodyHit = event.target.closest("[data-toggle-disclosure]");
        if (bodyHit) {
            const card = bodyHit.closest(".rcms-menu-builder-card");
            if (card) {
                const panel = card.querySelector(".rcms-menu-builder-card-disclosure");
                if (panel) {
                    panel.hidden = !panel.hidden;
                    card.classList.toggle("expanded", !panel.hidden);
                    return;
                }
            }
        }
        const btn = event.target.closest("button[data-action]");
        if (!btn) return;
        const card = btn.closest(".rcms-menu-builder-card");
        if (!card) return;
        const action = btn.dataset.action;
        if (action === "up") {
            const prev = card.previousElementSibling;
            if (prev) {
                tree.insertBefore(card, prev);
                normaliseDepths();
                markDirty();
            }
        } else if (action === "down") {
            const next = card.nextElementSibling;
            if (next) {
                tree.insertBefore(next, card);
                normaliseDepths();
                markDirty();
            }
        } else if (action === "indent") {
            // Can only indent under a preceding sibling at the same depth.
            const prev = card.previousElementSibling;
            const myDepth = parseInt(card.dataset.depth || "0", 10);
            const prevDepth = prev ? parseInt(prev.dataset.depth || "0", 10) : -1;
            if (prev && myDepth <= prevDepth && myDepth < MAX_DEPTH) {
                setDepth(card, myDepth + 1);
                markDirty();
            }
        } else if (action === "outdent") {
            const myDepth = parseInt(card.dataset.depth || "0", 10);
            if (myDepth > 0) {
                setDepth(card, myDepth - 1);
                markDirty();
            }
        } else if (action === "remove") {
            card.remove();
            normaliseDepths();
            markDirty();
            if (tree.children.length === 0 && empty) {
                tree.parentNode.insertBefore(empty, tree.nextSibling);
            }
        }
    });

    // Disclosure inputs — label override + new-tab toggle.
    tree.addEventListener("input", (event) => {
        const labelInput = event.target.closest("[data-disclosure-label]");
        if (labelInput) {
            const card = labelInput.closest(".rcms-menu-builder-card");
            if (!card) return;
            const value = labelInput.value;
            card.dataset.label = value;
            // Mirror to the visible label row. Falls back to the
            // placeholder hint when emptied.
            const labelRow = card.querySelector(".rcms-menu-builder-card-label");
            if (labelRow) {
                labelRow.textContent =
                    value || labelInput.getAttribute("placeholder")?.replace(/^Falls back to /, "") || "(no label)";
            }
            markDirty();
        }
    });
    tree.addEventListener("change", (event) => {
        const cb = event.target.closest("[data-disclosure-new-tab]");
        if (!cb) return;
        const card = cb.closest(".rcms-menu-builder-card");
        if (!card) return;
        card.dataset.openNewTab = cb.checked ? "true" : "false";
        markDirty();
    });

    // ------------------------------------------------------------------
    // Depth normalisation — a card can only sit at depth N if the
    // previous DOM card is at depth >= N - 1. Run after every
    // reorder so the tree stays valid.

    function normaliseDepths() {
        const cards = Array.from(tree.querySelectorAll(".rcms-menu-builder-card"));
        cards.forEach((card, i) => {
            if (i === 0) {
                setDepth(card, 0);
                return;
            }
            const prev = cards[i - 1];
            const prevDepth = parseInt(prev.dataset.depth || "0", 10);
            const myDepth = parseInt(card.dataset.depth || "0", 10);
            // Allow same depth, one deeper, or shallower; but no
            // jumps of more than one deeper.
            if (myDepth > prevDepth + 1) setDepth(card, prevDepth + 1);
        });
    }

    // ------------------------------------------------------------------
    // Save.

    saveBtn?.addEventListener("click", async () => {
        const cards = Array.from(tree.querySelectorAll(".rcms-menu-builder-card"));
        // Walk in order, computing parent_local_id from the most-
        // recent card at depth = myDepth - 1.
        const items = [];
        const depthToLocal = new Map(); // depth → most-recent local_id seen
        cards.forEach((card, i) => {
            const depth = parseInt(card.dataset.depth || "0", 10);
            const parentLocal = depth > 0 ? depthToLocal.get(depth - 1) ?? null : null;
            items.push({
                id: card.dataset.itemId ? parseInt(card.dataset.itemId, 10) : null,
                local_id: i,
                parent_local_id: parentLocal,
                label: card.dataset.label || "",
                page_id: card.dataset.pageId ? parseInt(card.dataset.pageId, 10) : null,
                external_url: card.dataset.externalUrl || null,
                open_in_new_tab: card.dataset.openNewTab === "true",
            });
            depthToLocal.set(depth, i);
            // Clear deeper-depth entries so a sibling later can't
            // accidentally adopt a nephew as its parent.
            for (let d = depth + 1; d <= MAX_DEPTH; d += 1) depthToLocal.delete(d);
        });
        const csrf = document.cookie
            .split(";")
            .map((s) => s.trim())
            .find((c) => c.startsWith("rustango_csrf="));
        const token = csrf ? decodeURIComponent(csrf.slice("rustango_csrf=".length)) : "";
        saveBtn.disabled = true;
        try {
            const resp = await fetch(`/cms-admin/navigation/${menuId}/save-tree`, {
                method: "POST",
                headers: {
                    "content-type": "application/json",
                    "x-csrf-token": token,
                },
                body: JSON.stringify({ items }),
            });
            if (!resp.ok) {
                const body = await resp.text();
                throw new Error(`Save failed (${resp.status}): ${body.slice(0, 200)}`);
            }
            const data = await resp.json();
            clearDirty();
            window.rcmsToast?.({
                level: "success",
                body: `Menu saved — ${data.items_saved} item${data.items_saved === 1 ? "" : "s"}.`,
            });
        } catch (e) {
            window.rcmsToast?.({ level: "error", body: e.message });
        } finally {
            saveBtn.disabled = false;
        }
    });

    // Custom link picker → tree.
    document.getElementById("menu-builder-custom-add")?.addEventListener("click", () => {
        const url = document.getElementById("menu-builder-custom-url").value.trim();
        const label = document.getElementById("menu-builder-custom-label").value.trim();
        if (!url || !label) {
            window.rcmsToast?.({ level: "warning", body: "Both URL and label are required." });
            return;
        }
        const fake = document.createElement("div");
        fake.dataset.label = label;
        fake.dataset.externalUrl = url;
        fake.dataset.pageId = "";
        const card = createCardFromPicker(fake);
        tree.appendChild(card);
        markDirty();
        removeEmptyState();
        document.getElementById("menu-builder-custom-url").value = "";
        document.getElementById("menu-builder-custom-label").value = "";
    });

    // Tab switching in the picker.
    document.querySelectorAll(".rcms-menu-builder-tab").forEach((tab) => {
        tab.addEventListener("click", () => {
            document.querySelectorAll(".rcms-menu-builder-tab").forEach((t) => {
                t.classList.remove("active");
                t.setAttribute("aria-selected", "false");
            });
            tab.classList.add("active");
            tab.setAttribute("aria-selected", "true");
            const target = tab.dataset.tab;
            document.querySelectorAll(".rcms-menu-builder-tab-panel").forEach((p) => {
                p.hidden = p.dataset.panel !== target;
            });
        });
    });

    // Page search.
    document.querySelector(".rcms-menu-builder-page-search")?.addEventListener("input", (event) => {
        const q = event.target.value.toLowerCase();
        document.querySelectorAll(".rcms-menu-builder-picker-item").forEach((item) => {
            const label = (item.dataset.label || "").toLowerCase();
            const url = (item.dataset.urlPath || "").toLowerCase();
            item.hidden = q && !label.includes(q) && !url.includes(q);
        });
    });

    // Warn on navigate-away when dirty.
    window.addEventListener("beforeunload", (event) => {
        if (!dirty) return;
        event.preventDefault();
        event.returnValue = "";
    });

    initialiseDepths();
    computeDepths();
})();
