/**
 * `GET /api/v2/pages/` — the flat list. Pagination, ordering, filters,
 * search, sparse fields.
 *
 * This is the endpoint a SPA hits hardest — index pages, search, "more
 * like this". It is now viewer-aware like every other page surface;
 * `security.spec.ts` owns the proof.
 */
import { expect, test } from "@playwright/test";
import { getJson, listIds } from "./helpers/client";
import { BULK_COUNT, ids } from "./helpers/fixture";

const f = ids();
const LIST = "/api/v2/pages/";

test.describe("page list", () => {
  test("returns the meta + items envelope", async ({ request }) => {
    const body = await getJson(request, `${LIST}?limit=5`);
    expect(Object.keys(body).sort()).toEqual(["items", "meta"]);
    expect(body.meta.limit).toBe(5);
    expect(body.meta.offset).toBe(0);
    expect(body.meta.total_count).toBeGreaterThan(BULK_COUNT);
    expect(body.items).toHaveLength(5);
    expect(body.meta).not.toHaveProperty("did_you_mean");
  });

  test("limit clamps to 100 and floors at 1", async ({ request }) => {
    expect((await getJson(request, `${LIST}?limit=9999`)).meta.limit).toBe(100);
    expect((await getJson(request, `${LIST}?limit=0`)).meta.limit).toBe(1);
  });

  test("an offset past the end is an empty page, not an error", async ({ request }) => {
    const body = await getJson(request, `${LIST}?offset=999999`);
    expect(body.items).toEqual([]);
    expect(body.meta.total_count, "total_count still describes the whole set").toBeGreaterThan(0);
  });

  test("empty query values are accepted", async ({ request }) => {
    const res = await request.get(
      `${LIST}?limit=&offset=&fields=&order=&search=&child_of=&descendant_of=&type=&tag=&locale=`,
    );
    expect(res.status()).toBe(200);
  });

  test("negative and unparseable numbers are rejected", async ({ request }) => {
    expect((await request.get(`${LIST}?limit=-1`)).status()).toBe(400);
    expect((await request.get(`${LIST}?limit=abc`)).status()).toBe(400);
  });

  test("?child_of= returns direct children only", async ({ request }) => {
    const body = await getJson(request, `${LIST}?child_of=${f.page.docs}&limit=100`);
    const t = body.items.map((i: any) => i.title).sort();
    expect(t).toEqual(["T Guide", "T Intro"]);
  });

  test("?descendant_of= spans the subtree and excludes the anchor", async ({ request }) => {
    const body = await getJson(request, `${LIST}?descendant_of=${f.page.docs}&limit=100`);
    const t = body.items.map((i: any) => i.title);
    expect(t.sort()).toEqual(["T Deep", "T Guide", "T Intro"]);
  });

  test("?ancestor_of= returns the chain above a page, excluding it", async ({ request }) => {
    const body = await getJson(request, `${LIST}?ancestor_of=${f.page.deep}&limit=100`);
    const t = body.items.map((i: any) => i.title).sort();
    expect(t).toEqual(["T Docs", "T Home", "T Intro"]);
  });

  test("?type= filters by handler name", async ({ request }) => {
    // HomePage, not MembersPage: the members page is gated, so an
    // anonymous caller correctly sees nothing of that type.
    const body = await getJson(request, `${LIST}?type=HomePage&limit=100`);
    expect(body.items.map((i: any) => i.title)).toEqual(["T Home"]);
  });

  test("?type= on a gated type returns nothing to an anonymous caller", async ({ request }) => {
    const body = await getJson(request, `${LIST}?type=MembersPage&limit=100`);
    expect(body.items).toEqual([]);
  });

  test("?fields= narrows the payload but always keeps id and meta", async ({ request }) => {
    const body = await getJson(request, `${LIST}?fields=title&limit=1`);
    expect(Object.keys(body.items[0]).sort()).toEqual(["id", "meta", "title"]);
  });

  test("?order= sorts, and `-` reverses", async ({ request }) => {
    const asc = (await getJson(request, `${LIST}?order=title&limit=100`)).items.map(
      (i: any) => i.title,
    );
    const desc = (await getJson(request, `${LIST}?order=-title&limit=100`)).items.map(
      (i: any) => i.title,
    );
    expect(asc).toEqual([...asc].sort());
    expect(desc[0] >= desc[desc.length - 1]).toBe(true);
  });

  test("?search= matches titles", async ({ request }) => {
    const body = await getJson(request, `${LIST}?search=Documentation-nope-xyz&limit=10`);
    expect(body.items).toEqual([]);
    const hit = await getJson(request, `${LIST}?search=t-docs&limit=10`);
    expect(hit.items.length).toBeGreaterThan(0);
  });

  test("?search= folds case beyond ASCII (#746)", async ({ request }) => {
    // Stored as "Т Новини"; the query is all lower case.
    const hit = await getJson(request, `${LIST}?search=${encodeURIComponent("новини")}&limit=10`);
    expect(hit.items.map((i: any) => i.meta.slug)).toContain("t-news");
  });

  test("the list serves the same pages every other surface does", async ({ request }) => {
    const all = (await getJson(request, `${LIST}?limit=100&order=title`)).items.map(
      (i: any) => i.title,
    );
    expect(all).not.toContain("T Draft");
    expect(all).not.toContain("T Expired");
    // `archived` is public per `PageStatus::is_public`, so the renderer,
    // the sitemap, the tree and the menus all serve it. The list was the
    // last surface pretending otherwise.
    expect(all).toContain("T Archived");
  });

  // -------------------------------------------------------------------

  test("paging is stable even when the sort keys tie", async ({ request }) => {
    // Three fixture pages share a title and a published_at. With no
    // unique tiebreaker their order came from the driver, so on Postgres
    // an unrelated write could make a client see one page twice and miss
    // another while walking `?offset=`. Both the query and the
    // comparators now end in `id`.
    const seen = new Set<number>();
    const dupes: number[] = [];
    let total = 0;
    for (let offset = 0; offset < 260; offset += 20) {
      const body = await getJson(request, `${LIST}?limit=20&offset=${offset}`);
      total = body.meta.total_count;
      for (const id of listIds(body)) {
        if (seen.has(id)) dupes.push(id);
        seen.add(id);
      }
    }
    expect(dupes, "a page appeared at two different offsets").toEqual([]);
    expect(seen.size, "every page was reachable by walking offsets").toBe(total);
  });

  test("an unknown ?order= field is refused, not swallowed", async ({ request }) => {
    // `parse_order` accepted any name and `compare_pages` skipped
    // unknown ones per pair, so `?order=nosuchfield` compared every pair
    // Equal: the sort became a no-op and took the default ordering with
    // it. That was first patched by falling back to the default, which
    // fixed the ordering but still said nothing.
    //
    // A 400 is what the same typo already gets one endpoint over
    // (`/search/?type=bogus`), and it is the only answer that tells the
    // client its ordering was never applied.
    const res = await request.get(`${LIST}?order=nosuchfield&limit=20`);
    expect(res.status()).toBe(400);
    const body = await res.json();
    expect(body.error.code).toBe("bad_request");
    expect(body.error.message, "must name the offending field").toContain("nosuchfield");
    expect(body.error.message, "and list the ones that work").toContain("title");

    // A field that does exist is unaffected.
    const ok = await request.get(`${LIST}?order=title&limit=20`);
    expect(ok.status()).toBe(200);
  });

  test("an unknown ?fields= name is refused, not silently empty", async ({ request }) => {
    // `?fields=titel` returned `{id, meta}` for every row — each object
    // stripped to nothing, with no hint that the one field the client
    // asked for was misspelled.
    const res = await request.get(`${LIST}?fields=titel&limit=5`);
    expect(res.status()).toBe(400);
    const body = await res.json();
    expect(body.error.message, "must name the offending field").toContain("titel");
    expect(body.error.message, "and list the ones that work").toContain("title");

    // The real name still filters, and id/meta survive as documented.
    const ok = await getJson(request, `${LIST}?fields=title&limit=1`);
    expect(Object.keys(ok.items[0]).sort()).toEqual(["id", "meta", "title"]);
  });

  test("?translation_of= returns the other translations, not the anchor", async ({
    request,
  }) => {
    const body = await getJson(request, `${LIST}?translation_of=${f.page.docs}&limit=100`);
    expect(body.items.map((i: any) => i.id)).not.toContain(f.page.docs);
  });

  test("an unknown locale falls back instead of emptying the list", async ({ request }) => {
    // Every other endpoint falls back to the tenant default; this one
    // cleared the result set and ignored `active` entirely.
    const body = await getJson(request, `${LIST}?locale=zz&limit=10`);
    expect(body.items.length).toBeGreaterThan(0);
  });

  test("?type= accepts both the bare and the dotted form", async ({ request }) => {
    // `type_name` stores the bare handler name with `app_label` in its
    // own column, so the dotted form the docs advertise could never
    // match. Both work now.
    const bare = await getJson(request, `${LIST}?type=HomePage&limit=10`);
    const dotted = await getJson(request, `${LIST}?type=cms_pages.HomePage&limit=10`);
    expect(bare.items.map((i: any) => i.id)).toEqual([f.page.home]);
    expect(dotted.items.map((i: any) => i.id)).toEqual([f.page.home]);
  });

  test("?order=random with an offset is refused, not silently wrong", async ({ request }) => {
    // A random order reshuffles per request, so an offset into it is a
    // fresh shuffle with rows skipped — overlapping and incomplete.
    const res = await request.get(`${LIST}?order=random&limit=20&offset=20`);
    expect(res.status()).toBe(400);
    expect((await res.json()).error.code).toBe("bad_request");
    // Without an offset it is fine.
    expect((await request.get(`${LIST}?order=random&limit=20`)).status()).toBe(200);
  });
});
