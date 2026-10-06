/**
 * `GET /api/v2/pages/tree/` — the endpoint a SPA builds its navigation
 * from. Shape, bounds, ordering, localization.
 */
import { expect, test } from "@playwright/test";
import { findNode, getJson, titles } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();
const TREE = "/api/v2/pages/tree/";

test.describe("page tree", () => {
  test("nests children under their parent", async ({ request }) => {
    const body = await getJson(request, `${TREE}?root=${f.page.docs}&depth=10`);
    const kids = body.items.map((n: any) => n.title);
    expect(kids.sort()).toEqual(["T Guide", "T Intro"]);

    const intro = findNode(body.items, "title", "T Intro");
    expect(intro.children.map((c: any) => c.title)).toEqual(["T Deep"]);
    expect(intro.has_children).toBe(true);
    expect(intro.url).toBe("/t-home/t-docs/t-intro");
    expect(intro.detail_url).toBe(`/api/v2/pages/${f.page.intro}/`);
  });

  test("depth bounds the walk but has_children still tells the truth", async ({ request }) => {
    const d1 = await getJson(request, `${TREE}?root=${f.page.docs}&depth=1`);
    expect(d1.meta.depth).toBe(1);
    const intro = findNode(d1.items, "title", "T Intro");
    expect(intro.children).toEqual([]);
    expect(intro.has_children, "depth cut the subtree, not the fact of it").toBe(true);

    const d2 = await getJson(request, `${TREE}?root=${f.page.docs}&depth=2`);
    expect(findNode(d2.items, "title", "T Deep")).toBeTruthy();
  });

  test("depth defaults to 3 and clamps to 10", async ({ request }) => {
    expect((await getJson(request, TREE)).meta.depth).toBe(3);
    expect((await getJson(request, `${TREE}?depth=9999`)).meta.depth).toBe(10);
    expect((await getJson(request, `${TREE}?depth=0`)).meta.depth).toBe(1);
  });

  test("empty query values mean 'not specified', not 400", async ({ request }) => {
    // A client rendering a URL template with unset values sends exactly
    // this. It used to be a 400 on every parameter.
    const res = await request.get(`${TREE}?root=&depth=&locale=`);
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body.meta.depth).toBe(3);
    expect(body.meta.root).toBeNull();
  });

  test("a non-empty unparseable value is still a 400", async ({ request }) => {
    // Coercing `?depth=abc` to the default would hide a real client bug.
    expect((await request.get(`${TREE}?depth=abc`)).status()).toBe(400);
  });

  test("?root= excludes the root itself and returns its children", async ({ request }) => {
    const body = await getJson(request, `${TREE}?root=${f.page.home}&depth=1`);
    expect(body.meta.root).toBe(f.page.home);
    expect(titles(body.items)).not.toContain("T Home");
    expect(titles(body.items)).toContain("T About");
  });

  test("drafts and expired pages are absent; archived pages are present", async ({ request }) => {
    const all = titles((await getJson(request, `${TREE}?depth=10`)).items);
    expect(all).not.toContain("T Draft");
    expect(all).not.toContain("T Expired");
    // The tree deliberately matches the renderer, which serves archived.
    expect(all).toContain("T Archived");
  });

  test("error pages are excluded", async ({ request }) => {
    expect(titles((await getJson(request, `${TREE}?depth=10`)).items)).not.toContain("T Error");
  });

  test("?locale= translates titles and falls back per page", async ({ request }) => {
    const body = await getJson(request, `${TREE}?depth=10&locale=fr`);
    expect(body.meta.locale).toBe("fr");
    const all = titles(body.items);
    expect(all).toContain("T Documentation");
    expect(all).toContain("T Introduction");
    expect(all, "untranslated pages keep canonical rather than blanking").toContain("T About");
    expect(all).not.toContain("T Docs");
  });

  test("an unknown locale falls back to the default and says so", async ({ request }) => {
    const body = await getJson(request, `${TREE}?depth=10&locale=zz`);
    expect(body.meta.locale, "meta.locale echoes what was used, not what was asked").toBe("en");
    expect(titles(body.items)).toContain("T Docs");
  });

  test("meta.truncated is false for a site under the node cap", async ({ request }) => {
    const body = await getJson(request, `${TREE}?depth=10`);
    expect(body.meta.truncated).toBe(false);
    expect(body.meta.total_count).toBeGreaterThan(200);
  });

  // -------------------------------------------------------------------

  test("siblings come back in the order the site renders them", async ({ request }) => {
    // The fixture gives T Guide sort_order 0 and T Intro sort_order 10,
    // but Guide has the higher id and so the later path. Every template's
    // `children` loop renders Guide first; ordering the API by `path`
    // meant a SPA's nav silently disagreed with the server-rendered one.
    const body = await getJson(request, `${TREE}?root=${f.page.docs}&depth=1`);
    expect(body.items.map((n: any) => n.title)).toEqual(["T Guide", "T Intro"]);
  });

  test("meta says how many nodes the cap dropped", async ({ request }) => {
    // Once truncated, `total_count` equals the items length, so without
    // `dropped_count` a client cannot tell whether it lost one node or
    // ten thousand.
    const body = await getJson(request, `${TREE}?depth=10`);
    expect(body.meta).toHaveProperty("dropped_count");
    expect(body.meta.dropped_count).toBe(0);
    expect(body.meta.truncated).toBe(false);
  });
});
