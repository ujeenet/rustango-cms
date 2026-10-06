import { test as setup, expect } from "@playwright/test";

// Signs in once; every chromium-project spec reuses the saved session.
setup("authenticate as admin", async ({ page }) => {
  await page.goto("/login");
  await expect(page.getByRole("heading", { name: "Welcome back" })).toBeVisible();
  await page.getByLabel(/username/i).or(page.locator('input[name="username"]')).first().fill("admin");
  await page.locator('input[type="password"]').fill("TestPw123!");
  await page.locator('button[type="submit"]').click();
  await page.waitForURL("**/cms-admin/**");
  await page.context().storageState({ path: ".state/admin.json" });
});
