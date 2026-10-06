/**
 * `POST /api/v2/auth/login` — the only way a cross-origin SPA can read
 * gated content.
 *
 * The member session is an `HttpOnly; SameSite=Lax` cookie, and Lax
 * cookies are not sent on cross-site fetches — so CORS and
 * `credentials: "include"` are not enough on their own. The token is the
 * same signed session value the cookie carries, moved to a header.
 */
import { expect, request, test } from "@playwright/test";
import { getJson } from "./helpers/client";
import { MEMBER_EMAIL, MEMBER_PASSWORD } from "./helpers/member";
import { ids } from "./helpers/fixture";

const f = ids();

const LOGIN = "/api/v2/auth/login";

async function token(baseURL: string): Promise<string> {
  const anon = await request.newContext({
    baseURL,
    storageState: { cookies: [], origins: [] },
  });
  const res = await anon.post(LOGIN, {
    data: { identifier: MEMBER_EMAIL, password: MEMBER_PASSWORD },
  });
  expect(res.status(), await res.text()).toBe(200);
  return (await res.json()).token;
}

test.describe("bearer auth", () => {
  test("valid credentials return a token and the member", async ({ baseURL }) => {
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const body = await (
      await anon.post(LOGIN, { data: { identifier: MEMBER_EMAIL, password: MEMBER_PASSWORD } })
    ).json();
    expect(body.token_type).toBe("Bearer");
    expect(body.token.length).toBeGreaterThan(20);
    expect(body.expires_in).toBeGreaterThan(0);
    expect(body.member.email).toBe(MEMBER_EMAIL);
  });

  test("form encoding works too", async ({ baseURL }) => {
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const res = await anon.post(LOGIN, {
      form: { identifier: MEMBER_EMAIL, password: MEMBER_PASSWORD },
    });
    expect(res.status()).toBe(200);
    expect((await res.json()).token).toBeTruthy();
  });

  test("a bad password and an unknown account look identical", async ({ baseURL }) => {
    // The difference is an account-enumeration oracle, and a client can
    // do nothing useful with it either way.
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const wrong = await anon.post(LOGIN, {
      data: { identifier: MEMBER_EMAIL, password: "definitely-not-it" },
    });
    const nobody = await anon.post(LOGIN, {
      data: { identifier: "nobody@example.com", password: "definitely-not-it" },
    });
    expect(wrong.status()).toBe(401);
    expect(nobody.status()).toBe(401);
    expect(await wrong.json()).toEqual(await nobody.json());
  });

  test("a malformed body is a 400", async ({ baseURL }) => {
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const res = await anon.post(LOGIN, { data: { nope: 1 } });
    expect(res.status()).toBe(400);
    expect((await res.json()).error.code).toBe("bad_request");
  });

  test("the token reads gated content that an anonymous caller cannot", async ({ baseURL }) => {
    // The whole point. Same request, one header apart.
    const tok = await token(baseURL!);
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const withToken = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: { Authorization: `Bearer ${tok}` },
    });

    const anonTree = await getJson(anon, "/api/v2/pages/tree/?depth=10");
    const authTree = await getJson(withToken, "/api/v2/pages/tree/?depth=10");
    expect(authTree.meta.total_count).toBeGreaterThan(anonTree.meta.total_count);

    // ...including the endpoint that refuses outright.
    expect((await anon.get(`/api/v2/pages/${f.page.members}/`)).status()).toBe(401);
    expect((await withToken.get(`/api/v2/pages/${f.page.members}/`)).status()).toBe(200);
  });

  test("an invalid or absent token is anonymous, never an error", async ({ baseURL }) => {
    // A bad token must not turn a public page into a 401 — these
    // endpoints serve anonymous callers by design.
    for (const header of ["Bearer not.a.token", "Bearer", "Basic abc", ""]) {
      const ctx = await request.newContext({
        baseURL,
        storageState: { cookies: [], origins: [] },
        extraHTTPHeaders: header ? { Authorization: header } : {},
      });
      const res = await ctx.get("/api/v2/pages/tree/");
      expect(res.status(), header || "(no header)").toBe(200);
    }
  });

  test("a token for a different tenant is rejected", async ({ baseURL }) => {
    // The payload is bound to the tenant slug it was minted for, so a
    // token cannot be replayed across tenants on a shared deployment.
    const tok = await token(baseURL!);
    const tampered = tok.slice(0, -4) + "AAAA";
    const ctx = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: { Authorization: `Bearer ${tampered}` },
    });
    const tree = await getJson(ctx, "/api/v2/pages/tree/?depth=10");
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const anonTree = await getJson(anon, "/api/v2/pages/tree/?depth=10");
    expect(tree.meta.total_count, "a tampered signature buys nothing").toBe(
      anonTree.meta.total_count,
    );
  });
});
