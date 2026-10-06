import { test, expect } from "@playwright/test";

// Auth flows run WITHOUT the shared session — a fresh context per test.
test.use({ storageState: { cookies: [], origins: [] } });

test.describe("authentication", () => {
  test("anonymous /cms-admin redirects to the login gate", async ({ page }) => {
    await page.goto("/cms-admin/pages");
    await page.waitForURL(/\/login\?next=/);
    await expect(page.getByRole("heading", { name: "Welcome back" })).toBeVisible();
  });

  test("wrong password bounces back with a friendly rcms-error", async ({ page }) => {
    await page.goto("/login");
    await page.locator('input[name="username"]').fill("admin");
    await page.locator('input[type="password"]').fill("nope-nope");
    await page.locator('button[type="submit"]').click();
    await page.waitForURL(/\/login/);
    await expect(page.locator(".rcms-error, .rcms-alert, [class*=error]").first()).toBeVisible();
  });

  test("valid credentials land in the admin, sign-out gates again", async ({ page }) => {
    await page.goto("/login");
    await page.locator('input[name="username"]').fill("admin");
    await page.locator('input[type="password"]').fill("TestPw123!");
    await page.locator('button[type="submit"]').click();
    await page.waitForURL("**/cms-admin/**");
    await expect(page.locator(".rcms-sidebar-brand, aside, nav").first()).toBeVisible();

    // Sign out (sidebar form POST) → the admin gates again.
    await page.locator('button.rcms-sidebar-logout[type="submit"], form[action*="logout"] button').first().click();
    await page.goto("/cms-admin/pages");
    await page.waitForURL(/\/login/);
  });

  test("media picker endpoint is login-gated", async ({ request }) => {
    const res = await request.get("/cms-admin/__media-picker", { maxRedirects: 0 });
    expect([302, 303, 307]).toContain(res.status());
  });
});
