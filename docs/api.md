# Headless JSON API

The read API a decoupled frontend consumes: pages, the site's **structure**,
and the **navigation menus** — localized, and filtered to what the caller is
allowed to see.

Everything here is `GET`, anonymous, and read-only. Mount it alongside the
public router:

```rust
let app = cms_admin
    .merge(rustango_cms::rendition_route::router())
    .merge(rustango_cms::api::router_with(&rustango_cms::api::Cors::from_env()))
    .merge(public_pages);
```

`api::router()` is the same thing with CORS off.

Register it **explicitly**, and before the public router in your own reading
order: the public router ends in a catch-all that renders the CMS 404 page, so
a host that skips this merge answers every `/api/v2/…` request with HTML.

### Authenticating a member

The member session is an `HttpOnly; SameSite=Lax` cookie, and Lax cookies
are **not sent on cross-site fetches** — so a SPA on another origin cannot
read gated content with the cookie alone, no matter how CORS is
configured. Exchange credentials for a token instead:

```console
$ curl -X POST https://example.com/api/v2/auth/login \
       -H 'Content-Type: application/json' \
       -d '{"identifier":"reader@example.com","password":"…"}'
{ "token": "…", "token_type": "Bearer", "expires_in": 604800,
  "member": { "id": 7, "username": "reader", "email": "reader@example.com" } }
```

Then send `Authorization: Bearer <token>` on any read endpoint.

The token is **not a new credential format**: it is the same signed value
the cookie carries, so it inherits the same expiry, the same tenant
binding, and the same rule that changing a password invalidates
outstanding sessions. Only the transport differs.

An invalid or absent token means *anonymous*, never an error — these
endpoints serve anonymous callers by design, and a stale token must not
turn a public page into a 401. A bad password and an unknown account
return the identical 401, since the difference is an account-enumeration
oracle.

Precedence, when more than one is present: member cookie, then bearer
token, then admin session.

### Cross-origin

**CORS is off unless you turn it on.** Enabling it makes every endpoint
readable by any page a browser will load, so it stays an explicit act:

```sh
RCMS_API_CORS_ORIGINS=https://app.example.com,https://admin.example.com
```

Listed origins may call `POST /api/v2/auth/login` and send an
`Authorization: Bearer` token, which is how a SPA on another origin reads
gated content (see [Authenticating a member](#authenticating-a-member) —
the session cookie does not travel on cross-site requests). They also get
`Access-Control-Allow-Credentials`, for a frontend on a sibling subdomain
of the same site, where the cookie does travel. The literal `*` reflects
any origin **without** credentials — development only. Unset, or empty, means no CORS headers and no preflight handling,
which is what every existing deployment already had.

`ETag` and `Allow` are named in `Access-Control-Expose-Headers`. Browsers
hide every response header from JavaScript except a short safelist, so
without this a cross-origin SPA could not read the `ETag` it is supposed
to send back in `If-None-Match` — conditional requests would be dead on
arrival for browser clients specifically.

### Trailing slash

Both forms work: `/api/v2/pages` and `/api/v2/pages/` are the same route.

## What is public

| | Served |
| --- | --- |
| Page status | `published` **and** `archived` |
| Error pages | excluded from `children`, the tree, menus and the sitemap; listed by `pages/` like any page |
| Legacy locale variants | excluded (except under `?translation_of=`) |
| View-restricted pages | excluded unless the caller may see them |

Every surface agrees, deliberately. `archived` is public
(`PageStatus::is_public`): the page resolves at its URL, appears in the
sitemap, and can be linked from a menu — so hiding it from a listing would
describe a site that doesn't exist.

### Access control

**Every** endpoint resolves the caller — a member session outranks an admin
one, matching the public renderer — and drops anything gated by a login /
group / permission restriction. Filtering is **subtree-aware**: gating a page
gates its descendants, so a member-only section never leaks its child titles
or URLs to an anonymous caller.

That includes the flat list and its `?child_of=` / `?descendant_of=` /
`?ancestor_of=` filters, whose anchors resolve against the *visible* set — a
caller cannot pivot off a page they cannot see. `find/` answers a gated page
and a missing one identically, and the `did_you_mean` search hint is only
offered when the suggested title belongs to a page the caller may see.

Images and documents gate on their **collection**'s restriction, matching
`/__media__/`, which refuses to serve the bytes. Listing an asset whose
collection is closed would publish its title, filename and dimensions even
though the file itself is protected.

---

## List parameters

Every collection list — pages, images, documents, snippets — takes these,
on top of its own filters below:

| Parameter | Default | Notes |
| --- | --- | --- |
| `limit` | `20` | Clamped to `1..=100`. See [Pagination](#pagination). |
| `offset` | `0` | Rows to skip. |
| `fields` | all | Comma-separated top-level keys to keep; `id` and `meta` always stay. An unknown name is a `400`. See [Sparse fields](#sparse-fields). |
| `order` | per endpoint | `title`, `-title`, `a,-b`, or `random`. See [Ordering](#ordering). |
| `search` | none | Case-insensitive substring match; which fields it reads is listed per endpoint. |
| `updated_since` | none | RFC 3339; see [Polling for changes](#polling-for-changes). |

A list answers `{ "meta": { … }, "items": [ … ] }`, where `meta` is the
paging block from [Pagination](#pagination). A detail answers one object
of the same shape as a list item (plus the extras noted below) and takes
`?fields=` too. Every list is filtered *before* paging, so `total_count`
counts only what this caller may see.

## `GET /api/v2/pages/`

Public pages (see [What is public](#what-is-public)), minus anything this
caller may not see.

| Parameter | Notes |
| --- | --- |
| `type` | Page type name, bare (`BlogPage`) or with its app label (`blog.BlogPage`). An unknown type returns an empty list. |
| `child_of` | Direct children of page N. |
| `descendant_of` | Every descendant of page N, excluding N. |
| `ancestor_of` | Every ancestor of page N, excluding N. |
| `translation_of` | Legacy per-locale copies of page N (the `locale_variant_of` rows, otherwise excluded). |
| `tag` | Pages carrying this tag. |
| `id_in` | `3,9,14` — see [Fetching several pages at once](#fetching-several-pages-at-once). |
| `locale` | A legacy row filter, **not** a content selector — see [A note on `?locale=`](#a-note-on-locale). Use `translation_of` or the detail endpoint's `?locale=`. |
| `search` | Title, slug and URL path, plus hits from the configured search backend (Postgres FTS or Elasticsearch). Backend hits come first in relevance order unless `order` is given; editorial search promotions are pinned to the front. A search with no hits may set `meta.did_you_mean`. |

Filters combine (AND). `child_of` / `descendant_of` / `ancestor_of`
anchors resolve against the visible set, so a gated anchor returns an
empty list, same as a missing one. Default order: newest `published_at`
first, then title. Sortable fields: `title`, `slug`, `url_path`, `depth`,
`sort_order`, `published_at`, `updated_at`, `created_at`.

```json
{
  "meta": { "total_count": 1, "limit": 20, "offset": 0, "has_more": false },
  "items": [
    {
      "id": 3,
      "meta": { "type": "BlogPage", "detail_url": "/api/v2/pages/3/",
                "html_url": "/blog/hello", "slug": "hello", "depth": 3,
                "parent_id": 2, "alias_of": null,
                "first_published_at": "2026-01-02T15:04:05Z",
                "updated_at": "2026-01-03T09:00:00Z" },
      "title": "Hello", "url": "/blog/hello",
      "seo_title": "", "seo_description": "",
      "robots_index": true, "sitemap_priority": 0.5, "show_in_menus": true
    }
  ]
}
```

## `GET /api/v2/pages/{id}/`

The list item, plus:

| Key | Present when |
| --- | --- |
| `extension` | The page type's `load_extension` returns data. A StreamField column is the block array (`[{ "type", "id", "value" }, …]`), not JSON text. |
| `builder` | The page type has a UI-defined body schema. Values only; the client renders. |
| `routes` | The page type declares routable sub-URLs: `[{ "name", "pattern" }]`. |
| `children` | Always — see [`children` on page detail](#children-on-page-detail). |
| `meta.locale` | Always — the locale the body was built in. |

| Parameter | Notes |
| --- | --- |
| `locale` | Content selector: overlays that locale's translations field by field, falling back to canonical content where untranslated. |
| `fields` | Sparse selection. |
| `preview_token` | A signed token for this id unlocks a draft — see [Previewing a draft](#previewing-a-draft). |

A draft and a missing id both answer `404`. A
view-restricted page answers `401`, `403` or `password_required` — see
[Errors](#errors). An alias serves its source page's content under its own
id and URL; the alias's own restrictions apply.

## `GET /api/v2/images/`

Image rows from the media library. Assets in a collection the caller may
not see are dropped.

| Parameter | Notes |
| --- | --- |
| `collection` | Only images in collection N. |
| `search` | Title, filename and alt text. |

Default order: newest upload first. Sortable fields: `title`, `filename`,
`size`, `uploaded_at`. `updated_since` compares against `uploaded_at`.

```json
{
  "id": 7,
  "meta": { "type": "cms.Image", "detail_url": "/api/v2/images/7/",
            "download_url_template": "/__media__/{filter_spec}/7?v=3f1a9c0e2b7d" },
  "renditions": { "thumbnail": "…", "medium": "…", "large": "…" },
  "title": "Docs diagram", "filename": "diagram.png", "alt_text": "…",
  "description": "", "mime": "image/png", "size": 48213,
  "width": 1600, "height": 900, "focal_point_x": null, "focal_point_y": null,
  "collection_id": null, "uploaded_at": "2026-01-01T09:00:00Z"
}
```

`renditions` are ready-to-use URLs (`fill-100x100`, `width-800`,
`width-1600`), signed server-side when rendition signing is on.
`download_url_template` lets a client build other sizes by substituting
a filter spec, but it cannot sign them.

## `GET /api/v2/images/{id}/`

One image, same shape. A non-image id and an image in a gated collection
both answer `404`.

## `GET /api/v2/documents/`

Every non-image media row. Same collection gating as images.

| Parameter | Notes |
| --- | --- |
| `collection` | Only documents in collection N. |
| `search` | Title and filename. |

Order and `updated_since` behave as for images.

```json
{
  "id": 12,
  "meta": { "type": "cms.Document", "detail_url": "/api/v2/documents/12/",
            "storage_key": "…", "content_hash": "…" },
  "title": "Price list", "filename": "prices.pdf", "mime": "application/pdf",
  "size": 90112, "kind": "document", "collection_id": null,
  "uploaded_at": "2026-01-01T09:00:00Z"
}
```

## `GET /api/v2/documents/{id}/`

One document, same shape. An image id and a gated collection both answer
`404`.

## `GET /api/v2/snippets/`

Library snippets. Snippets carry no publish state or view restriction, so
every snippet in the tenant is listed, for every caller.

| Parameter | Notes |
| --- | --- |
| `type` | Only snippets of this library type name. |
| `search` | Title, slug, type name and markdown body. |

Default order: most recently updated first. Sortable fields: `title`,
`slug`, `type_name` (or `type`), `updated_at`, `created_at`.

```json
{
  "id": 4,
  "meta": { "type": "cms.Snippet", "type_name": "faq",
            "detail_url": "/api/v2/snippets/4/",
            "updated_at": "2026-01-01T09:00:00Z" },
  "type_name": "faq", "slug": "shipping", "title": "Shipping",
  "folder_path": "", "body_markdown": "…", "data": {}
}
```

## `GET /api/v2/snippets/{id}/`

One snippet, same shape.

---

## `GET /api/v2/pages/tree/`

The site's page hierarchy, already nested.

| Parameter | Default | Notes |
| --- | --- | --- |
| `root` | whole site | Return the subtree under this page id. The page itself is excluded; its children become the top level. |
| `depth` | `3` | Levels to walk. Clamped to `10`. |
| `locale` | tenant default | Localize titles via `cms_translation`. |

```console
$ curl 'https://example.com/api/v2/pages/tree/?depth=2'
```

```json
{
  "meta": { "depth": 2, "root": null, "locale": "en", "total_count": 4, "truncated": false },
  "items": [
    {
      "id": 1,
      "title": "Home",
      "slug": "home",
      "url": "/",
      "depth": 1,
      "has_children": true,
      "detail_url": "/api/v2/pages/1/",
      "children": [
        { "id": 2, "title": "About", "slug": "about", "url": "/about",
          "depth": 2, "has_children": false,
          "detail_url": "/api/v2/pages/2/", "children": [] }
      ]
    }
  ]
}
```

An **alias** page appears as its own node — it is a genuine second URL — and
carries an extra `"alias_of": <source id>`. Its title is resolved from the
source page, because the alias row stores a stale copy taken at creation and
the renderer shadows it the same way.

`has_children` describes the page, not the response: a node cut off by `depth`
still reports `true`, so a client knows there is more to fetch.

`?root=` returns **404** for a page that doesn't exist, isn't public, or is
gated for this caller — the three are indistinguishable from outside on
purpose, so the parameter can't be used to probe for hidden pages.

### Bounds

The list endpoints' `limit` is a *row* cap and means nothing to a tree, so
this endpoint carries its own: `depth` defaults to 3 and is clamped to 10, and
the response is capped at **5000 nodes**. Past that the tree is truncated in
depth-first order, `meta.truncated` is `true` and `meta.dropped_count` says
how many nodes the cap removed — the shape stays valid.

### Cost

One `ORDER BY path` fetch (already tree pre-order, and `path` is indexed),
bucketed by parent in memory. The query count does not grow with the number of
nodes. It deliberately does **not** query by `parent_id`: `cms_page` has no
index on that column, so a per-parent lookup would be an N+1 *and* a scan per
node.

---

## `children` on page detail

`GET /api/v2/pages/{id}/` now includes the page's **direct** children, so a
client can walk down from any page without fetching the tree first:

```json
"children": [
  { "id": 3, "title": "Docs", "slug": "docs", "url": "/docs",
    "has_children": true, "detail_url": "/api/v2/pages/3/" }
]
```

One level, one query — children and grandchildren are fetched together and
sliced by `depth`, so `has_children` costs nothing extra. A leaf page returns
`"children": []` rather than omitting the key. Capped at **100**; use
`/api/v2/pages/?child_of=` when you need the whole set, which is paged.

---

## `GET /api/v2/menus/`

```json
{
  "meta": { "total_count": 1, "limit": 20, "offset": 0,
            "has_more": false, "locale": "en" },
  "items": [
    { "slug": "main", "name": "Main navigation", "detail_url": "/api/v2/menus/main/" }
  ]
}
```

Paged like every other list (`?limit=`, `?offset=`). `detail_url` is
percent-encoded, so a slug containing a `/` still yields a URL the route
can match.

## `GET /api/v2/menus/{slug}/`

| Parameter | Notes |
| --- | --- |
| `locale` | Localized labels (see below). |
| `current` | Page id of the page being viewed — marks the active item and its trail. |

```console
$ curl 'https://example.com/api/v2/menus/main/?locale=fr&current=4'
```

```json
{
  "meta": { "slug": "main", "name": "Main navigation", "locale": "fr",
            "current": 4, "total_count": 3 },
  "items": [
    {
      "id": 2, "label": "Documentation", "url": "/docs", "is_page": true,
      "page_id": 3, "open_in_new_tab": false,
      "is_active": false, "in_active_trail": true,
      "children": [
        { "id": 3, "label": "Introduction", "url": "/docs/intro", "is_page": true,
          "page_id": 4, "open_in_new_tab": false,
          "is_active": true, "in_active_trail": false, "children": [] }
      ]
    }
  ]
}
```

`meta.total_count` spans the whole nesting, not just the top level.

An item whose target page is unpublished or gated is **dropped**, along with
any submenu it parents — no dead links, and no leaked titles.

### Active marking

- `is_active` — this item targets `?current=`.
- `in_active_trail` — this item targets an **ancestor** of the current page (a
  `path` prefix test, so it costs no query), *or* it parents a subtree
  containing the active item. The second case is what highlights a top-level
  section whose own entry is a grouping label or an external link.

An unresolvable `?current=` is treated as a hint, not an error: nothing is
marked and the menu still returns 200. A stale link shouldn't take a navbar
down.

### Localized labels

Two different things needed translating, and both now do:

1. **An authored label** — stored in `cms_menu_item_translation`, keyed
   `(item_id, locale_id, field_path)`, exactly like `cms_translation` for
   pages and `cms_snippet_translation` for snippets. Editable at
   **Navigation → *menu* → *locale*** in the admin. Translatable fields are
   `label` and `external_url` (a per-locale outbound link is a real editorial
   need — a country site, a translated PDF).
2. **An empty label**, which falls back to the target page's title. That
   fallback now reads the page's `cms_translation` title, so an unlabelled
   item in a French menu no longer shows an English page title.

Resolution order for an item pointing at a page:

```text
item label translation  →  authored label  →  page title translation  →  page title
```

An authored label with no translation stays canonical; it does **not** fall
through to the page title, because that would silently change what the item
says.

Leave a translation empty to delete it and fall back. A partly-translated menu
degrades per item rather than blanking.

Menu-item ids are **stable across a menu save** — the builder updates rows in
place rather than dropping and re-inserting them. That is what makes
translations survive a reorder.

---

## Previewing a draft

A draft is invisible to the API — `/api/v2/pages/{id}/` 404s it — unless
the request carries a signed `?preview_token=`. Tokens are page-scoped,
and expire in an hour. They are signed with `RCMS_SECRET_KEY`, or — when it
is unset — with a key the CMS generates once into
`./var/.rustango_cms_signing.key`. Several servers must share one of the two.

For a site the CMS renders, the admin's own preview pane handles this.
For a **decoupled frontend**, point the tenant at it under
**Site settings → Headless preview** (a walkthrough with the example shop:
[Preview drafts on your own frontend](shop-dev-headless-preview.md)):

```text
https://app.example.com/api/preview?token={token}&path={path}
```

`{token}`, `{path}` and `{id}` are substituted and percent-encoded —
`{path}` keeps its `/` separators, so it works in the path position as
well as in a query value. A plain URL with no placeholders gets them
appended as query parameters, so `https://app.example.com/preview` works
too.

### When one page does not follow the pattern

`{path}` is the CMS's own `url_path`, which assumes your frontend mirrors
the CMS's URL structure. Many do not. Each page has a **Frontend route**
field (Promote tab) that overrides it:

| Value | Effect |
| --- | --- |
| *empty* | use the page's CMS path — the common case |
| `/product/ai-engine` | replaces `{path}`; host and token still come from the site-wide template |
| `https://other.example.com/x?t={token}` | replaces the template outright, for a page a different app renders — works even with no site-wide frontend configured |

A scheme-relative value (`//host/x`) counts as a *path*, not a URL: it is
far more often a typo than a deliberate protocol-relative URL, and
treating it as absolute would silently drop the configured host.

The override is not copied when a page is cloned or aliased — a copy has
its own CMS path, and inheriting the original's frontend route would
point two pages at one place.

The page editor then gains two things: a **Preview on site** action that
opens your frontend in a new tab, and a third surface in the preview pane
(alongside *Rendered HTML* and *API response*) that embeds it right next
to the fields. The viewport buttons drive it, so a decoupled site can be
checked at 375/768/1280 like any other.

That pane cannot live-update as you type — it is another origin, so the
CMS cannot script into the frame. Save, then reload it. A frontend that
refuses to be framed (`X-Frame-Options`, CSP `frame-ancestors`) will show
blank; the bar carries an open-in-new-tab button for that case.

Either way your frontend reads the token and calls:

```
GET /api/v2/pages/{id}/?preview_token=<token>
```

No session, no cookie, no CORS credentials needed — the token is the
whole authorisation, which is why it is short-lived and scoped to one
page. A token minted for one page does not unlock another, and a
tampered signature is indistinguishable from no token at all.

View restrictions still apply on top: a token lets you read a *draft*,
not a page you would otherwise be forbidden.

## Resolving a URL

`GET /api/v2/pages/find/?html_path=/features` answers **302** with the
page's detail URL in `Location`, so a client that lets `fetch` follow
redirects turns a browser route into a hydrated page in one hop.

Every other parameter you send is forwarded onto that `Location`, so
`find/?html_path=/features&locale=fr` lands on
`/api/v2/pages/2/?locale=fr` and you get French in the same hop.
`html_path` itself is consumed and does not travel on.

A path with no public page — and a path whose page you may not see —
both answer `404 page not found`. The two are deliberately
indistinguishable, so a 404 is not proof a page does not exist.

## Errors

Every failure is JSON, with a stable code:

```json
{ "error": { "code": "not_found", "message": "page not found" } }
```

Branch on `code` and the HTTP status; `message` is for humans and may be
reworded. The codes are `not_found`, `bad_request`, `unauthenticated`,
`forbidden`, `password_required`, `method_not_allowed` and
`internal_error`.

*Every* failure means every one — including the **405** you get for a
write verb, since the API is read-only. Its `Allow` header names the
methods that path does serve, and the body is the same envelope as any
other error, so a client that parses failures as JSON does not need a
special case for it.

A gated page answers **401** (sign in and retry) or **403** (signed in,
still not allowed) rather than redirecting to the members login. The HTML
page routes still redirect — only `/api/v2/` paths return a status,
because a `fetch()` follows a redirect and would otherwise hand the
client a login page under status 200.

## Caching

Every response carries a weak `ETag` over the body and
`Cache-Control: private, no-cache`, plus `Vary: Cookie` — the body
depends on the caller's session, so a shared cache must not reuse it
across viewers.

Send the tag back to skip the transfer:

```console
$ curl -H 'If-None-Match: W/"3f1a…-812"' https://example.com/api/v2/menus/main/
HTTP/1.1 304 Not Modified
```

The handler still runs; this saves bandwidth, not database work. Error
responses carry no `ETag`.

## Pagination

List `meta` carries cursors so a client doesn't have to derive them:

```json
"meta": { "total_count": 213, "limit": 20, "offset": 20,
          "has_more": true, "next_offset": 40, "previous_offset": 0 }
```

Offsets rather than URLs, because the server often sits behind a proxy
and cannot reliably rebuild its own public address — a wrong link is
worse than none. `next_offset` and `previous_offset` are omitted at the
ends of the range.

## Fetching several pages at once

`?id_in=3,9,14` on `/api/v2/pages/`. A resolved menu gives you `page_id`
per node and nothing else, so without this hydrating a navbar is one
request per item. Unknown and non-public ids are simply absent from the
result rather than an error, so a stale id costs one missing row instead
of the whole batch. Bounded by `?limit=` like any list.

A segment that is not an integer *is* a `400`, though — `?id_in=3,abc`
names a client bug, not a stale row, and silently returning two of the
three requested pages would read as "that page was deleted".

## `GET /api/v2/openapi.json`

The API describes itself. Generate a typed client rather than hand-writing
one:

```sh
npx openapi-typescript https://example.com/api/v2/openapi.json -o api.d.ts
```

The **route list cannot drift**: it comes from the same constant the
router is built from, and a test fails if a path is served but
undocumented, or documented but gone. The response *schemas* are
hand-maintained — the handlers build their bodies from inline JSON
literals, so there is no struct to derive them from. Treat them as
accurate but not compiler-enforced.

## `GET /api/v2/search/`

One search box, every content type. Four separate `?search=` endpoints
meant a client fired four requests and merged them by hand — with no score
in any response to rank by.

| Parameter | Notes |
| --- | --- |
| `q` | The needle. Required; empty is a `400`, because answering it with "everything" is the worst possible guess. |
| `type` | `page,image,document,snippet` — any subset. An unknown name is a `400`, so a typo doesn't look like "no matches". |
| `limit` / `offset` | Paged like every other list. |

```json
{
  "meta": { "q": "docs", "types": ["page","image","document","snippet"],
            "total_count": 4, "limit": 20, "offset": 0, "has_more": false },
  "items": [
    { "type": "page", "id": 3, "title": "Docs", "url": "/docs",
      "detail_url": "/api/v2/pages/3/", "score": 1.0 },
    { "type": "image", "id": 7, "title": "Docs diagram", "url": null,
      "detail_url": "/api/v2/images/7/", "score": 0.8 }
  ]
}
```

`score` is `0.0..=1.0` and means the same thing for every type — exact
title, then prefix, then substring, then a secondary field (slug,
filename, body). It is deliberately simple, because the point is that
hits are *comparable across types*; a per-type relevance number nobody can
reconcile is what the client already had.

Pages route through the configured search backend, so Postgres FTS or
Elasticsearch applies where a tenant has one — and so do editorial search
promotions, which no API client could see before.

Viewer-aware, like everything else: gated pages and assets in closed
collections are dropped before ranking, so search can't be used to
enumerate what the caller may not read.

## Polling for changes

`?updated_since=2026-01-02T15:04:05Z` on any list endpoint returns only
rows touched at or after that instant. RFC 3339; a value that doesn't
parse is a `400` rather than a silent full result, since ignoring it would
tell a client nothing had changed since a timestamp the server never
applied.

Pair it with the `ETag` above: revalidate cheaply, and when something did
change, ask only for the difference.

### `GET /api/v2/changes/`

`?updated_since=` narrows one collection, but a client still had to ask
all five to learn whether any of them moved. This is the cheap tick:

```json
{
  "meta": { "checked_at": "2026-01-02T15:04:05+00:00", "usage": "..." },
  "collections": {
    "pages":     { "count": 213, "latest": "2026-01-02T14:00:00+00:00" },
    "images":    { "count": 2,   "latest": "2026-01-01T09:00:00+00:00" },
    "documents": { "count": 2,   "latest": "2026-01-01T09:00:00+00:00" },
    "snippets":  { "count": 2,   "latest": "2026-01-01T09:00:00+00:00" },
    "menus":     { "count": 12,  "latest": null }
  }
}
```

Hold the answer; re-fetch only the collections whose entry moved, passing
the `latest` you held as `?updated_since=`.

**Why `count` as well as `latest`:** a deletion moves no timestamp.
Without the count, a client that had cached a page would serve it forever
after an editor removed it. The pair catches additions, edits and
removals between them.

It is **not** a push feed — there is no SSE or WebSocket, and the client
still picks its cadence. And it is not free: the counts reflect what
*this* caller may see, so it runs the same visibility filters the list
endpoints do. Roughly one list request's cost, replacing five.

## Sparse fields

`?fields=title,seo_title` works on every list **and** every detail
endpoint — pages, images, documents and snippets. `id` and `meta` are
always kept. On page detail it is the way to avoid pulling `extension`,
`builder` and the whole `children` array when all you wanted was a title.

On a list endpoint, a name it does not serve is a `400` listing the ones
it does, rather than items quietly stripped to `{"id": …, "meta": …}`. A
detail endpoint ignores names it does not serve.

## Ordering

`?order=title`, `?order=-updated_at`, `?order=title,-created_at`, or
`?order=random`. Which fields a collection accepts differs — pages sort
by `title`, `slug`, `url_path`, `depth`, `sort_order`, `published_at`,
`updated_at` and `created_at`; media by `title`, `filename`, `size` and
`uploaded_at`.

An unrecognised field is a `400` naming it, for the same reason
`/search/?type=bogus` is: a typo that silently returns rows in some other
order is indistinguishable from one the server honoured.

`random` cannot be combined with `?offset=` — a fresh shuffle with rows
skipped is not a second page, so the combination is refused.

## `GET /api/v2/locales/`

```json
{ "meta": { "total_count": 2, "default": "en" },
  "items": [ { "code": "en", "name": "English", "is_default": true },
             { "code": "fr", "name": "Français", "is_default": false } ] }
```

Inactive locales are omitted. This exists because `?locale=` falls back
to the tenant default *silently* on an unrecognised code — without a way
to enumerate the real ones, a client cannot tell a typo from a locale.
`/api/v2/pages/{id}/` echoes the locale it actually used as
`meta.locale`, so the fallback is detectable after the fact too.

## Empty parameters

An **empty value means "not specified"** on every optional parameter, across
every endpoint: `?root=&depth=&locale=` is exactly equivalent to sending none
of them. That is what a client produces when it renders a URL from a template
and the values happen to be unset, so it is an ordinary request, not an error.

A non-empty value that doesn't parse is still a `400` — `?depth=abc` is a real
client bug, and quietly falling back to the default would hide it.

## A note on `?locale=`

`?locale=` means "give me this locale's content" everywhere in this document,
and falls back to the tenant default for an unknown or inactive code. `meta.locale`
echoes what was actually used, so a client can tell whether its code was honoured.

The one exception is **`?locale=` on the flat `GET /api/v2/pages/` list**,
which is an older *row filter* over the deprecated `locale_variant_of` column,
and it has entirely different semantics. It is not the same parameter.
