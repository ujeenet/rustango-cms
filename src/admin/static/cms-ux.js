// CMS admin UX helpers (#25):
//   - `rcmsConfirm({ title, message, confirmLabel, danger })` -> Promise<bool>
//     via a <dialog> element. Replaces browser `confirm()` so destructive
//     actions get a styled, themeable prompt with proper focus management.
//   - `rcmsToast({ level, body, ttl })` pushes a toast onto a corner stack.
//     Replaces `<div class="rcms-app-flash">` banners with a dismissable,
//     timed toast that doesn't shove page content down on every page load.
//
// Auto-wiring:
//   1. Forms with `data-confirm="text"` intercept their submit, raise the
//      modal, and submit only on confirmation.
//   2. <a data-confirm="text"> links intercept the click the same way.
//   3. On `DOMContentLoaded`, every `.rcms-app-flash .rcms-flash` element gets
//      hoisted out of the DOM and re-rendered as a toast — back-compat
//      with handlers that still use `render_with_csrf` + the messages bag.
//
// All public API attached to `window` so handler templates can call
// `rcmsToast(...)` directly without import boilerplate.

(() => {
    const DIALOG_ID = "rcms-confirm-dialog";

    function ensureDialog() {
        let dlg = document.getElementById(DIALOG_ID);
        if (dlg) return dlg;
        dlg = document.createElement("dialog");
        dlg.id = DIALOG_ID;
        dlg.className = "rcms-confirm-dialog";
        dlg.innerHTML = `
            <form method="dialog" class="rcms-confirm-form">
                <h2 data-title>Confirm action</h2>
                <p data-message></p>
                <div class="rcms-confirm-actions">
                    <button type="button" class="rcms-btn rcms-btn-text" data-cancel>Cancel</button>
                    <button type="submit" class="rcms-btn" data-confirm-btn>Confirm</button>
                </div>
            </form>
        `;
        document.body.appendChild(dlg);
        return dlg;
    }

    function rcmsConfirm(options) {
        const opts = options || {};
        const dlg = ensureDialog();
        const titleEl = dlg.querySelector("[data-title]");
        const msgEl = dlg.querySelector("[data-message]");
        const confirmBtn = dlg.querySelector("[data-confirm-btn]");
        const cancelBtn = dlg.querySelector("[data-cancel]");
        titleEl.textContent = opts.title || "Are you sure?";
        msgEl.textContent = opts.message || "";
        confirmBtn.textContent = opts.confirmLabel || "Confirm";
        confirmBtn.classList.toggle("rcms-btn-danger", !!opts.danger);
        return new Promise((resolve) => {
            const onClose = () => {
                dlg.removeEventListener("close", onClose);
                cancelBtn.removeEventListener("click", onCancel);
                resolve(dlg.returnValue === "ok");
            };
            const onCancel = () => {
                dlg.returnValue = "cancel";
                dlg.close();
            };
            const onSubmit = (e) => {
                // Native dialog form-submit closes with returnValue ===
                // submit button's value. Default is "" so we coerce.
                dlg.returnValue = "ok";
            };
            dlg.addEventListener("close", onClose);
            cancelBtn.addEventListener("click", onCancel);
            confirmBtn.addEventListener("click", onSubmit, { once: true });
            dlg.showModal();
            // Focus management: default focus to Cancel so Enter
            // doesn't accidentally trigger destructive actions.
            setTimeout(() => cancelBtn.focus(), 0);
        });
    }

    function ensureStack() {
        let stack = document.getElementById("rcms-toast-stack");
        if (stack) return stack;
        stack = document.createElement("div");
        stack.id = "rcms-toast-stack";
        stack.className = "rcms-toast-stack";
        stack.setAttribute("role", "status");
        stack.setAttribute("aria-live", "polite");
        document.body.appendChild(stack);
        return stack;
    }

    const DEFAULT_TTL = {
        success: 4000,
        info: 5000,
        warning: 8000,
        error: 12000,
    };

    function rcmsToast(options) {
        const opts = options || {};
        const level = ["success", "info", "warning", "error"].includes(opts.level)
            ? opts.level
            : "info";
        const body = opts.body || "";
        const ttl = typeof opts.ttl === "number" ? opts.ttl : DEFAULT_TTL[level];
        const stack = ensureStack();
        const toast = document.createElement("div");
        toast.className = `rcms-toast rcms-toast--${level}`;
        const iconName = {
            success: "check_circle",
            info: "info",
            warning: "warning",
            error: "error",
        }[level];
        toast.innerHTML = `
            <span class="material-symbols-rounded rcms-toast-icon" aria-hidden="true">${iconName}</span>
            <span class="rcms-toast-body"></span>
            <button type="button" class="rcms-toast-close" aria-label="Dismiss">
                <span class="material-symbols-rounded">close</span>
            </button>
        `;
        toast.querySelector(".rcms-toast-body").textContent = body;
        stack.appendChild(toast);
        // Animate in next frame so transitions fire.
        requestAnimationFrame(() => toast.classList.add("rcms-toast--in"));
        const dismiss = () => {
            toast.classList.remove("rcms-toast--in");
            toast.classList.add("rcms-toast--out");
            setTimeout(() => toast.remove(), 200);
        };
        toast.querySelector(".rcms-toast-close").addEventListener("click", dismiss);
        if (ttl > 0) setTimeout(dismiss, ttl);
        return { dismiss };
    }

    // ------------------------------------------------------------------
    // Auto-wiring on DOM ready.

    function autowire() {
        // 1. Intercept forms with data-confirm. When the submit was
        //    triggered by a button carrying its own data-confirm
        //    (multi-action bars where each verb has a distinct
        //    prompt), the button's attribute wins over the form's.
        document.body.addEventListener("submit", async (event) => {
            const form = event.target;
            if (!(form instanceof HTMLFormElement)) return;
            const submitter = event.submitter;
            const msg = submitter?.dataset?.confirm || form.dataset.confirm;
            if (!msg) return;
            if (form.dataset.confirmed === "1") return;
            event.preventDefault();
            const style = submitter?.dataset?.confirmStyle || form.dataset.confirmStyle;
            const ok = await rcmsConfirm({
                message: msg,
                title: submitter?.dataset?.confirmTitle || form.dataset.confirmTitle || "Are you sure?",
                confirmLabel: submitter?.dataset?.confirmLabel || form.dataset.confirmLabel || "Confirm",
                danger: style === "danger",
            });
            if (ok) {
                form.dataset.confirmed = "1";
                // Re-create a hidden input echoing the submitter's
                // name/value so the resubmit still carries the
                // action that the user clicked (HTML's default form
                // submit drops the submitter on .submit()).
                if (submitter && submitter.name) {
                    const echo = document.createElement("input");
                    echo.type = "hidden";
                    echo.name = submitter.name;
                    echo.value = submitter.value;
                    echo.dataset.rcmsSubmitter = "1";
                    form.appendChild(echo);
                }
                form.submit();
            }
        });
        // 2. Intercept link clicks with data-confirm.
        document.body.addEventListener("click", async (event) => {
            const link = event.target.closest("a[data-confirm]");
            if (!link) return;
            if (link.dataset.confirmed === "1") return;
            event.preventDefault();
            const ok = await rcmsConfirm({
                message: link.dataset.confirm,
                title: link.dataset.confirmTitle || "Are you sure?",
                confirmLabel: link.dataset.confirmLabel || "Confirm",
                danger: link.dataset.confirmStyle === "danger",
            });
            if (ok) {
                link.dataset.confirmed = "1";
                window.location.href = link.href;
            }
        });
        // 3. Hoist server-rendered flash banners into toasts.
        const flashRegion = document.querySelector(".rcms-app-flash");
        if (flashRegion) {
            flashRegion
                .querySelectorAll(".rcms-flash")
                .forEach((node) => {
                    const level = (node.className.match(/flash--(\w+)/) || [])[1] || "info";
                    const body = node.querySelector(".rcms-flash-body")?.textContent?.trim() || node.textContent.trim();
                    if (body) rcmsToast({ level, body });
                });
            flashRegion.remove();
        }
        // 4. Cmd+S / Ctrl+S → submit the primary form (#63). Pages with
        //    a save-able form mark it `data-primary-save`. The
        //    listener attaches at the document level so the shortcut
        //    works regardless of focus (a stream-block textarea, etc.).
        document.addEventListener("keydown", (event) => {
            const isSaveCombo =
                (event.metaKey || event.ctrlKey) &&
                !event.altKey &&
                !event.shiftKey &&
                event.key.toLowerCase() === "s";
            if (!isSaveCombo) return;
            const form = document.querySelector("form[data-primary-save]");
            if (!form) return;
            event.preventDefault();
            // Pass through native form validity — if a required field
            // is empty, requestSubmit triggers the browser's UI hint
            // instead of POSTing silently.
            if (typeof form.requestSubmit === "function") {
                form.requestSubmit();
            } else {
                form.submit();
            }
            rcmsToast({ level: "info", body: "Saving…", ttl: 1500 });
        });
        // 5. #77 — Global search. `/` from anywhere focuses the
        //    topbar input. Live autocomplete on input (200ms debounce);
        //    Esc closes the dropdown; arrow keys navigate results;
        //    Enter on a highlighted result navigates to it.
        const searchInput = document.querySelector("[data-search-input]");
        const searchDropdown = document.querySelector("[data-search-dropdown]");
        const searchRoot = document.querySelector("[data-search-root]");
        if (searchInput && searchDropdown && searchRoot) {
            // Focusing a `display: none` input is a no-op, which is what
            // the collapsed rail does to this one — so widen the sidebar
            // first, then focus.
            const openSearch = () => {
                if (document.documentElement.hasAttribute("data-sidebar-collapsed")) {
                    window.rcmsExpandSidebar?.();
                }
                searchInput.focus();
                searchInput.select();
            };

            document.addEventListener("keydown", (event) => {
                if (event.key !== "/" || event.metaKey || event.ctrlKey) return;
                const tag = (event.target?.tagName || "").toLowerCase();
                if (tag === "input" || tag === "textarea" || tag === "select" || event.target?.isContentEditable) return;
                event.preventDefault();
                openSearch();
            });

            // On the rail the capsule is just an icon: clicking it did
            // nothing at all, since a <form> with no submit button and a
            // hidden input has nothing to trigger.
            searchRoot.addEventListener("click", (event) => {
                if (!document.documentElement.hasAttribute("data-sidebar-collapsed")) return;
                event.preventDefault();
                openSearch();
            });

            let debounceTimer = 0;
            let lastQuery = "";
            const renderDropdown = (data) => {
                const sections = [
                    { key: "pages", label: "Pages", icon: "article", urlPrefix: "/cms-admin/pages/", urlSuffix: "/edit" },
                    { key: "snippets", label: "Library", icon: "library_books", urlPrefix: "/cms-admin/library/", urlSuffix: "/edit" },
                    { key: "media", label: "Media", icon: "image", urlPrefix: "/cms-admin/media/", urlSuffix: "/edit" },
                ];
                let html = "";
                let totalHits = 0;
                for (const sec of sections) {
                    const items = data[sec.key] || [];
                    totalHits += items.length;
                    if (items.length === 0) continue;
                    html += `<div style="padding: 4px 12px; font-size: 11px; color: var(--md-sys-color-on-surface-variant); text-transform: uppercase; letter-spacing: 0.04em; background: var(--md-sys-color-surface-container);"><span class="material-symbols-rounded sm">${sec.icon}</span> ${sec.label}</div>`;
                    for (const item of items) {
                        const url = `${sec.urlPrefix}${item.id}${sec.urlSuffix}`;
                        const title = item.title || item.filename || `#${item.id}`;
                        const sub = sec.key === "pages" ? (item.url_path || item.slug) : (item.slug || item.filename || "");
                        html += `<a href="${url}" data-search-hit style="display: block; padding: 8px 12px; text-decoration: none; color: var(--md-sys-color-on-surface); border-top: 1px solid var(--md-sys-color-outline-variant);">
                            <strong>${escapeHtml(title)}</strong>
                            ${sub ? `<br><code style="font-size: 11px; color: var(--md-sys-color-on-surface-variant);">${escapeHtml(sub)}</code>` : ""}
                        </a>`;
                    }
                }
                if (totalHits === 0) {
                    html = `<div style="padding: 12px; color: var(--md-sys-color-on-surface-variant); font-size: 13px;">No matches for “${escapeHtml(data.q)}”.</div>`;
                } else {
                    html += `<a href="/cms-admin/search?q=${encodeURIComponent(data.q)}" style="display: block; padding: 8px 12px; text-decoration: none; color: var(--md-sys-color-primary); border-top: 1px solid var(--md-sys-color-outline-variant); font-size: 13px;">View all ${totalHits} result${totalHits !== 1 ? "s" : ""} →</a>`;
                }
                searchDropdown.innerHTML = html;
                searchDropdown.style.display = "block";
            };

            const doSearch = async (query) => {
                if (query === lastQuery) return;
                lastQuery = query;
                if (!query) {
                    searchDropdown.style.display = "none";
                    return;
                }
                try {
                    const res = await fetch(`/cms-admin/search?q=${encodeURIComponent(query)}&autocomplete=1`);
                    if (!res.ok) return;
                    const data = await res.json();
                    if (data.q !== query && data.q !== lastQuery) return; // stale
                    renderDropdown(data);
                } catch (_) {
                    // ignore
                }
            };

            searchInput.addEventListener("input", () => {
                clearTimeout(debounceTimer);
                debounceTimer = setTimeout(() => doSearch(searchInput.value.trim()), 200);
            });
            searchInput.addEventListener("keydown", (event) => {
                if (event.key === "Escape") {
                    searchDropdown.style.display = "none";
                    searchInput.blur();
                }
            });
            document.addEventListener("click", (event) => {
                if (!searchRoot.contains(event.target)) {
                    searchDropdown.style.display = "none";
                }
            });
        }

        // 6.1. #139 — banner dismissibles. Any element with
        //   `data-dismissible="<key>"` listens for clicks on a child
        //   `[data-dismiss-btn]`; on click, POST to the dismissals
        //   endpoint and hide the banner. Best-effort — UI hides even
        //   if the POST fails so the editor isn't blocked.
        document.querySelectorAll("[data-dismissible]").forEach((el) => {
            const btn = el.querySelector("[data-dismiss-btn]");
            if (!btn) return;
            const key = el.getAttribute("data-dismissible");
            btn.addEventListener("click", (event) => {
                event.preventDefault();
                el.style.display = "none";
                const csrf =
                    document.querySelector('input[name="csrfmiddlewaretoken"]')?.value ||
                    document.querySelector('meta[name="csrf-token"]')?.content ||
                    "";
                fetch(`/cms-admin/dismissibles/${encodeURIComponent(key)}`, {
                    method: "POST",
                    headers: { "X-CSRFToken": csrf },
                    credentials: "same-origin",
                }).catch(() => {
                    // Network error → keep dismissed locally for the
                    // session; it'll resurface next reload.
                });
            });
        });

        // 6.7. #267 — only one row-level kebab open at a time. The
        //   <details>'s `toggle` event doesn't bubble, so the
        //   document-level listener attaches in capture phase. When a
        //   `.rcms-kebab-menu` opens, close every other open kebab. Outside-
        //   click closes any open kebab. `Esc` is already wired in the
        //   keyboard-shortcuts section below.
        document.addEventListener("toggle", (event) => {
            const det = event.target;
            if (!(det instanceof HTMLDetailsElement)) return;
            if (!det.classList.contains("rcms-kebab-menu")) return;
            if (!det.open) return;
            document.querySelectorAll("details.rcms-kebab-menu[open]").forEach((other) => {
                if (other !== det) other.open = false;
            });
        }, true);
        document.addEventListener("click", (event) => {
            if (event.target?.closest?.("details.rcms-kebab-menu")) return;
            document.querySelectorAll("details.rcms-kebab-menu[open]").forEach((el) => {
                el.open = false;
            });
        });

        // 6.8. #262 — multipart-form CSRF round-trip. The framework's
        //   CSRF middleware only reads the token from a header or
        //   from a `_csrf` field in an `application/x-www-form-
        //   urlencoded` body. Native multipart submits don't carry
        //   either, so multipart POSTs (branding upload, snippet /
        //   redirect import, account-preferences avatar, media edit)
        //   silently 403 before reaching the handler. Hijack the
        //   submit, post via fetch with the `X-CSRF-Token` header
        //   read from the `rustango_csrf` cookie, then navigate to
        //   the final URL the server redirected to. Opt-out for
        //   forms that already drive their own fetch flow (media
        //   upload's staged-upload pipeline) via `data-multipart-no-hijack`.
        wireMultipartCsrf();

        // 6.10. Phases 4–5 — page / snippet / document chooser
        //   overlay. Any `[data-chooser]` wraps a hidden input + an
        //   "open" button + an optional clear button. On open, we
        //   materialise a lazy `<dialog>` with a debounced filter
        //   that hits `/cms-admin/__<kind>-chooser` and renders a
        //   list of matching rows. Clicking a row closes the dialog
        //   + populates the hidden input + the visible label.
        wireChoosers();
        // Server-rendered media widgets only know the raw id ("#42") —
        // resolve titles in one batched request so labels read as names.
        hydrateMediaChooserLabels();

        // Wagtail TitleFieldPanel parity. Any
        //   `<input data-slug-source="#id_title">` auto-fills from
        //   slugified source-field text until the editor manually
        //   edits the slug field (then sync stops for the session).
        //   On edit screens (where `data-slug-locked="1"` is set
        //   server-side), sync is disabled from the start.
        wireSlugSync();

        // 6.20. #205 — markdown toolbar enhancer. Any
        //   `[data-md-toolbar-for]` block becomes a working toolbar
        //   that inserts markdown syntax at the cursor of its
        //   target textarea. Progressive enhancement — the textarea
        //   keeps working without JS; the toolbar shows only when
        //   we've attached.
        wireMarkdownToolbars();

        // 6.22. #263 — richtext preview-on-blur. Each
        //   `textarea[data-widget-mode="richtext"]` gets a small
        //   Preview / Edit toggle and a sibling preview pane. On
        //   blur (or explicit toggle click) the textarea source
        //   POSTs to `/cms-admin/__richtext-preview` and the
        //   returned sanitized HTML lands in the preview pane.
        //   Source is still the source of truth — the preview is
        //   purely a viewport.
        wireRichtextPreview();

        // 6.24. #243 — multi-value widget shim. Any
        //   `[data-widget-multivalue]` group (checkboxes / multiselect /
        //   snippetm2m) authoritatively drives its visible control from
        //   the persisted hidden JSON-array value on init, then mirrors
        //   selections back into that hidden input on change so the
        //   chosen ids actually POST. Without this the visible control
        //   has no `name` and selections are never submitted.
        wireMultiValueWidgets();
        wireMissingThumbnails();
        wireSelectAll();

        // 6.5. #133 — per-user column picker. Any `.rcms-data` table that
        //   declares `data-col-picker="<list-name>"` gets a Columns
        //   button next to its toolbar. `<th data-col-key="x">` cells
        //   become toggle entries; matching `<td>` cells hide via the
        //   `rcms-col-hidden` class. Selection persists in localStorage
        //   keyed by (list-name, tenant-slug).
        wireColumnPickers();

        // 6.26. #418 — unsaved-changes guard. Any edit form marked
        //   `data-primary-save` raises the browser's native "discard
        //   changes?" prompt on navigate-away / tab-close once a field
        //   has been edited. A genuine save (the primary submit, via
        //   click or Cmd+S) and the page editor's draft autosave clear
        //   the guard so it never double-prompts. Opt out per-form with
        //   `data-no-unsaved-warning`.
        wireUnsavedChangesGuard();

        // 6. #121 — keyboard shortcuts beyond Cmd+S.
        //   • `?` opens a cheat-sheet modal listing every binding.
        //   • `Esc` closes open <details>, dismisses the cheat sheet,
        //     blurs the active input.
        //   Body may opt out with `data-no-shortcuts`.
        if (!document.body.hasAttribute("data-no-shortcuts")) {
            wireShortcuts();
        }
    }

    // Phases 4–5 — shared chooser overlay. Lazy-mounted single
    // `<dialog>` overlay reused across page / snippet / document
    // choosers. The `data-chooser-kind` attribute on each
    // `[data-chooser]` widget routes to the right JSON endpoint;
    // optional `data-chooser-filter` further narrows snippet
    // choosers to one type_name.
    const CHOOSER_CONFIG = {
        page:     { url: "/cms-admin/__page-chooser",     placeholder: "Choose a page…",     subKey: "url_path",  emptyText: "No pages yet.",     icon: "article" },
        snippet:  { url: "/cms-admin/__snippet-chooser",  placeholder: "Choose a snippet…",  subKey: "slug",       emptyText: "No snippets yet.",  icon: "library_books" },
        document: { url: "/cms-admin/__document-chooser", placeholder: "Choose a document…", subKey: "filename",   emptyText: "No documents yet.", icon: "description" },
        // The media kind gets the full picker experience (grid/list views,
        // detail pane, collection filter, inline upload) backed by the
        // richer /__media-picker endpoint; `media: true` routes it to that
        // UI in openChooserDialog/doMediaPickerFetch.
        media:    { url: "/cms-admin/__media-picker",     placeholder: "Choose an image…",   subKey: "filename",   emptyText: "No media yet.",     icon: "image", media: true },
    };
    // #421 — generic ChooserViewSet. A `data-chooser-kind` that isn't
    // one of the built-ins is treated as a registered chooser
    // slug, served by the generic `/cms-admin/__chooser/<slug>`
    // endpoint (which normalises every row to `{id, title, sub, …}`).
    // So registering a chooser needs no JS change — the widget just
    // sets its slug as the kind.
    function chooserCfg(kind) {
        return (
            CHOOSER_CONFIG[kind] || {
                url: "/cms-admin/__chooser/" + encodeURIComponent(kind || ""),
                placeholder: "Choose…",
                subKey: "sub",
                emptyText: "No matches.",
                icon: "article",
            }
        );
    }
    const escapeHtml = (s) => String(s ?? "")
        .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
    let chooserDialog = null;
    let chooserActiveTarget = null;
    let chooserActiveKind = null;
    let chooserDebounce = 0;
    // #294 — programmatic (non-form-widget) chooser use, e.g. the richtext
    // editor's "link to page" button. When `chooserOnPick` is set the
    // single-pick path resolves it with the chosen item instead of filling
    // a `[data-chooser]` widget; `chooserActiveFilter` supplies the
    // type_name filter that a widget would otherwise carry. Both are null
    // for ordinary form-widget choosers, so that path is unchanged.
    let chooserOnPick = null;
    let chooserActiveFilter = null;

    function ensureChooserDialog() {
        if (chooserDialog) return chooserDialog;
        chooserDialog = document.createElement("dialog");
        chooserDialog.className = "rcms-chooser-dialog";
        chooserDialog.setAttribute("aria-label", "Choose an item");
        chooserDialog.innerHTML = `
            <article class="rcms-picker">
                <header class="rcms-picker__header">
                    <span class="material-symbols-rounded" data-pc-icon>article</span>
                    <strong data-pc-heading>Choose an item</strong>
                    <button type="button" class="rcms-btn rcms-btn-text rcms-btn-small" data-pc-close>
                        <span class="material-symbols-rounded sm">close</span>
                    </button>
                </header>
                <div class="rcms-picker__toolbar">
                    <input type="text" data-pc-filter class="rcms-picker__search" placeholder="Filter…">
                    <span data-pc-media-tools class="rcms-picker__media-tools" hidden>
                        <select data-pc-collection class="rcms-picker__collection" aria-label="Collection"></select>
                        <span class="rcms-picker__viewtoggle" role="group" aria-label="View">
                            <button type="button" data-pc-view="grid" aria-pressed="true" title="Grid view">
                                <span class="material-symbols-rounded sm">grid_view</span>
                            </button>
                            <button type="button" data-pc-view="list" aria-pressed="false" title="List view">
                                <span class="material-symbols-rounded sm">view_list</span>
                            </button>
                        </span>
                        <button type="button" class="rcms-btn rcms-btn-outlined rcms-btn-small" data-pc-upload>
                            <span class="material-symbols-rounded sm">upload</span> Upload
                        </button>
                        <input type="file" data-pc-file hidden multiple accept="image/*">
                    </span>
                </div>
                <div class="rcms-picker__body">
                    <ul data-pc-results class="rcms-picker__results"></ul>
                    <div data-pc-media-body class="rcms-picker__media" hidden>
                        <div data-pc-media-results class="rcms-picker__grid" role="listbox" aria-label="Media"></div>
                        <aside data-pc-detail class="rcms-picker__detail"></aside>
                        <div data-pc-dropzone class="rcms-picker__dropzone" hidden>
                            <span class="material-symbols-rounded">upload</span>
                            Drop images to upload
                        </div>
                    </div>
                </div>
                <footer class="rcms-picker__footer">
                    <span data-pc-notice class="rcms-picker__notice"></span>
                    <button type="button" class="rcms-btn rcms-btn-text rcms-btn-small" data-pc-loadmore hidden>Load more</button>
                    <span data-pc-multi-footer class="rcms-picker__multi" style="display: none;">
                        <span data-pc-multi-count>0 selected</span>
                        <button type="button" class="rcms-btn rcms-btn-text rcms-btn-small" data-pc-multi-cancel>Cancel</button>
                        <button type="button" class="rcms-btn rcms-btn-small" data-pc-multi-pick>Pick selected</button>
                    </span>
                    <button type="button" class="rcms-btn rcms-btn-small" data-pc-choose hidden disabled>Choose</button>
                </footer>
            </article>
        `;
        document.body.appendChild(chooserDialog);

        const closeBtn = chooserDialog.querySelector("[data-pc-close]");
        closeBtn?.addEventListener("click", () => chooserDialog.close());
        chooserDialog.addEventListener("click", (e) => {
            if (e.target === chooserDialog) chooserDialog.close();
        });
        // #294 — a programmatic open dismissed without a pick resolves its
        // callback with null (so the editor link flow can simply abort).
        chooserDialog.addEventListener("close", () => {
            if (chooserOnPick) {
                const cb = chooserOnPick;
                chooserOnPick = null;
                cb(null);
            }
        });

        const filter = chooserDialog.querySelector("[data-pc-filter]");
        const results = chooserDialog.querySelector("[data-pc-results]");
        filter.addEventListener("input", () => {
            clearTimeout(chooserDebounce);
            chooserDebounce = setTimeout(() => {
                if (chooserCfg(chooserActiveKind).media) {
                    mediaState.q = filter.value.trim();
                    doMediaPickerFetch({ reset: true });
                } else {
                    doChooserFetch(filter.value);
                }
            }, 150);
        });
        results.addEventListener("click", (e) => {
            const row = e.target.closest("[data-pc-row]");
            if (!row) return;
            // #294 — programmatic consumer (e.g. the richtext "link to
            // page" button) consumes the pick via a one-shot callback.
            if (chooserOnPick) {
                const cb = chooserOnPick;
                chooserOnPick = null;
                cb({ id: row.dataset.id, title: row.dataset.title });
                chooserDialog.close();
                return;
            }
            if (!chooserActiveTarget) return;
            const multi = chooserActiveTarget.getAttribute("data-chooser-multi") === "1";
            if (multi) {
                // Toggle the row's checkbox + repaint the count.
                const cb = row.querySelector("input[type=checkbox]");
                if (cb && e.target !== cb) cb.checked = !cb.checked;
                refreshMultiCount();
                return;
            }
            applyChooserSelection(chooserActiveTarget, {
                id: row.dataset.id,
                title: row.dataset.title,
            });
            chooserDialog.close();
        });

        // Multi-pick footer actions.
        chooserDialog.querySelector("[data-pc-multi-cancel]")
            ?.addEventListener("click", () => chooserDialog.close());
        chooserDialog.querySelector("[data-pc-multi-pick]")
            ?.addEventListener("click", () => {
                if (!chooserActiveTarget) return;
                const picked = Array.from(
                    chooserDialog.querySelectorAll("input[type=checkbox]:checked")
                ).map((cb) => ({
                    id: cb.dataset.id,
                    title: cb.dataset.title,
                }));
                applyChooserMultiSelection(chooserActiveTarget, picked);
                chooserDialog.close();
            });

        // ---- media picker controls (inert for other kinds) ----
        chooserDialog.querySelector("[data-pc-collection]")?.addEventListener("change", (e) => {
            mediaState.collection = e.target.value;
            doMediaPickerFetch({ reset: true });
        });
        chooserDialog.querySelectorAll("[data-pc-view]").forEach((btn) => {
            btn.addEventListener("click", () => setPickerView(btn.getAttribute("data-pc-view")));
        });
        chooserDialog.querySelector("[data-pc-loadmore]")?.addEventListener("click", () => {
            mediaState.offset += 60;
            doMediaPickerFetch({});
        });
        const mediaResults = chooserDialog.querySelector("[data-pc-media-results]");
        mediaResults?.addEventListener("click", (e) => {
            const item = e.target.closest("[data-pc-item]");
            if (!item) return;
            const multi = chooserActiveTarget?.getAttribute("data-chooser-multi") === "1";
            if (multi) {
                const cb = item.querySelector("input[type=checkbox]");
                if (cb && e.target !== cb) cb.checked = !cb.checked;
                refreshMultiCount();
            }
            selectMediaItem(item.dataset.id);
        });
        mediaResults?.addEventListener("dblclick", (e) => {
            const item = e.target.closest("[data-pc-item]");
            if (!item) return;
            const multi = chooserActiveTarget?.getAttribute("data-chooser-multi") === "1";
            if (!multi) confirmMediaPick(item.dataset.id);
        });
        chooserDialog.querySelector("[data-pc-choose]")?.addEventListener("click", () => {
            if (mediaState.selectedId != null) confirmMediaPick(mediaState.selectedId);
        });
        // Inline upload — button + hidden file input + drag & drop over the
        // dialog (media mode only).
        const fileInput = chooserDialog.querySelector("[data-pc-file]");
        chooserDialog.querySelector("[data-pc-upload]")?.addEventListener("click", () => fileInput?.click());
        fileInput?.addEventListener("change", () => {
            if (fileInput.files?.length) mediaPickerUpload(fileInput.files);
            fileInput.value = "";
        });
        const dropzone = chooserDialog.querySelector("[data-pc-dropzone]");
        chooserDialog.addEventListener("dragover", (e) => {
            if (!chooserCfg(chooserActiveKind).media) return;
            if (!e.dataTransfer?.types?.includes("Files")) return;
            e.preventDefault();
            chooserDialog.classList.add("is-dragover");
            if (dropzone) dropzone.hidden = false;
        });
        chooserDialog.addEventListener("dragleave", (e) => {
            if (e.target !== chooserDialog && chooserDialog.contains(e.relatedTarget)) return;
            chooserDialog.classList.remove("is-dragover");
            if (dropzone) dropzone.hidden = true;
        });
        chooserDialog.addEventListener("drop", (e) => {
            if (!chooserCfg(chooserActiveKind).media) return;
            e.preventDefault();
            chooserDialog.classList.remove("is-dragover");
            if (dropzone) dropzone.hidden = true;
            if (e.dataTransfer?.files?.length) mediaPickerUpload(e.dataTransfer.files);
        });

        return chooserDialog;
    }

    function refreshMultiCount() {
        if (!chooserDialog) return;
        const checked = chooserDialog.querySelectorAll("input[type=checkbox]:checked").length;
        const counter = chooserDialog.querySelector("[data-pc-multi-count]");
        if (counter) counter.textContent = `${checked} selected`;
    }

    // ---- media picker (kind="media") -------------------------------------
    // Per-open state; only the grid/list preference persists across opens.
    let mediaState = null;
    function resetMediaState() {
        let view = "grid";
        try { if (localStorage.getItem("rcms.pickerView") === "list") view = "list"; } catch (e) { /* private mode */ }
        mediaState = {
            q: "", collection: "", view,
            offset: 0, total: 0, hasMore: false,
            items: new Map(), order: [],
            collections: [], uncategorizedCount: 0,
            selectedId: null, uploading: false,
        };
    }

    function formatBytes(n) {
        if (!Number.isFinite(n) || n < 0) return "";
        if (n < 1024) return `${n} B`;
        if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
        return `${(n / (1024 * 1024)).toFixed(1)} MB`;
    }

    function pickerNotice(text, isError) {
        const el = chooserDialog?.querySelector("[data-pc-notice]");
        if (!el) return;
        el.textContent = text || "";
        el.classList.toggle("rcms-picker__notice--error", !!isError);
    }

    async function doMediaPickerFetch(opts) {
        opts = opts || {};
        const dialog = ensureChooserDialog();
        const grid = dialog.querySelector("[data-pc-media-results]");
        const loadmore = dialog.querySelector("[data-pc-loadmore]");
        if (opts.reset) {
            mediaState.offset = 0;
            mediaState.items.clear();
            mediaState.order = [];
            mediaState.selectedId = null;
            renderMediaDetail(null);
        }
        try {
            const params = new URLSearchParams();
            if (mediaState.q) params.set("q", mediaState.q);
            if (mediaState.collection !== "") params.set("collection", mediaState.collection);
            const kindAttr = chooserActiveTarget?.getAttribute("data-chooser-media-kind");
            if (kindAttr) params.set("kind", kindAttr);
            params.set("limit", "60");
            params.set("offset", String(mediaState.offset));
            const resp = await fetch(`${chooserCfg("media").url}?${params}`, { credentials: "same-origin" });
            if (!resp.ok) {
                grid.innerHTML = `<div class="rcms-picker__error">Lookup failed (HTTP ${resp.status}).</div>`;
                return;
            }
            const data = await resp.json();
            mediaState.total = data.total || 0;
            mediaState.hasMore = !!data.has_more;
            mediaState.collections = data.collections || [];
            mediaState.uncategorizedCount = data.uncategorized_count || 0;
            for (const it of (data.items || [])) {
                const key = String(it.id);
                if (!mediaState.items.has(key)) {
                    mediaState.items.set(key, it);
                    mediaState.order.push(key);
                }
            }
            renderCollectionOptions();
            renderMediaItems();
            if (loadmore) loadmore.hidden = !mediaState.hasMore;
        } catch (err) {
            grid.innerHTML = `<div class="rcms-picker__error">Network error: ${err.message}</div>`;
        }
    }

    function renderCollectionOptions() {
        const select = chooserDialog?.querySelector("[data-pc-collection]");
        if (!select) return;
        const current = mediaState.collection;
        const opts = [
            `<option value="">All collections</option>`,
            `<option value="0">Uncategorized (${mediaState.uncategorizedCount})</option>`,
        ];
        for (const c of mediaState.collections) {
            const indent = "&mdash; ".repeat(c.depth || 0);
            opts.push(`<option value="${c.id}">${indent}${escapeHtml(c.name)} (${c.count ?? 0})</option>`);
        }
        select.innerHTML = opts.join("");
        select.value = current;
        if (select.value !== current) select.value = "";
    }

    function renderMediaItems() {
        const container = chooserDialog?.querySelector("[data-pc-media-results]");
        if (!container) return;
        const isGrid = mediaState.view === "grid";
        container.className = isGrid ? "rcms-picker__grid" : "rcms-picker__list";
        const multi = chooserActiveTarget?.getAttribute("data-chooser-multi") === "1";
        const preSelected = new Set(
            (chooserActiveTarget?.querySelector("[data-chooser-input]")?.value || "")
                .split(",").map((s) => s.trim()).filter(Boolean)
        );
        // Keep checkbox state across re-renders (view toggle / load more).
        for (const cb of container.querySelectorAll("input[type=checkbox]:checked")) {
            preSelected.add(String(cb.dataset.id));
        }
        if (mediaState.order.length === 0) {
            container.innerHTML = `<div class="rcms-picker__empty">${mediaState.q ? `No matches for "${escapeHtml(mediaState.q)}".` : "No media yet — upload your first image."}</div>`;
            return;
        }
        container.innerHTML = mediaState.order.map((key) => {
            const it = mediaState.items.get(key);
            const selected = String(mediaState.selectedId) === key ? " rcms-picker-card--selected" : "";
            const checked = preSelected.has(key) ? "checked" : "";
            const checkboxHtml = multi
                ? `<input type="checkbox" class="rcms-picker-card__check" data-id="${it.id}" data-title="${escapeHtml(it.title || "")}" ${checked}>`
                : "";
            if (isGrid) {
                const thumb = it.thumb_url
                    ? `<img class="rcms-picker-card__thumb" src="${escapeHtml(it.thumb_url)}" alt="" loading="lazy">`
                    : `<span class="rcms-picker-card__icon material-symbols-rounded">${it.kind === "document" ? "description" : "draft"}</span>`;
                return `
                <button type="button" class="rcms-picker-card${selected}" data-pc-item
                        data-id="${it.id}" data-title="${escapeHtml(it.title || "")}" title="${escapeHtml(it.filename || "")}">
                    ${checkboxHtml}${thumb}
                    <span class="rcms-picker-card__name">${escapeHtml(it.title || it.filename || "#" + it.id)}</span>
                </button>`;
            }
            const rowThumb = it.thumb_url
                ? `<img class="rcms-picker-row__thumb" src="${escapeHtml(it.thumb_url)}" alt="" loading="lazy">`
                : `<span class="rcms-picker-row__thumb rcms-picker-row__thumb--icon material-symbols-rounded">${it.kind === "document" ? "description" : "draft"}</span>`;
            return `
            <button type="button" class="rcms-picker-row rcms-picker-row--media${selected}" data-pc-item
                    data-id="${it.id}" data-title="${escapeHtml(it.title || "")}">
                ${checkboxHtml}${rowThumb}
                <span class="rcms-picker-row__text">
                    <span class="rcms-picker-row__title">${escapeHtml(it.title || "")}</span>
                    <span class="rcms-picker-row__sub">${escapeHtml(it.filename || "")}</span>
                </span>
                <span class="rcms-picker-row__meta">${it.width && it.height ? `${it.width}×${it.height}` : ""}</span>
                <span class="rcms-picker-row__meta">${formatBytes(it.size)}</span>
            </button>`;
        }).join("");
        if (multi) refreshMultiCount();
    }

    function selectMediaItem(id) {
        mediaState.selectedId = id;
        const container = chooserDialog?.querySelector("[data-pc-media-results]");
        container?.querySelectorAll("[data-pc-item]").forEach((el) => {
            el.classList.toggle("rcms-picker-card--selected", el.dataset.id === String(id));
        });
        renderMediaDetail(mediaState.items.get(String(id)) || null);
        const chooseBtn = chooserDialog?.querySelector("[data-pc-choose]");
        if (chooseBtn) chooseBtn.disabled = mediaState.selectedId == null;
    }

    function renderMediaDetail(it) {
        const pane = chooserDialog?.querySelector("[data-pc-detail]");
        if (!pane) return;
        if (!it) {
            pane.innerHTML = `<div class="rcms-picker__detail-empty">Select an item to preview</div>`;
            return;
        }
        const collectionName = it.collection_id == null
            ? "Uncategorized"
            : (mediaState.collections.find((c) => c.id === it.collection_id)?.name || `#${it.collection_id}`);
        const img = it.preview_url || it.thumb_url;
        pane.innerHTML = `
            ${img ? `<img class="rcms-picker__detail-img" src="${escapeHtml(img)}" alt="">` : ""}
            <div class="rcms-picker__detail-name">${escapeHtml(it.title || it.filename || "")}</div>
            <dl class="rcms-picker__detail-meta">
                <dt>File</dt><dd>${escapeHtml(it.filename || "")}</dd>
                ${it.width && it.height ? `<dt>Dimensions</dt><dd>${it.width} × ${it.height}</dd>` : ""}
                <dt>Size</dt><dd>${formatBytes(it.size)}</dd>
                <dt>Type</dt><dd>${escapeHtml(it.mime || "")}</dd>
                <dt>Collection</dt><dd>${escapeHtml(collectionName)}</dd>
                ${it.alt_text ? `<dt>Alt text</dt><dd>${escapeHtml(it.alt_text)}</dd>` : ""}
            </dl>`;
    }

    function setPickerView(mode) {
        mediaState.view = mode === "list" ? "list" : "grid";
        try { localStorage.setItem("rcms.pickerView", mediaState.view); } catch (e) { /* private mode */ }
        chooserDialog?.querySelectorAll("[data-pc-view]").forEach((btn) => {
            btn.setAttribute("aria-pressed", btn.getAttribute("data-pc-view") === mediaState.view ? "true" : "false");
        });
        renderMediaItems();
    }

    function confirmMediaPick(id) {
        const it = mediaState.items.get(String(id));
        const picked = it ? { id: it.id, title: it.title || it.filename || "" } : { id, title: "" };
        if (chooserOnPick) {
            const cb = chooserOnPick;
            chooserOnPick = null;
            cb(picked);
            chooserDialog.close();
            return;
        }
        if (!chooserActiveTarget) return;
        applyChooserSelection(chooserActiveTarget, picked);
        chooserDialog.close();
    }

    function getCsrfToken() {
        const m = document.cookie.match(/(?:^|;\s*)rustango_csrf=([^;]+)/);
        return m ? decodeURIComponent(m[1]) : "";
    }

    async function mediaPickerUpload(fileList) {
        if (mediaState.uploading) return;
        const kindAttr = chooserActiveTarget?.getAttribute("data-chooser-media-kind");
        const files = Array.from(fileList).filter(
            (f) => kindAttr === "any" || f.type.startsWith("image/")
        );
        if (files.length === 0) {
            pickerNotice("Only images can be uploaded here.", true);
            return;
        }
        const csrf = getCsrfToken();
        if (!csrf) {
            pickerNotice("Session expired — reload and sign in again.", true);
            return;
        }
        mediaState.uploading = true;
        const uploadBtn = chooserDialog?.querySelector("[data-pc-upload]");
        if (uploadBtn) uploadBtn.disabled = true;
        pickerNotice(`Uploading ${files.length} file${files.length > 1 ? "s" : ""}…`);
        const staged = [];
        const existingIds = [];
        const failures = [];
        try {
            for (const file of files) {
                const fd = new FormData();
                fd.append("file", file, file.name);
                const resp = await fetch("/cms-admin/media/upload-staged", {
                    method: "POST",
                    credentials: "same-origin",
                    headers: { "X-CSRF-Token": csrf },
                    body: fd,
                });
                if (!resp.ok) {
                    failures.push(`${file.name} (HTTP ${resp.status})`);
                    continue;
                }
                const j = await resp.json();
                if (j.id != null) staged.push({ id: j.id, name: file.name });
                else if (j.duplicate_of != null) existingIds.push(j.duplicate_of);
                else failures.push(file.name);
            }
            let committed = [];
            if (staged.length > 0) {
                const activeCollection =
                    mediaState.collection && mediaState.collection !== "0"
                        ? Number(mediaState.collection)
                        : null;
                const resp = await fetch("/cms-admin/media/upload-staged/commit", {
                    method: "POST",
                    credentials: "same-origin",
                    headers: { "X-CSRF-Token": csrf, "Content-Type": "application/json" },
                    body: JSON.stringify({
                        items: staged.map((s) => ({
                            uploaded_file_id: s.id,
                            title: s.name.replace(/\.[^.]+$/, ""),
                            alt_text: "",
                            collection_id: activeCollection,
                        })),
                    }),
                });
                if (resp.ok) {
                    const j = await resp.json();
                    committed = j.committed || [];
                    for (const f of j.failed || []) failures.push(f.filename || "commit failed");
                } else {
                    failures.push(`commit failed (HTTP ${resp.status})`);
                }
            }
            const ids = committed.concat(existingIds);
            const bits = [];
            if (committed.length) bits.push(`Uploaded ${committed.length}`);
            if (existingIds.length) bits.push(`${existingIds.length} already in library — selected existing`);
            if (failures.length) bits.push(`Failed: ${failures.join(", ")}`);
            pickerNotice(bits.join(" · "), failures.length > 0);
            if (ids.length > 0) {
                const multi = chooserActiveTarget?.getAttribute("data-chooser-multi") === "1";
                await doMediaPickerFetch({ reset: true });
                if (multi) {
                    const container = chooserDialog?.querySelector("[data-pc-media-results]");
                    for (const id of ids) {
                        const cb = container?.querySelector(`input[type=checkbox][data-id="${id}"]`);
                        if (cb) cb.checked = true;
                    }
                    refreshMultiCount();
                } else {
                    confirmMediaPick(ids[0]);
                }
            }
        } catch (err) {
            pickerNotice(`Upload error: ${err.message}`, true);
        } finally {
            mediaState.uploading = false;
            if (uploadBtn) uploadBtn.disabled = false;
        }
    }

    async function doChooserFetch(q) {
        const dialog = ensureChooserDialog();
        const results = dialog.querySelector("[data-pc-results]");
        const cfg = chooserCfg(chooserActiveKind);
        try {
            const params = new URLSearchParams();
            if (q) params.set("q", q);
            const filterAttr = chooserActiveFilter || chooserActiveTarget?.getAttribute("data-chooser-filter");
            if (filterAttr) params.set("type_name", filterAttr);
            const url = `${cfg.url}${params.toString() ? "?" + params.toString() : ""}`;
            const resp = await fetch(url, { credentials: "same-origin" });
            if (!resp.ok) {
                results.innerHTML = `<li class="rcms-picker__error">Lookup failed (HTTP ${resp.status}).</li>`;
                return;
            }
            const data = await resp.json();
            const items = data.items || [];
            if (items.length === 0) {
                results.innerHTML = `<li class="rcms-picker__empty">${q ? `No matches for "${escapeHtml(q)}".` : cfg.emptyText}</li>`;
                return;
            }
            const multi = chooserActiveTarget?.getAttribute("data-chooser-multi") === "1";
            // Pre-selected ids — comma-separated in the hidden input
            // when multi. Highlights / pre-checks rows already picked.
            const preSelected = new Set(
                (chooserActiveTarget?.querySelector("[data-chooser-input]")?.value || "")
                    .split(",")
                    .map((s) => s.trim())
                    .filter(Boolean)
            );
            results.innerHTML = items.map((it) => {
                const sub = escapeHtml(it[cfg.subKey] || "");
                const tag = it.status || it.kind || it.type_name || "";
                const checked = preSelected.has(String(it.id)) ? "checked" : "";
                const checkboxHtml = multi
                    ? `<input type="checkbox" data-id="${it.id}" data-title="${escapeHtml(it.title || "")}" ${checked}>`
                    : "";
                // Image media → show a thumbnail; other choosers (page/snippet/
                // document) have no `kind: "image"` so render no thumb.
                const thumbHtml = (it.kind === "image" && it.id != null)
                    ? `<img class="rcms-picker-row__thumb" src="/__media__/raw/${it.id}" alt="" loading="lazy"
                           onerror="this.style.display='none'">`
                    : "";
                return `
                <li data-pc-row class="rcms-picker-row"
                    data-id="${it.id}"
                    data-title="${escapeHtml(it.title || "")}">
                    ${checkboxHtml}${thumbHtml}
                    <div class="rcms-picker-row__text">
                        <div class="rcms-picker-row__title">${escapeHtml(it.title || "")}</div>
                        ${sub ? `<div class="rcms-picker-row__sub">${sub}</div>` : ""}
                    </div>
                    ${tag ? `<span class="rcms-tag rcms-tag-small">${escapeHtml(tag)}</span>` : ""}
                </li>`;
            }).join("");
            if (multi) refreshMultiCount();
        } catch (err) {
            results.innerHTML = `<li class="rcms-picker__error">Network error: ${err.message}</li>`;
        }
    }

    function applyChooserMultiSelection(target, items) {
        const hidden = target.querySelector("[data-chooser-input]");
        const label = target.querySelector("[data-chooser-label]");
        const ids = items.map((it) => String(it.id)).join(",");
        if (hidden) {
            hidden.value = ids;
            hidden.dispatchEvent(new Event("input", { bubbles: true }));
            hidden.dispatchEvent(new Event("change", { bubbles: true }));
        }
        if (label) {
            const kind = target.getAttribute("data-chooser-kind") || "page";
            const cfg = chooserCfg(kind);
            if (items.length === 0) {
                label.textContent = chooserPrompt(target, cfg);
            } else if (items.length === 1) {
                label.textContent = items[0].title || `#${items[0].id}`;
            } else {
                label.textContent = `${items.length} selected`;
            }
        }
    }

    // The widget's own (translated, type-aware) prompt, e.g. "Choose a
    // form…"; the kind's English default when a widget doesn't carry one.
    function chooserPrompt(target, cfg) {
        return target?.getAttribute("data-chooser-placeholder") || cfg.placeholder;
    }

    function applyChooserSelection(target, item) {
        const hidden = target.querySelector("[data-chooser-input]");
        const label = target.querySelector("[data-chooser-label]");
        if (hidden) {
            hidden.value = item.id || "";
            hidden.dispatchEvent(new Event("input", { bubbles: true }));
            hidden.dispatchEvent(new Event("change", { bubbles: true }));
        }
        if (label) {
            const kind = target.getAttribute("data-chooser-kind") || "page";
            const cfg = chooserCfg(kind);
            label.textContent = item.title || (item.id ? `#${item.id}` : chooserPrompt(target, cfg));
        }
        // Live image preview (media chooser) + clear-button visibility, so a
        // freshly-picked image shows immediately and can be cleared.
        const preview = target.querySelector("[data-chooser-preview]");
        if (preview) {
            if (item.id) {
                preview.src = "/__media__/raw/" + item.id;
                preview.hidden = false;
                preview.style.display = "";
            } else {
                preview.hidden = true;
                preview.removeAttribute("src");
            }
        }
        const clearBtn = target.querySelector("[data-chooser-clear]");
        if (clearBtn) clearBtn.style.display = item.id ? "" : "none";
    }

    // Shared open sequence for both entry points (form widgets +
    // programmatic rcmsOpenChooser). Sets module state, adapts the dialog
    // chrome to the kind (media gets the picker toolbar/grid/detail; other
    // kinds keep the plain list), kicks off the first fetch, shows the
    // modal, and focuses the search box.
    function openChooserDialog(kind, opts) {
        opts = opts || {};
        const dialog = ensureChooserDialog();
        chooserActiveTarget = opts.target || null;
        chooserActiveKind = kind || "page";
        chooserActiveFilter = opts.filter || null;
        chooserOnPick = opts.onPick || null;
        const cfg = chooserCfg(chooserActiveKind);
        const isMedia = !!cfg.media;
        const multi = chooserActiveTarget?.getAttribute("data-chooser-multi") === "1";
        dialog.classList.toggle("rcms-chooser-dialog--media", isMedia);
        const heading = dialog.querySelector("[data-pc-heading]");
        if (heading) heading.textContent = chooserPrompt(chooserActiveTarget, cfg).replace("…", "");
        const icon = dialog.querySelector("[data-pc-icon]");
        if (icon) icon.textContent = cfg.icon || "article";
        const multiFooter = dialog.querySelector("[data-pc-multi-footer]");
        if (multiFooter) multiFooter.style.display = multi ? "inline-flex" : "none";
        dialog.querySelector("[data-pc-media-tools]").hidden = !isMedia;
        dialog.querySelector("[data-pc-results]").hidden = isMedia;
        dialog.querySelector("[data-pc-media-body]").hidden = !isMedia;
        const chooseBtn = dialog.querySelector("[data-pc-choose]");
        if (chooseBtn) { chooseBtn.hidden = !isMedia || multi; chooseBtn.disabled = true; }
        dialog.querySelector("[data-pc-loadmore]").hidden = true;
        const filter = dialog.querySelector("[data-pc-filter]");
        if (filter) {
            filter.value = "";
            filter.placeholder = isMedia ? "Search by name…" : "Filter…";
        }
        if (isMedia) {
            resetMediaState();
            dialog.querySelectorAll("[data-pc-view]").forEach((btn) => {
                btn.setAttribute("aria-pressed", btn.getAttribute("data-pc-view") === mediaState.view ? "true" : "false");
            });
            renderMediaDetail(null);
            pickerNotice("");
            doMediaPickerFetch({ reset: true });
        } else {
            doChooserFetch("");
        }
        if (typeof dialog.showModal === "function") dialog.showModal();
        else dialog.setAttribute("open", "");
        setTimeout(() => filter?.focus(), 30);
    }

    // #294 — reusable single-pick chooser for non-form-widget consumers
    // (the richtext editor's internal-link buttons). Opens the shared
    // dialog for `kind` (page/snippet/document/media) and resolves with
    // the chosen `{ id, title }`, or `null` if dismissed.
    window.rcmsOpenChooser = function (kind, opts) {
        opts = opts || {};
        return new Promise((resolve) => {
            openChooserDialog(kind || "page", { filter: opts.filter || null, onPick: resolve });
        });
    };

    function wireChoosers() {
        // Event delegation — bind once at document level so dynamically
        // inserted choosers (e.g. via the stream-block editor) light up
        // without needing a re-wire pass per insertion.
        document.addEventListener("click", (e) => {
            const openBtn = e.target.closest("[data-chooser-open]");
            if (openBtn) {
                const target = openBtn.closest("[data-chooser]");
                if (!target) return;
                openChooserDialog(target.getAttribute("data-chooser-kind") || "page", { target });
                return;
            }
            const clearBtn = e.target.closest("[data-chooser-clear]");
            if (clearBtn) {
                const target = clearBtn.closest("[data-chooser]");
                if (!target) return;
                const multi = target.getAttribute("data-chooser-multi") === "1";
                if (multi) {
                    applyChooserMultiSelection(target, []);
                } else {
                    applyChooserSelection(target, { id: "", title: "" });
                }
                clearBtn.style.display = "none";
            }
        });
    }

    // Server-rendered chooser widgets label an existing value as "#<id>"
    // (templates only have the raw id). Resolve every such id per kind in
    // one batched request and swap the labels for names. Multi-pick widgets
    // keep their "N selected" label; a fresh pick already carries its name.
    // A failed lookup keeps the #id fallback — never break the editor.
    const CHOOSER_HYDRATION = {
        // Media reads as the file's name alone.
        media: { extra: { kind: "any" }, label: (it) => it.title || it.filename || "" },
        // Snippets read as "Name (#id)" — several snippets can share a title,
        // so keep the id visible.
        snippet: { extra: {}, label: (it) => (it.title ? `${it.title} (#${it.id})` : "") },
    };
    async function hydrateMediaChooserLabels() {
        for (const [kind, cfg] of Object.entries(CHOOSER_HYDRATION)) {
            const pending = new Map(); // id -> [label elements]
            document.querySelectorAll(`[data-chooser][data-chooser-kind="${kind}"]`).forEach((w) => {
                if (w.getAttribute("data-chooser-multi") === "1") return;
                const id = (w.querySelector("[data-chooser-input]")?.value || "").trim();
                const label = w.querySelector("[data-chooser-label]");
                if (!/^\d+$/.test(id) || !label) return;
                if (label.textContent.trim() !== `#${id}`) return; // already named
                if (!pending.has(id)) pending.set(id, []);
                pending.get(id).push(label);
            });
            if (pending.size === 0) continue;
            try {
                const params = new URLSearchParams({ ids: [...pending.keys()].join(","), ...cfg.extra });
                const resp = await fetch(`${chooserCfg(kind).url}?${params}`, { credentials: "same-origin" });
                if (!resp.ok) continue;
                const data = await resp.json();
                for (const it of data.items || []) {
                    const name = cfg.label(it);
                    if (!name) continue;
                    for (const label of pending.get(String(it.id)) || []) {
                        label.textContent = name;
                    }
                }
            } catch (e) { /* keep the #id fallback */ }
        }
    }

    // #262 — multipart-form CSRF: hijack native submits on
    // `<form enctype="multipart/form-data">`, post via fetch with
    // the `X-CSRF-Token` header read from the `rustango_csrf`
    // cookie, then navigate to wherever the server redirected to.
    // Opt-out via `data-multipart-no-hijack` for forms that drive
    // their own fetch pipeline (e.g. media-upload's staged flow).
    function wireMultipartCsrf() {
        const forms = document.querySelectorAll(
            "form[enctype='multipart/form-data']:not([data-multipart-no-hijack])"
        );
        forms.forEach((form) => {
            form.addEventListener("submit", async (event) => {
                event.preventDefault();
                const token = readCsrfCookieValue();
                const action = form.getAttribute("action") || window.location.href;
                const fd = new FormData(form);
                try {
                    const resp = await fetch(action, {
                        method: (form.method || "POST").toUpperCase(),
                        body: fd,
                        headers: token ? { "X-CSRF-Token": token } : {},
                        credentials: "same-origin",
                        redirect: "follow",
                    });
                    // `fetch` with `redirect: follow` resolves `resp.url`
                    // to the final URL after any 3xx chain — navigate
                    // there so messages-framework flash cookies on the
                    // success page get a fresh paint, mirroring the
                    // native form-submit redirect behaviour. Fall back
                    // to a reload if the server returned the same URL.
                    if (resp.url && resp.url !== window.location.href) {
                        window.location.assign(resp.url);
                    } else {
                        window.location.reload();
                    }
                } catch (err) {
                    rcmsToast({
                        level: "error",
                        body: "Upload failed: " + (err && err.message ? err.message : String(err)),
                    });
                }
            });
        });
    }

    function readCsrfCookieValue() {
        const raw = document.cookie || "";
        for (const part of raw.split(";")) {
            const trimmed = part.trim();
            if (trimmed.startsWith("rustango_csrf=")) {
                return decodeURIComponent(trimmed.slice("rustango_csrf=".length));
            }
        }
        return "";
    }

    // Wagtail TitleFieldPanel auto-slug.
    function wireSlugSync() {
        document.querySelectorAll("[data-slug-source]").forEach((slugInput) => {
            const sourceSel = slugInput.getAttribute("data-slug-source");
            if (!sourceSel) return;
            const source = document.querySelector(sourceSel);
            if (!source) return;
            // `data-slug-locked="1"` means "skip auto-sync" — handler
            // sets it on edit screens so renaming the title doesn't
            // silently flip the URL behind the editor's back.
            let locked = slugInput.dataset.slugLocked === "1";
            // Also lock if the slug field already has a non-empty value
            // — defensive against templates that omit data-slug-locked.
            if (!locked && slugInput.value && slugInput.value.trim() !== "") {
                locked = true;
            }
            slugInput.addEventListener("input", () => {
                // Once the editor types in the slug, stop syncing.
                locked = true;
            });
            source.addEventListener("input", () => {
                if (locked) return;
                slugInput.value = slugify(source.value);
            });
        });
    }

    function slugify(s) {
        return String(s || "")
            .toLowerCase()
            .normalize("NFKD")
            .replace(/[̀-ͯ]/g, "") // strip diacritics
            .replace(/[^a-z0-9\s-]/g, "")
            .trim()
            .replace(/\s+/g, "-")
            .replace(/-+/g, "-")
            .slice(0, 80);
    }

    // #263 — richtext widget preview-on-blur. Wraps each
    // `textarea[data-widget-mode="richtext"]` with a toggle row + a
    // preview pane. Source is still the source of truth (we never
    // overwrite the textarea from the rendered preview); the preview
    // is a read-only viewport rendered server-side so the bytes
    // match the public render.
    //
    // States:
    //   "edit"    — textarea visible, preview hidden, toggle says "Preview"
    //   "preview" — preview visible, textarea hidden, toggle says "Edit"
    //
    // Transitions:
    //   • blur on the textarea → fetch + render → swap to "preview"
    //   • click preview pane → swap to "edit", focus textarea
    //   • click toggle → flip
    //
    // The fetch is debounced+coalesced via a per-textarea token so a
    // fast blur/focus cycle doesn't race responses.
    // #243 — multi-value widget shim. The checkboxes / multiselect /
    // snippetm2m render arms emit a hidden `input[name]` carrying a JSON
    // array of i64 ids plus a visible control (a `<select multiple
    // data-multivalue-source>` or `[data-multivalue-option]` checkboxes)
    // that has NO `name` so it never self-submits. This shim is the half
    // the template contract assumes: it (1) sets the visible control's
    // selection authoritatively from the parsed hidden value on init
    // (which also corrects any server-side substring false-positives in
    // the `is containing` selected check), and (2) writes selections back
    // into the hidden input on change, dispatching input/change so
    // autosave dirty-tracking fires.
    //
    // Scope: wires every `[data-widget-multivalue]` group present at load.
    // The AC3 snippetm2m chooser is a page-extension widget rendered by
    // `widgets()`, always present on first paint, so it is fully covered.
    // A multi-value field inside a stream block added *after* load is not
    // yet re-wired (the stream editor would need to re-invoke this on
    // insert) — tracked as a follow-up; takes `root` so that wiring is a
    // one-liner once the stream editor calls it.
    // Media thumbnails whose file is gone from storage 404. `error` does
    // not bubble, so a delegated listener has to run in the CAPTURE
    // phase; a normal bubbling listener on document never fires for it.
    function wireMissingThumbnails() {
        document.addEventListener(
            "error",
            (event) => {
                const img = event.target;
                if (!(img instanceof HTMLImageElement)) return;
                const thumb = img.closest(".rcms-media-card-thumb");
                if (thumb) thumb.classList.add("is-missing");
            },
            true,
        );
    }

    /* Select-all in a list toolbar.
     *
     * Three pages each carried their own inline `onclick` querying a
     * different selector (`input[name=id]`, `input[name=ids][form=…]`),
     * so the behaviour drifted and none of them kept the box in sync
     * when rows were ticked individually. One handler, driven by
     * `data-select-all="<row checkbox selector>"`, covers all of them.
     */
    function wireSelectAll() {
        const boxes = document.querySelectorAll("[data-select-all]");
        for (const box of boxes) {
            const selector = box.getAttribute("data-select-all");
            if (!selector) continue;
            const rows = () => document.querySelectorAll(selector);
            box.addEventListener("change", () => {
                for (const row of rows()) row.checked = box.checked;
            });
            // Untick the header box as soon as a row disagrees with it,
            // and re-tick it once every row is selected by hand.
            document.addEventListener("change", (event) => {
                if (!(event.target instanceof HTMLInputElement)) return;
                if (!event.target.matches(selector)) return;
                const all = [...rows()];
                box.checked = all.length > 0 && all.every((r) => r.checked);
                box.indeterminate =
                    !box.checked && all.some((r) => r.checked);
            });
        }
    }

    function wireMultiValueWidgets(root) {
        const scope = root || document;
        const groups = scope.querySelectorAll("[data-widget-multivalue]");
        groups.forEach((group) => {
            if (group.dataset.multivalueWired === "1") return;
            group.dataset.multivalueWired = "1";

            const hidden = group.querySelector('input[type="hidden"][name]');
            if (!hidden) return;
            const selects = Array.from(
                group.querySelectorAll("select[data-multivalue-source]")
            );
            const checks = Array.from(
                group.querySelectorAll("[data-multivalue-option]")
            );

            const parseHidden = () => {
                let parsed;
                try {
                    parsed = JSON.parse(hidden.value || "[]");
                } catch (_e) {
                    parsed = [];
                }
                if (!Array.isArray(parsed)) return [];
                return parsed
                    .map((v) => Number(v))
                    .filter((n) => Number.isFinite(n));
            };

            const collect = () => {
                const ids = [];
                selects.forEach((sel) => {
                    Array.from(sel.selectedOptions).forEach((opt) => {
                        const n = Number(opt.value);
                        if (Number.isFinite(n)) ids.push(n);
                    });
                });
                checks.forEach((cb) => {
                    if (cb.checked) {
                        const n = Number(cb.value);
                        if (Number.isFinite(n)) ids.push(n);
                    }
                });
                return ids;
            };

            const writeBack = () => {
                const next = JSON.stringify(collect());
                if (hidden.value === next) return;
                hidden.value = next;
                hidden.dispatchEvent(new Event("input", { bubbles: true }));
                hidden.dispatchEvent(new Event("change", { bubbles: true }));
            };

            // Authoritative init from the persisted value.
            const selected = new Set(parseHidden());
            selects.forEach((sel) => {
                Array.from(sel.options).forEach((opt) => {
                    opt.selected = selected.has(Number(opt.value));
                });
            });
            checks.forEach((cb) => {
                cb.checked = selected.has(Number(cb.value));
            });
            // Normalize the hidden input to the canonical int-array shape.
            hidden.value = JSON.stringify(Array.from(selected));

            selects.forEach((sel) => sel.addEventListener("change", writeBack));
            checks.forEach((cb) => cb.addEventListener("change", writeBack));
        });
    }

    // #418 — warn before leaving an edit form with unsaved changes.
    // Mirrors the menu builder's beforeunload guard, generalised to
    // every `data-primary-save` editor (pages, snippets, users,
    // settings, redirects, locales, workflows, roles).
    //
    // Dirty becomes true on the first real edit. We listen to bubbling
    // `input`/`change` — covering native typing plus the synthetic
    // events the chooser / multi-value widgets fire on a user pick.
    // No editor script dispatches those events at load time (the stream
    // editor and multi-value init write `.value` directly without
    // dispatching), so a freshly-rendered form never trips the guard.
    //
    // The guard is cleared by an intentional navigation:
    //   • a genuine form submit anywhere on the page (Save / Publish via
    //     click or Cmd+S requestSubmit, Revert, logout, …). Captured so
    //     we record it before another handler can `preventDefault`.
    //   • the page editor's draft autosave (`rcms:autosaved`) — once a
    //     draft is safely persisted there is nothing to lose.
    // A subsequent edit re-arms the guard if a submit was abandoned
    // (validation failure, cancelled confirm dialog).
    function wireUnsavedChangesGuard() {
        const forms = Array.from(
            document.querySelectorAll("form[data-primary-save]")
        ).filter((f) => !f.hasAttribute("data-no-unsaved-warning"));
        if (forms.length === 0) return;

        let dirty = false;
        let submitting = false;

        const onEdit = () => {
            dirty = true;
            submitting = false;
        };

        forms.forEach((form) => {
            form.addEventListener("input", onEdit);
            form.addEventListener("change", onEdit);
            form.addEventListener("rcms:autosaved", () => {
                dirty = false;
            });
        });

        document.addEventListener(
            "submit",
            () => {
                submitting = true;
            },
            true
        );

        window.addEventListener("beforeunload", (event) => {
            if (submitting || !dirty) return;
            // Browsers ignore any custom string; calling preventDefault
            // and assigning returnValue is what triggers the native
            // confirmation prompt across Chrome / Firefox / Safari.
            event.preventDefault();
            event.returnValue = "";
        });
    }

    function wireRichtextPreview() {
        const textareas = document.querySelectorAll('textarea[data-widget-mode="richtext"]');
        textareas.forEach((ta) => {
            if (ta.dataset.richtextWired === "1") return;
            ta.dataset.richtextWired = "1";

            const wrap = document.createElement("div");
            wrap.className = "rcms-richtext-wrap";
            wrap.dataset.mode = "edit";

            const toolbarRow = document.createElement("div");
            toolbarRow.className = "rcms-richtext-toolbar-row";
            const toggle = document.createElement("button");
            toggle.type = "button";
            toggle.className = "rcms-richtext-toggle";
            toggle.textContent = "Preview";
            toggle.title = "Switch to rendered preview";
            toolbarRow.appendChild(toggle);

            const preview = document.createElement("div");
            preview.className = "rcms-richtext-preview";
            preview.hidden = true;
            preview.setAttribute("role", "region");
            preview.setAttribute("aria-label", "Rendered preview — click to edit");
            preview.tabIndex = 0;

            // Insert wrap in place, then move the textarea + add chrome.
            ta.parentNode.insertBefore(wrap, ta);
            wrap.appendChild(toolbarRow);
            wrap.appendChild(ta);
            wrap.appendChild(preview);

            // Promote a sibling `.rcms-md-toolbar` (the markdown toolbar
            // from #205) into the wrap so the editor sees it next to
            // the textarea, not stranded above the new chrome.
            const sibling = wrap.previousElementSibling;
            if (sibling && sibling.classList && sibling.classList.contains("rcms-md-toolbar")) {
                toolbarRow.insertBefore(sibling, toggle);
            }

            let token = 0;
            async function renderPreview() {
                const my = ++token;
                const body = ta.value || "";
                try {
                    const csrf = readCsrfCookieValue();
                    const resp = await fetch("/cms-admin/__richtext-preview", {
                        method: "POST",
                        credentials: "same-origin",
                        headers: {
                            "Content-Type": "application/x-www-form-urlencoded",
                            "X-CSRF-Token": csrf,
                            "Accept": "text/html",
                        },
                        body: "body=" + encodeURIComponent(body),
                    });
                    if (my !== token) return; // a later edit superseded us
                    if (resp.ok) {
                        preview.innerHTML = await resp.text();
                    } else {
                        // Friendly fallback — show source verbatim so
                        // the editor isn't stuck staring at nothing.
                        preview.textContent = body;
                    }
                } catch (_) {
                    if (my !== token) return;
                    preview.textContent = body;
                }
            }

            function setMode(mode) {
                wrap.dataset.mode = mode;
                if (mode === "preview") {
                    ta.hidden = true;
                    preview.hidden = false;
                    toggle.textContent = "Edit";
                    toggle.title = "Switch back to source edit";
                } else {
                    preview.hidden = true;
                    ta.hidden = false;
                    toggle.textContent = "Preview";
                    toggle.title = "Switch to rendered preview";
                }
            }

            ta.addEventListener("blur", async () => {
                // Don't swap if the textarea is empty — keep the
                // editor in edit mode so the first paint isn't a
                // blank preview pane.
                if (!ta.value.trim()) return;
                await renderPreview();
                setMode("preview");
            });

            preview.addEventListener("click", () => {
                setMode("edit");
                ta.focus();
            });
            preview.addEventListener("keydown", (e) => {
                if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    setMode("edit");
                    ta.focus();
                }
            });

            toggle.addEventListener("click", async () => {
                if (wrap.dataset.mode === "edit") {
                    await renderPreview();
                    setMode("preview");
                } else {
                    setMode("edit");
                    ta.focus();
                }
            });
        });
    }

    // #205 — markdown / rich-text toolbar enhancer. Attaches click
    // handlers to every `[data-md-toolbar-for]` block, mapping each
    // button to a wrap-or-prefix transformation against the
    // targeted textarea's selection. Plain-textarea fallback still
    // works without JS — we just reveal the toolbar after wiring.
    function wireMarkdownToolbars() {
        const toolbars = document.querySelectorAll("[data-md-toolbar-for]");
        toolbars.forEach((bar) => {
            const targetId = bar.getAttribute("data-md-toolbar-for");
            const target = document.getElementById(targetId);
            if (!target) return;
            bar.hidden = false;

            const wrap = (before, after) => {
                const start = target.selectionStart;
                const end = target.selectionEnd;
                const selected = target.value.slice(start, end);
                const replacement = `${before}${selected || ""}${after}`;
                target.setRangeText(replacement, start, end, "end");
                if (!selected) {
                    // Drop the caret between the markers so the user
                    // can type the contents in place.
                    const newPos = start + before.length;
                    target.setSelectionRange(newPos, newPos);
                }
                target.focus();
                // Notify any listeners (live preview, change tracker)
                // that the value moved.
                target.dispatchEvent(new Event("input", { bubbles: true }));
            };

            const prefixLines = (prefix) => {
                const start = target.selectionStart;
                const end = target.selectionEnd;
                const value = target.value;
                // Expand selection to whole-line boundaries so the
                // prefix lands on the line, not in the middle.
                const lineStart = value.lastIndexOf("\n", start - 1) + 1;
                const lineEnd = (() => {
                    const idx = value.indexOf("\n", end);
                    return idx === -1 ? value.length : idx;
                })();
                const block = value.slice(lineStart, lineEnd);
                const out = block
                    .split("\n")
                    .map((line, idx) => {
                        if (prefix === "1. ") return `${idx + 1}. ${line}`;
                        return `${prefix}${line}`;
                    })
                    .join("\n");
                target.setRangeText(out, lineStart, lineEnd, "end");
                target.focus();
                target.dispatchEvent(new Event("input", { bubbles: true }));
            };

            const insertLink = () => {
                const start = target.selectionStart;
                const end = target.selectionEnd;
                const selected = target.value.slice(start, end);
                const url = window.prompt("URL?", "https://");
                if (!url) return;
                const label = selected || "link text";
                target.setRangeText(`[${label}](${url})`, start, end, "end");
                target.focus();
                target.dispatchEvent(new Event("input", { bubbles: true }));
            };

            bar.querySelectorAll("[data-md-action]").forEach((btn) => {
                btn.addEventListener("click", (event) => {
                    event.preventDefault();
                    const action = btn.getAttribute("data-md-action");
                    switch (action) {
                        case "bold": wrap("**", "**"); break;
                        case "italic": wrap("_", "_"); break;
                        case "h2": prefixLines("## "); break;
                        case "h3": prefixLines("### "); break;
                        case "ul": prefixLines("- "); break;
                        case "ol": prefixLines("1. "); break;
                        case "quote": prefixLines("> "); break;
                        case "code": wrap("`", "`"); break;
                        case "codeblock": wrap("\n```\n", "\n```\n"); break;
                        case "link": insertLink(); break;
                    }
                });
            });

            // Cmd+B / Cmd+I / Cmd+K shortcuts within the textarea.
            target.addEventListener("keydown", (event) => {
                if (!(event.metaKey || event.ctrlKey)) return;
                const k = event.key.toLowerCase();
                if (k === "b") { event.preventDefault(); wrap("**", "**"); }
                else if (k === "i") { event.preventDefault(); wrap("_", "_"); }
                else if (k === "k") { event.preventDefault(); insertLink(); }
            });
        });
    }

    function wireColumnPickers() {
        const tables = document.querySelectorAll("table[data-col-picker]");
        if (!tables.length) return;
        const tenantSlug = document.documentElement.dataset.tenantSlug || "_";
        tables.forEach((table) => {
            const listName = table.getAttribute("data-col-picker");
            const storageKey = `rcms-col-pick:${tenantSlug}:${listName}`;
            const headRow = table.querySelector("thead tr");
            if (!headRow) return;
            const cols = Array.from(headRow.querySelectorAll("th[data-col-key]"));
            if (!cols.length) return;

            // Read saved state. Missing entry → visible.
            let hidden = {};
            try {
                hidden = JSON.parse(localStorage.getItem(storageKey) || "{}");
            } catch (_) {
                hidden = {};
            }

            function applyState() {
                const ths = Array.from(headRow.children);
                const colKeys = ths.map((th) => th.getAttribute("data-col-key") || "");
                ths.forEach((th, idx) => {
                    const key = colKeys[idx];
                    if (!key) return;
                    th.classList.toggle("rcms-col-hidden", !!hidden[key]);
                });
                table.querySelectorAll("tbody tr").forEach((tr) => {
                    Array.from(tr.children).forEach((td, idx) => {
                        const key = colKeys[idx];
                        if (!key) return;
                        td.classList.toggle("rcms-col-hidden", !!hidden[key]);
                    });
                });
            }

            function save() {
                try {
                    localStorage.setItem(storageKey, JSON.stringify(hidden));
                } catch (_) {
                    // localStorage full / disabled → state stays in memory only
                }
            }

            // Build the picker popover.
            const wrap = document.createElement("details");
            wrap.className = "rcms-col-picker";
            wrap.innerHTML = `
                <summary class="rcms-btn rcms-btn-outlined rcms-btn-small rcms-col-picker-trigger" aria-label="Columns">
                    <span class="material-symbols-rounded sm">view_column</span> Columns
                </summary>
                <div class="rcms-col-picker-panel" role="menu"></div>
            `;
            const panel = wrap.querySelector(".rcms-col-picker-panel");
            cols.forEach((th) => {
                const key = th.getAttribute("data-col-key");
                const label = th.getAttribute("data-col-label") || th.textContent.trim();
                const id = `colpick-${listName}-${key}`;
                const row = document.createElement("label");
                row.className = "rcms-col-picker-row";
                row.setAttribute("for", id);
                row.innerHTML = `<input type="checkbox" id="${id}" ${hidden[key] ? "" : "checked"}> <span>${label}</span>`;
                row.querySelector("input").addEventListener("change", (event) => {
                    hidden[key] = !event.target.checked;
                    save();
                    applyState();
                });
                panel.appendChild(row);
            });
            const reset = document.createElement("button");
            reset.type = "button";
            reset.className = "rcms-btn rcms-btn-text rcms-btn-small rcms-col-picker-reset";
            reset.textContent = "Reset to defaults";
            reset.addEventListener("click", (event) => {
                event.preventDefault();
                hidden = {};
                save();
                applyState();
                panel.querySelectorAll("input[type=checkbox]").forEach((inp) => {
                    inp.checked = true;
                });
            });
            panel.appendChild(reset);

            // Mount the trigger next to the table — into a
            // data-col-picker-mount element if present, otherwise
            // before the table itself. The mount usually sits OUTSIDE
            // the table's own `.rcms-table-wrap` (so it can't be scrolled
            // out of view), so search from the marked wrapper when
            // there is one.
            const mountScope =
                table.closest("[data-col-picker-mount-wrap]") || table.parentElement;
            const mount = mountScope.querySelector("[data-col-picker-mount]");
            if (mount) {
                mount.appendChild(wrap);
            } else {
                table.parentElement.insertBefore(wrap, table);
            }
            applyState();
        });
    }

    function wireShortcuts() {
        // Build the cheat-sheet modal lazily on first open so it
        // doesn't bloat every page render.
        let modal = null;
        function buildModal() {
            if (modal) return modal;
            const isMac = /Mac|iPhone|iPad/i.test(navigator.platform || "");
            const cmd = isMac ? "⌘" : "Ctrl";
            modal = document.createElement("dialog");
            modal.className = "rcms-kbd-modal";
            modal.setAttribute("aria-label", "Keyboard shortcuts");
            modal.innerHTML = `
                <article>
                    <header>
                        <h2>Keyboard shortcuts</h2>
                        <button type="button" class="rcms-btn rcms-btn-text rcms-btn-small" data-kbd-close aria-label="Close">
                            <span class="material-symbols-rounded sm">close</span>
                        </button>
                    </header>
                    <section>
                        <h3>Global</h3>
                        <dl>
                            <dt><kbd>/</kbd></dt><dd>Focus the search box</dd>
                            <dt><kbd>${cmd}</kbd> + <kbd>K</kbd></dt><dd>Open the command palette (jump to anywhere)</dd>
                            <dt><kbd>?</kbd></dt><dd>Open this cheat sheet</dd>
                            <dt><kbd>Esc</kbd></dt><dd>Close menus, dismiss panels, blur the active input</dd>
                            <dt><kbd>g</kbd> then <kbd>l</kbd></dt><dd>Go to Pages list</dd>
                            <dt><kbd>g</kbd> then <kbd>m</kbd></dt><dd>Go to Media library</dd>
                        </dl>
                        <h3>Form editing</h3>
                        <dl>
                            <dt><kbd>${cmd}</kbd> + <kbd>S</kbd></dt><dd>Save the current form</dd>
                        </dl>
                    </section>
                </article>
            `;
            document.body.appendChild(modal);
            modal.addEventListener("click", (event) => {
                if (event.target === modal || event.target.closest("[data-kbd-close]")) {
                    modal.close();
                }
            });
            return modal;
        }
        function openCheatSheet() {
            const m = buildModal();
            if (typeof m.showModal === "function") {
                m.showModal();
            } else {
                m.setAttribute("open", "");
            }
        }

        // ----- #206 — Cmd+K command palette -----
        //
        // Floating modal: Cmd+K (Ctrl+K on Linux/Windows) opens a
        // search overlay that fuzzy-matches across pages, snippets,
        // media, and a curated list of admin routes. Up/Down + Enter
        // navigates; Esc / outside click closes. Reuses the existing
        // /cms-admin/search?autocomplete=1 endpoint for content hits.
        const ADMIN_ROUTES = [
            { label: "Pages", url: "/cms-admin/pages", icon: "article", keywords: "pages tree list" },
            { label: "Media (images)", url: "/cms-admin/media", icon: "image", keywords: "media images library" },
            { label: "Documents", url: "/cms-admin/documents", icon: "description", keywords: "documents files pdf" },
            { label: "Library / Snippets", url: "/cms-admin/library", icon: "library_books", keywords: "snippets library reusable" },
            { label: "Locales", url: "/cms-admin/locales", icon: "translate", keywords: "locales languages i18n" },
            { label: "Navigation", url: "/cms-admin/navigation", icon: "menu", keywords: "navigation menus" },
            { label: "Workflows", url: "/cms-admin/workflows", icon: "rule", keywords: "workflows review approval" },
            { label: "Redirects", url: "/cms-admin/redirects", icon: "alt_route", keywords: "redirects 301 url rewrite" },
            { label: "Settings", url: "/cms-admin/settings", icon: "settings", keywords: "settings site config" },
            { label: "Themes", url: "/cms-admin/themes", icon: "palette", keywords: "themes rcms-brand colors" },
            { label: "Users", url: "/cms-admin/users", icon: "group", keywords: "users team accounts" },
            { label: "Roles", url: "/cms-admin/roles", icon: "shield_person", keywords: "roles permissions groups" },
            { label: "Reports — locked pages", url: "/cms-admin/reports/locked-pages", icon: "lock", keywords: "reports locked locks" },
            { label: "Reports — unused media", url: "/cms-admin/media/unused", icon: "delete_sweep", keywords: "reports unused media orphan" },
            { label: "Reports — aging pages", url: "/cms-admin/reports/aging-pages", icon: "schedule", keywords: "reports aging stale old" },
            { label: "Dashboard", url: "/cms-admin/dashboard", icon: "dashboard", keywords: "dashboard home" },
            { label: "Preferences", url: "/cms-admin/me", icon: "person", keywords: "preferences account profile theme notifications" },
            { label: "Styleguide", url: "/cms-admin/styleguide", icon: "design_services", keywords: "styleguide components gallery" },
            { label: "History", url: "/cms-admin/history", icon: "history", keywords: "history audit log" },
        ];

        let palette = null;
        let paletteResults = [];
        let paletteCursor = 0;
        let paletteDebounce = 0;
        let paletteLastQuery = null;

        function ensurePalette() {
            if (palette) return palette;
            palette = document.createElement("dialog");
            palette.className = "rcms-cmd-palette";
            palette.setAttribute("aria-label", "Command palette");
            palette.innerHTML = `
                <article style="padding: 0; min-width: 540px; max-width: 720px; width: 90vw;">
                    <header style="display: flex; align-items: center; gap: 8px; padding: 12px 16px; border-bottom: 1px solid var(--md-sys-color-outline-variant);">
                        <span class="material-symbols-rounded" style="opacity: 0.6;">search</span>
                        <input type="text" data-palette-input placeholder="Jump to page, snippet, media, or admin section…"
                               style="flex: 1; border: none; outline: none; background: transparent; font-size: 15px;"
                               autocomplete="off">
                        <kbd style="font-size: 11px; opacity: 0.6;">Esc</kbd>
                    </header>
                    <ul data-palette-results
                        style="list-style: none; padding: 4px 0; margin: 0; max-height: 60vh; overflow-y: auto;">
                    </ul>
                    <footer style="padding: 8px 16px; border-top: 1px solid var(--md-sys-color-outline-variant); display: flex; gap: 16px; font-size: 11px; color: var(--md-sys-color-on-surface-variant);">
                        <span><kbd>↑</kbd> <kbd>↓</kbd> navigate</span>
                        <span><kbd>Enter</kbd> open</span>
                        <span><kbd>Esc</kbd> close</span>
                    </footer>
                </article>
            `;
            document.body.appendChild(palette);
            const input = palette.querySelector("[data-palette-input]");
            const results = palette.querySelector("[data-palette-results]");

            const renderResults = () => {
                if (paletteResults.length === 0) {
                    const q = input.value.trim();
                    results.innerHTML = q
                        ? `<li style="padding: 24px; text-align: center; color: var(--md-sys-color-on-surface-variant); font-size: 13px;">No matches for "${escapeHtml(q)}".</li>`
                        : `<li style="padding: 24px; text-align: center; color: var(--md-sys-color-on-surface-variant); font-size: 13px;">Start typing to search pages, snippets, media, or admin routes.</li>`;
                    return;
                }
                paletteCursor = Math.max(0, Math.min(paletteCursor, paletteResults.length - 1));
                let html = "";
                paletteResults.forEach((r, idx) => {
                    const active = idx === paletteCursor;
                    html += `<li data-palette-idx="${idx}"
                                 style="display: flex; align-items: center; gap: 12px; padding: 8px 16px; cursor: pointer; ${active ? "background: var(--md-sys-color-surface-container-high);" : ""}">
                        <span class="material-symbols-rounded" style="opacity: 0.7;">${r.icon}</span>
                        <div style="flex: 1; min-width: 0;">
                            <div style="white-space: nowrap; overflow: hidden; text-overflow: ellipsis;">${escapeHtml(r.title)}</div>
                            ${r.sub ? `<div style="font-size: 11px; color: var(--md-sys-color-on-surface-variant); white-space: nowrap; overflow: hidden; text-overflow: ellipsis;">${escapeHtml(r.sub)}</div>` : ""}
                        </div>
                        <span style="font-size: 10px; color: var(--md-sys-color-on-surface-variant); text-transform: uppercase; letter-spacing: 0.04em;">${r.category}</span>
                    </li>`;
                });
                results.innerHTML = html;
            };

            const matchesRoute = (route, q) => {
                if (!q) return false;
                const needle = q.toLowerCase();
                return route.label.toLowerCase().includes(needle)
                    || route.keywords.toLowerCase().includes(needle);
            };

            const runSearch = async (rawQuery) => {
                const q = rawQuery.trim();
                if (q === paletteLastQuery) return;
                paletteLastQuery = q;
                paletteResults = [];

                if (!q) {
                    paletteCursor = 0;
                    renderResults();
                    return;
                }

                // Static admin routes first — they're cheap and most-
                // expected for "Settings" / "Workflows" / etc.
                for (const route of ADMIN_ROUTES) {
                    if (matchesRoute(route, q)) {
                        paletteResults.push({
                            title: route.label,
                            sub: route.url,
                            icon: route.icon,
                            category: "Admin",
                            url: route.url,
                        });
                    }
                }

                // Content hits via the existing autocomplete endpoint.
                try {
                    const res = await fetch(`/cms-admin/search?q=${encodeURIComponent(q)}&autocomplete=1`);
                    if (res.ok) {
                        const data = await res.json();
                        if (paletteLastQuery !== q) return; // stale
                        for (const p of (data.pages || []).slice(0, 5)) {
                            paletteResults.push({
                                title: p.title || `Page #${p.id}`,
                                sub: p.url_path || p.slug,
                                icon: "article",
                                category: "Page",
                                url: `/cms-admin/pages/${p.id}/edit`,
                            });
                        }
                        for (const s of (data.snippets || []).slice(0, 5)) {
                            paletteResults.push({
                                title: s.title || `Snippet #${s.id}`,
                                sub: s.slug || s.type_name || "",
                                icon: "library_books",
                                category: "Snippet",
                                url: `/cms-admin/library/${s.id}/edit`,
                            });
                        }
                        for (const m of (data.media || []).slice(0, 5)) {
                            paletteResults.push({
                                title: m.title || m.filename || `Media #${m.id}`,
                                sub: m.filename || "",
                                icon: m.kind === "image" ? "image" : "description",
                                category: "Media",
                                url: `/cms-admin/media/${m.id}/edit`,
                            });
                        }
                    }
                } catch (_) {
                    // network error — admin routes still render
                }

                // Cap at 10 results for the palette.
                paletteResults = paletteResults.slice(0, 10);
                paletteCursor = 0;
                renderResults();
            };

            input.addEventListener("input", () => {
                clearTimeout(paletteDebounce);
                paletteDebounce = setTimeout(() => runSearch(input.value), 120);
            });

            input.addEventListener("keydown", (event) => {
                if (event.key === "ArrowDown") {
                    event.preventDefault();
                    if (paletteResults.length) {
                        paletteCursor = (paletteCursor + 1) % paletteResults.length;
                        renderResults();
                    }
                } else if (event.key === "ArrowUp") {
                    event.preventDefault();
                    if (paletteResults.length) {
                        paletteCursor = (paletteCursor - 1 + paletteResults.length) % paletteResults.length;
                        renderResults();
                    }
                } else if (event.key === "Enter") {
                    event.preventDefault();
                    const hit = paletteResults[paletteCursor];
                    if (hit && hit.url) {
                        palette.close();
                        window.location.assign(hit.url);
                    }
                } else if (event.key === "Escape") {
                    event.preventDefault();
                    palette.close();
                }
            });

            results.addEventListener("click", (event) => {
                const li = event.target.closest("[data-palette-idx]");
                if (!li) return;
                const idx = parseInt(li.dataset.paletteIdx, 10);
                const hit = paletteResults[idx];
                if (hit && hit.url) {
                    palette.close();
                    window.location.assign(hit.url);
                }
            });

            palette.addEventListener("click", (event) => {
                // Click on the dialog backdrop (not the inner article)
                // closes the palette — native dialog backdrop click
                // bubbles to the dialog itself.
                if (event.target === palette) palette.close();
            });

            renderResults();
            return palette;
        }

        function openPalette() {
            const p = ensurePalette();
            paletteLastQuery = null;
            paletteResults = [];
            paletteCursor = 0;
            const input = p.querySelector("[data-palette-input]");
            if (input) input.value = "";
            const results = p.querySelector("[data-palette-results]");
            if (results) {
                results.innerHTML = `<li style="padding: 24px; text-align: center; color: var(--md-sys-color-on-surface-variant); font-size: 13px;">Start typing to search pages, snippets, media, or admin routes.</li>`;
            }
            if (typeof p.showModal === "function") {
                p.showModal();
            } else {
                p.setAttribute("open", "");
            }
            setTimeout(() => input?.focus(), 30);
        }

        document.addEventListener("keydown", (event) => {
            // Cmd+K / Ctrl+K opens the palette from anywhere.
            const key = event.key?.toLowerCase();
            if (key === "k" && (event.metaKey || event.ctrlKey)) {
                event.preventDefault();
                openPalette();
            }
        });

        // #127 — sidebar Help entry "Keyboard shortcuts" uses a
        // `data-kbd-modal-open` attr to trigger the modal in-page.
        document.querySelectorAll("[data-kbd-modal-open]").forEach((el) => {
            el.addEventListener("click", (event) => {
                event.preventDefault();
                openCheatSheet();
            });
        });
        // Sequential-key buffer for `g l` etc.
        let pending = "";
        let pendingTimer = 0;
        function resetPending() {
            pending = "";
            clearTimeout(pendingTimer);
        }
        function navigateTo(url) {
            window.location.assign(url);
        }

        document.addEventListener("keydown", (event) => {
            const tag = (event.target?.tagName || "").toLowerCase();
            const inField = tag === "input" || tag === "textarea" || tag === "select" || event.target?.isContentEditable;

            // `?` — cheat sheet (Shift+/ on most keyboards). Suppress
            // when typing in a field.
            if (event.key === "?" && !inField && !event.metaKey && !event.ctrlKey) {
                event.preventDefault();
                openCheatSheet();
                return;
            }

            // `Esc` — close menus + blur. Always allowed so a
            // mistyped input can be escaped.
            if (event.key === "Escape") {
                // Inside an open modal <dialog> (a chooser, the palette), Esc
                // belongs to the dialog: preventDefault here would cancel its
                // close request, leaving it open with the field just blurred.
                if (event.target instanceof Element && event.target.closest("dialog[open]")) {
                    return;
                }
                let acted = false;
                // Close every open <details> (kebab menus, side panels).
                document.querySelectorAll("details[open]").forEach((el) => {
                    el.open = false;
                    acted = true;
                });
                // Dismiss the cheat sheet if it's open.
                if (modal && modal.open) {
                    modal.close();
                    acted = true;
                }
                if (inField) {
                    event.target.blur();
                    acted = true;
                }
                if (acted) event.preventDefault();
                return;
            }

            // `g <key>` — Vim-style sequential navigation. Only when
            // not in a field.
            if (inField) return;
            if (event.key === "g" && !event.metaKey && !event.ctrlKey && !event.altKey) {
                pending = "g";
                clearTimeout(pendingTimer);
                pendingTimer = setTimeout(resetPending, 1500);
                return;
            }
            if (pending === "g") {
                if (event.key === "l") {
                    event.preventDefault();
                    navigateTo("/cms-admin/pages");
                    resetPending();
                    return;
                }
                if (event.key === "m") {
                    event.preventDefault();
                    navigateTo("/cms-admin/media");
                    resetPending();
                    return;
                }
                resetPending();
            }
        });
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", autowire);
    } else {
        autowire();
    }

    window.rcmsConfirm = rcmsConfirm;
    window.rcmsToast = rcmsToast;
})();

// #271 — sidebar collapse / expand toggle. The icons-only rail is
// driven by the `data-sidebar-collapsed` attribute on <html>; the
// no-flash <script> in _base.html sets it before first paint from
// localStorage, so reloads don't flicker. This block:
//   1. Mirrors each sidebar link's text label into its `rcms-title` attr
//      so the native browser tooltip surfaces the label on hover
//      when the rail is collapsed.
//   2. Wires the toggle button to flip the attribute + persist the
//      pick under a per-tenant localStorage key.
//   3. Updates the toggle's aria-label to match the next state, so
//      the screen-reader announcement stays accurate.
(() => {
    const ATTR = "data-sidebar-collapsed";

    function setTooltipsFromLabels() {
        // Walk direct children, accumulate only the label text — skip
        // the leading <span class="material-symbols-rounded"> icon
        // whose textContent is the glyph name ("article", "image", …).
        // Without this skip the title attr would read "article Pages"
        // instead of just "Pages" on collapsed-rail hover.
        for (const a of document.querySelectorAll(".rcms-sidebar-link, .rcms-sidebar-logout")) {
            if (a.title) continue;
            let label = "";
            for (const node of a.childNodes) {
                if (node.nodeType === Node.TEXT_NODE) {
                    label += node.textContent;
                } else if (
                    node.nodeType === Node.ELEMENT_NODE
                    && !node.classList.contains("material-symbols-rounded")
                ) {
                    label += node.textContent;
                }
            }
            label = label.replace(/\s+/g, " ").trim();
            if (label) a.title = label;
        }
    }

    function storageKey() {
        const slug = document.documentElement.getAttribute("data-tenant-slug") || "";
        return "rcms-admin-sidebar-collapsed:" + slug;
    }

    function applyAriaLabel(btn, collapsed) {
        btn.setAttribute(
            "aria-label",
            collapsed ? "Expand rcms-sidebar" : "Collapse rcms-sidebar to icons"
        );
        btn.setAttribute("aria-pressed", collapsed ? "true" : "false");
    }

    // Locale form (#i18n): live "core support" hint next to the code input.
    // Reads the embedded config (core's known-locale metadata + localized
    // phrases) and reflects it as the operator types. Wrapped defensively so
    // it can never break the form.
    function wireLocaleSupportHint() {
        const input = document.querySelector("[data-locale-code]");
        const out = document.querySelector("[data-locale-support]");
        const cfgEl = document.querySelector("[data-locale-support-config]");
        if (!input || !out || !cfgEl) return;
        let cfg;
        try {
            cfg = JSON.parse(cfgEl.textContent);
        } catch (_e) {
            return;
        }
        const meta = cfg.meta || {};
        const phrases = cfg.phrases || {};
        const update = () => {
            const raw = (input.value || "").trim().replace(/_/g, "-");
            if (!raw) {
                out.textContent = "";
                out.className = "";
                return;
            }
            const base = raw.split("-")[0].toLowerCase();
            const info = meta[base] || meta[raw.toLowerCase()];
            let text;
            if (info) {
                text = (info.ui ? "✓ " + phrases.full : "⚠ " + phrases.content_only);
                if (info.is_rtl) text += " · " + phrases.rtl;
            } else {
                text = "⚠ " + phrases.unknown;
            }
            out.textContent = text;
            out.className = info && info.ui ? "rcms-text-muted-2" : "rcms-text-muted";
        };
        input.addEventListener("input", update);
        update();
    }

    function setCollapsed(collapsed) {
        const root = document.documentElement;
        if (collapsed) root.setAttribute(ATTR, "");
        else root.removeAttribute(ATTR);
        try {
            localStorage.setItem(storageKey(), collapsed ? "1" : "0");
        } catch (_) { /* ignore — private mode etc. */ }
        const btn = document.querySelector("[data-sidebar-collapse-toggle]");
        if (btn) applyAriaLabel(btn, collapsed);
    }

    // The rail hides the search input, so anything that wants to focus it
    // has to widen the sidebar first. Exposed because the search lives in
    // another module.
    window.rcmsExpandSidebar = () => setCollapsed(false);

    function init() {
        setTooltipsFromLabels();
        wireLocaleSupportHint();
        const btn = document.querySelector("[data-sidebar-collapse-toggle]");
        if (!btn) return;
        applyAriaLabel(btn, document.documentElement.hasAttribute(ATTR));
        btn.addEventListener("click", () => {
            setCollapsed(!document.documentElement.hasAttribute(ATTR));
        });
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", init);
    } else {
        init();
    }
})();

// #272 — Sidebar group (collapsible <details>) smooth expand/collapse
// + accordion behaviour + open-group persistence. CSS alone can't
// transition between `height: 0` and `height: auto`, so we
// intercept the <summary> click, measure the items wrapper's
// scrollHeight, write that to inline `style.height`, let the
// transition run, then clear the inline style at transitionend
// so the [open] rule's `height: auto` takes over (the group can
// then grow naturally if items are added later).
//
// Accordion: opening one group animates closed every other open
// group. Persistence stores the name of the currently-open group
// under a single per-tenant key so reloads restore exactly that
// one. Active-route precedence: when the current page lives
// inside a group, the server marks the group `.active` + `[open]`;
// we snap-open it at init (no animation) and skip persistence on
// it so the user's manual pick on OTHER routes survives.
(() => {
    function tenantSlug() {
        return document.documentElement.getAttribute("data-tenant-slug") || "";
    }
    function storageKey() {
        return "rcms-admin-sidebar-open-group:" + tenantSlug();
    }

    function clearInline(items) {
        items.style.height = "";
        items.style.opacity = "";
    }

    function snapOpen(g) {
        g.setAttribute("open", "");
        const items = g.querySelector(".rcms-sidebar-group-items");
        if (items) clearInline(items);
    }

    function snapClosed(g) {
        g.removeAttribute("open");
        const items = g.querySelector(".rcms-sidebar-group-items");
        if (items) clearInline(items);
    }

    function cancelAnimation(items) {
        if (items._rcmsCancel) {
            items._rcmsCancel();
            items._rcmsCancel = null;
        }
    }

    function animateOpen(g) {
        const items = g.querySelector(".rcms-sidebar-group-items");
        if (!items) return;
        cancelAnimation(items);
        // Add [open] so CSS would normally jump to `height: auto`,
        // then measure the resulting natural height. We immediately
        // lock the inline height back to 0 to override the auto
        // before paint, force reflow, and animate to the measured
        // target.
        g.setAttribute("open", "");
        const target = items.scrollHeight;
        items.style.height = "0px";
        items.style.opacity = "0";
        // Force reflow so the 0px start state commits before the
        // mutation that should be transitioned to.
        // eslint-disable-next-line no-unused-expressions
        items.offsetHeight;
        items.style.height = target + "px";
        items.style.opacity = "1";
        const handler = (e) => {
            if (e.propertyName !== "height" || e.target !== items) return;
            items.removeEventListener("transitionend", handler);
            items._rcmsCancel = null;
            clearInline(items);
        };
        items.addEventListener("transitionend", handler);
        items._rcmsCancel = () => {
            items.removeEventListener("transitionend", handler);
        };
    }

    function animateClose(g) {
        const items = g.querySelector(".rcms-sidebar-group-items");
        if (!items) return;
        cancelAnimation(items);
        // Lock current rendered height inline (overrides the [open]
        // rule's `height: auto`), force reflow, then transition to 0.
        // After transitionend, drop [open] so the closed-state CSS
        // takes over.
        const start = items.scrollHeight;
        items.style.height = start + "px";
        items.style.opacity = "1";
        // eslint-disable-next-line no-unused-expressions
        items.offsetHeight;
        items.style.height = "0px";
        items.style.opacity = "0";
        const handler = (e) => {
            if (e.propertyName !== "height" || e.target !== items) return;
            items.removeEventListener("transitionend", handler);
            items._rcmsCancel = null;
            g.removeAttribute("open");
            clearInline(items);
        };
        items.addEventListener("transitionend", handler);
        items._rcmsCancel = () => {
            items.removeEventListener("transitionend", handler);
        };
    }

    function closeOthers(except) {
        for (const g of document.querySelectorAll("details[data-sidebar-group]")) {
            if (g !== except && g.hasAttribute("open")) animateClose(g);
        }
    }

    function persistOpenGroup(g) {
        if (g.classList.contains("active")) return; // active wins; don't overwrite
        try {
            localStorage.setItem(
                storageKey(),
                g.hasAttribute("open") ? g.getAttribute("data-sidebar-group") : ""
            );
        } catch (_) { /* private mode */ }
    }

    function init() {
        const groups = Array.from(document.querySelectorAll("details[data-sidebar-group]"));
        if (groups.length === 0) return;

        // Initial state: enforce accordion + active precedence +
        // localStorage restore. All transitions disabled here —
        // jumps are fine before first paint.
        const activeGroup = groups.find((g) => g.classList.contains("active"));
        if (activeGroup) {
            for (const g of groups) {
                if (g === activeGroup) snapOpen(g); else snapClosed(g);
            }
        } else {
            let stored = null;
            try { stored = localStorage.getItem(storageKey()); }
            catch (_) { /* private mode */ }
            for (const g of groups) {
                if (stored && g.getAttribute("data-sidebar-group") === stored) {
                    snapOpen(g);
                } else {
                    snapClosed(g);
                }
            }
        }

        for (const g of groups) {
            const summary = g.querySelector("summary");
            if (!summary) continue;
            summary.addEventListener("click", (event) => {
                // Take over the native <details> toggle so we can
                // animate instead of snapping.
                event.preventDefault();
                if (g.hasAttribute("open")) {
                    animateClose(g);
                    persistOpenGroup(g);
                } else {
                    closeOthers(g);
                    animateOpen(g);
                    persistOpenGroup(g);
                }
            });
        }
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", init);
    } else {
        init();
    }
})();
