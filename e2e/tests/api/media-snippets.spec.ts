/**
 * `/api/v2/images/`, `/documents/`, `/snippets/` — the other content a
 * SPA renders, and the access-control asymmetry between them and the
 * media routes that serve the actual bytes.
 */
import { expect, request, test } from "@playwright/test";
import { getJson } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();

test.describe("media and snippets", () => {
  test("all three list endpoints share the ListEnvelope shape", async ({ request }) => {
    for (const kind of ["images", "documents", "snippets"]) {
      const body = await getJson(request, `/api/v2/${kind}/?limit=5`);
      expect(Object.keys(body).sort(), kind).toEqual(["items", "meta"]);
      expect(body.meta, kind).toHaveProperty("total_count");
      expect(body.meta, kind).toHaveProperty("limit");
      expect(body.meta, kind).toHaveProperty("offset");
    }
  });

  test("empty query values are accepted on every list", async ({ request }) => {
    for (const kind of ["images", "documents", "snippets"]) {
      const res = await request.get(`/api/v2/${kind}/?limit=&offset=&collection=&search=&order=`);
      expect(res.status(), kind).toBe(200);
    }
  });

  test("an unknown id 404s with a coded JSON error", async ({ request }) => {
    const cases: Array<[string, string]> = [
      ["images", "image not found"],
      ["documents", "document not found"],
      ["snippets", "snippet not found"],
    ];
    for (const [kind, msg] of cases) {
      const res = await request.get(`/api/v2/${kind}/99999/`);
      expect(res.status(), kind).toBe(404);
      expect(await res.json(), kind).toEqual({
        error: { code: "not_found", message: msg },
      });
    }
  });

  test("?fields= narrows a detail response on every type", async ({ request }) => {
    // Sparse selection used to be list-only, so a client fetching one row
    // had no way to trim the payload.
    for (const kind of ["images", "documents", "snippets"]) {
      const list = await getJson(request, `/api/v2/${kind}/?limit=1`);
      if (!list.items.length) continue;
      const id = list.items[0].id;
      const body = await getJson(request, `/api/v2/${kind}/${id}/?fields=title`);
      expect(Object.keys(body).sort(), kind).toEqual(["id", "meta", "title"]);
    }
  });

  test("[G33] form definitions are exposed unauthenticated with their full schema", async ({
    request,
  }) => {
    // `form`-typed snippets are the form builder's storage. Whatever a
    // tenant has built is readable by anyone, including field names and
    // validation rules.
    const body = await getJson(request, "/api/v2/snippets/?type=form&limit=10");
    expect(body.items.length, "the fixture's form snippet is listed").toBeGreaterThan(0);
    expect(body.items[0]).toHaveProperty("data");
  });

  test("a gated collection's assets are hidden from anonymous listings", async ({
    request: r,
    baseURL,
  }) => {
    // `/__media__/` already refused to serve the bytes; the listings
    // published the catalogue — title, filename, dimensions — anyway.
    const images = (await getJson(r, "/api/v2/images/?limit=100")).items.map((i: any) => i.title);
    expect(images).toContain("T Docs diagram");
    expect(images).not.toContain("T Gated image");

    const docs = (await getJson(r, "/api/v2/documents/?limit=100")).items.map((i: any) => i.title);
    expect(docs).toContain("T Docs handbook");
    expect(docs).not.toContain("T Gated doc");

    const m = await request.newContext({ baseURL, storageState: ".state/member.json" });
    const asMember = (await getJson(m, "/api/v2/images/?limit=100")).items.map(
      (i: any) => i.title,
    );
    expect(asMember, "a signed-in member sees them").toContain("T Gated image");
  });

  test("a gated asset's detail 404s exactly like a missing one", async ({
    request: r,
    baseURL,
  }) => {
    // Anything else tells an anonymous caller which ids exist behind the
    // gate.
    const gated = await r.get(`/api/v2/images/${f.media.gatedImage}/`);
    const missing = await r.get("/api/v2/images/99999/");
    expect(gated.status()).toBe(404);
    expect(await gated.json()).toEqual(await missing.json());

    const gatedDoc = await r.get(`/api/v2/documents/${f.media.gatedDoc}/`);
    expect(gatedDoc.status()).toBe(404);

    // Open assets still resolve, and a member reaches the gated one.
    expect((await r.get(`/api/v2/images/${f.media.openImage}/`)).status()).toBe(200);
    const m = await request.newContext({ baseURL, storageState: ".state/member.json" });
    expect((await m.get(`/api/v2/images/${f.media.gatedImage}/`)).status()).toBe(200);
  });
});
