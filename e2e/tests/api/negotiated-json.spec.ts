/**
 * `Accept: application/json` on a public page URL.
 *
 * An `api` view-mode page type serves the *same* object as
 * `/api/v2/pages/{id}/`. That equality is the reason the editor's JSON
 * preview can be trusted, so it is worth asserting rather than assuming.
 */
import { expect, test } from "@playwright/test";
import { ids } from "./helpers/fixture";

const f = ids();

test.describe("Accept negotiation", () => {
  test("an ordinary page ignores Accept: application/json", async ({ request }) => {
    // The safety property for `auto` view mode: adding an Accept header
    // must never turn an HTML page into a JSON one, or every page on the
    // site would gain a representation it never opted into.
    const res = await request.get("/t-home/t-docs", {
      headers: { Accept: "application/json" },
    });
    expect(res.headers()["content-type"] ?? "").toContain("text/html");
  });

  test("a q-value that prefers HTML still gets HTML", async ({ request }) => {
    const res = await request.get("/t-home/t-docs", {
      headers: { Accept: "application/json;q=0.1, text/html" },
    });
    expect(res.headers()["content-type"] ?? "").toContain("text/html");
  });

  test("the negotiated body equals the v2 detail body for an api-view page", async ({
    request,
  }) => {
    // The demo registers ProductFeed with view_mode = "api"; the fixture
    // seeds one (`t-feed`).
    const list = await request.get("/api/v2/pages/?type=ProductFeed&limit=1");
    const body = await list.json();
    expect(body.items.length, "the fixture's ProductFeed page is listed").toBeGreaterThan(0);

    const id = body.items[0].id;
    const url = body.items[0].meta.html_url;
    const negotiated = await request.get(url);
    expect(negotiated.headers()["content-type"] ?? "").toContain("application/json");
    const viaApi = await request.get(`/api/v2/pages/${id}/`);
    expect(await negotiated.json()).toEqual(await viaApi.json());
  });

  test("an unhonoured locale is detectable from the response", async ({ request }) => {
    // `?locale=` falls back silently on an unknown code, so the echo is
    // the only way a client can tell a typo from a real locale.
    const good = await request.get(`/api/v2/pages/${f.page.docs}/?locale=fr`);
    const typo = await request.get(`/api/v2/pages/${f.page.docs}/?locale=fr-typo`);
    expect(good.status()).toBe(200);
    expect(typo.status()).toBe(200);
    expect((await good.json()).meta.locale).toBe("fr");
    expect((await typo.json()).meta.locale).toBe("en");
  });
});
