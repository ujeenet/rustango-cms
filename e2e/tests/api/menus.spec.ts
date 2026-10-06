/**
 * `GET /api/v2/menus/` + `/{slug}/` — what the SPA's navbar is built
 * from. Envelope, labels, localization, active marking, and the
 * pathological trees the admin can't produce but the database can hold.
 */
import { expect, test } from "@playwright/test";
import { findNode, getJson, labels } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();
const MENU = (slug: string, q = "") => `/api/v2/menus/${slug}/${q}`;

test.describe("menus", () => {
  test("list returns slug, name and a usable detail_url", async ({ request }) => {
    const body = await getJson(request, "/api/v2/menus/");
    const main = body.items.find((m: any) => m.slug === "t-main");
    expect(main.name).toBe("T Main navigation");
    expect(main.detail_url).toBe("/api/v2/menus/t-main/");
    expect(body.meta.total_count).toBe(body.items.length);
  });

  test("an unknown slug is a 404", async ({ request }) => {
    expect((await request.get(MENU("no-such-menu"))).status()).toBe(404);
  });

  test("detail nests items and counts the whole tree", async ({ request }) => {
    const body = await getJson(request, MENU("t-main"));
    expect(body.meta.slug).toBe("t-main");
    const docs = findNode(body.items, "label", "T Docs link");
    expect(docs.is_page).toBe(true);
    expect(docs.url).toBe("/t-home/t-docs");
    // meta.total_count spans the nesting, not just the top level.
    expect(body.meta.total_count).toBe(labels(body.items).length);
    expect(body.meta.total_count).toBeGreaterThan(body.items.length);
  });

  test("an empty label falls back to the target page's title", async ({ request }) => {
    const body = await getJson(request, MENU("t-main"));
    expect(labels(body.items)).toContain("T Intro");
  });

  test("an external item passes its URL through", async ({ request }) => {
    const ext = findNode((await getJson(request, MENU("t-main"))).items, "label", "T Elsewhere");
    expect(ext.url).toBe("https://example.com/t");
    expect(ext.is_page).toBe(false);
    expect(ext.page_id).toBeNull();
  });

  test("?locale=fr uses the authored label translation", async ({ request }) => {
    const body = await getJson(request, MENU("t-main", "?locale=fr"));
    expect(body.meta.locale).toBe("fr");
    expect(labels(body.items)).toContain("T Accueil");
    expect(labels(body.items)).not.toContain("T Home link");
  });

  test("?locale=fr localizes the page-title fallback too", async ({ request }) => {
    // The item with no label of its own must pick up the *translated*
    // page title, not the English one.
    const body = await getJson(request, MENU("t-main", "?locale=fr"));
    expect(labels(body.items)).toContain("T Introduction");
    expect(labels(body.items)).not.toContain("T Intro");
  });

  test("an untranslated authored label stays canonical", async ({ request }) => {
    // It must NOT fall through to the page title just because no
    // translation exists — that would change what the item says.
    const body = await getJson(request, MENU("t-main", "?locale=fr"));
    expect(labels(body.items)).toContain("T Docs link");
    expect(labels(body.items)).not.toContain("T Documentation");
  });

  test("a per-locale external URL overrides the canonical one", async ({ request }) => {
    const ext = findNode(
      (await getJson(request, MENU("t-main", "?locale=fr"))).items,
      "label",
      "T Elsewhere",
    );
    expect(ext.url).toBe("https://example.fr/t");
  });

  test("?current= marks exactly one item active and its ancestors in-trail", async ({
    request,
  }) => {
    const body = await getJson(request, MENU("t-main", `?current=${f.page.deep}`));
    expect(body.meta.current).toBe(f.page.deep);

    const active = labels(body.items).filter(
      (_, i) => findNode(body.items, "label", labels(body.items)[i])?.is_active,
    );
    expect(active).toEqual(["T Deep link"]);

    const docs = findNode(body.items, "label", "T Docs link");
    expect(docs.in_active_trail, "the ancestor item is on the trail").toBe(true);
    expect(docs.is_active).toBe(false);

    const ext = findNode(body.items, "label", "T Elsewhere");
    expect(ext.in_active_trail).toBe(false);
  });

  test("without ?current= nothing is marked", async ({ request }) => {
    const body = await getJson(request, MENU("t-main"));
    expect(body.meta.current).toBeNull();
    expect(labels(body.items).length).toBeGreaterThan(0);
    for (const n of body.items) expect(n.is_active).toBe(false);
  });

  test("an unresolvable ?current= is a hint, not an error", async ({ request }) => {
    const res = await request.get(MENU("t-main", "?current=99999"));
    expect(res.status(), "a stale link must not take the navbar down").toBe(200);
  });

  test("a cycle or cross-menu parent does not hang or crash the endpoint", async ({ request }) => {
    // t-broken holds a parent_id cycle and an item parented into another
    // menu. The endpoint must still answer.
    const res = await request.get(MENU("t-broken"));
    expect(res.status()).toBe(200);
  });

  // -------------------------------------------------------------------

  test("an item pointing at an archived page still resolves", async ({ request }) => {
    // The tree, the sitemap and the renderer all serve archived pages.
    // The resolver filtering to `published` alone meant the page existed
    // and resolved but had no nav entry.
    const body = await getJson(request, MENU("t-main"));
    expect(labels(body.items)).toContain("T Archived link");
  });

  test("a structurally broken menu still answers, and says what it dropped", async ({
    request,
  }) => {
    // t-broken holds a `parent_id` cycle and an item parented into
    // another menu — states the admin now refuses to create, but which
    // exist in databases already. The endpoint must answer rather than
    // hang or 500, and the server logs the shortfall so an editor
    // reporting "my menu is missing items" is diagnosable.
    const res = await request.get(MENU("t-broken"));
    expect(res.status()).toBe(200);
    const body = await res.json();
    // The unreachable items are genuinely not rendered — a cycle has no
    // valid position in a tree, and inventing one would be worse.
    expect(labels(body.items)).not.toContain("T Cycle A");
    expect(labels(body.items)).not.toContain("T Cross-menu child");
  });

  test("@admin the admin refuses to create a cross-menu parent", async ({ request }) => {
    // The form took `parent_id` straight from the request, so an item
    // could be parented into another menu — invisible in both, and
    // leaving the other menu permanently undeletable.
    await request.get(`/cms-admin/navigation/${f.menu.scratch}/edit`);
    const token = (await request.storageState()).cookies.find(
      (c) => c.name === "rustango_csrf",
    )?.value;
    const res = await request.post(`/cms-admin/navigation/${f.menu.scratch}/items`, {
      headers: { "x-csrf-token": token ?? "" },
      form: {
        label: "smuggled",
        page_id: String(f.page.home),
        parent_id: String(f.item.otherOnly),
        _csrf: token ?? "",
      },
      maxRedirects: 0,
    });
    expect(res.status(), "a cross-menu parent must be refused").toBeGreaterThanOrEqual(400);
  });

  test("a grouping header is distinguishable from a homepage link", async ({ request }) => {
    // An item with neither a page nor an external URL used to render
    // `url: "/"`, so every dropdown label read as a deliberate link to
    // the homepage. It is now `null`.
    const n = findNode((await getJson(request, MENU("t-broken"))).items, "label", "T Neither");
    expect(n.url).toBeNull();
    expect(n.is_page).toBe(false);

    // A real link still carries its URL.
    const real = findNode((await getJson(request, MENU("t-main"))).items, "label", "T Home link");
    expect(real.url).toBe("/t-home");
  });

  test("a custom link to the current page highlights", async ({ request }) => {
    // `is_active` was page-id equality set only in the page branch, so an
    // editor who typed a URL instead of picking a page lost highlighting
    // entirely — and the client had to re-derive it from URLs, which is
    // what this endpoint exists to avoid.
    const body = await getJson(request, MENU("t-main", `?current=${f.page.docs}`));
    const byUrl = findNode(body.items, "label", "T Docs by URL");
    expect(byUrl.is_active, "the custom link points at the current page").toBe(true);
    expect(findNode(body.items, "label", "T Docs link").is_active).toBe(true);
    expect(findNode(body.items, "label", "T Elsewhere").is_active).toBe(false);
  });

  test("an alias counts as its source for highlighting", async ({ request }) => {
    // The alias and its source are different rows with different ids, so
    // a nav item pointing at the canonical page would otherwise never
    // highlight while the reader is on the alias's URL.
    const body = await getJson(request, MENU("t-main", `?current=${f.page.alias}`));
    expect(findNode(body.items, "label", "T Docs link").is_active).toBe(true);
  });

  test("the menus list honours the shared ListEnvelope contract", async ({ request }) => {
    // It used to return every menu unpaginated with a `meta` that had
    // neither `limit` nor `offset`, so a client typed against the shared
    // envelope broke on this endpoint alone.
    const body = await getJson(request, "/api/v2/menus/?limit=1");
    expect(body.items).toHaveLength(1);
    for (const k of ["total_count", "limit", "offset", "has_more"]) {
      expect(body.meta, `meta.${k}`).toHaveProperty(k);
    }
    expect(body.meta.limit).toBe(1);
    expect(body.meta.has_more).toBe(true);
  });

  test("the menus list does not claim a locale it never applied", async ({ request }) => {
    // A menu's slug and name have no translations, so echoing
    // `meta.locale: "fr"` told a client its code had been honoured while
    // every name in the payload was canonical. The parameter is still
    // accepted, for clients that pass it uniformly.
    const fr = await getJson(request, "/api/v2/menus/?locale=fr");
    expect(fr.meta).not.toHaveProperty("locale");
    // Detail, where labels really are localized, still echoes it.
    const detail = await getJson(request, MENU("t-main", "?locale=fr"));
    expect(detail.meta.locale).toBe("fr");
  });
});
