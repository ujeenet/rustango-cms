# tiptap.bundle.js — vendored richtext editor (#294)

Pre-built, self-hosted IIFE bundle of [TipTap](https://tiptap.dev) (MIT) +
ProseMirror (MIT). Exposes `window.RcmsRichtext.create({element, content, onUpdate})`.
The toolbar UI + wiring live in the hand-authored `richtext-editor.js`.

## Rebuild

TipTap is a headless npm library; this project has no runtime JS build step,
so the bundle is committed like `axe-core.min.js`. The build source is
committed alongside it as **`tiptap.entry.js`** (so a rebuild is
reproducible — don't hand-edit the minified bundle). To regenerate:

```sh
mkdir -p /tmp/tiptap-build && cd /tmp/tiptap-build
npm init -y >/dev/null && npm pkg set type=module
npm install @tiptap/core@~2.27 @tiptap/starter-kit@~2.27 @tiptap/extension-link@~2.27 \
  @tiptap/extension-image@~2.27 @tiptap/extension-table@~2.27 \
  @tiptap/extension-table-row@~2.27 @tiptap/extension-table-header@~2.27 \
  @tiptap/extension-table-cell@~2.27 @tiptap/pm@^2 esbuild
cp <repo>/src/admin/static/vendor/tiptap.entry.js entry.js
./node_modules/.bin/esbuild entry.js --bundle --minify --format=iife --target=es2020 --outfile=tiptap.bundle.js
cp tiptap.bundle.js <repo>/src/admin/static/vendor/tiptap.bundle.js
```

Pinned at build time: TipTap 2.27.2. Extensions: StarterKit (headings h2–h4,
bold/italic/strike/code, lists, blockquote, code-block, hr) + Link + Image +
Table (table-row/header/cell, non-resizable).

The Link mark is extended (see `tiptap.entry.js`) to carry
**move-safe** attributes — `<a linktype="page|media" id="N">` (no `href`).
Stock Link only matches `a[href]` and would drop `linktype`/`id` on edit,
silently breaking links that survive page/media moves; the override adds an
`a[linktype]` parse rule + round-trips both attributes.
