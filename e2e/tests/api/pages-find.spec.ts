/**
 * `GET /api/v2/pages/find/?html_path=` — how a SPA turns a browser route
 * into a page id.
 */
import { expect, request, test } from "@playwright/test";
import { ids } from "./helpers/fixture";

const f = ids();

/** `find` answers with a redirect, so the raw status has to be observable. */
const noFollow = (baseURL: string) =>
  request.newContext({ baseURL, maxRedirects: 0, storageState: { cookies: [], origins: [] } });

test.describe("page find", () => {
  test("resolves a URL path to the detail endpoint", async ({ baseURL }) => {
    const r = await noFollow(baseURL!);
    const res = await r.get("/api/v2/pages/find/?html_path=/t-home/t-docs");
    expect(res.status()).toBe(302);
    expect(res.headers()["location"]).toBe(`/api/v2/pages/${f.page.docs}/`);
  });

  test("tolerates a trailing slash on either side", async ({ baseURL }) => {
    const r = await noFollow(baseURL!);
    const withSlash = await r.get("/api/v2/pages/find/?html_path=/t-home/t-docs/");
    expect(withSlash.headers()["location"]).toBe(`/api/v2/pages/${f.page.docs}/`);
  });

  test("an unknown path is a 404", async ({ baseURL }) => {
    const r = await noFollow(baseURL!);
    const res = await r.get("/api/v2/pages/find/?html_path=/nope/nope");
    expect(res.status()).toBe(404);
    expect((await res.json()).error.code).toBe("not_found");
  });

  test("a missing or empty html_path is a legible 400", async ({ baseURL }) => {
    const r = await noFollow(baseURL!);
    for (const q of ["", "?html_path="]) {
      const res = await r.get(`/api/v2/pages/find/${q}`);
      expect(res.status()).toBe(400);
      const body = await res.json();
      expect(body.error.code).toBe("bad_request");
      expect(body.error.message).toContain("html_path");
    }
  });

  test("an unpublished page is not findable", async ({ baseURL }) => {
    const r = await noFollow(baseURL!);
    expect((await r.get("/api/v2/pages/find/?html_path=/t-home/t-draft")).status()).toBe(404);
  });

  test("the redirect carries the query, so one hop is enough", async ({ baseURL }) => {
    // A SPA resolves its route with `find/` and lets fetch follow the
    // 302 — that is why this endpoint answers with one. The Location
    // named only the id, so `?locale=` was dropped in flight and the
    // client silently got the default locale back while believing it had
    // asked for another.
    const r = await noFollow(baseURL!);
    const res = await r.get(`/api/v2/pages/find/?html_path=/t-home/t-docs&locale=fr`);
    expect(res.status()).toBe(302);
    expect(res.headers()["location"]).toBe(`/api/v2/pages/${f.page.docs}/?locale=fr`);

    // `html_path` is consumed here and must not travel on.
    expect(res.headers()["location"]).not.toContain("html_path");

    // And following it really does yield the locale that was asked for.
    const followed = await (await request.newContext({ baseURL })).get(
      `/api/v2/pages/find/?html_path=/t-home/t-docs&locale=fr`,
    );
    expect(followed.status()).toBe(200);
    expect((await followed.json()).meta.locale).toBe("fr");
  });

  test("an archived page resolves, like it does everywhere else", async ({
    baseURL,
    request: rq,
  }) => {
    // `list`, `tree` and `detail` all serve archived pages — the tree
    // even hands out a `detail_url` for one. `find` matched on
    // `published` alone, so it was the single endpoint that 404'd a page
    // the rest of the API was happy to return: a client resolving a URL
    // it had just been given got nothing.
    const detail = await rq.get(`/api/v2/pages/${f.page.archived}/`);
    expect(detail.status(), "precondition: detail serves it").toBe(200);

    const r = await noFollow(baseURL!);
    const res = await r.get("/api/v2/pages/find/?html_path=/t-home/t-archived");
    expect(res.status()).toBe(302);
    expect(res.headers()["location"]).toBe(`/api/v2/pages/${f.page.archived}/`);
  });

  test("a missing page and a gated one give the same message", async ({ request: rq }) => {
    // They must stay indistinguishable — a distinct message would put
    // back the existence oracle the 404-for-both was chosen to remove.
    const missing = await rq.get("/api/v2/pages/find/?html_path=/no/such/page");
    const gated = await rq.get("/api/v2/pages/find/?html_path=/t-home/t-members/t-secret");
    expect(missing.status()).toBe(404);
    expect(gated.status()).toBe(404);
    const a = await missing.json();
    const b = await gated.json();
    expect(a).toEqual(b);
    // And it must read like the rest of the API, not like a sentence
    // with " not found" bolted onto the end of it.
    expect(a.error.message).toBe("page not found");
  });

  test("[G31] there is no find/ equivalent for other content types", async ({ request: r }) => {
    // Pages can be resolved from a URL; images, documents and snippets
    // cannot, so a client-side route pointing at one of those has no way
    // to become an id.
    for (const kind of ["images", "documents", "snippets"]) {
      const res = await r.get(`/api/v2/${kind}/find/?html_path=/whatever`);
      expect(res.status(), `/api/v2/${kind}/find/`).not.toBe(302);
    }
  });
});
