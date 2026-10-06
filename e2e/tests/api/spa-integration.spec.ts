/**
 * What a Vue 2 app actually experiences.
 *
 * Half of this file asserts behaviour that was recently added — JSON
 * errors, conditional requests, batch fetch, locale discovery — and half
 * still asserts the *absence* of something a client wants. The second
 * group are the acceptance criteria for the next pass; when one is
 * implemented, its test goes red and gets rewritten like these were.
 */
import { expect, request, test } from "@playwright/test";
import { getJson } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();
const ORIGIN = "https://spa.example.com";
const CHANGES_PATH = "/api/v2/changes/";
const ENDPOINTS = [
  "/api/v2/pages/",
  "/api/v2/pages/tree/",
  `/api/v2/pages/${f.page.docs}/`,
  "/api/v2/menus/",
  "/api/v2/menus/t-main/",
  "/api/v2/locales/",
];

test.describe("SPA integration", () => {
  // ---- cross-origin --------------------------------------------------

  test("[G1] CORS is off unless the host opts in", async ({ request: r }) => {
    // Deliberate default: enabling cross-origin reads opens every
    // endpoint — including the ones that still leak gated content — to
    // any page a browser will load. Hosts turn it on with
    // `RCMS_API_CORS_ORIGINS`, which the demo reads via `Cors::from_env`.
    const res = await r.get("/api/v2/pages/tree/", { headers: { Origin: ORIGIN } });
    expect(res.status()).toBe(200);
    expect(res.headers()["access-control-allow-origin"]).toBeUndefined();
  });

  test("[G2] OPTIONS is a 405 while CORS is off", async ({ request: r }) => {
    // With an allowlist configured the CORS layer answers preflight
    // itself; with none, there is nothing to preflight.
    const res = await r.fetch("/api/v2/pages/tree/", {
      method: "OPTIONS",
      headers: { Origin: ORIGIN, "Access-Control-Request-Method": "GET" },
    });
    expect(res.status()).toBe(405);
  });

  // ---- error handling ------------------------------------------------

  test("errors are JSON with a machine-readable code", async ({ request: r }) => {
    // Previously `text/plain`, so a client's `res.json()` threw a
    // SyntaxError on every failure and reported a parse problem rather
    // than the 404 that happened.
    const res = await r.get("/api/v2/pages/99999/");
    expect(res.status()).toBe(404);
    expect(res.headers()["content-type"] ?? "").toContain("application/json");
    expect(await res.json()).toEqual({
      error: { code: "not_found", message: "page not found" },
    });
  });

  test("the error code is stable across endpoints; only the message differs", async ({
    request: r,
  }) => {
    // A client branches on `code`, never on English prose.
    const page = await (await r.get("/api/v2/pages/99999/")).json();
    const menu = await (await r.get("/api/v2/menus/nope/")).json();
    expect(page.error.code).toBe("not_found");
    expect(menu.error.code).toBe("not_found");
    expect(page.error.message).not.toBe(menu.error.message);
  });

  test("a malformed query is a bad_request, not a serde string", async ({ request: r }) => {
    const res = await r.get("/api/v2/pages/find/");
    expect(res.status()).toBe(400);
    const body = await res.json();
    expect(body.error.code).toBe("bad_request");
    expect(body.error.message).toContain("html_path");
  });

  // ---- caching ---------------------------------------------------------

  test("every response carries an ETag and a cache directive", async ({ request: r }) => {
    for (const path of ENDPOINTS) {
      const h = (await r.get(path)).headers();
      expect(h["etag"], `${path} ETag`).toMatch(/^W\//);
      expect(h["cache-control"], `${path} Cache-Control`).toBe("private, no-cache");
    }
  });

  test("If-None-Match gets a 304 with no body", async ({ request: r }) => {
    const first = await r.get("/api/v2/menus/t-main/");
    const etag = first.headers()["etag"];
    expect(etag).toBeTruthy();

    const again = await r.get("/api/v2/menus/t-main/", {
      headers: { "If-None-Match": etag },
    });
    expect(again.status()).toBe(304);
    expect((await again.body()).length).toBe(0);
  });

  test("the ETag changes when the content does", async ({ request: r }) => {
    // A validator that never moves is worse than none — the client would
    // cache the first response forever.
    const en = (await r.get("/api/v2/pages/tree/?locale=en")).headers()["etag"];
    const fr = (await r.get("/api/v2/pages/tree/?locale=fr")).headers()["etag"];
    expect(en).not.toBe(fr);
  });

  test("a stale If-None-Match still gets the body", async ({ request: r }) => {
    const res = await r.get("/api/v2/menus/t-main/", {
      headers: { "If-None-Match": 'W/"0000000000000000-0"' },
    });
    expect(res.status()).toBe(200);
    expect((await res.json()).items.length).toBeGreaterThan(0);
  });

  test("errors are not given a validator", async ({ request: r }) => {
    // Handing a 404 an ETag invites a client to revalidate its way into
    // believing the page still doesn't exist.
    const res = await r.get("/api/v2/pages/99999/");
    expect(res.headers()["etag"]).toBeUndefined();
  });

  test("a viewer-dependent body says Vary: Cookie", async ({ request: r }) => {
    // Without it a shared cache keyed on the URL alone can hand a
    // member's tree to the next anonymous caller.
    for (const path of ENDPOINTS) {
      const vary = (await r.get(path)).headers()["vary"] ?? "";
      expect(vary.toLowerCase(), `${path} Vary`).toContain("cookie");
    }
  });

  // ---- fetching efficiency ---------------------------------------------

  test("?id_in= hydrates a menu in one request", async ({ request: r }) => {
    // The N+1 a headless client used to be forced into: the menu returns
    // `page_id` per node and nothing else.
    const menu = await getJson(r, "/api/v2/menus/t-main/");
    const pageIds: number[] = [];
    const walk = (items: any[]) => {
      for (const i of items) {
        if (i.page_id) pageIds.push(i.page_id);
        walk(i.children ?? []);
      }
    };
    walk(menu.items);
    expect(pageIds.length).toBeGreaterThan(1);

    const batch = await getJson(r, `/api/v2/pages/?id_in=${pageIds.join(",")}&limit=100`);
    expect(batch.items.map((i: any) => i.id).sort()).toEqual([...new Set(pageIds)].sort());
  });

  test("?id_in= ignores unknown ids rather than failing the batch", async ({ request: r }) => {
    // A stale client id should cost one missing row, not the request.
    const body = await getJson(r, `/api/v2/pages/?id_in=${f.page.docs},99999&limit=10`);
    expect(body.items.map((i: any) => i.id)).toEqual([f.page.docs]);
  });

  test("?id_in= refuses a segment that is not an id", async ({ request: r }) => {
    // An unknown id is deliberately absent rather than an error, so one
    // stale id costs a batch a row instead of the whole thing. But a
    // segment that is not a number was never an id and cannot be stale;
    // dropping it made the client's own typo look like a deleted page,
    // on the endpoint whose entire job is hydrating ids a menu handed
    // over.
    const res = await r.get(`/api/v2/pages/?id_in=${f.page.docs},abc`);
    expect(res.status()).toBe(400);
    const body = await res.json();
    expect(body.error.code).toBe("bad_request");
    expect(body.error.message, "must name the bad segment").toContain("abc");

    // An unknown-but-numeric id stays absent, not an error.
    const ok = await getJson(r, `/api/v2/pages/?id_in=${f.page.docs},99999999`);
    expect(ok.items.map((i: any) => i.id)).toEqual([f.page.docs]);
  });

  test("list responses carry paging cursors", async ({ request: r }) => {
    const first = await getJson(r, "/api/v2/pages/?limit=5&offset=0");
    expect(first.meta.has_more).toBe(true);
    expect(first.meta.next_offset).toBe(5);
    expect(first.meta.previous_offset).toBeUndefined();

    const second = await getJson(r, `/api/v2/pages/?limit=5&offset=${first.meta.next_offset}`);
    expect(second.meta.previous_offset).toBe(0);
    // And the pages really are disjoint.
    const a = first.items.map((i: any) => i.id);
    const b = second.items.map((i: any) => i.id);
    expect(a.filter((id: number) => b.includes(id))).toEqual([]);
  });

  test("?fields= works on detail, not just on lists", async ({ request: r }) => {
    const body = await getJson(r, `/api/v2/pages/${f.page.docs}/?fields=title`);
    expect(Object.keys(body).sort()).toEqual(["id", "meta", "title"]);
    // Notably it drops `children`, which is unbounded on a busy page.
    expect(body).not.toHaveProperty("children");
  });

  // ---- discovery -------------------------------------------------------

  test("locales are discoverable", async ({ request: r }) => {
    const body = await getJson(r, "/api/v2/locales/");
    const codes = body.items.map((l: any) => l.code);
    expect(codes).toContain("en");
    expect(codes).toContain("fr");
    expect(body.meta.default).toBe("en");
    expect(body.items.find((l: any) => l.code === "en").is_default).toBe(true);
  });

  test("detail echoes the locale it actually used", async ({ request: r }) => {
    // So a client can tell `?locale=fr` from `?locale=fr-typo`, which
    // silently falls back.
    const good = await getJson(r, `/api/v2/pages/${f.page.docs}/?locale=fr`);
    const typo = await getJson(r, `/api/v2/pages/${f.page.docs}/?locale=fr-typo`);
    expect(good.meta.locale).toBe("fr");
    expect(typo.meta.locale).toBe("en");
  });

  test("both trailing-slash forms work", async ({ request: r }) => {
    for (const path of ["/api/v2/pages", "/api/v2/pages/tree", "/api/v2/menus"]) {
      const res = await r.get(path);
      expect(res.status(), path).toBe(200);
      expect(res.headers()["content-type"] ?? "", path).toContain("application/json");
    }
  });

  // ---- authentication ---------------------------------------------------

  test("a gated page answers 401, not an HTML login page", async ({ baseURL }) => {
    // The one that bit hardest: `fetch()` follows a 303, so the client
    // used to receive status 200 with an HTML body and no signal it was
    // unauthenticated.
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const res = await anon.get(`/api/v2/pages/${f.page.members}/`);
    expect(res.status()).toBe(401);
    expect(res.headers()["content-type"] ?? "").toContain("application/json");
    expect((await res.json()).error.code).toBe("unauthenticated");
  });

  test("@member a member gets the gated page", async ({ request: r }) => {
    const res = await r.get(`/api/v2/pages/${f.page.members}/`);
    expect(res.status()).toBe(200);
    expect((await res.json()).title).toBe("T Members");
  });

  test("a first-ever form view can be submitted", async ({ baseURL }) => {
    // The CSRF layer used to mint a second cookie after the handler had
    // already rendered its own token, and a browser keeps the last — so a
    // brand-new visitor's first submit of any public form was a 403.
    // Reloading masked it, which is why only new visitors ever hit it.
    const fresh = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const html = await (await fresh.get("/members/signup")).text();
    const rendered = /name="_csrf" value="([^"]+)"/.exec(html)?.[1];
    const cookie = (await fresh.storageState()).cookies.find((c) => c.name === "rustango_csrf");
    expect(rendered).toBeTruthy();
    expect(cookie?.value, "the form's token is the one that was stored").toBe(rendered);
  });

  // ---- still open --------------------------------------------------------

  test("[G14] no write verbs — and the refusal says so", async ({ request: r }) => {
    // Read-only by design. The refusal is now `405 Method Not Allowed`,
    // which describes the situation; it used to be a `403` from the
    // host's CSRF layer, sending a client off to look for a permissions
    // problem that did not exist.
    //
    // The status alone is not the assertion. This test used to check only
    // that, and so missed that the 405 came back with an empty body on an
    // API whose every other failure is JSON — a client calling
    // `res.json()` got a parser error instead of the refusal.
    for (const method of ["POST", "PUT", "PATCH", "DELETE"] as const) {
      const res = await r.fetch("/api/v2/pages/", { method, data: {} });
      expect(res.status(), `${method} /api/v2/pages/`).toBe(405);
      expect(
        res.headers()["content-type"],
        `${method} refusal must be parseable`,
      ).toContain("application/json");
      expect(await res.json(), `${method} refusal envelope`).toMatchObject({
        error: { code: "method_not_allowed" },
      });
      // RFC 9110: a 405 must say what would have worked.
      expect(res.headers()["allow"], `${method} Allow header`).toContain("GET");
    }
  });

  test("a malformed query value is refused in JSON, not plain text", async ({
    request: r,
  }) => {
    // The failures that never reach a handler. axum's extractors refuse
    // these in `text/plain`, so a client whose wrapper parses failures as
    // JSON raised a SyntaxError from its own parser instead of reporting
    // the 400 — on the same surface `?root=` was reported on, and for the
    // most ordinary mistake there is: a route param of the wrong type.
    for (const path of [
      "/api/v2/pages/?limit=abc",
      "/api/v2/pages/?offset=abc",
      "/api/v2/pages/tree/?root=notanumber",
      "/api/v2/pages/tree/?depth=deep",
      "/api/v2/pages/abc/",
      "/api/v2/images/abc/",
      "/api/v2/snippets/abc/",
    ]) {
      const res = await r.get(path);
      expect(res.status(), path).toBe(400);
      expect(res.headers()["content-type"], path).toContain("application/json");
      expect(await res.json(), path).toMatchObject({
        error: { code: "bad_request" },
      });
    }
  });

  test("the refusal still names the parameter that was wrong", async ({ request: r }) => {
    // Wrapping the rejection must not throw serde's text away: where it
    // names the field, that is the client's only clue about *which*
    // parameter it got wrong.
    const res = await r.get("/api/v2/pages/tree/?root=notanumber");
    const body = await res.json();
    expect(body.error.message, JSON.stringify(body)).toContain("root");
  });

  test("[G15] change detection is polling, not push", async ({ request: r }) => {
    // `/api/v2/changes/` makes the tick cheap and `?updated_since=` makes
    // the follow-up narrow, but a client still decides its own cadence —
    // there is no SSE or WebSocket to subscribe to. Asserted so that
    // adding one turns this red rather than leaving a stale claim in the
    // docs.
    expect((await r.get(CHANGES_PATH)).status()).toBe(200);
    for (const push of ["/api/v2/events/", "/api/v2/stream/"]) {
      expect((await r.get(push)).status(), push).not.toBe(200);
    }
  });

  test("site-wide search is one request", async ({ request: r }) => {
    // Four `?search=` endpoints meant the client merged and ranked them
    // itself, with no score in any response to rank by.
    const res = await r.get("/api/v2/search/?q=T Docs");
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(new Set(body.items.map((i: any) => i.type)).size).toBeGreaterThan(1);
    expect(body.items[0]).toHaveProperty("score");
  });

  test("?updated_since= asks only for what changed", async ({ request: r }) => {
    // Polling used to mean re-fetching a collection and diffing it.
    const all = await getJson(r, "/api/v2/pages/?limit=1");
    expect(all.meta.total_count).toBeGreaterThan(0);
    const future = await getJson(r, "/api/v2/pages/?updated_since=2099-01-01T00:00:00Z&limit=5");
    expect(future.items).toEqual([]);
    const past = await getJson(r, "/api/v2/pages/?updated_since=2000-01-01T00:00:00Z&limit=5");
    expect(past.meta.total_count).toBe(all.meta.total_count);
  });

  test("a malformed ?updated_since= is refused, not ignored", async ({ request: r }) => {
    // Ignoring it would tell a polling client nothing had changed since a
    // timestamp the server never applied.
    const res = await r.get("/api/v2/pages/?updated_since=yesterday");
    expect(res.status()).toBe(400);
    expect((await res.json()).error.code).toBe("bad_request");
  });
});
