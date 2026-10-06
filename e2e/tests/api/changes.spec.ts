/**
 * `GET /api/v2/changes/` — the cheap polling tick.
 *
 * `?updated_since=` fetches only what moved in one collection, but a
 * client still had to ask all five to find out whether any of them did.
 */
import { expect, request, test } from "@playwright/test";
import { getJson } from "./helpers/client";

const CHANGES = "/api/v2/changes/";
const COLLECTIONS = ["pages", "images", "documents", "snippets", "menus"];

test.describe("changes", () => {
  test("reports a count and a timestamp per collection", async ({ request: r }) => {
    const body = await getJson(r, CHANGES);
    for (const name of COLLECTIONS) {
      const entry = body.collections[name];
      expect(entry, name).toBeTruthy();
      expect(typeof entry.count, `${name}.count`).toBe("number");
      expect(entry, name).toHaveProperty("latest");
    }
    expect(body.meta.checked_at).toBeTruthy();
  });

  test("counts agree with what the list endpoints return", async ({ request: r }) => {
    // The whole contract: if these disagree, a client polling `changes`
    // draws the wrong conclusion about whether to re-fetch.
    const body = await getJson(r, CHANGES);
    for (const [name, path] of [
      ["pages", "/api/v2/pages/"],
      ["images", "/api/v2/images/"],
      ["documents", "/api/v2/documents/"],
      ["snippets", "/api/v2/snippets/"],
    ] as const) {
      const list = await getJson(r, `${path}?limit=1`);
      expect(body.collections[name].count, name).toBe(list.meta.total_count);
    }
  });

  test("its `latest` is a usable ?updated_since= value", async ({ request: r }) => {
    // The documented loop: hold `latest`, pass it back when something
    // moves. Anything at-or-after it must come back.
    const body = await getJson(r, CHANGES);
    const latest = body.collections.pages.latest;
    expect(latest).toBeTruthy();
    const since = await getJson(r, `/api/v2/pages/?updated_since=${encodeURIComponent(latest)}`);
    expect(since.meta.total_count).toBeGreaterThan(0);
  });

  test("is viewer-aware, so it cannot count gated content", async ({ baseURL }) => {
    // Otherwise the tick itself reports that member-only pages exist.
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const m = await request.newContext({ baseURL, storageState: ".state/member.json" });
    const anonBody = await getJson(anon, CHANGES);
    const memberBody = await getJson(m, CHANGES);
    expect(memberBody.collections.pages.count).toBeGreaterThan(
      anonBody.collections.pages.count,
    );
    expect(memberBody.collections.images.count).toBeGreaterThan(
      anonBody.collections.images.count,
    );
  });

  test("is cacheable like every other response", async ({ request: r }) => {
    const res = await r.get(CHANGES);
    expect(res.headers()["etag"]).toMatch(/^W\//);
    expect(res.headers()["cache-control"]).toBe("private, no-cache");
  });
});
