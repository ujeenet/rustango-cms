/**
 * Seeds the API fixture and mints the member session the leak matrix
 * needs.
 *
 * Runs as its own Playwright project so both happen exactly once, before
 * any spec, regardless of which project a spec belongs to.
 */
import { expect, test as setup } from "@playwright/test";
import { seed } from "./helpers/fixture";
import { MEMBER_EMAIL, MEMBER_PASSWORD } from "./helpers/member";

const DB = process.env.E2E_DB ?? ".state/demo.db";
export { MEMBER_EMAIL, MEMBER_PASSWORD } from "./helpers/member";

setup("seed the API fixture", async () => {
  const f = seed(DB);
  expect(f.page.home).toBeGreaterThan(0);
});

/**
 * A real member, registered through the real signup form.
 *
 * The whole point of the leak matrix is comparing an anonymous caller
 * with an authenticated one, and a hand-forged cookie would prove
 * nothing about how the server actually authenticates. `/members/signup`
 * mints a member session and sets it on the context, which we save.
 */
setup("register a member", async ({ page, baseURL }) => {
  // The signup form is CSRF-protected. Load it TWICE: on a first-ever
  // visit the template renders a freshly minted token while the CSRF
  // layer sets a *different* one on the response, so the first submit
  // always 403s (B59). The second load reads the cookie and the two
  // agree. This is a real bug, pinned in spa-integration.spec.ts — the
  // reload here is the workaround, not the assertion.
  await page.goto("/members/signup");
  await page.reload();

  const alreadyIn = page.url().includes("/members/signup") === false;
  if (alreadyIn) {
    await page.context().storageState({ path: ".state/member.json" });
    return;
  }

  await page.locator('input[name="email"]').fill(MEMBER_EMAIL);
  await page.locator('input[name="password"]').fill(MEMBER_PASSWORD);
  await page.locator('input[name="password_confirm"]').fill(MEMBER_PASSWORD);
  const displayName = page.locator('input[name="display_name"]');
  if (await displayName.count()) await displayName.fill("API Tester");
  await page.locator('form button[type="submit"]').click();

  // Signup either signs us straight in, or bounces back with
  // `?error=email_taken` on a re-run against a warm server. Both are
  // fine; the second just needs a login instead.
  await page.waitForLoadState("networkidle");

  // Signup bounces back to the form with `?error=<reason>` on any
  // validation or server failure. `email_taken` just means a warm server
  // — log in instead. Anything else is a real problem and should say so.
  const bounced = new URL(page.url()).searchParams.get("error");
  if (bounced && bounced !== "email_taken") {
    throw new Error(`member signup rejected: ${bounced} (${page.url()})`);
  }
  if (bounced === "email_taken") {
    await page.goto("/members/login");
    await page.locator('input[name="email"]').fill(MEMBER_EMAIL);
    await page.locator('input[name="password"]').fill(MEMBER_PASSWORD);
    await page.locator('form button[type="submit"]').click();
    await page.waitForLoadState("networkidle");
  }

  const cookies = await page.context().cookies();
  const session = cookies.find((c) => c.name.includes("member"));
  expect(
    session,
    `no member session cookie after signup/login at ${baseURL}. ` +
      `Landed on ${page.url()} with cookies [${cookies.map((c) => c.name).join(", ")}]`,
  ).toBeTruthy();

  await page.context().storageState({ path: ".state/member.json" });
});
