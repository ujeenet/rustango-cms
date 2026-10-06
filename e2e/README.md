# E2E suite (Playwright)

Browser tests for the cms-admin, driven against the `cms_demo` example on a
fresh sqlite state (`e2e/.state`, wiped per run by `serve.sh`).

Needs **Node 22.13 or newer**: the API fixture seeds the database with the
built-in `node:sqlite` module, which older Node versions lack or keep behind
a flag. `.nvmrc` pins 22 for `nvm use`.

```sh
cd e2e
npm install            # once
npx playwright install chromium   # once
npm test               # builds cms_demo, boots it, runs the suite
npm run report         # open the HTML report
```

- Server: `serve.sh` provisions tenant `demo` (host `demo.localhost`, user
  `admin` / `TestPw123!`, superuser) and serves on `127.0.0.1:${E2E_PORT:-8210}`.
  Chromium resolves `*.localhost` to loopback — no /etc/hosts edits.
- Sessions: `tests/auth.setup.ts` logs in once and saves
  `.state/admin.json`; auth.spec runs with a clean context instead.
- Specs own their data (pages/media/forms they create); `media-picker.spec.ts`
  and `forms.spec.ts` are serial internally. `workers: 1` — one shared server.
- Selector gotchas encoded in the specs: the sidebar Sign-out is the FIRST
  `button[type=submit]` in DOM order (never `.first()` a bare submit); the
  page editor has a second, hidden media chooser on the Promote tab (always
  filter `visible=true`).

---

## API suite (`tests/api/`)

Exercises the headless JSON API — pages, tree, menus, media, snippets —
the way a decoupled frontend would: anonymously, as a signed-in member,
across locales, and while an editor writes underneath.

```sh
npm test                                    # everything
npx playwright test --project=api-anon      # the read surface, no cookies
npx playwright test --project=api-admin     # the editor write-back
```

### Projects

| Project | Session | Runs |
|---|---|---|
| `api-setup` | admin | seeds the fixture, registers a member |
| `api-anon` | **none** | every untagged spec |
| `api-member` | member | specs tagged `@member` |
| `api-admin` | admin | specs tagged `@admin` |

`api-anon` is the important one. Most of the value here is "can an
anonymous caller see this?", and a stray `storageState` would make the
whole leak matrix vacuously green — so untagged specs run there and only
there.

### The fixture

`helpers/fixture.ts` seeds `.state/demo.db` directly, with **fixed ids**
(pages from 1000, menus from 100, items from 200), so a failing assertion
names a row you can look up. It is re-run safe.

Direct sqlite writes rather than the admin UI, because most of what these
specs observe cannot be *expressed* through the admin: an alias whose
source was later retitled, a menu item parented into another menu, a
`parent_id` cycle, an `expired` page. The admin write path isn't skipped,
just moved — `menu-writeback.spec.ts` drives the real builder.

Menus: `t-main` is the clean one, `t-other` is a second menu (multi-menu
batching, and deliberately left undeletable to hold B33), `t-broken`
holds the pathological trees, and **`t-scratch` is the only menu write
tests may mutate**.

### Known bugs are pinned, not skipped

A bug that isn't fixed yet gets `test.fail()` with its register id in the
title:

```ts
test("[B27] save-tree deletes items before their translations", async () => {
  test.fail(true, "admin/handlers.rs:15102 — FK has no ON DELETE");
  ...
});
```

`test.fail()` asserts the test *does* fail, so the suite stays green while
the bug is open — and the moment someone fixes it Playwright reports
**"Expected to fail, but passed"** and the run goes red. That is the
signal to delete the `test.fail()` line and keep the assertion. Skipped
tests rot silently; these can't.

31 bugs are pinned this way. The register lives in the plan document; the
titles carry the ids.

### Dialect-dependent bugs

Two findings only manifest on Postgres and are `test.skip`ped here with a
reason:

- **B20** (unstable pagination) — sqlite returns rows in rowid order, which
  masks the missing `ORDER BY`.
- Parts of **B25** — `ORDER BY parent_id` puts NULLs *last* on Postgres and
  *first* on sqlite, so the menu builder flattens the tree on one and
  re-parents on the other. The sqlite variant is covered.

Set `E2E_DIALECT=postgres` and point the suite at a Postgres-backed host
to run them.

### Traps

- Error bodies are `text/plain`, so `res.json()` throws on every failure.
  Use `json()` / `getJson()` from `helpers/client.ts` — they check
  `content-type` first and report what the server actually sent.
- Playwright's default request context **follows redirects**. A gated page
  then arrives as a 200 with an HTML login body. Use `maxRedirects: 0`
  when the redirect itself is the thing under test.
- The first-ever view of any CSRF form renders a token that doesn't match
  its cookie (**B59**), so a first POST 403s. Load the form twice. This is
  a real bug, pinned in `spa-integration.spec.ts`.
