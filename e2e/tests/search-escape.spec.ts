import { test, expect } from "@playwright/test";

// #713 — the header search dropdown put page titles into innerHTML raw, so
// an author could plant markup that ran in whoever searched for it.
test("the header search dropdown shows a markup title as text", async ({ page }) => {
  const stamp = `xss${Date.now()}`;
  const title = `${stamp} <img src=x data-planted onerror="window.__planted=1">`;

  await page.goto("/cms-admin/pages");
  await page.getByRole("link", { name: /new root page/i }).click();
  await page.getByRole("link", { name: /sectioned page/i }).first().click();
  await page.locator('input[name="title"]').fill(title);
  await page.locator('input[name="slug"]').fill(stamp);
  await page.locator('button[form="page-edit-form"][value="save"]').click();
  await page.waitForURL(/\/cms-admin\/pages(?!\/new)/);

  await page.locator("[data-search-input]").fill(stamp);
  const hit = page.locator("[data-search-dropdown] [data-search-hit]").first();
  await expect(hit).toContainText(title);
  await expect(page.locator("[data-search-dropdown] img[data-planted]")).toHaveCount(0);
  expect(await page.evaluate(() => (window as unknown as { __planted?: number }).__planted)).toBeUndefined();
});
