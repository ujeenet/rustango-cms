/**
 * The leak matrix.
 *
 * The fixture gates `t-members` with a `login` restriction; `t-secret`
 * sits under it and inherits the gate through the materialized path. For
 * every read surface, the question is the same: can an anonymous caller
 * see either of them?
 *
 * Each test builds both personas itself rather than relying on the
 * project's storage state, so anonymous-vs-member is a direct comparison
 * inside one assertion instead of two runs a reader has to correlate.
 */
import { expect, request, test } from "@playwright/test";
import { getJson, json, titles } from "./helpers/client";
import { ids } from "./helpers/fixture";

const f = ids();
const GATED = "T Members";
const NESTED = "T Secret";

const member = (baseURL: string) =>
  request.newContext({ baseURL, storageState: ".state/member.json" });
const anon = (baseURL: string) =>
  request.newContext({ baseURL, storageState: { cookies: [], origins: [] } });

test.describe("view restrictions", () => {
  test("tree hides the gated subtree from anonymous and shows it to a member", async ({
    baseURL,
  }) => {
    const a = await anon(baseURL!);
    const m = await member(baseURL!);

    const anonTitles = titles((await getJson(a, "/api/v2/pages/tree/?depth=10")).items);
    const memberTitles = titles((await getJson(m, "/api/v2/pages/tree/?depth=10")).items);

    expect(anonTitles, "gated page leaked to anonymous").not.toContain(GATED);
    expect(anonTitles, "page under a gated ancestor leaked").not.toContain(NESTED);
    expect(memberTitles).toContain(GATED);
    expect(memberTitles).toContain(NESTED);

    // The two responses must differ *only* by the gated subtree — if the
    // member view also lost or gained something else, the filter is doing
    // more than it claims.
    const extra = memberTitles.filter((t) => !anonTitles.includes(t)).sort();
    expect(extra).toEqual([NESTED, GATED].sort());
  });

  test("?root= on a gated page is a 404, not a 403", async ({ baseURL }) => {
    // Anti-probe: unknown, non-public and denied must be indistinguishable.
    const a = await anon(baseURL!);
    const gated = await a.get(`/api/v2/pages/tree/?root=${f.page.members}`);
    const missing = await a.get("/api/v2/pages/tree/?root=99999");
    const draft = await a.get(`/api/v2/pages/tree/?root=${f.page.draft}`);
    expect(gated.status()).toBe(404);
    expect(missing.status()).toBe(404);
    expect(draft.status()).toBe(404);
    expect(await gated.text()).toBe(await missing.text());
  });

  test("child summaries hide a gated child from anonymous", async ({ baseURL }) => {
    const a = await anon(baseURL!);
    const m = await member(baseURL!);
    const anonKids = (await getJson(a, `/api/v2/pages/${f.page.home}/`)).children.map(
      (c: any) => c.title,
    );
    const memberKids = (await getJson(m, `/api/v2/pages/${f.page.home}/`)).children.map(
      (c: any) => c.title,
    );
    expect(anonKids).not.toContain(GATED);
    expect(memberKids).toContain(GATED);
  });

  test("menus hide an item pointing at a gated page", async ({ baseURL }) => {
    const a = await anon(baseURL!);
    const m = await member(baseURL!);
    const anonLabels = (await getJson(a, "/api/v2/menus/t-main/")).items.map((i: any) => i.label);
    const memberLabels = (await getJson(m, "/api/v2/menus/t-main/")).items.map(
      (i: any) => i.label,
    );
    expect(anonLabels).not.toContain("T Members link");
    expect(memberLabels).toContain("T Members link");
  });

  test("detail refuses a gated page to anonymous and serves it to a member", async ({
    baseURL,
  }) => {
    const m = await member(baseURL!);
    const ok = await m.get(`/api/v2/pages/${f.page.members}/`);
    expect(ok.status()).toBe(200);

    // Anonymous must NOT get the content. What it gets instead is B4's
    // problem, asserted separately — here we only require "not the page".
    const noRedirect = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
      maxRedirects: 0,
    });
    const denied = await noRedirect.get(`/api/v2/pages/${f.page.members}/`);
    expect(denied.status()).not.toBe(200);
  });

  // -------------------------------------------------------------------
  // The flat list and its filters. These were the widest leak in the
  // API: no viewer, no restriction check, and `?descendant_of=` against
  // a member-only section returned the entire subtree.
  // -------------------------------------------------------------------

  test("list hides gated pages and shows them to a member", async ({ baseURL }) => {
    const a = await anon(baseURL!);
    const m = await member(baseURL!);
    const anonTitles = (await getJson(a, "/api/v2/pages/?limit=100&search=T Members")).items.map(
      (i: any) => i.title,
    );
    const memberTitles = (
      await getJson(m, "/api/v2/pages/?limit=100&search=T Members")
    ).items.map((i: any) => i.title);
    expect(anonTitles).not.toContain(GATED);
    expect(memberTitles).toContain(GATED);
  });

  test("?descendant_of= on a gated anchor returns nothing to anonymous", async ({ baseURL }) => {
    // The anchor is resolved against the *visible* set, so a caller
    // cannot pivot off a page they can't see — `tree/?root=` refuses this
    // same anchor.
    const a = await anon(baseURL!);
    const m = await member(baseURL!);
    const anonBody = await getJson(a, `/api/v2/pages/?descendant_of=${f.page.members}&limit=100`);
    expect(anonBody.items).toEqual([]);
    const memberBody = await getJson(
      m,
      `/api/v2/pages/?descendant_of=${f.page.members}&limit=100`,
    );
    expect(memberBody.items.map((i: any) => i.title)).toContain(NESTED);
  });

  test("?child_of= and ?ancestor_of= are gated the same way", async ({ baseURL }) => {
    const a = await anon(baseURL!);
    expect((await getJson(a, `/api/v2/pages/?child_of=${f.page.members}`)).items).toEqual([]);
    // t-secret sits under the gated subtree, so its ancestor chain is
    // not walkable by an anonymous caller either.
    expect((await getJson(a, `/api/v2/pages/?ancestor_of=${f.page.secret}`)).items).toEqual([]);
  });

  test("find/ cannot distinguish a gated page from a missing one", async ({ baseURL }) => {
    const a = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
      maxRedirects: 0,
    });
    const gated = await a.get("/api/v2/pages/find/?html_path=/t-home/t-members");
    const missing = await a.get("/api/v2/pages/find/?html_path=/t-home/does-not-exist");
    expect(gated.status()).toBe(missing.status());
    expect(gated.status()).toBe(404);
    // ...and a member still resolves it.
    const m = await request.newContext({
      baseURL,
      storageState: ".state/member.json",
      maxRedirects: 0,
    });
    const found = await m.get("/api/v2/pages/find/?html_path=/t-home/t-members");
    expect(found.status()).toBe(302);
    expect(found.headers()["location"]).toBe(`/api/v2/pages/${f.page.members}/`);
  });

  test("a gated page is refused with a status a client can branch on", async ({ baseURL }) => {
    // Was a 303 into an HTML login page, which `fetch()` followed into a
    // 200 the client could not distinguish from success.
    const a = await anon(baseURL!);
    const res = await a.get(`/api/v2/pages/${f.page.members}/`);
    expect(res.status()).toBe(401);
    expect((await res.json()).error.code).toBe("unauthenticated");
  });

  test("?current= cannot be used to probe for hidden pages", async ({ baseURL }) => {
    // Echoing the id back only for rows that exist told an anonymous
    // caller whether a given id is a real draft or member-only page, and
    // `in_active_trail` then revealed which published subtree an
    // unreleased draft lives under.
    const a = await anon(baseURL!);
    const nonexistent = await getJson(a, "/api/v2/menus/t-main/?current=99999");
    for (const hidden of [f.page.draft, f.page.members, f.page.secret]) {
      const probe = await getJson(a, `/api/v2/menus/t-main/?current=${hidden}`);
      expect(probe.meta.current, `page ${hidden} leaked through ?current=`).toBe(
        nonexistent.meta.current,
      );
    }

    // A page the caller may actually see still marks the trail.
    const visible = await getJson(a, `/api/v2/menus/t-main/?current=${f.page.docs}`);
    expect(visible.meta.current).toBe(f.page.docs);

    // ...and a member gets the marking for the gated one.
    const m = await member(baseURL!);
    const asMember = await getJson(m, `/api/v2/menus/t-main/?current=${f.page.members}`);
    expect(asMember.meta.current).toBe(f.page.members);
  });
});
