/**
 * #691 — a sitemap shard fetches only its own window of pages. Under one
 * shard's worth of URLs, shard 1 must list exactly what the flat sitemap
 * lists, and the shard after it must not exist.
 */
import { expect, test } from "@playwright/test";

const locs = (xml: string) => [...xml.matchAll(/<loc>([^<]+)<\/loc>/g)].map((m) => m[1]).sort();

test("shard 1 lists the same URLs as the flat sitemap", async ({ request }) => {
  const flat = await request.get("/sitemap.xml");
  expect(flat.status()).toBe(200);
  const shard = await request.get("/sitemap/1");
  expect(shard.status()).toBe(200);
  const want = locs(await flat.text());
  expect(want.length, "the fixture has public pages").toBeGreaterThan(0);
  expect(locs(await shard.text())).toEqual(want);
  expect((await request.get("/sitemap/2")).status()).toBe(404);
});
