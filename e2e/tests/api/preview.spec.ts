/**
 * Headless preview: an editor previewing a draft in a separate frontend.
 *
 * The token machinery already worked, but the mint endpoint lives inside
 * the session-gated admin — so the only way to use it was to copy a
 * token out by hand, which meant decoupled sites had no preview at all.
 *
 * Runs as `@admin`: the editor half needs a session, and the frontend
 * half deliberately does not.
 */
import { expect, request, test } from "@playwright/test";
import { ids } from "./helpers/fixture";

const f = ids();

/**
 * The "Preview on site" action.
 *
 * A DOM locator, not `getByRole`: the action lives inside a closed
 * `<details>` kebab menu, so it is hidden from the accessibility tree and
 * a role query matches nothing whether or not the link exists — which
 * would make the "no link when unconfigured" test pass vacuously.
 */
const previewLink = (page: any) =>
  page.locator('a[role="menuitem"]', { hasText: "Preview on site" });

/** Point the tenant at a frontend, the way an editor would in Settings. */
async function configureFrontend(page: any, template: string) {
  await page.goto("/cms-admin/site-settings/preview/edit");
  const input = page.locator('input[name="base_url"]');
  await expect(input, "the Headless preview setting renders a form").toBeVisible();
  await input.fill(template);
  // Scoped to the settings form: the sidebar's Sign-out form is first in
  // the DOM, so `.first()` on a bare submit button logs you out instead
  // — the trap e2e/README.md warns about.
  await page.locator('form:has(input[name="base_url"]) button[type="submit"]').click();
  await page.waitForLoadState("networkidle");

  // Confirm it round-tripped. Without this a save that silently failed
  // would surface later as "the link is missing", which points at the
  // wrong half of the feature.
  await page.goto("/cms-admin/site-settings/preview/edit");
  await expect(page.locator('input[name="base_url"]')).toHaveValue(template);
}

test.describe("@admin headless preview", () => {
  test("no frontend configured means no link", async ({ page }) => {
    // The action must not appear as a dead link on a CMS-rendered site.
    await configureFrontend(page, "");
    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);
    await expect(previewLink(page)).toHaveCount(0);
  });

  test("a configured frontend gets a link carrying a token", async ({ page, baseURL }) => {
    await configureFrontend(
      page,
      "https://frontend.example.com/api/preview?token={token}&path={path}",
    );
    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);

    const link = previewLink(page);
    await expect(link).toHaveCount(1);
    const href = await link.getAttribute("href");
    expect(href).toBeTruthy();

    const url = new URL(href!);
    expect(url.origin + url.pathname).toBe("https://frontend.example.com/api/preview");
    const token = url.searchParams.get("token");
    expect(token, "the link carries a token").toBeTruthy();
    expect(url.searchParams.get("path")).toBe("/t-home/t-draft");

    // The frontend's half: no admin session, just the token.
    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    const draft = await anon.get(
      `/api/v2/pages/${f.page.draft}/?preview_token=${encodeURIComponent(token!)}`,
    );
    expect(draft.status(), "the token reads the draft").toBe(200);
    expect((await draft.json()).title).toBe("T Draft");

    // ...and nothing else does.
    expect((await anon.get(`/api/v2/pages/${f.page.draft}/`)).status()).toBe(404);
  });

  test("a token unlocks only the page it was minted for", async ({ page, baseURL }) => {
    await configureFrontend(page, "https://frontend.example.com/p?token={token}");
    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);
    const href = await previewLink(page).getAttribute("href");
    const token = new URL(href!).searchParams.get("token")!;

    const anon = await request.newContext({
      baseURL,
      storageState: { cookies: [], origins: [] },
    });
    // It reads the page it was minted for...
    expect(
      (await anon.get(`/api/v2/pages/${f.page.draft}/?preview_token=${token}`)).status(),
    ).toBe(200);

    // ...and not another non-public page. `expired` is the honest
    // target: it is 404 anonymously, so a 404 here means the token was
    // rejected, not that the page was public anyway.
    expect(
      (await anon.get(`/api/v2/pages/${f.page.expired}/`)).status(),
      "expired is hidden without a token, so this is a real test",
    ).toBe(404);
    expect(
      (await anon.get(`/api/v2/pages/${f.page.expired}/?preview_token=${token}`)).status(),
      "a token minted for one page must not unlock another",
    ).toBe(404);

    // A tampered signature is worth no more than no token at all.
    // Flip the last hex digit, so the result always differs — a fixed
    // suffix equals the real signature 1 time in 256.
    const last = token.slice(-1);
    const tampered = `${token.slice(0, -1)}${last === "0" ? "1" : "0"}`;
    expect(
      (await anon.get(`/api/v2/pages/${f.page.draft}/?preview_token=${tampered}`)).status(),
      "a tampered signature must not unlock a draft",
    ).toBe(404);
  });

  test("{path} in the path position keeps its separators", async ({ page }) => {
    // The template a decoupled site actually configures. Escaping the
    // separators produced `http://host%2Ffeatures%2Fai-engine`, whose
    // authority is nonsense and which no browser can follow — and the
    // unit test that claimed to cover "placeholder in the path" used
    // `{id}`, a bare number that encoding cannot change, so it missed it.
    await configureFrontend(page, "http://localhost:5173{path}?token={token}");
    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);
    const href = await previewLink(page).getAttribute("href");

    expect(href, "separators must survive").toContain("/t-home/t-draft?token=");
    expect(href, "and must not be escaped").not.toContain("%2F");

    const url = new URL(href!);
    expect(url.host).toBe("localhost:5173");
    expect(url.pathname).toBe("/t-home/t-draft");
    expect(url.searchParams.get("token")).toBeTruthy();
  });

  /**
   * Set the page's own frontend route.
   *
   * The field lives in the **Promote** tab, whose panel is hidden until
   * the tab is clicked — `fill` refuses an invisible input, so driving
   * this the way an editor does is not optional here.
   */
  async function setFrontendRoute(page: any, id: number, value: string) {
    await page.goto(`/cms-admin/pages/${id}/edit`);
    await page.getByRole("tab", { name: /promote/i }).click();
    const input = page.locator('input[name="preview_path"]');
    await expect(input).toBeVisible();
    await input.fill(value);
    // The save buttons sit in the topbar, *outside* the form, associated
    // by `form="page-edit-form"` — so a `form:has(…)` scope cannot reach
    // them. Target that association, and the `save` action specifically:
    // an unscoped submit would hit the sidebar's Sign-out form instead.
    await page.locator('button[type="submit"][form="page-edit-form"][value="save"]').click();
    await page.waitForLoadState("networkidle");
    // Confirm it round-tripped, so a silently-failed save surfaces here
    // rather than later as "the override did nothing".
    await page.goto(`/cms-admin/pages/${id}/edit`);
    await page.getByRole("tab", { name: /promote/i }).click();
    await expect(page.locator('input[name="preview_path"]')).toHaveValue(value);
  }

  test("a page can override the route the frontend uses", async ({ page }) => {
    // A decoupled renderer need not mirror the CMS's URL structure —
    // `/t-home/t-draft` here can be `/product/whatever` there. Without a
    // per-page override the site-wide `{path}` silently assumes it does.
    await configureFrontend(page, "http://localhost:5173{path}?token={token}");
    await setFrontendRoute(page, f.page.draft, "/product/ai-engine");

    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);
    const href = await previewLink(page).getAttribute("href");
    const url = new URL(href!);
    expect(url.pathname, "the page's own route wins").toBe("/product/ai-engine");
    expect(url.host, "the host still comes from the site template").toBe("localhost:5173");
    expect(url.searchParams.get("token"), "and so does the token").toBeTruthy();

    // Clearing it falls back to the CMS path.
    await setFrontendRoute(page, f.page.draft, "");
    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);
    const back = new URL((await previewLink(page).getAttribute("href"))!);
    expect(back.pathname).toBe("/t-home/t-draft");
  });

  test("an absolute override sends one page somewhere else entirely", async ({ page }) => {
    // The case a path cannot express: this page is rendered by a
    // different app. It must work even with no site-wide frontend set.
    await configureFrontend(page, "");
    await setFrontendRoute(page, f.page.draft, "https://other.example.com/x?t={token}");

    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);
    const link = previewLink(page);
    await expect(link, "an absolute override alone is enough to get a link").toHaveCount(1);
    const url = new URL((await link.getAttribute("href"))!);
    expect(url.host).toBe("other.example.com");
    expect(url.searchParams.get("t"), "placeholders still resolve").toBeTruthy();

    await setFrontendRoute(page, f.page.draft, "");
  });

  test("the editor previews the frontend in its own pane", async ({ page }) => {
    // The link opens a tab; this is the same URL embedded in the editor's
    // preview pane, so an author sees their decoupled site next to the
    // fields instead of alternating between windows.
    await configureFrontend(page, "http://localhost:5173{path}?token={token}");
    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);

    const siteBtn = page.locator('[data-preview-surface="site"]');
    await expect(siteBtn, "a third surface appears once a frontend is set").toHaveCount(1);

    const siteStage = page.locator('[data-preview-stage][data-surface="site"]');
    await expect(siteStage).toBeHidden();
    await siteBtn.click();
    await expect(siteStage).toBeVisible();
    await expect(page.locator('[data-preview-stage][data-surface="html"]')).toBeHidden();
    await expect(siteBtn).toHaveAttribute("aria-pressed", "true");

    // The frame is lazy — `data-src` until the surface is actually shown,
    // so opening the editor never pulls a whole external site into a pane
    // nobody looked at.
    const frame = siteStage.locator("iframe");
    await expect(frame).toHaveAttribute("src", /token=/);

    // Width checks apply here too, not just to the CMS-rendered surface.
    await expect(page.locator("[data-viewport-group]")).toBeVisible();
    await page.locator('[data-viewport="375"]').click();
    await expect(frame).toHaveAttribute("style", /--rcms-pv-frame-w:\s*375px/);
  });

  test("no frontend configured means no third surface", async ({ page }) => {
    // The pane must not offer a mode that leads to a blank frame.
    await configureFrontend(page, "");
    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);
    await expect(page.locator('[data-preview-surface="site"]')).toHaveCount(0);
    await expect(page.locator('[data-preview-stage][data-surface="site"]')).toHaveCount(0);
  });

  test("a plain base URL works without placeholders", async ({ page }) => {
    // The simplest thing an editor might paste.
    await configureFrontend(page, "https://frontend.example.com/preview");
    await page.goto(`/cms-admin/pages/${f.page.draft}/edit`);
    const href = await previewLink(page).getAttribute("href");
    const url = new URL(href!);
    expect(url.searchParams.get("token")).toBeTruthy();
    expect(url.searchParams.get("path")).toBe("/t-home/t-draft");
    expect(url.searchParams.get("id")).toBe(String(f.page.draft));
  });
});
