/**
 * Seeds the site the API specs run against, straight into the demo's
 * sqlite file.
 *
 * Why not drive the admin UI? Most of what these specs need to observe
 * cannot be *expressed* through the admin: an alias whose source has
 * since been retitled, a menu item whose parent lives in another menu, a
 * `parent_id` cycle, an `expired` page. Seeding at the storage layer is
 * the only way to reach those states, and it makes the fixture
 * deterministic — ids are fixed constants, so a failing assertion names
 * a page you can look up here.
 *
 * The admin write path is not skipped, only moved: `menu-writeback.spec`
 * drives the real builder and asserts what it does to this data.
 *
 * `serve.sh` wipes `.state/` on a cold start, but Playwright reuses a
 * running server locally, so `seed()` deletes its own id ranges first
 * and is safe to re-run.
 */
import { DatabaseSync } from "node:sqlite";

/** Everything below this is ours; nothing the demo seeds comes near it. */
const PAGE_BASE = 1000;
const BULK_BASE = 1100;
export const BULK_COUNT = 200;
const MENU_BASE = 100;
const ITEM_BASE = 200;
const FR_LOCALE = 2;
const COLLECTION_BASE = 100;
const MEDIA_BASE = 100;
const SNIPPET_BASE = 100;

/** `MaterializedPath` — 4-wide lowercase hex, trailing slash (`src/tree.rs:97`). */
const seg = (id: number) => `${id.toString(16).padStart(4, "0")}/`;

export type Fixture = ReturnType<typeof ids>;

/** Stable ids every spec imports. Kept in one place so a failure is greppable. */
export function ids() {
  const page = {
    home: PAGE_BASE,
    about: PAGE_BASE + 1,
    docs: PAGE_BASE + 2,
    intro: PAGE_BASE + 3,
    deep: PAGE_BASE + 4,
    guide: PAGE_BASE + 5,
    archived: PAGE_BASE + 6,
    draft: PAGE_BASE + 7,
    expired: PAGE_BASE + 8,
    members: PAGE_BASE + 9,
    secret: PAGE_BASE + 10,
    alias: PAGE_BASE + 11,
    errorPage: PAGE_BASE + 12,
    // Three pages sharing a title, so the default (published_at, title)
    // comparator ties and pagination has nothing left to order by (B20).
    dupA: PAGE_BASE + 13,
    dupB: PAGE_BASE + 14,
    dupC: PAGE_BASE + 15,
    // A `view_mode = "api"` page, for the negotiated-JSON spec (#751).
    feed: PAGE_BASE + 16,
    // A non-Latin title, for Unicode-aware `?search=` folding (#746).
    news: PAGE_BASE + 17,
  };
  // `scratch` is the only menu the write-back spec mutates. `other` is
  // deliberately left undeletable by the cross-menu item below (B33), so
  // write tests must not use it.
  const menu = {
    main: MENU_BASE,
    other: MENU_BASE + 1,
    broken: MENU_BASE + 2,
    scratch: MENU_BASE + 3,
  };
  const item = {
    home: ITEM_BASE,
    docs: ITEM_BASE + 1,
    introNoLabel: ITEM_BASE + 2,
    deep: ITEM_BASE + 3,
    members: ITEM_BASE + 4,
    external: ITEM_BASE + 5,
    archivedTarget: ITEM_BASE + 6,
    // A custom link written as a site-relative path — the shape an
    // editor produces when they type a URL instead of picking a page.
    relativeExternal: ITEM_BASE + 7,
    otherOnly: ITEM_BASE + 10,
    // A nested pair whose sort_order runs *backwards* relative to depth.
    // `clone` sorts non-roots by sort_order alone, which is not
    // topological, so the grandchild is processed before its parent (B31).
    otherChild: ITEM_BASE + 20,
    otherGrand: ITEM_BASE + 21,
    crossMenuChild: ITEM_BASE + 11,
    cycleA: ITEM_BASE + 12,
    cycleB: ITEM_BASE + 13,
    bothTargets: ITEM_BASE + 14,
    neitherTarget: ITEM_BASE + 15,
  };
  // One open collection and one login-gated one, each holding an image
  // and a document. `/__media__/` refuses the gated bytes; the JSON
  // listings must refuse the catalogue entry too.
  const collection = { open: COLLECTION_BASE, gated: COLLECTION_BASE + 1 };
  const media = {
    openImage: MEDIA_BASE,
    gatedImage: MEDIA_BASE + 1,
    openDoc: MEDIA_BASE + 2,
    gatedDoc: MEDIA_BASE + 3,
  };
  // Snippets, and media titled to share a distinctive word with a page —
  // so the unified search has something to return from four types at
  // once, which is the whole point of that endpoint.
  const snippet = { docsNote: SNIPPET_BASE, other: SNIPPET_BASE + 1, form: SNIPPET_BASE + 2 };
  return {
    page,
    menu,
    item,
    collection,
    media,
    snippet,
    frLocale: FR_LOCALE,
    bulkBase: BULK_BASE,
  };
}

type Row = {
  id: number;
  slug: string;
  title: string;
  parent: number | null;
  status?: string;
  type?: string;
  sortOrder?: number;
  aliasOf?: number | null;
  showInMenus?: boolean;
};

export function seed(dbPath: string): Fixture {
  const db = new DatabaseSync(dbPath);
  const f = ids();

  // Page types the demo registers at boot; looked up rather than assumed,
  // because their ids depend on registration order.
  const typeId = (name: string): number => {
    const row = db.prepare("SELECT id FROM cms_page_type WHERE type_name = ?").get(name) as
      | { id: number }
      | undefined;
    if (!row) throw new Error(`page type ${name} not registered — did the demo boot?`);
    return row.id;
  };
  const ARTICLE = typeId("ArticlePage");
  const HOME = typeId("HomePage");
  const MEMBERS = typeId("MembersPage");
  const ERROR = typeId("ErrorPage");
  const FEED = typeId("ProductFeed");

  wipe(db);

  // `fr` alongside the demo's default `en`. Fixed id so translation rows
  // can reference it without a lookup.
  db.prepare(
    `INSERT INTO cms_locale (id, code, name, is_default, active, sort_order)
     VALUES (?, 'fr', 'Français', 0, 1, 1)`,
  ).run(FR_LOCALE);

  const p = f.page;
  const rows: Row[] = [
    { id: p.home, slug: "t-home", title: "T Home", parent: null, type: "home", showInMenus: true },
    { id: p.about, slug: "t-about", title: "T About", parent: p.home, showInMenus: true },
    { id: p.docs, slug: "t-docs", title: "T Docs", parent: p.home, showInMenus: true },
    // sort_order deliberately disagrees with path order — see below.
    { id: p.intro, slug: "t-intro", title: "T Intro", parent: p.docs, sortOrder: 10, showInMenus: true },
    { id: p.deep, slug: "t-deep", title: "T Deep", parent: p.intro },
    { id: p.guide, slug: "t-guide", title: "T Guide", parent: p.docs, sortOrder: 0 },
    { id: p.archived, slug: "t-archived", title: "T Archived", parent: p.home, status: "archived" },
    { id: p.draft, slug: "t-draft", title: "T Draft", parent: p.home, status: "draft" },
    { id: p.expired, slug: "t-expired", title: "T Expired", parent: p.home, status: "expired" },
    { id: p.members, slug: "t-members", title: "T Members", parent: p.home, type: "members" },
    { id: p.secret, slug: "t-secret", title: "T Secret", parent: p.members },
    // An alias carries a stale copy of the source title, taken at
    // creation. The renderer shadows it with the source's; `detail` does
    // not (B10), which is exactly what the contract spec measures.
    {
      id: p.alias,
      slug: "t-alias",
      title: "T Docs (STALE ALIAS TITLE)",
      parent: p.home,
      aliasOf: p.docs,
    },
    {
      id: p.errorPage,
      slug: "t-error",
      title: "T Error",
      parent: p.home,
      type: "error",
      showInMenus: true,
    },
    { id: p.dupA, slug: "t-dup-a", title: "T Duplicate", parent: p.home },
    { id: p.dupB, slug: "t-dup-b", title: "T Duplicate", parent: p.home },
    { id: p.dupC, slug: "t-dup-c", title: "T Duplicate", parent: p.home },
    { id: p.feed, slug: "t-feed", title: "T Product feed", parent: p.home, type: "feed" },
    { id: p.news, slug: "t-news", title: "Т Новини", parent: p.home },
  ];

  const typeFor = (r: Row) =>
    r.type === "home"
      ? HOME
      : r.type === "members"
        ? MEMBERS
        : r.type === "error"
          ? ERROR
          : r.type === "feed"
            ? FEED
            : ARTICLE;

  const pathOf = new Map<number, string>();
  const urlOf = new Map<number, string>();
  const insertPage = db.prepare(
    `INSERT INTO cms_page
       (id, page_type_id, title, slug, path, url_path, depth, parent_id,
        locale_variant_of, alias_of, theme_id, sort_order, status,
        seo_title, seo_description, robots_index, sitemap_priority,
        show_in_menus, og_title, og_description, twitter_card,
        notification_pre_published_sent, published_at)
     VALUES (?,?,?,?,?,?,?,?, NULL,?, NULL,?,?, '','',1,0.5,?, '','','summary',0,?)`,
  );

  for (const r of rows) {
    const path = r.parent === null ? seg(r.id) : `${pathOf.get(r.parent)}${seg(r.id)}`;
    const url =
      r.parent === null ? `/${r.slug}` : `${urlOf.get(r.parent)}/${r.slug}`;
    pathOf.set(r.id, path);
    urlOf.set(r.id, url);
    const status = r.status ?? "published";
    insertPage.run(
      r.id,
      typeFor(r),
      r.title,
      r.slug,
      path,
      url,
      path.split("/").filter(Boolean).length,
      r.parent,
      r.aliasOf ?? null,
      r.sortOrder ?? 0,
      status,
      r.showInMenus ? 1 : 0,
      status === "published" || status === "archived" ? "2026-01-01T00:00:00Z" : null,
    );
  }

  // Bulk filler for pagination and response-size assertions.
  for (let i = 0; i < BULK_COUNT; i += 1) {
    const id = BULK_BASE + i;
    const slug = `t-bulk-${String(i).padStart(3, "0")}`;
    insertPage.run(
      id,
      ARTICLE,
      `T Bulk ${String(i).padStart(3, "0")}`,
      slug,
      `${pathOf.get(p.home)}${seg(id)}`,
      `${urlOf.get(p.home)}/${slug}`,
      2,
      p.home,
      null,
      i,
      "published",
      0,
      "2026-01-01T00:00:00Z",
    );
  }

  // Login-gate the members subtree. `denied_page_ids` is subtree-aware via
  // `path`, so t-secret inherits this without a row of its own.
  db.prepare(
    `INSERT INTO cms_page_view_restriction
       (page_id, kind, password_hash, group_ids, codenames)
     VALUES (?, 'login', '', '[]', '[]')`,
  ).run(p.members);

  // Partial French coverage: two pages translated, the rest must fall
  // back to canonical rather than blanking.
  const tr = db.prepare(
    "INSERT INTO cms_translation (page_id, locale_id, field_path, value) VALUES (?,?,?,?)",
  );
  tr.run(p.docs, FR_LOCALE, "title", "T Documentation");
  tr.run(p.intro, FR_LOCALE, "title", "T Introduction");

  seedMedia(db, f);
  seedSnippets(db, f);
  seedMenus(db, f, pathOf);
  db.close();
  return f;
}

function seedMedia(db: DatabaseSync, f: Fixture) {
  const { collection, media } = f;
  const col = db.prepare(
    "INSERT INTO cms_media_collection (id, name, parent_id, sort_order) VALUES (?,?,NULL,?)",
  );
  col.run(collection.open, "T Open collection", 0);
  col.run(collection.gated, "T Gated collection", 1);

  // Login-gated, like the members page. Anonymous callers must not see
  // the assets inside, in any listing.
  db.prepare(
    `INSERT INTO cms_collection_view_restriction
       (collection_id, kind, password_hash, group_ids, codenames)
     VALUES (?, 'login', '', '[]', '[]')`,
  ).run(collection.gated);

  const m = db.prepare(
    `INSERT INTO cms_media
       (id, collection_id, kind, title, filename, mime, size, alt_text,
        description, content_hash, storage_key, width, height)
     VALUES (?,?,?,?,?,?,?, '', '', ?, ?, ?, ?)`,
  );
  m.run(media.openImage, collection.open, "image", "T Docs diagram",
        "t-docs-diagram.png", "image/png", 100, "hash-open-img", "t/open.png", 10, 10);
  m.run(media.gatedImage, collection.gated, "image", "T Gated image",
        "gated.png", "image/png", 100, "hash-gated-img", "t/gated.png", 10, 10);
  m.run(media.openDoc, collection.open, "document", "T Docs handbook",
        "t-docs-handbook.pdf", "application/pdf", 100, "hash-open-doc", "t/open.pdf", null, null);
  m.run(media.gatedDoc, collection.gated, "document", "T Gated doc",
        "gated.pdf", "application/pdf", 100, "hash-gated-doc", "t/gated.pdf", null, null);
}

function seedSnippets(db: DatabaseSync, f: Fixture) {
  const sn = db.prepare(
    `INSERT INTO cms_snippet (id, slug, title, type_name, body_markdown, data, folder_path)
     VALUES (?,?,?,?,?, '{}', '')`,
  );
  sn.run(f.snippet.docsNote, "t-docs-note", "T Docs note", "note", "About the T Docs section.");
  sn.run(f.snippet.other, "t-other-note", "T Other note", "note", "Unrelated body text.");
  // A form definition, for the unauthenticated-form-schema spec (#751).
  db.prepare(
    `INSERT INTO cms_snippet (id, slug, title, type_name, body_markdown, data, folder_path)
     VALUES (?, 't-form', 'T Contact form', 'form', '', ?, '')`,
  ).run(
    f.snippet.form,
    JSON.stringify({ fields: [{ name: "email", kind: "email", label: "Email", required: true }] }),
  );
}

function seedMenus(db: DatabaseSync, f: Fixture, _pathOf: Map<number, string>) {
  const { menu, item, page } = f;
  const m = db.prepare("INSERT INTO cms_menu (id, slug, name) VALUES (?,?,?)");
  m.run(menu.main, "t-main", "T Main navigation");
  m.run(menu.other, "t-other", "T Other menu");
  m.run(menu.broken, "t-broken", "T Broken menu");
  m.run(menu.scratch, "t-scratch", "T Scratch menu");

  const it = db.prepare(
    `INSERT INTO cms_menu_item
       (id, menu_id, parent_id, sort_order, label, page_id, external_url, open_in_new_tab)
     VALUES (?,?,?,?,?,?,?,0)`,
  );

  // t-main — the clean menu every ordinary assertion uses.
  it.run(item.home, menu.main, null, 10, "T Home link", page.home, null);
  it.run(item.docs, menu.main, null, 20, "T Docs link", page.docs, null);
  //  Empty label on purpose: it must fall back to the target page's
  //  title, and to the *translated* title under `?locale=fr`.
  it.run(item.introNoLabel, menu.main, item.docs, 30, "", page.intro, null);
  it.run(item.deep, menu.main, item.introNoLabel, 40, "T Deep link", page.deep, null);
  it.run(item.members, menu.main, null, 50, "T Members link", page.members, null);
  it.run(item.external, menu.main, null, 60, "T Elsewhere", null, "https://example.com/t");
  //  Points at an archived page: the tree serves it, the resolver drops
  //  it (B36).
  it.run(item.archivedTarget, menu.main, null, 70, "T Archived link", page.archived, null);
  it.run(item.relativeExternal, menu.main, null, 80, "T Docs by URL", null, "/t-home/t-docs");

  // t-other — a second menu, so the multi-menu batching path is covered.
  it.run(item.otherOnly, menu.other, null, 10, "T Other home", page.home, null);
  it.run(item.otherChild, menu.other, item.otherOnly, 50, "T Other child", page.about, null);
  it.run(item.otherGrand, menu.other, item.otherChild, 10, "T Other grandchild", page.docs, null);

  // t-broken — states the admin cannot produce, kept out of t-main so the
  // ordinary assertions stay readable.
  //  parent in a different menu (B33/B37)
  it.run(item.crossMenuChild, menu.broken, item.otherOnly, 10, "T Cross-menu child", page.about, null);
  //  a parent_id cycle (B37). Inserted flat, then closed with an UPDATE —
  //  neither row can reference the other before both exist.
  it.run(item.cycleA, menu.broken, null, 20, "T Cycle A", page.about, null);
  it.run(item.cycleB, menu.broken, null, 30, "T Cycle B", page.about, null);
  const setParent = db.prepare("UPDATE cms_menu_item SET parent_id = ? WHERE id = ?");
  setParent.run(item.cycleB, item.cycleA);
  setParent.run(item.cycleA, item.cycleB);
  //  both targets, and neither (B35)
  it.run(item.bothTargets, menu.broken, null, 40, "T Both", page.about, "https://example.com/both");
  it.run(item.neitherTarget, menu.broken, null, 50, "T Neither", null, null);

  // A translated label + a per-locale external URL on the clean menu.
  const mt = db.prepare(
    "INSERT INTO cms_menu_item_translation (item_id, locale_id, field_path, value) VALUES (?,?,?,?)",
  );
  mt.run(item.home, FR_LOCALE, "label", "T Accueil");
  mt.run(item.external, FR_LOCALE, "external_url", "https://example.fr/t");
}

/**
 * Remove a previous run's fixture, FK-safely.
 *
 * Self-FKs (`cms_page.parent_id`/`alias_of`, `cms_menu_item.parent_id`)
 * carry no `ON DELETE`, and sqlite pools enable enforcement, so the
 * references are nulled before anything is deleted rather than trying to
 * find a valid delete order.
 */
function wipe(db: DatabaseSync) {
  const pageFloor = PAGE_BASE;
  const menuFloor = MENU_BASE;
  db.exec("PRAGMA foreign_keys = ON");
  db.exec(`
    UPDATE cms_menu_item SET parent_id = NULL WHERE menu_id >= ${menuFloor};
    DELETE FROM cms_menu_item_translation
      WHERE item_id IN (SELECT id FROM cms_menu_item WHERE menu_id >= ${menuFloor});
    DELETE FROM cms_menu_item WHERE menu_id >= ${menuFloor};
    DELETE FROM cms_menu WHERE id >= ${menuFloor};
    UPDATE cms_page SET parent_id = NULL, alias_of = NULL WHERE id >= ${pageFloor};
    DELETE FROM cms_translation WHERE page_id >= ${pageFloor};
    DELETE FROM cms_page_view_restriction WHERE page_id >= ${pageFloor};
    DELETE FROM cms_page WHERE id >= ${pageFloor};
    DELETE FROM cms_locale WHERE id = ${FR_LOCALE};
    DELETE FROM cms_media WHERE id >= ${MEDIA_BASE};
    DELETE FROM cms_snippet WHERE id >= ${SNIPPET_BASE};
    DELETE FROM cms_collection_view_restriction WHERE collection_id >= ${COLLECTION_BASE};
    DELETE FROM cms_media_collection WHERE id >= ${COLLECTION_BASE};
  `);
}
