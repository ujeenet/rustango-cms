// Sidebar preview driver — shared by every editor that has a template to
// render (pages, forms, …). Nothing here knows what is being previewed:
// the surface, the endpoint and the form to read unsaved values from all
// arrive as data attributes on `.rcms-preview-pane`, so adopting the pane
// is the whole integration. See `_preview_pane.html`.
//
// This lived inline in `page_form.html`, which is why the form builder
// grew a second preview that re-rendered the layout in JavaScript and
// could only drift from what the server actually produces.
(function () {
    "use strict";
    const root = document.querySelector(".rcms-edit-with-preview");
    if (!root) return;
    const cfgPane = root.querySelector(".rcms-preview-pane");
    // Namespaces the remembered split width / viewport / surface. Was the
    // tenant slug plus the page-type id; any editor supplies its own so
    // two different editors do not fight over one stored width.
    const SCOPE = (cfgPane && cfgPane.dataset.previewScope) || "default";
    const VARIANT = (cfgPane && cfgPane.dataset.previewVariant) || "0";
    const left = root.querySelector(".rcms-edit-pane");
    const handle = root.querySelector(".rcms-split-handle");
    const pane = root.querySelector(".rcms-preview-pane");
    const iframe = root.querySelector(".rcms-preview-frame");
    // Which representation the pane is showing. Server-chosen initial
    // value (`json` when the type has no HTML representation), then the
    // editor's choice, remembered per tenant.
    const jsonWrap = root.querySelector("[data-preview-json]");
    const jsonBody = root.querySelector("[data-json-body]");
    const jsonStatus = root.querySelector("[data-json-status]");
    const jsonMeta = root.querySelector("[data-json-meta]");
    const htmlStage = root.querySelector('[data-preview-stage][data-surface="html"]');
    // The decoupled frontend's pane. Present only when the tenant has
    // configured one, so every path below has to tolerate it being null.
    const siteStage = root.querySelector('[data-preview-stage][data-surface="site"]');
    const siteFrame = siteStage && siteStage.querySelector(".rcms-preview-frame");
    const siteSizer = siteStage && siteStage.querySelector("[data-preview-frame-sizer]");
    // Measured instead of the stage: the stage also holds the status bar.
    const siteArea = siteStage && siteStage.querySelector("[data-preview-frame-area]");
    const SITE_AVAILABLE = !!siteStage;
    let siteLoaded = false;
    function ensureSiteLoaded() {
        if (siteLoaded || !siteFrame) return;
        const src = siteFrame.dataset.src;
        if (!src) return;
        siteLoaded = true;
        siteFrame.src = src;
    }
    const SURFACE_KEY = "rcms_preview_surface:" + SCOPE;
    // Only the "html" mode has a template that renders. Both "json"
    // (API-only) and "none" lack one, so a remembered "html" preference
    // must not carry over onto a page that cannot honour it.
    const HTML_AVAILABLE = (pane && pane.dataset.previewMode) === "html";
    let surface = (pane && pane.dataset.previewMode) === "html" ? "html" : "json";
    try {
        const saved = localStorage.getItem(SURFACE_KEY);
        // A remembered choice is only honoured where the page can serve
        // it: "html" needs a template, "site" needs a configured
        // frontend. Otherwise the editor opens on a blank pane and the
        // preference is invisible to undo.
        if (saved === "json"
            || (saved === "html" && HTML_AVAILABLE)
            || (saved === "site" && SITE_AVAILABLE)) surface = saved;
    } catch (_) { /* private mode */ }
    // #perf — the preview frame is loaded lazily (see the `data-src` note
    // above). `previewLoaded` flips the first time anything populates the
    // frame (initial src, or a `srcdoc` refresh) so we never clobber a
    // live-refreshed preview with a stale initial load.
    let previewLoaded = false;
    function ensurePreviewLoaded() {
        if (previewLoaded || !iframe) return;
        const src = iframe.dataset.src;
        if (!src) return;
        previewLoaded = true;
        iframe.src = src;
    }
    // Load once the editor is interactive, and only if the pane is shown.
    function maybeLoadPreviewSoon() {
        if (root.hasAttribute("data-preview-hidden")) return;
        const go = function () {
            // In JSON mode the iframe is never fetched at all — that is
            // what keeps axe-core (540 KB) and the a11y scan out of an
            // API page. Deferred through the same idle callback as the
            // HTML path: calling `reloadPreview` synchronously from here
            // hits the temporal dead zone on `inFlight`, which is
            // declared further down.
            if (surface === "json") { reloadPreview(); return; }
            if (surface === "site") { ensureSiteLoaded(); return; }
            ensurePreviewLoaded();
        };
        if ("requestIdleCallback" in window) {
            requestIdleCallback(go, { timeout: 2000 });
        } else {
            setTimeout(go, 200);
        }
    }
    // Refresh + viewport buttons live in the topbar's action_bar
    // block, OUTSIDE `.rcms-edit-with-preview` — query the document for
    // them, not `root`.
    const refresh = document.querySelector("[data-preview-refresh]");
    // Multiple toggle buttons stay in sync: one in the topbar
    // (always reachable) + one close-X inside the preview pane
    // (disappears with the pane, but the topbar one survives).
    const toggles = document.querySelectorAll("[data-preview-toggle]");

    // Per-tenant + per-page-type localStorage key so different
    // contexts can settle on different widths.
    const STORAGE_KEY = "rcms_edit_split:" + SCOPE + ":" + VARIANT;
    const HIDE_KEY    = "rcms_edit_split_hidden:" + SCOPE;

    // Restore previously-picked width. Skipped when the preview is
    // hidden — in that state the editor must run flex: 1 1 100% so
    // it can fill the viewport. The inline flex is reapplied on
    // toggle-back so the user's resize choice persists.
    function applySavedFlex() {
        const saved = parseInt(localStorage.getItem(STORAGE_KEY) || "", 10);
        if (saved && !Number.isNaN(saved) && left) {
            left.style.flex = "0 0 " + saved + "px";
        }
    }
    function clearFlex() {
        if (left) left.style.flex = "";
    }
    if (!(localStorage.getItem(HIDE_KEY) === "1")) {
        applySavedFlex();
    }

    // Sync every `[data-preview-toggle]` button's icon + label to
    // the current hidden state. The topbar toggle has explicit
    // `.rcms-preview-toggle-icon` / `.rcms-preview-toggle-label` children;
    // the in-pane close button uses the inner icon as its label.
    function syncToggles(hidden) {
        for (const btn of toggles) {
            const iconEl =
                btn.querySelector(".rcms-preview-toggle-icon") ||
                btn.querySelector(".material-symbols-rounded");
            const labelEl = btn.querySelector(".rcms-preview-toggle-label");
            if (iconEl) iconEl.textContent = hidden ? "visibility" : "close";
            if (labelEl) labelEl.textContent = hidden ? "Preview" : "Hide preview";
            btn.title = hidden ? "Show preview" : "Hide preview";
            btn.setAttribute("aria-pressed", hidden ? "false" : "true");
        }
    }

    // Mirror the hidden flag to <html> so CSS selectors in the
    // sticky topbar (outside `.rcms-app-content`) can also key off it —
    // e.g. `[data-preview-hidden] .rcms-preview-controls { display: none }`.
    function setHidden(hidden) {
        if (hidden) {
            root.setAttribute("data-preview-hidden", "");
            document.documentElement.setAttribute("data-preview-hidden", "");
        } else {
            root.removeAttribute("data-preview-hidden");
            document.documentElement.removeAttribute("data-preview-hidden");
            // First reveal → fetch the preview now.
            ensurePreviewLoaded();
        }
    }

    // Restore hidden state from prior session.
    if (localStorage.getItem(HIDE_KEY) === "1") {
        setHidden(true);
    }
    syncToggles(root.hasAttribute("data-preview-hidden"));
    maybeLoadPreviewSoon();

    // -- drag-to-resize --
    let dragging = false;
    if (handle) {
        handle.addEventListener("mousedown", function (e) {
            dragging = true;
            handle.classList.add("dragging");
            document.body.style.cursor = "col-resize";
            // Suppress iframe pointer events so the cursor doesn't
            // get eaten by the embedded document mid-drag.
            if (iframe) iframe.style.pointerEvents = "none";
            e.preventDefault();
        });
        document.addEventListener("mousemove", function (e) {
            if (!dragging || !left) return;
            const rect = root.getBoundingClientRect();
            const newLeft = e.clientX - rect.left;
            const minLeft = 320;
            const minRight = 280;
            const max = rect.width - minRight - 6 /* handle width */;
            const clamped = Math.max(minLeft, Math.min(newLeft, max));
            left.style.flex = "0 0 " + clamped + "px";
            // #256 — also pin the preview pane's flex-basis so its
            // iframe redraws to the new column width. Without this
            // Chromium / Safari sometimes hold the iframe at its
            // original layout-time width when only the partner pane's
            // basis changes.
            if (pane) {
                const previewWidth = Math.max(minRight, rect.width - clamped - 6);
                pane.style.flex = "0 0 " + previewWidth + "px";
            }
            try { localStorage.setItem(STORAGE_KEY, String(Math.round(clamped))); } catch (_) {}
        });
        document.addEventListener("mouseup", function () {
            if (!dragging) return;
            dragging = false;
            handle.classList.remove("dragging");
            document.body.style.cursor = "";
            if (iframe) iframe.style.pointerEvents = "";
            // #256 — nudge the iframe document to recompute its
            // responsive layout. Browsers don't fire `resize` inside
            // an iframe when its CONTAINER changes width — same-
            // origin pages must be told explicitly.
            try {
                if (iframe && iframe.contentWindow) {
                    iframe.contentWindow.dispatchEvent(new Event("resize"));
                }
            } catch (_) {
                // Cross-origin preview — can't reach across the boundary;
                // the browser will eventually repaint on its own.
            }
        });
    }

    // -- preview reload --
    //
    // Flow: client POSTs
    // form data to the preview endpoint, server renders a virtual
    // (unsaved) page from those values + returns the rendered HTML,
    // client writes the HTML into the iframe via `srcdoc` so we get
    // a fresh paint with zero browser-cache involvement.
    //
    // Why fetch() not POST-targeting-iframe: the iframe `target=`
    // approach silently swallows errors + relies on browser to honor
    // the POST in a frame load (Safari is moody about this). With
    // fetch we (a) see HTTP errors in DevTools, (b) cancel in-flight
    // requests via AbortController when a newer edit lands, (c) get
    // exact bytes-in-iframe via `srcdoc`.
    // Tera HTML-escapes attribute values inside the rendered JS,
    // so URLs come through as `&#x2F;cms-admin&#x2F;…` — useless for
    // DOM selectors. Use `| safe` for the preview URL (it lands in
    // a fetch arg, not HTML) and id-based lookup for the form (the
    // edit form sets `id="page-edit-form"` exactly for this).
    const PREVIEW_URL = pane ? (pane.dataset.previewUrl || "") : "";
    // The form whose current values are POSTed for the unsaved preview.
    // Absent (or naming nothing) means "no live draft" — the pane still
    // shows the saved render, it just cannot preview unsaved edits.
    const editForm = pane && pane.dataset.previewForm
        ? document.getElementById(pane.dataset.previewForm)
        : null;
    let inFlight = null;
    /// Render a JSON response into the `<pre>`, pretty-printing when it
    /// parses and falling back to the raw text when it doesn't — an
    /// error body is the single most useful thing an API preview can
    /// show, so it must never be swallowed.
    function renderJson(text, status, statusText, ms) {
        if (!jsonBody) return;
        const keepScroll = jsonBody.scrollTop;
        let out = text;
        try {
            out = JSON.stringify(JSON.parse(text), null, 2);
        } catch (_) { /* not JSON — show it verbatim */ }
        const LIMIT = 1000000;
        if (out.length > LIMIT) {
            out = out.slice(0, LIMIT) + "\n\n… response truncated";
        }
        jsonBody.textContent = out;
        jsonBody.scrollTop = keepScroll;
        if (jsonStatus) {
            jsonStatus.textContent = status + " " + statusText;
            jsonStatus.className = "rcms-tag " + (status < 400 ? "published" : "expired");
        }
        if (jsonMeta) {
            const bytes = new TextEncoder().encode(text).length;
            jsonMeta.textContent = bytes.toLocaleString() + " B · " + Math.round(ms) + " ms";
        }
    }

    async function reloadPreview() {
        if (surface === "html" && !iframe) return;
        if (surface === "html" && !editForm) {
            // Never loaded yet (lazy) → the initial load *is* the refresh;
            // reloading an about:blank frame would just re-blank it.
            if (!previewLoaded) { ensurePreviewLoaded(); return; }
            try { iframe.contentWindow.location.reload(); }
            catch (_) { iframe.src = PREVIEW_URL; }
            return;
        }
        // The srcdoc path below populates the frame itself.
        if (surface === "html") previewLoaded = true;
        if (jsonWrap && surface === "json") jsonWrap.dataset.stale = "";
        const t0 = performance.now();
        // Cancel any prior in-flight refresh so the user always sees
        // the result of the LATEST edit, not whichever response
        // happens to arrive last.
        if (inFlight) inFlight.abort();
        const ctrl = new AbortController();
        inFlight = ctrl;
        // rustango CSRF is double-submit-cookie: the request must
        // carry an `X-CSRF-Token` header whose value matches the
        // `rustango_csrf` cookie (see `rustango::forms::csrf::layer()`).
        // The axum `Form` extractor only accepts
        // `application/x-www-form-urlencoded`, so we serialize via
        // URLSearchParams (which sets that content-type
        // automatically) rather than FormData (multipart).
        const params = new URLSearchParams();
        for (const [k, v] of new FormData(editForm).entries()) {
            if (typeof v === "string") params.append(k, v);
        }
        const csrfCookie = (document.cookie.split(";").find(
            c => c.trim().startsWith("rustango_csrf=")
        ) || "").split("=")[1] || "";
        try {
            // `?format=json` is an explicit editor request, honoured for
            // ANY page type — unlike the public URL, where only a type
            // that opted into `view_mode` serves JSON.
            const url = surface === "json"
                ? PREVIEW_URL + (PREVIEW_URL.includes("?") ? "&" : "?") + "format=json"
                : PREVIEW_URL;
            const resp = await fetch(url, {
                method: "POST",
                body: params,
                credentials: "same-origin",
                signal: ctrl.signal,
                headers: {
                    "Accept": surface === "json" ? "application/json" : "text/html",
                    "X-CSRF-Token": csrfCookie,
                },
            });
            const text = await resp.text();
            if (surface === "json") {
                // Deliberately no `resp.ok` guard here: a 4xx/5xx body is
                // exactly what an API author needs to see.
                renderJson(text, resp.status, resp.statusText, performance.now() - t0);
            } else {
                if (!resp.ok) {
                    console.warn("preview reload: HTTP", resp.status);
                    return;
                }
                // `srcdoc` injects the bytes directly — no cache, no
                // separate network fetch, and the iframe's parent
                // origin still applies.
                iframe.srcdoc = text;
            }
        } catch (e) {
            if (e.name !== "AbortError") {
                console.warn("preview reload failed:", e);
            }
        } finally {
            if (inFlight === ctrl) inFlight = null;
            if (jsonWrap) delete jsonWrap.dataset.stale;
        }
    }
    if (refresh) {
        refresh.addEventListener("click", function () {
            // In site mode there is nothing to re-render server-side —
            // the frontend fetches the page itself — so reload the frame
            // rather than POSTing the form to a preview route.
            if (surface === "site" && siteFrame) {
                if (!siteLoaded) ensureSiteLoaded();
                // eslint-disable-next-line no-self-assign
                else siteFrame.src = siteFrame.src;
                return;
            }
            reloadPreview();
        });
    }

    // -- HTML / JSON / site surface switch --
    // The viewport buttons mean something for either iframe, so they
    // hide only in JSON mode rather than sitting there inert.
    const surfaceBtns = document.querySelectorAll("[data-preview-surface]");
    const viewportGroup = document.querySelector("[data-viewport-group]");
    function applySurface(next, { refetch = true, persist = true } = {}) {
        if (next === "html" && !HTML_AVAILABLE) return;
        if (next === "site" && !SITE_AVAILABLE) return;
        surface = next;
        // Only a click is a preference. An API-only page *forces* JSON,
        // and persisting that would silently rewrite the editor's choice
        // for every other page they open next.
        if (persist) {
            try { localStorage.setItem(SURFACE_KEY, next); } catch (_) { /* private mode */ }
        }
        for (const b of surfaceBtns) {
            b.setAttribute("aria-pressed", String(b.dataset.previewSurface === next));
        }
        if (htmlStage) htmlStage.hidden = next !== "html";
        if (siteStage) siteStage.hidden = next !== "site";
        if (jsonWrap) jsonWrap.hidden = next !== "json";
        // Widths apply to anything rendered in a frame — checking a
        // decoupled site at 375px is worth as much as checking the
        // CMS-rendered one.
        if (viewportGroup) viewportGroup.hidden = next === "json";
        // Re-measure: the newly shown stage was display:none a moment
        // ago, so it had no box to scale against.
        if (next !== "json") remeasureViewport();
        if (!refetch) return;
        if (next === "html") {
            // First switch into HTML may be the frame's first ever load.
            if (!previewLoaded) ensurePreviewLoaded(); else reloadPreview();
        } else if (next === "site") {
            // Cross-origin: we cannot push edits in, only (re)load it.
            if (!siteLoaded) ensureSiteLoaded(); else if (siteFrame) {
                // eslint-disable-next-line no-self-assign
                siteFrame.src = siteFrame.src;
            }
        } else {
            reloadPreview();
        }
    }
    for (const b of surfaceBtns) {
        b.addEventListener("click", () => applySurface(b.dataset.previewSurface));
    }
    // Reflect the restored choice without firing a fetch — the lazy
    // loader below decides when the first request actually happens.
    applySurface(surface, { refetch: false, persist: false });

    // -- auto-refresh preview after edits --
    // Debounce 1s after the last input/change so editors see their
    // edits without spamming the server on every keystroke. Skipped
    // when the preview pane is hidden.
    const REFRESH_DEBOUNCE_MS = 1000;
    let refreshTimer = null;
    function scheduleRefresh() {
        if (document.documentElement.hasAttribute("data-preview-hidden")) return;
        if (refreshTimer) clearTimeout(refreshTimer);
        refreshTimer = setTimeout(reloadPreview, REFRESH_DEBOUNCE_MS);
    }
    // Document-level catch-all: any user input ANYWHERE on the page
    // (form field, stream-internal input, etc.) re-arms the timer.
    // This catches edits the form-scoped listener would miss (e.g.
    // stream block inputs whose `name=""` keeps them out of the
    // form's bubble path but they still emit `input`).
    document.addEventListener("input", scheduleRefresh);
    document.addEventListener("change", scheduleRefresh);
    // Stream block add/remove/reorder clicks don't fire `input` —
    // hook them explicitly. Same data-action verbs the stream
    // editor JS uses.
    document.addEventListener("click", function (e) {
        const action = e.target.closest("[data-action]");
        if (!action) return;
        const verb = action.dataset.action;
        if (verb === "remove" || verb === "move-up" || verb === "move-down" || verb === "pick-block" || verb === "duplicate") {
            // Stream JS updates the hidden input asynchronously on
            // the same click; nudge our schedule slightly after so
            // the next reloadPreview FormData snapshot is fresh.
            setTimeout(scheduleRefresh, 50);
        }
    });

    // -- hide / show toggle (any `[data-preview-toggle]` button) --
    // Hiding clears the drag handler's inline flex so the stylesheet
    // rule `[data-preview-hidden] .rcms-edit-pane { flex: 1 1 100% }`
    // wins — without this the editor stays stuck at whatever px
    // width the user dragged to. Re-showing restores the saved
    // width so the previous split is preserved.
    for (const btn of toggles) {
        btn.addEventListener("click", function () {
            const willHide = !root.hasAttribute("data-preview-hidden");
            if (willHide) {
                setHidden(true);
                localStorage.setItem(HIDE_KEY, "1");
                clearFlex();
            } else {
                setHidden(false);
                localStorage.setItem(HIDE_KEY, "0");
                applySavedFlex();
            }
            syncToggles(willHide);
        });
    }

    // -- viewport buttons — fit-to-pane scaling --
    //
    // The iframe DOM stays at the chosen DEVICE WIDTH (375 / 768 /
    // 1280) so the embedded page's media queries fire as on a real
    // phone / tablet / desktop. CSS `transform: scale(N)` on the
    // iframe shrinks (or grows, if pane is wider than device) the
    // visual output to fit the editor's preview pane. A wrapping
    // `.rcms-preview-frame-sizer` carries the SCALED footprint so flex
    // layout reserves the right space and centers the iframe.
    //
    // `full` mode clears the scaling — iframe fills the pane,
    // matching whatever the actual pane width is.
    //
    // Recomputed on:
    //   - viewport-btn click (the picker)
    //   - split-handle drag mouseup (pane width changed)
    //   - window resize (pane width may have changed)
    //
    // Active state is mirrored to `aria-pressed`; pick persists
    // per tenant in localStorage.
    const VP_KEY = "rcms_preview_viewport:" + SCOPE;
    const stage = root.querySelector('[data-preview-stage]:not([data-surface="site"])');
    const sizer = stage && stage.querySelector("[data-preview-frame-sizer]");
    // Viewport buttons live in the topbar's action_bar block, outside
    // `root` — query the document.
    const vpButtons = document.querySelectorAll("[data-viewport]");
    let currentViewport = "full";

    // Whichever frame is on screen. Width checks are worth as much on a
    // decoupled frontend as on the CMS-rendered one, so the viewport
    // buttons drive whichever stage is visible rather than always the
    // HTML one.
    function activeFrame() {
        if (surface === "site" && siteStage) {
            return { stage: siteArea, sizer: siteSizer, frame: siteFrame };
        }
        return { stage: stage, sizer: sizer, frame: iframe };
    }

    function paneAvailable(el) {
        if (!el) return { w: 0, h: 0 };
        const cs = getComputedStyle(el);
        const padX = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
        const padY = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom);
        return {
            w: Math.max(0, el.clientWidth - padX),
            h: Math.max(0, el.clientHeight - padY),
        };
    }

    // `applySurface` runs during initialisation, *before* the viewport
    // block below has evaluated — reading `currentViewport` from there
    // would hit its temporal dead zone and throw. `var` hoists and
    // initialises to `undefined`, so this flag can be read early; the
    // viewport block sets it once it has applied the first width itself.
    // eslint-disable-next-line no-var
    var viewportReady = false;
    function remeasureViewport() {
        if (!viewportReady) return;
        applyViewport(currentViewport);
    }

    function applyViewport(vp) {
        const { stage, sizer, frame: iframe } = activeFrame();
        if (!stage || !sizer || !iframe) return;
        currentViewport = vp || "full";
        const { w: paneW, h: paneH } = paneAvailable(stage);
        // Guard a transient 0-height pane so we never set a 0px iframe.
        const fitH = paneH > 0 ? paneH : 320;
        if (currentViewport === "full") {
            // Iframe fills the pane 1:1; its own document scrolls. Set
            // explicit px (not 100% / removeProperty) so the height can't
            // collapse through the flex chain — that was a stuck-scroll
            // source — and so the stage has nothing to pan.
            sizer.style.setProperty("--rcms-pv-sizer-w", paneW + "px");
            sizer.style.setProperty("--rcms-pv-sizer-h", fitH + "px");
            iframe.style.setProperty("--rcms-pv-frame-w", paneW + "px");
            iframe.style.setProperty("--rcms-pv-frame-h", fitH + "px");
            iframe.style.setProperty("--rcms-pv-scale", "1");
        } else {
            const deviceW = parseInt(currentViewport, 10);
            // Clamp scale so a tiny pane doesn't try to draw the
            // iframe at 0px (which Safari hangs on). Always allow
            // upscaling (HTML rendering is resolution-independent —
            // no blur from scale > 1).
            const scale = paneW > 0 ? Math.max(0.1, paneW / deviceW) : 1;
            // Device pixel height — keep the iframe DOM tall enough
            // that what shows in the visual pane is one pane-height
            // worth of page content. Scaled, this is exactly the pane
            // height, so the scaled footprint == the stage (no outer pan).
            const deviceH = fitH / scale;
            sizer.style.setProperty("--rcms-pv-sizer-w", paneW + "px");
            sizer.style.setProperty("--rcms-pv-sizer-h", fitH + "px");
            iframe.style.setProperty("--rcms-pv-frame-w", deviceW + "px");
            iframe.style.setProperty("--rcms-pv-frame-h", deviceH + "px");
            iframe.style.setProperty("--rcms-pv-scale", String(scale));
        }
        for (const b of vpButtons) {
            b.setAttribute("aria-pressed",
                b.dataset.viewport === currentViewport ? "true" : "false");
        }
        // Tell the embedded document its layout box changed — same-
        // origin previews react to this for any responsive JS they
        // wire on the window `resize` event.
        try {
            if (iframe.contentWindow) {
                iframe.contentWindow.dispatchEvent(new Event("resize"));
            }
        } catch (_) { /* cross-origin — ignore */ }
        try { localStorage.setItem(VP_KEY, currentViewport); } catch (_) {}
    }

    // Re-fit after every iframe (re)load — the initial `src` and each
    // `srcdoc` swap on edit — so the fresh document is sized correctly.
    if (iframe) {
        iframe.addEventListener("load", function () {
            applyViewport(currentViewport);
        });
    }

    if (vpButtons.length > 0) {
        const initial = (function () {
            try { return localStorage.getItem(VP_KEY) || "full"; }
            catch (_) { return "full"; }
        })();
        applyViewport(initial);
        // From here on `currentViewport` is initialised, so a surface
        // switch may safely re-measure through `remeasureViewport`.
        viewportReady = true;
        for (const b of vpButtons) {
            b.addEventListener("click", function () {
                applyViewport(b.dataset.viewport);
            });
        }
        // Pane width changes (split-handle drag, window resize) need
        // to re-derive the scale so the iframe keeps fitting.
        window.addEventListener("resize", function () {
            applyViewport(currentViewport);
        });
        // Hook the drag-mouseup so the scale snaps to the new pane
        // width as soon as the split settles.
        if (handle) {
            handle.addEventListener("mouseup", function () {
                // Tiny delay so the flex layout has settled before
                // we measure `stage.clientWidth`.
                setTimeout(function () {
                    applyViewport(currentViewport);
                }, 0);
            });
        }
    }
})();
