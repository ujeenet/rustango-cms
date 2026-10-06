/**
 * `GET /api/v2/openapi.json` — the API describing itself.
 *
 * There was no schema of any kind, so a client had to be hand-written
 * from prose and re-checked by hand whenever anything moved.
 */
import { expect, test } from "@playwright/test";
import { getJson } from "./helpers/client";

const SCHEMA = "/api/v2/openapi.json";

test.describe("openapi", () => {
  test("serves a valid OpenAPI 3.1 shell", async ({ request }) => {
    const doc = await getJson(request, SCHEMA);
    expect(doc.openapi).toBe("3.1.0");
    expect(doc.info.title).toBeTruthy();
    expect(Object.keys(doc.paths).length).toBeGreaterThan(10);
    expect(Object.keys(doc.components.schemas).length).toBeGreaterThan(10);
  });

  test("every documented path actually answers", async ({ request }) => {
    // The route list comes from the same constant the router is built
    // from, but that only proves they agree in *source*. This proves the
    // server really serves each one.
    const doc = await getJson(request, SCHEMA);
    for (const [path, ops] of Object.entries<any>(doc.paths)) {
      // Walk each path with the verb it actually declares — the login
      // route is a POST, and GETting it would prove nothing but 405.
      const url = path.replace("{id}", "1").replace("{slug}", "nope");
      const res = ops.get
        ? await request.get(
            url +
              (path.includes("find") ? "?html_path=/x" : "") +
              (path.includes("search") ? "?q=x" : ""),
          )
        : await request.post(url, { data: {} });
      expect(
        res.status(),
        `${path} is documented but the router does not serve it`,
      ).not.toBe(405);
      // A 4xx body is fine (no such row, bad credentials); an HTML body
      // means the request fell through to the CMS page router.
      expect(res.headers()["content-type"] ?? "", path).toContain("application/json");
    }
  });

  test("declares the error shape clients branch on", async ({ request }) => {
    const doc = await getJson(request, SCHEMA);
    const codes = doc.components.schemas.Error.properties.error.properties.code.enum;
    expect(codes).toContain("not_found");
    expect(codes).toContain("unauthenticated");

    // And the real thing matches what is declared.
    const res = await request.get("/api/v2/pages/999999/");
    const body = await res.json();
    expect(codes).toContain(body.error.code);
  });

  test("the schema itself is cacheable like every other response", async ({ request }) => {
    const res = await request.get(SCHEMA);
    expect(res.headers()["etag"]).toMatch(/^W\//);
    const again = await request.get(SCHEMA, {
      headers: { "If-None-Match": res.headers()["etag"] },
    });
    expect(again.status()).toBe(304);
  });
});
