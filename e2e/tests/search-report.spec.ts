import { test, expect } from "@playwright/test";

// #847 — searches made through the public API show up in the admin's
// search report, not only searches typed into the admin.
test("a visitor's API search appears in the search report", async ({ page, playwright, baseURL }) => {
  const needle = `teapot${Date.now()}`;
  // A visitor with no session.
  const visitor = await playwright.request.newContext({ baseURL });
  expect((await visitor.get(`/api/v2/search/?q=${needle}`)).status()).toBe(200);
  // Paging through results is not a new search.
  await visitor.get(`/api/v2/search/?q=${needle}x&offset=20`);
  await visitor.dispose();

  await expect(async () => {
    await page.goto("/cms-admin/reports/search");
    await expect(page.getByText(needle, { exact: true })).toBeVisible({ timeout: 1000 });
  }).toPass({ timeout: 10000 });
  await expect(page.getByText(`${needle}x`)).toHaveCount(0);
});
