# codemirror.bundle.js — vendored template editor

Pre-built, self-hosted bundle of [CodeMirror 5](https://codemirror.net/5/) (MIT),
pinned at **5.65.21**. Exposes the usual global `CodeMirror`, plus a `tera` mode
registered by the entry script. Lazy-loaded by `template-editor.js` only on the
template-editor page, the same way `tiptap.bundle.js` is — so no other admin
page pays for it.

CodeMirror 5 rather than 6 because 6 is ESM-only and needs a bundler at runtime;
this project has no runtime JS build step. Monaco was rejected as ~5 MB.

The CSS is inlined into the bundle and injected as a `<style>` on load, so this
really is one file and works under a strict `script-src 'self'` CSP.

## What's in it

Core, plus `xml`/`javascript`/`css`/`htmlmixed` modes, the `overlay` mode addon,
`matchbrackets`, `closetag`, `active-line`, `placeholder`, `dialog`,
`searchcursor`/`search` (Ctrl-F) and `comment` (Ctrl-/).

The **`tera` mode** is ours: `htmlmixed` with an overlay that highlights
`{{ … }}`, `{% … %}` and `{# … #}`. Tera has no upstream CodeMirror mode and
the stock `jinja2` one does not nest inside HTML.

## Rebuild

The build source is committed as **`codemirror.entry.mjs`** — don't hand-edit
the minified bundle.

```sh
mkdir -p /tmp/cm-build && cd /tmp/cm-build
npm init -y >/dev/null
npm install codemirror@5.65.21 esbuild
cp <repo>/src/admin/static/vendor/codemirror.entry.mjs build.mjs
node build.mjs                       # concatenates lib + modes + addons + tera mode
npx esbuild codemirror.bundle.js --minify --target=es2017 \
  --outfile=codemirror.bundle.min.js
cp codemirror.bundle.min.js <repo>/src/admin/static/vendor/codemirror.bundle.js
```

~263 KB raw, ~86 KB gzipped (TipTap, for scale, is 353 KB raw).
