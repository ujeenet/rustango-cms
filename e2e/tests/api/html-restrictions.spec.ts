/**
 * View-restriction enforcement on the public HTML and media paths (#685).
 *
 * `security.spec.ts` covers the JSON API. This covers what a browser gets:
 * the rendered page (`t-members`, and `t-secret` beneath it, gated by
 * inheritance) and the bytes of media in the login-gated collection, both
 * as the original file and as a rendition. Anonymous callers must be
 * turned away; a member must get through. Deleting the page guard or the
 * collection check now fails here.
 */
import { expect, test } from "@playwright/test";
import { ids } from "./helpers/fixture";

const f = ids();
const GATED = "/t-home/t-members";
const NESTED = "/t-home/t-members/t-secret";
const OPEN = "/t-home/t-about";

const denied = (status: number) => [302, 303, 401, 403].includes(status);

test.describe("public HTML and media honour view restrictions", () => {
  test("an anonymous visitor is turned away from a gated page and its subtree", async ({ request }) => {
    expect((await request.get(OPEN)).status(), "control: the open page renders").toBe(200);
    for (const path of [GATED, NESTED]) {
      const res = await request.get(path, { maxRedirects: 0 });
      expect(denied(res.status()), `${path} answered ${res.status()} to an anonymous visitor`).toBe(true);
      expect(await res.text(), `${path} leaked its title`).not.toContain(path === GATED ? "T Members" : "T Secret");
    }
  });

  test("a member reads the gated page and its subtree @member", async ({ request }) => {
    const gated = await request.get(GATED);
    expect(gated.status()).toBe(200);
    expect(await gated.text()).toContain("T Members");
    expect((await request.get(NESTED)).status()).toBe(200);
  });

  test("an anonymous visitor cannot fetch media in a gated collection", async ({ request }) => {
    for (const path of [
      `/__media__/raw/${f.media.gatedDoc}`,
      `/__media__/raw/${f.media.gatedImage}`,
      `/__media__/width-100/${f.media.gatedImage}`,
    ]) {
      const res = await request.get(path, { maxRedirects: 0 });
      expect(denied(res.status()), `${path} answered ${res.status()} to an anonymous visitor`).toBe(true);
    }
    // Control: an open asset is not refused on permission grounds (the
    // fixture has no bytes on disk, so it may 404 — but never 401/403).
    const open = await request.get(`/__media__/raw/${f.media.openDoc}`, { maxRedirects: 0 });
    expect([401, 403]).not.toContain(open.status());
  });

  test("a member is not refused media in the gated collection @member", async ({ request }) => {
    const res = await request.get(`/__media__/raw/${f.media.gatedDoc}`, { maxRedirects: 0 });
    // No bytes on disk in the fixture, so 404 is the success signal here:
    // the request got past the restriction check to the file lookup.
    expect([401, 403]).not.toContain(res.status());
  });
});
