/**
 * `GET /api/v2/search/` — one search box across every content type.
 *
 * Previously a site-wide search meant four parallel requests the client
 * merged and ranked itself, with no score in any response to rank by.
 */
import { expect, request, test } from "@playwright/test";
import { getJson } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();
const SEARCH = (qs: string) => `/api/v2/search/?${qs}`;

test.describe("unified search", () => {
  test("returns typed hits from more than one content type", async ({ request: r }) => {
    // "T Docs" names a page, an image, a document and a snippet — the
    // four requests a client used to make and merge by hand.
    const body = await getJson(r, SEARCH("q=T Docs"));
    const kinds = new Set(body.items.map((i: any) => i.type));
    expect([...kinds].sort(), `only found ${[...kinds]}`).toEqual([
      "document",
      "image",
      "page",
      "snippet",
    ]);
    for (const item of body.items) {
      expect(item).toHaveProperty("detail_url");
      expect(item).toHaveProperty("score");
      expect(["page", "image", "document", "snippet"]).toContain(item.type);
    }
  });

  test("every hit's detail_url resolves", async ({ request: r }) => {
    const body = await getJson(r, SEARCH("q=T Docs"));
    expect(body.items.length).toBeGreaterThan(0);
    for (const item of body.items.slice(0, 8)) {
      const res = await r.get(item.detail_url);
      expect(res.status(), `${item.type} ${item.title} → ${item.detail_url}`).toBe(200);
    }
  });

  test("results are ranked, best first", async ({ request: r }) => {
    const body = await getJson(r, SEARCH("q=T Docs"));
    const scores = body.items.map((i: any) => i.score);
    expect(scores).toEqual([...scores].sort((a: number, b: number) => b - a));
    // An exact title match should lead.
    expect(body.items[0].title).toBe("T Docs");
  });

  test("?type= narrows to the named types", async ({ request: r }) => {
    const body = await getJson(r, SEARCH("q=T&type=snippet,image"));
    expect(body.meta.types).toEqual(["snippet", "image"]);
    for (const item of body.items) {
      expect(["snippet", "image"]).toContain(item.type);
    }
  });

  test("an unknown ?type= is a 400, not an empty result", async ({ request: r }) => {
    // A typo must not look identical to "nothing matched".
    const res = await r.get(SEARCH("q=T&type=pages"));
    expect(res.status()).toBe(400);
    const body = await res.json();
    expect(body.error.code).toBe("bad_request");
    expect(body.error.message).toContain("pages");
  });

  test("a missing q is a 400", async ({ request: r }) => {
    for (const qs of ["", "q="]) {
      const res = await r.get(`/api/v2/search/?${qs}`);
      expect(res.status(), qs).toBe(400);
      expect((await res.json()).error.code).toBe("bad_request");
    }
  });

  test("results are paged like every other list", async ({ request: r }) => {
    const first = await getJson(r, SEARCH("q=T&limit=3&offset=0"));
    expect(first.items).toHaveLength(3);
    expect(first.meta.has_more).toBe(true);
    const second = await getJson(r, SEARCH(`q=T&limit=3&offset=${first.meta.next_offset}`));
    const a = first.items.map((i: any) => `${i.type}:${i.id}`);
    const b = second.items.map((i: any) => `${i.type}:${i.id}`);
    expect(a.filter((k: string) => b.includes(k)), "pages must be disjoint").toEqual([]);
  });

  test("no matches is an empty list, not an error", async ({ request: r }) => {
    const body = await getJson(r, SEARCH("q=zzzznotathing"));
    expect(body.items).toEqual([]);
    expect(body.meta.total_count).toBe(0);
  });

  test("search does not surface gated pages", async ({ baseURL }) => {
    // Otherwise search becomes a way to enumerate a member-only section
    // by guessing words.
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const m = await request.newContext({ baseURL, storageState: ".state/member.json" });
    const anonTitles = (await getJson(anon, SEARCH("q=T Members"))).items.map(
      (i: any) => i.title,
    );
    const memberTitles = (await getJson(m, SEARCH("q=T Members"))).items.map(
      (i: any) => i.title,
    );
    expect(anonTitles).not.toContain("T Members");
    expect(memberTitles).toContain("T Members");
  });

  test("search does not surface assets in a gated collection", async ({ baseURL }) => {
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const titles = (await getJson(anon, SEARCH("q=T Gated"))).items.map((i: any) => i.title);
    expect(titles).not.toContain("T Gated image");
    expect(titles).not.toContain("T Gated doc");
    // The open ones in the same query space are still found.
    const open = (await getJson(anon, SEARCH("q=T Docs"))).items.map((i: any) => i.title);
    expect(open).toContain("T Docs diagram");
  });
});
