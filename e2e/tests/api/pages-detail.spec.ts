/**
 * `GET /api/v2/pages/{id}/` — the object a SPA renders a page from,
 * plus the `children` summaries it walks the site with.
 */
import { expect, test } from "@playwright/test";
import { getJson } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();
const detail = (id: number, q = "") => `/api/v2/pages/${id}/${q}`;

test.describe("page detail", () => {
  test("returns the summary shape plus children", async ({ request }) => {
    const body = await getJson(request, detail(f.page.docs));
    expect(body.id).toBe(f.page.docs);
    expect(body.title).toBe("T Docs");
    expect(body.meta.type).toBe("ArticlePage");
    expect(body.meta.html_url).toBe("/t-home/t-docs");
    expect(body.meta.detail_url).toBe(`/api/v2/pages/${f.page.docs}/`);
    expect(Array.isArray(body.children)).toBe(true);
  });

  test("children are direct only, with has_children and a usable detail_url", async ({
    request,
  }) => {
    const body = await getJson(request, detail(f.page.docs));
    const kids = body.children.map((c: any) => c.title);
    expect(kids.slice().sort()).toEqual(["T Guide", "T Intro"]);
    expect(body.children.map((c: any) => c.title)).not.toContain("T Deep");

    const intro = body.children.find((c: any) => c.title === "T Intro");
    expect(intro.has_children).toBe(true);
    expect(intro.url).toBe("/t-home/t-docs/t-intro");
    expect(intro.detail_url).toBe(`/api/v2/pages/${f.page.intro}/`);

    const guide = body.children.find((c: any) => c.title === "T Guide");
    expect(guide.has_children).toBe(false);
  });

  test("children come back in the order the site renders them", async ({ request }) => {
    // Same contract as the tree: sort_order, not path.
    const body = await getJson(request, detail(f.page.docs));
    expect(body.children.map((c: any) => c.title)).toEqual(["T Guide", "T Intro"]);
  });

  test("a leaf reports children as an empty array, not a missing key", async ({ request }) => {
    // Present-but-empty: a client shouldn't have to distinguish "no
    // children" from "this endpoint forgot to say".
    const body = await getJson(request, detail(f.page.deep));
    expect(body.children).toEqual([]);
  });

  test("unpublished children are not advertised", async ({ request }) => {
    const kids = (await getJson(request, detail(f.page.home))).children.map((c: any) => c.title);
    expect(kids).not.toContain("T Draft");
    expect(kids).not.toContain("T Expired");
  });

  test("error pages are excluded from children, matching the tree", async ({ request }) => {
    // Regression for the fix: a 404 handler parented under Home used to
    // show up as one of its children while `tree` hid it.
    const kids = (await getJson(request, detail(f.page.home))).children.map((c: any) => c.title);
    expect(kids).not.toContain("T Error");
  });

  test("an archived page is served, so the tree's detail_url is not a dead link", async ({
    request,
  }) => {
    // Regression for the fix: `detail` used to 404 `archived` while
    // `tree` and `children` both listed it and handed out this exact URL.
    const res = await request.get(detail(f.page.archived));
    expect(res.status(), "archived is public per PageStatus::is_public").toBe(200);
    expect((await res.json()).title).toBe("T Archived");
  });

  test("a draft 404s without a preview token", async ({ request }) => {
    expect((await request.get(detail(f.page.draft))).status()).toBe(404);
  });

  test("an unknown id 404s and a non-numeric id 400s", async ({ request }) => {
    expect((await request.get(detail(99999))).status()).toBe(404);
    expect((await request.get("/api/v2/pages/not-a-number/")).status()).toBe(400);
  });

  test("?locale= overlays translations and falls back per field", async ({ request }) => {
    const fr = await getJson(request, detail(f.page.docs, "?locale=fr"));
    expect(fr.title).toBe("T Documentation");
    const untranslated = await getJson(request, detail(f.page.about, "?locale=fr"));
    expect(untranslated.title, "no override → canonical, not blank").toBe("T About");
  });

  // -------------------------------------------------------------------

  test("an alias serves the source's live content under its own URL", async ({ request }) => {
    // The fixture's alias stores "T Docs (STALE ALIAS TITLE)" — the copy
    // taken when it was created. Every other surface shadows it with the
    // source's live title; detail used to return the stale one, with no
    // extension and no builder.
    const body = await getJson(request, detail(f.page.alias));
    expect(body.title).toBe("T Docs");
    expect(body.meta.alias_of).toBe(f.page.docs);
    // Identity stays the alias's — it is a genuinely distinct URL.
    expect(body.id).toBe(f.page.alias);
    expect(body.meta.slug).toBe("t-alias");
    expect(body.meta.html_url).toBe("/t-home/t-alias");
  });

  test("detail echoes the locale it used, so a typo is detectable", async ({ request }) => {
    const fell_back = await getJson(request, detail(f.page.docs, "?locale=zz"));
    expect(fell_back.meta.locale).toBe("en");
    const honoured = await getJson(request, detail(f.page.docs, "?locale=fr"));
    expect(honoured.meta.locale).toBe("fr");
  });

  test("?fields= narrows a detail response and always keeps id + meta", async ({ request }) => {
    const body = await getJson(request, detail(f.page.docs, "?fields=title"));
    expect(Object.keys(body).sort()).toEqual(["id", "meta", "title"]);
  });

  test("children is capped, so a busy page has a bounded response", async ({ request }) => {
    // T Home parents 200+ bulk pages. Every detail request used to pay
    // for all of them, including each `Accept: application/json` render
    // of the page itself.
    const body = await getJson(request, detail(f.page.home));
    expect(body.children.length).toBeLessThanOrEqual(100);
    expect(body.children.length).toBeGreaterThan(0);
  });
});
